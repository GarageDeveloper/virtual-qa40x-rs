//! The bootloader persona: an emulated NXP MCUBOOT/KBOOT USB-HID device
//! (VID 0x1FC9, PID 0x0022), as the QA40x re-enumerates after the register
//! 0x0F magic.
//!
//! Protocol (confirmed by the flash captures + NXP references):
//! * HID reports, 60 bytes: `[report id][0x00][len:2 LE][payload][zero pad]`.
//!   Report ids: 1 = command OUT, 2 = data OUT, 3 = response IN, 4 = data IN.
//! * `ReceiveSbFile` (tag 0x08, param0 = byte count) starts the transfer; the
//!   SB2.1 image follows in data reports (28 payload bytes each); the target
//!   answers with generic responses (tag 0xA0, status u32 LE) after the
//!   command and after the last byte.
//!
//! On a successful "flash" the persona validates the SB2.1 shape ("STMP" at
//! offset 0x14), reports the image to the simulator (sha256/registry lookup
//! decides the new firmware version) and "resets" back into the analyzer.

use crate::backend::{DeviceSummary, SetupPacket, Stall, UsbBackend};
use crate::identity::{bootloader_identity, Identity};
use crate::options::SimOptions;
use crate::sim::SimEvent;
use async_trait::async_trait;
use log::{info, warn};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

const REPORT_SIZE: usize = 60;

const REPORT_CMD_OUT: u8 = 1;
const REPORT_DATA_OUT: u8 = 2;
const REPORT_RESPONSE_IN: u8 = 3;

const CMD_RECEIVE_SB_FILE: u8 = 0x08;
const CMD_RESET: u8 = 0x0B;
const RESPONSE_GENERIC: u8 = 0xA0;

/// kStatus codes (KBOOT).
const STATUS_SUCCESS: u32 = 0;
const STATUS_ROM_LDR_SIGNATURE: u32 = 10101;
const STATUS_UNKNOWN_COMMAND: u32 = 10003;

enum Phase {
    Idle,
    Receiving { expected: usize, image: Vec<u8> },
    Done,
}

struct BState {
    phase: Phase,
    /// Queued IN reports (responses), already framed to 60 bytes.
    responses: VecDeque<Vec<u8>>,
    /// Simulated flash-write pacing: delay applied per data report
    /// (flash_secs spread over the announced report count).
    per_report: Duration,
}

pub struct Bootloader {
    identity: Identity,
    opts: Arc<SimOptions>,
    events: mpsc::UnboundedSender<SimEvent>,
    st: Mutex<BState>,
    in_notify: Notify,
}

impl Bootloader {
    pub fn new(opts: Arc<SimOptions>, events: mpsc::UnboundedSender<SimEvent>) -> Self {
        Self {
            identity: bootloader_identity(),
            opts,
            events,
            st: Mutex::new(BState {
                phase: Phase::Idle,
                responses: VecDeque::new(),
                per_report: Duration::ZERO,
            }),
            in_notify: Notify::new(),
        }
    }

    /// Frame a KBOOT packet into a response-IN HID report.
    fn frame_response(packet: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; REPORT_SIZE];
        r[0] = REPORT_RESPONSE_IN;
        r[1] = 0x00;
        r[2] = (packet.len() & 0xff) as u8;
        r[3] = (packet.len() >> 8) as u8;
        r[4..4 + packet.len()].copy_from_slice(packet);
        r
    }

    /// Generic response: tag 0xA0, two params (status, command tag).
    fn generic_response(status: u32, cmd_tag: u8) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(12);
        pkt.push(RESPONSE_GENERIC);
        pkt.push(0x00); // flags
        pkt.push(0x00); // reserved
        pkt.push(0x02); // paramCount
        pkt.extend_from_slice(&status.to_le_bytes());
        pkt.extend_from_slice(&(cmd_tag as u32).to_le_bytes());
        Self::frame_response(&pkt)
    }

    fn queue_response(&self, st: &mut BState, report: Vec<u8>) {
        st.responses.push_back(report);
        self.in_notify.notify_waiters();
    }

    /// Handle one OUT report (from the interrupt OUT endpoint or a SET_REPORT).
    fn handle_report(&self, data: &[u8]) {
        if data.len() < 4 {
            warn!("HID report shorter than the 4-byte framing — ignored");
            return;
        }
        let report_id = data[0];
        let declared = u16::from_le_bytes([data[2], data[3]]) as usize;
        let payload_end = (4 + declared).min(data.len());
        let payload = &data[4..payload_end];

        let mut st = self.st.lock().unwrap();
        match report_id {
            REPORT_CMD_OUT => self.handle_command(&mut st, payload),
            REPORT_DATA_OUT => self.handle_data(&mut st, payload),
            other => warn!("OUT report with unexpected id {other} — ignored"),
        }
    }

    fn handle_command(&self, st: &mut BState, pkt: &[u8]) {
        if pkt.len() < 4 {
            warn!("command packet too short — ignored");
            return;
        }
        let tag = pkt[0];
        match tag {
            CMD_RECEIVE_SB_FILE => {
                let expected = if pkt.len() >= 8 {
                    u32::from_le_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]) as usize
                } else {
                    0
                };
                info!("KBOOT ReceiveSbFile: {expected} bytes announced");
                // Spread the configured flash time over the data reports
                // (28 payload bytes each) so the host's progress bar moves
                // like a real flash.
                let reports = expected.div_ceil(28).max(1);
                st.per_report =
                    Duration::from_secs_f32(self.opts.flash_secs.max(0.0) / reports as f32);
                if !st.per_report.is_zero() {
                    info!(
                        "simulating flash write: ~{:.1} s over {reports} reports",
                        self.opts.flash_secs
                    );
                }
                st.phase = Phase::Receiving {
                    expected,
                    image: Vec::with_capacity(expected),
                };
                // Initial ack (the vendor app treats it as optional).
                let r = Self::generic_response(STATUS_SUCCESS, CMD_RECEIVE_SB_FILE);
                self.queue_response(st, r);
            }
            CMD_RESET => {
                info!("KBOOT Reset — rebooting back into the analyzer persona");
                let r = Self::generic_response(STATUS_SUCCESS, CMD_RESET);
                self.queue_response(st, r);
                let _ = self.events.send(SimEvent::BootloaderReset);
            }
            other => {
                warn!("KBOOT command 0x{other:02X} not implemented — k_UnknownCommand");
                let r = Self::generic_response(STATUS_UNKNOWN_COMMAND, other);
                self.queue_response(st, r);
            }
        }
    }

    fn handle_data(&self, st: &mut BState, payload: &[u8]) {
        let Phase::Receiving { expected, image } = &mut st.phase else {
            warn!("data report outside of a ReceiveSbFile transfer — ignored");
            return;
        };
        let expected = *expected;
        image.extend_from_slice(payload);

        if image.len() >= expected {
            image.truncate(expected);
            let image = std::mem::take(image);
            st.phase = Phase::Done;

            // Validate the SB2.1 container shape: "STMP" signature at 0x14.
            let looks_sb2 = image.len() >= 0x18 && &image[0x14..0x18] == b"STMP";
            let marker_zeroed =
                image.len() >= 4 && image[image.len() - 4] == 0 && image[image.len() - 3] == 0;
            if looks_sb2 {
                info!(
                    "SB2.1 image received: {} bytes, size−4 marker {} — flash \"succeeds\"",
                    image.len(),
                    if marker_zeroed {
                        "zeroed (wire image)"
                    } else {
                        "present (embedded image)"
                    },
                );
                let r = Self::generic_response(STATUS_SUCCESS, CMD_RECEIVE_SB_FILE);
                self.queue_response(st, r);
                let _ = self.events.send(SimEvent::FlashCompleted { image });
            } else {
                warn!(
                    "received {} bytes without an SB2.1 STMP signature — flash rejected",
                    image.len()
                );
                let r = Self::generic_response(STATUS_ROM_LDR_SIGNATURE, CMD_RECEIVE_SB_FILE);
                self.queue_response(st, r);
                st.phase = Phase::Idle;
            }
        }
    }
}

#[async_trait]
impl UsbBackend for Bootloader {
    fn summary(&self) -> DeviceSummary {
        self.identity.summary.clone()
    }

    async fn control(&self, setup: SetupPacket, data_out: &[u8]) -> Result<Vec<u8>, Stall> {
        if let Some(handled) = self.identity.handle_standard(setup) {
            return handled;
        }
        // HID class requests.
        if setup.bm_request_type & 0x60 == 0x20 {
            match setup.b_request {
                // SET_REPORT — hidapi's fallback path when there is no
                // interrupt OUT endpoint; treat like an OUT report.
                0x09 => {
                    self.handle_report(data_out);
                    return Ok(Vec::new());
                }
                // SET_IDLE / SET_PROTOCOL.
                0x0A | 0x0B => return Ok(Vec::new()),
                // GET_REPORT — serve a queued response if any.
                0x01 => {
                    let mut st = self.st.lock().unwrap();
                    let r = st
                        .responses
                        .pop_front()
                        .unwrap_or_else(|| vec![0; REPORT_SIZE]);
                    return Ok(r);
                }
                _ => {}
            }
        }
        Err(Stall)
    }

    async fn out_transfer(&self, _ep: u8, data: &[u8]) -> Result<usize, Stall> {
        // Simulated flash-write time: delay each data report's completion so
        // the host's writes are paced like a real erase/program cycle (the
        // URB completes only when the "flash" has absorbed the chunk).
        if data.first() == Some(&REPORT_DATA_OUT) {
            let per_report = self.st.lock().unwrap().per_report;
            if !per_report.is_zero() {
                tokio::time::sleep(per_report).await;
            }
        }
        self.handle_report(data);
        Ok(data.len())
    }

    async fn in_transfer(&self, _ep: u8, len: usize) -> Result<Vec<u8>, Stall> {
        loop {
            let notified = self.in_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(r) = self.st.lock().unwrap().responses.pop_front() {
                let n = r.len().min(len.max(1));
                return Ok(r[..n].to_vec());
            }
            notified.await;
        }
    }
}
