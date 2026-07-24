//! USB exchange recorder: a human-readable, replayable JSONL capture of every
//! URB the remote client submits and everything the device answers.
//!
//! One [`Recorder`] serves the whole server (all devices, all connections);
//! each attached client gets a [`ConnRecorder`] stamping its events with a
//! connection id. Register frames (bulk EP1) are decoded — address, name,
//! u32 value — next to their raw hex; audio stream transfers (bulk EP2) carry
//! their full payload as base64 so a session can be re-analysed or replayed
//! offline; control transfers keep their decoded SETUP packet. Bootloader
//! (HID) traffic is recorded as raw payloads.
//!
//! Format reference: the "Recording USB exchanges" section of the README.

use base64::Engine as _;
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use vqa40x_core::backend::{DeviceSummary, SetupPacket, Stall};

/// Raw (undecoded) payloads at most this long are logged as spaced hex;
/// longer ones switch to base64 to keep lines manageable.
const HEX_LIMIT: usize = 64;

/// One JSONL event. Optional fields are omitted from the line entirely.
#[derive(Serialize)]
struct Line<'a> {
    n: u64,
    /// Seconds since the recorder was created (µs resolution).
    t: f64,
    /// Connection id (one per attach), absent on the session header only.
    #[serde(skip_serializing_if = "Option::is_none")]
    c: Option<u64>,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    seq: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ep: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frame: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reg_name: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    len: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frames: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_b64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    // control-transfer SETUP fields
    #[serde(skip_serializing_if = "Option::is_none")]
    bm_request_type: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    b_request: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_name: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    w_value: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    w_index: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    w_length: Option<u16>,
    // attach / detach / unlink fields
    #[serde(skip_serializing_if = "Option::is_none")]
    busid: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    persona: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    victim: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
}

impl<'a> Line<'a> {
    fn new(n: u64, t: f64, kind: &'static str) -> Self {
        Self {
            n,
            t,
            c: None,
            kind,
            seq: None,
            dir: None,
            ep: None,
            frame: None,
            reg: None,
            reg_name: None,
            value: None,
            len: None,
            frames: None,
            hex: None,
            data_b64: None,
            status: None,
            bm_request_type: None,
            b_request: None,
            request_name: None,
            w_value: None,
            w_index: None,
            w_length: None,
            busid: None,
            peer: None,
            vid: None,
            pid: None,
            persona: None,
            reason: None,
            victim: None,
            outcome: None,
        }
    }
}

/// The shared recorder: one JSONL file for the server's lifetime.
pub struct Recorder {
    w: Mutex<BufWriter<File>>,
    start: Instant,
    seq: AtomicU64,
    conn_seq: AtomicU64,
    full_data: bool,
    path: PathBuf,
}

impl Recorder {
    /// Open the log at `path` (an existing directory gets an auto-named
    /// `usb-<unix-secs>.jsonl`) and write the session header line.
    /// `full_data: false` drops stream/raw payloads, keeping only lengths.
    pub fn create(path: &Path, full_data: bool) -> std::io::Result<Arc<Self>> {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = if path.is_dir() {
            path.join(format!("usb-{}.jsonl", unix_ms / 1000))
        } else {
            path.to_path_buf()
        };
        let mut w = BufWriter::new(File::create(&path)?);
        let header = serde_json::json!({
            "kind": "session",
            "format": "vqa40x-usb-log/1",
            "app": concat!("vqa40x ", env!("CARGO_PKG_VERSION")),
            "started_unix_ms": unix_ms,
            "data_payloads": if full_data { "full" } else { "summary" },
        });
        writeln!(w, "{header}")?;
        w.flush()?;
        Ok(Arc::new(Self {
            w: Mutex::new(w),
            start: Instant::now(),
            seq: AtomicU64::new(0),
            conn_seq: AtomicU64::new(0),
            full_data,
            path,
        }))
    }

    /// Where the log is being written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open a per-connection recorder and log the attach. Register decoding
    /// is enabled for the analyzer persona only — the bootloader speaks HID
    /// reports on the same endpoint numbers, recorded as raw payloads.
    pub fn connection(
        self: &Arc<Self>,
        busid: &str,
        peer: Option<std::net::SocketAddr>,
        summary: &DeviceSummary,
    ) -> Arc<ConnRecorder> {
        let is_hid = summary.interfaces.iter().any(|(class, _, _)| *class == 3);
        let conn = ConnRecorder {
            rec: self.clone(),
            conn: self.conn_seq.fetch_add(1, Ordering::SeqCst) + 1,
            decode_regs: !is_hid,
            pending_reads: Mutex::new(VecDeque::new()),
        };
        let mut l = conn.line("attach");
        l.busid = Some(busid);
        l.peer = peer.map(|p| p.to_string());
        l.vid = Some(format!("0x{:04X}", summary.vid));
        l.pid = Some(format!("0x{:04X}", summary.pid));
        l.persona = Some(if is_hid { "bootloader" } else { "analyzer" });
        conn.rec.emit(&l);
        Arc::new(conn)
    }

    fn stamp(&self) -> (u64, f64) {
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let t = (self.start.elapsed().as_secs_f64() * 1e6).round() / 1e6;
        (n, t)
    }

    fn emit(&self, line: &Line) {
        let Ok(mut w) = self.w.lock() else { return };
        if let Ok(json) = serde_json::to_string(line) {
            let _ = writeln!(w, "{json}");
            let _ = w.flush();
        }
    }

    /// Attach `data` to a line: decoded-register frames always carry hex;
    /// raw/stream payloads use hex up to [`HEX_LIMIT`], base64 beyond, and
    /// are dropped entirely (length stays) in summary mode.
    fn payload(&self, l: &mut Line, data: &[u8]) {
        if !self.full_data {
            return;
        }
        if data.len() <= HEX_LIMIT {
            l.hex = Some(hex_string(data));
        } else {
            l.data_b64 = Some(base64::engine::general_purpose::STANDARD.encode(data));
        }
    }
}

/// Per-attach event recorder handed through the URB loop and its IN workers.
pub struct ConnRecorder {
    rec: Arc<Recorder>,
    conn: u64,
    decode_regs: bool,
    /// Register addresses of in-flight read requests (`0x80|reg` frames),
    /// popped when the 4-byte reply leaves on EP1 IN. The device answers in
    /// order, so a FIFO pairs them correctly.
    pending_reads: Mutex<VecDeque<u8>>,
}

impl ConnRecorder {
    fn line(&self, kind: &'static str) -> Line<'static> {
        let (n, t) = self.rec.stamp();
        let mut l = Line::new(n, t, kind);
        l.c = Some(self.conn);
        l
    }

    /// The connection ended; `reason` is one of `client_detached`,
    /// `device_rebooted`, `connection_closed`, `protocol_error`.
    pub fn detach(&self, reason: &'static str) {
        let mut l = self.line("detach");
        l.reason = Some(reason);
        self.rec.emit(&l);
    }

    /// A control transfer was handled (request and response in one event).
    pub fn control(
        &self,
        seq: u32,
        setup: &SetupPacket,
        data_out: &[u8],
        result: &Result<Vec<u8>, Stall>,
    ) {
        let mut l = self.line("control");
        l.seq = Some(seq);
        l.dir = Some(if setup.is_in() { "in" } else { "out" });
        l.bm_request_type = Some(setup.bm_request_type);
        l.b_request = Some(setup.b_request);
        l.request_name = standard_request_name(setup);
        l.w_value = Some(setup.w_value);
        l.w_index = Some(setup.w_index);
        l.w_length = Some(setup.w_length);
        match result {
            Ok(reply) => {
                let data = if setup.is_in() { &reply[..] } else { data_out };
                l.len = Some(data.len());
                self.rec.payload(&mut l, data);
            }
            Err(Stall) => l.status = Some("stall"),
        }
        self.rec.emit(&l);
    }

    /// A bulk/interrupt OUT transfer was submitted (host → device bytes).
    pub fn out(&self, seq: u32, ep: u8, data: &[u8], stalled: bool) {
        let status = stalled.then_some("stall");
        if self.decode_regs && ep & 0x7f == 1 && !data.is_empty() && data.len().is_multiple_of(5) {
            // Register frames, possibly several per URB.
            let many = data.len() > 5;
            for (i, f) in data.chunks_exact(5).enumerate() {
                let read = f[0] & 0x80 != 0;
                let reg = f[0] & 0x7f;
                if read {
                    self.pending_reads.lock().unwrap().push_back(reg);
                }
                let mut l = self.line(if read { "reg_read_req" } else { "reg_write" });
                l.seq = Some(seq);
                l.dir = Some("out");
                l.ep = Some(format!("0x{ep:02X}"));
                l.frame = many.then_some(i);
                l.reg = Some(format!("0x{reg:02X}"));
                l.reg_name = vqa40x_core::analyzer::register_name(reg);
                if !read {
                    l.value = Some(u32::from_be_bytes([f[1], f[2], f[3], f[4]]));
                }
                l.hex = Some(hex_string(f));
                l.status = status;
                self.rec.emit(&l);
            }
            return;
        }
        let dac = self.decode_regs && ep & 0x7f == 2;
        let mut l = self.line(if dac { "dac_data" } else { "out" });
        l.seq = Some(seq);
        l.dir = Some("out");
        l.ep = Some(format!("0x{ep:02X}"));
        l.len = Some(data.len());
        l.frames = dac.then_some(data.len() / 8);
        l.status = status;
        self.rec.payload(&mut l, data);
        self.rec.emit(&l);
    }

    /// An IN URB was queued by the client (`len` = its buffer size).
    pub fn in_req(&self, seq: u32, ep: u8, len: usize) {
        let mut l = self.line("in_req");
        l.seq = Some(seq);
        l.dir = Some("in");
        l.ep = Some(format!("0x{ep:02X}"));
        l.len = Some(len);
        self.rec.emit(&l);
    }

    /// An IN transfer completed (device → host bytes, or a stall).
    pub fn in_result(&self, seq: u32, ep: u8, result: &Result<Vec<u8>, Stall>) {
        let reg_ep = self.decode_regs && ep & 0x7f == 1;
        let data = match result {
            Ok(d) => d,
            Err(Stall) => {
                if reg_ep {
                    self.pending_reads.lock().unwrap().pop_front();
                }
                let mut l = self.line("in");
                l.seq = Some(seq);
                l.dir = Some("in");
                l.ep = Some(format!("0x{ep:02X}"));
                l.status = Some("stall");
                self.rec.emit(&l);
                return;
            }
        };
        if reg_ep {
            let reg = self.pending_reads.lock().unwrap().pop_front();
            let mut l = self.line("reg_read");
            l.seq = Some(seq);
            l.dir = Some("in");
            l.ep = Some(format!("0x{ep:02X}"));
            if let Some(reg) = reg {
                l.reg = Some(format!("0x{reg:02X}"));
                l.reg_name = vqa40x_core::analyzer::register_name(reg);
            }
            if let Ok(bytes) = <[u8; 4]>::try_from(&data[..]) {
                l.value = Some(u32::from_be_bytes(bytes));
            }
            l.hex = Some(hex_string(data));
            self.rec.emit(&l);
            return;
        }
        let adc = self.decode_regs && ep & 0x7f == 2;
        let mut l = self.line(if adc { "adc_data" } else { "in" });
        l.seq = Some(seq);
        l.dir = Some("in");
        l.ep = Some(format!("0x{ep:02X}"));
        l.len = Some(data.len());
        l.frames = adc.then_some(data.len() / 8);
        self.rec.payload(&mut l, data);
        self.rec.emit(&l);
    }

    /// The client unlinked (cancelled) a pending URB.
    pub fn unlink(&self, seq: u32, victim: u32, cancelled: bool) {
        let mut l = self.line("unlink");
        l.seq = Some(seq);
        l.victim = Some(victim);
        l.outcome = Some(if cancelled {
            "cancelled"
        } else {
            "already_completed"
        });
        self.rec.emit(&l);
    }
}

/// Names for standard requests (bmRequestType type bits == standard).
fn standard_request_name(setup: &SetupPacket) -> Option<&'static str> {
    if setup.bm_request_type & 0x60 != 0 {
        return None;
    }
    Some(match setup.b_request {
        0x00 => "GET_STATUS",
        0x01 => "CLEAR_FEATURE",
        0x03 => "SET_FEATURE",
        0x05 => "SET_ADDRESS",
        0x06 => "GET_DESCRIPTOR",
        0x07 => "SET_DESCRIPTOR",
        0x08 => "GET_CONFIGURATION",
        0x09 => "SET_CONFIGURATION",
        0x0A => "GET_INTERFACE",
        0x0B => "SET_INTERFACE",
        _ => return None,
    })
}

fn hex_string(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 3);
    for (i, b) in data.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02X}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(analyzer: bool) -> DeviceSummary {
        DeviceSummary {
            vid: 0x16C0,
            pid: 0x4E37,
            bcd_device: 0,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            configuration_value: 1,
            num_configurations: 1,
            speed: vqa40x_core::backend::UsbSpeed::High,
            interfaces: if analyzer {
                vec![(0xFF, 0, 0)]
            } else {
                vec![(3, 0, 0)]
            },
        }
    }

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn records_decoded_register_and_stream_traffic() {
        let path =
            std::env::temp_dir().join(format!("vqa40x-usb-log-test-{}.jsonl", std::process::id()));
        let rec = Recorder::create(&path, true).unwrap();
        let conn = rec.connection("1-1", None, &summary(true));

        // write SAMPLE_RATE=1 and a FIRMWARE_VERSION read request in ONE URB,
        // then the 4-byte reply, one DAC block, one ADC block, an unlink.
        conn.out(10, 0x01, &[0x09, 0, 0, 0, 1, 0x90, 0, 0, 0, 0], false);
        conn.in_req(11, 0x81, 512);
        conn.in_result(11, 0x81, &Ok(vec![0, 0, 0, 60]));
        conn.out(12, 0x02, &[0u8; 16384], false);
        conn.in_result(13, 0x82, &Ok(vec![1u8; 16]));
        conn.unlink(14, 13, true);
        conn.detach("client_detached");

        let lines = read_lines(&path);
        std::fs::remove_file(&path).unwrap();

        assert_eq!(lines[0]["kind"], "session");
        assert_eq!(lines[0]["format"], "vqa40x-usb-log/1");

        assert_eq!(lines[1]["kind"], "attach");
        assert_eq!(lines[1]["persona"], "analyzer");
        assert_eq!(lines[1]["vid"], "0x16C0");

        assert_eq!(lines[2]["kind"], "reg_write");
        assert_eq!(lines[2]["seq"], 10);
        assert_eq!(lines[2]["frame"], 0);
        assert_eq!(lines[2]["reg"], "0x09");
        assert_eq!(lines[2]["reg_name"], "SAMPLE_RATE");
        assert_eq!(lines[2]["value"], 1);
        assert_eq!(lines[2]["hex"], "09 00 00 00 01");

        assert_eq!(lines[3]["kind"], "reg_read_req");
        assert_eq!(lines[3]["frame"], 1);
        assert_eq!(lines[3]["reg"], "0x10");
        assert_eq!(lines[3]["reg_name"], "FIRMWARE_VERSION");

        assert_eq!(lines[4]["kind"], "in_req");
        assert_eq!(lines[4]["len"], 512);

        assert_eq!(lines[5]["kind"], "reg_read");
        assert_eq!(lines[5]["reg"], "0x10");
        assert_eq!(lines[5]["value"], 60);
        assert_eq!(lines[5]["hex"], "00 00 00 3C");

        assert_eq!(lines[6]["kind"], "dac_data");
        assert_eq!(lines[6]["len"], 16384);
        assert_eq!(lines[6]["frames"], 2048);
        assert!(lines[6]["data_b64"].is_string());

        assert_eq!(lines[7]["kind"], "adc_data");
        assert_eq!(lines[7]["len"], 16);
        // Short payload: spaced hex, no base64.
        assert!(lines[7]["hex"].is_string());
        assert!(lines[7].get("data_b64").is_none());

        assert_eq!(lines[8]["kind"], "unlink");
        assert_eq!(lines[8]["victim"], 13);
        assert_eq!(lines[8]["outcome"], "cancelled");

        assert_eq!(lines[9]["kind"], "detach");
        assert_eq!(lines[9]["reason"], "client_detached");

        // Sequence numbers are contiguous from 1.
        for (i, l) in lines[1..].iter().enumerate() {
            assert_eq!(l["n"], (i + 1) as u64);
        }
    }

    #[test]
    fn bootloader_traffic_stays_raw_and_summary_drops_payloads() {
        let path = std::env::temp_dir().join(format!(
            "vqa40x-usb-log-test-boot-{}.jsonl",
            std::process::id()
        ));
        let rec = Recorder::create(&path, false).unwrap();
        let conn = rec.connection("1-1", None, &summary(false));

        // A 60-byte KBOOT report on EP1 OUT must NOT be decoded as registers
        // (even though 60 is a multiple of 5), and summary mode keeps only len.
        conn.out(1, 0x01, &[0u8; 60], false);
        conn.in_result(2, 0x81, &Ok(vec![3u8; 60]));

        let lines = read_lines(&path);
        std::fs::remove_file(&path).unwrap();

        assert_eq!(lines[1]["persona"], "bootloader");
        assert_eq!(lines[2]["kind"], "out");
        assert_eq!(lines[2]["len"], 60);
        assert!(lines[2].get("hex").is_none());
        assert!(lines[2].get("data_b64").is_none());
        assert_eq!(lines[3]["kind"], "in");
        assert_eq!(lines[3]["len"], 60);
    }

    #[test]
    fn control_transfers_keep_their_setup_decode() {
        let path = std::env::temp_dir().join(format!(
            "vqa40x-usb-log-test-ctrl-{}.jsonl",
            std::process::id()
        ));
        let rec = Recorder::create(&path, true).unwrap();
        let conn = rec.connection("1-1", None, &summary(true));

        let setup = SetupPacket::parse(&[0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]);
        conn.control(1, &setup, &[], &Ok(vec![0x12; 18]));
        let setup = SetupPacket::parse(&[0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]);
        conn.control(2, &setup, &[], &Err(Stall));

        let lines = read_lines(&path);
        std::fs::remove_file(&path).unwrap();

        assert_eq!(lines[2]["kind"], "control");
        assert_eq!(lines[2]["request_name"], "GET_DESCRIPTOR");
        assert_eq!(lines[2]["w_value"], 0x0100);
        assert_eq!(lines[2]["len"], 18);
        assert_eq!(lines[3]["status"], "stall");
    }
}
