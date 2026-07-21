//! The analyzer persona: QA402/QA403 register bus + fake ADC/DAC streaming.

use crate::backend::{DeviceSummary, SetupPacket, Stall, UsbBackend};
use crate::calpage;
use crate::identity::{analyzer_identity, Identity};
use crate::options::{GenChannel, SimOptions};
use crate::sim::SimEvent;
use async_trait::async_trait;
use log::{debug, info, warn};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};

/// Register addresses (mirrors the host project's register map).
mod reg {
    pub const LINK_KEEPALIVE: u8 = 0x00;
    pub const INPUT_GAIN: u8 = 0x05;
    pub const OUTPUT_GAIN: u8 = 0x06;
    pub const STREAM_CTRL: u8 = 0x08;
    pub const SAMPLE_RATE: u8 = 0x09;
    pub const UNKNOWN_INIT_0A: u8 = 0x0A;
    pub const CAL_PAGE_SELECT: u8 = 0x0D;
    pub const BOOTLOADER_ENTRY: u8 = 0x0F;
    pub const FIRMWARE_VERSION: u8 = 0x10;
    pub const TELEM_USB_VOLTAGE: u8 = 0x11;
    pub const TELEM_USB_CURRENT: u8 = 0x12;
    pub const TELEM_ISO_CURRENT: u8 = 0x13;
    pub const TELEM_EXTRA: u8 = 0x15;
    pub const TELEM_TEMPERATURE: u8 = 0x16;
    pub const CALIBRATION: u8 = 0x19;
    pub const CAPABILITY: u8 = 0x1B;
    pub const SERIAL_NUMBER: u8 = 0x1D;
    pub const STREAM_STATUS: u8 = 0x1E;
}

const BOOTLOADER_MAGIC_1: u32 = 0xDEAD_BEEF;
const BOOTLOADER_MAGIC_2: u32 = 0xCAFE_BABE;

/// Sample-rate index → Hz.
fn rate_hz(idx: u32) -> u32 {
    match idx {
        0 => 48_000,
        1 => 96_000,
        2 => 192_000,
        _ => 384_000,
    }
}

/// One active streaming session (STREAM_CTRL = 5).
struct Stream {
    started: Instant,
    /// Samples already handed to the host, for real-time pacing.
    emitted: u64,
    /// DAC samples received and not yet looped back, as (left, right).
    dac: VecDeque<(f32, f32)>,
    /// Loopback latency still to inject, in samples.
    delay: usize,
    /// Independent-generator phase (radians).
    phase: f64,
    /// Noise RNG state (xorshift64).
    rng: u64,
    /// Log DAC-queue overflow only once per stream.
    overflowed: bool,
}

impl Stream {
    fn new(latency: usize, seed: u64) -> Self {
        // Mix the seed so nearby user seeds decorrelate; xorshift64 sticks at
        // a zero state, so map that one value away.
        let state = crate::rng::splitmix64(seed);
        Self {
            started: Instant::now(),
            emitted: 0,
            dac: VecDeque::new(),
            delay: latency,
            phase: 0.0,
            rng: if state == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                state
            },
            overflowed: false,
        }
    }
}

/// Mutable device state. Locked briefly, never across an await.
struct AState {
    echo: u32,
    input_gain: u32,
    output_gain: u32,
    rate_idx: u32,
    reg0a: u32,
    /// Unmapped registers written by the host: stored and echoed on read.
    misc: HashMap<u8, u32>,
    cal_page: [u8; 512],
    cal_ptr: usize,
    /// Bootloader-entry latch: 1 after 0xDEADBEEF, waiting for 0xCAFEBABE.
    boot_stage: u8,
    last_stop: Option<Instant>,
    /// Queued register-read replies for bulk IN 0x81.
    replies: VecDeque<u32>,
    stream: Option<Stream>,
}

pub struct Analyzer {
    identity: Identity,
    opts: Arc<SimOptions>,
    fw_version: u32,
    events: mpsc::UnboundedSender<SimEvent>,
    boot: Instant,
    st: Mutex<AState>,
    reg_notify: Notify,
    stream_notify: Notify,
    /// Set once the bootloader magic completed — all traffic is going away.
    rebooting: AtomicBool,
}

impl Analyzer {
    pub fn new(
        opts: Arc<SimOptions>,
        fw_version: u32,
        events: mpsc::UnboundedSender<SimEvent>,
    ) -> Self {
        let identity = analyzer_identity(
            opts.vid,
            opts.pid,
            &opts.model.product_string(),
            &opts.serial,
        );
        Self {
            identity,
            fw_version,
            events,
            boot: Instant::now(),
            st: Mutex::new(AState {
                echo: 0,
                // Boot state of a freshly powered unit: max-headroom input,
                // lowest output range, 48 kHz. The vendor app re-forces
                // 42 dBV + a defined rate at connect anyway.
                input_gain: 7,
                output_gain: 0,
                rate_idx: 0,
                reg0a: 0,
                misc: HashMap::new(),
                cal_page: opts.cal_page.unwrap_or(*calpage::REAL_QA402_PAGE),
                cal_ptr: 0,
                boot_stage: 0,
                last_stop: None,
                replies: VecDeque::new(),
                stream: None,
            }),
            reg_notify: Notify::new(),
            stream_notify: Notify::new(),
            rebooting: AtomicBool::new(false),
            opts,
        }
    }

    /// Slowly wobbling telemetry so the host UI looks alive. Deterministic.
    fn telem(&self, base: f64, amplitude: f64, period_s: f64) -> u32 {
        let t = self.boot.elapsed().as_secs_f64();
        let v = base + amplitude * (t * std::f64::consts::TAU / period_s).sin();
        v.max(0.0) as u32
    }

    fn read_value(&self, st: &mut AState, r: u8) -> u32 {
        match r {
            reg::LINK_KEEPALIVE => st.echo,
            reg::INPUT_GAIN => st.input_gain,
            reg::OUTPUT_GAIN => st.output_gain,
            reg::STREAM_CTRL => {
                if st.stream.is_some() {
                    5
                } else {
                    0
                }
            }
            reg::SAMPLE_RATE => st.rate_idx,
            reg::UNKNOWN_INIT_0A => st.reg0a,
            reg::FIRMWARE_VERSION => self.fw_version,
            reg::TELEM_USB_VOLTAGE => self.telem(4952.0, 18.0, 7.0),
            reg::TELEM_USB_CURRENT => self.telem(788.0, 12.0, 5.0),
            reg::TELEM_ISO_CURRENT => self.telem(534.0, 8.0, 6.0),
            reg::TELEM_EXTRA => 0,
            reg::TELEM_TEMPERATURE => self.telem(250.0, 4.0, 60.0),
            reg::CALIBRATION => {
                // Each read returns the next 4 page bytes. The wire carries
                // them as a big-endian u32 that the host re-emits little-
                // endian, so pack the page chunk as LE here — the host's page
                // then matches ours byte for byte.
                let p = st.cal_ptr % 512;
                let chunk = [
                    st.cal_page[p],
                    st.cal_page[(p + 1) % 512],
                    st.cal_page[(p + 2) % 512],
                    st.cal_page[(p + 3) % 512],
                ];
                st.cal_ptr = (st.cal_ptr + 4) % 512;
                u32::from_le_bytes(chunk)
            }
            reg::CAPABILITY => 0x4000_0040,
            reg::SERIAL_NUMBER => self.opts.serial_u32(),
            reg::STREAM_STATUS => {
                // Reads 0x40 right after a stream stop, 0x00 when idle —
                // matches the vendor's stop/restart probe.
                match st.last_stop {
                    Some(t) if t.elapsed() < Duration::from_millis(500) => 0x40,
                    _ => 0,
                }
            }
            other => {
                let v = st.misc.get(&other).copied().unwrap_or(0);
                debug!("read of unmapped register 0x{other:02X} -> 0x{v:08X}");
                v
            }
        }
    }

    fn write_value(&self, st: &mut AState, r: u8, v: u32) {
        match r {
            reg::LINK_KEEPALIVE => st.echo = v,
            reg::INPUT_GAIN => {
                st.input_gain = v & 7;
                info!("input range set to index {} ({} dBV)", v & 7, (v & 7) * 6);
            }
            reg::OUTPUT_GAIN => {
                st.output_gain = v & 3;
                info!(
                    "output range set to index {} ({} dBV)",
                    v & 3,
                    calpage::DAC_LEVELS_DBV[(v & 3) as usize]
                );
            }
            reg::STREAM_CTRL => match v {
                5 => {
                    if st.stream.is_none() {
                        debug!("stream start (rate {} Hz)", rate_hz(st.rate_idx));
                        let seed = self
                            .opts
                            .noise_seed
                            .unwrap_or_else(crate::rng::entropy_seed);
                        st.stream = Some(Stream::new(self.opts.latency_samples, seed));
                        self.stream_notify.notify_waiters();
                    }
                }
                0 => {
                    if let Some(s) = st.stream.take() {
                        // Throughput check: effective rate ≈ nominal rate means
                        // the stream ran at hardware speed.
                        let secs = s.started.elapsed().as_secs_f64();
                        let nominal = rate_hz(st.rate_idx);
                        let effective = if secs > 0.0 {
                            s.emitted as f64 / secs
                        } else {
                            0.0
                        };
                        info!(
                            "stream stopped: {} samples in {:.2} s (effective {:.1} kS/s, nominal {:.0} kS/s)",
                            s.emitted,
                            secs,
                            effective / 1000.0,
                            nominal as f64 / 1000.0
                        );
                    }
                    st.last_stop = Some(Instant::now());
                    self.stream_notify.notify_waiters();
                }
                other => warn!("STREAM_CTRL written with unknown value {other}"),
            },
            reg::SAMPLE_RATE => {
                let max = self.opts.model.max_rate_index();
                let idx = v.min(max);
                if v > max {
                    warn!(
                        "sample-rate index {v} exceeds {} max ({max}); clamped",
                        self.opts.model.name()
                    );
                }
                st.rate_idx = idx;
                info!("sample rate set to {} Hz (index {idx})", rate_hz(idx));
            }
            reg::UNKNOWN_INIT_0A => {
                st.reg0a = v;
                debug!("init write 0x0A = {v}");
            }
            reg::CAL_PAGE_SELECT => {
                if v != 0x10 {
                    warn!("CAL_PAGE_SELECT written with 0x{v:X} (real app always writes 0x10)");
                }
                st.cal_ptr = 0;
            }
            reg::BOOTLOADER_ENTRY => {
                // Strict two-magic unlock, order-sensitive like the notes assume.
                if v == BOOTLOADER_MAGIC_1 {
                    st.boot_stage = 1;
                } else if v == BOOTLOADER_MAGIC_2 && st.boot_stage == 1 {
                    st.boot_stage = 0;
                    st.stream = None;
                    if !self.rebooting.swap(true, Ordering::SeqCst) {
                        info!("bootloader magic received — rebooting into NXP KBOOT persona");
                        let _ = self.events.send(SimEvent::EnterBootloader);
                    }
                } else {
                    warn!(
                        "BOOTLOADER_ENTRY: unexpected value 0x{v:08X} (stage {}) — latch reset",
                        st.boot_stage
                    );
                    st.boot_stage = 0;
                }
            }
            other => {
                debug!("write to unmapped register 0x{other:02X} = 0x{v:08X}");
                st.misc.insert(other, v);
            }
        }
    }

    /// Process one 5-byte register frame `[reg][value BE]`.
    fn handle_frame(&self, frame: &[u8]) {
        let r = frame[0];
        let v = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
        let mut st = self.st.lock().unwrap();
        if r & 0x80 != 0 {
            let value = self.read_value(&mut st, r & 0x7f);
            st.replies.push_back(value);
            drop(st);
            self.reg_notify.notify_waiters();
        } else {
            self.write_value(&mut st, r, v);
        }
    }

    /// Decode a DAC block (interleaved i32 LE, right sample first on the wire)
    /// and queue it for loopback.
    fn handle_dac_block(&self, data: &[u8]) {
        let mut st = self.st.lock().unwrap();
        let rate = rate_hz(st.rate_idx) as usize;
        let Some(s) = st.stream.as_mut() else {
            debug!(
                "DAC data while not streaming ({} bytes) — dropped",
                data.len()
            );
            return;
        };
        const FS: f32 = 2_147_483_648.0;
        let mut i = 0;
        while i + 8 <= data.len() {
            let r = i32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            let l = i32::from_le_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]);
            s.dac.push_back((l as f32 / FS, r as f32 / FS));
            i += 8;
        }
        // Cap the queue (10 s of audio) so a pathological host can't grow it
        // without bound.
        let cap = rate * 10;
        if s.dac.len() > cap {
            if !s.overflowed {
                warn!("DAC queue overflow — dropping oldest samples");
                s.overflowed = true;
            }
            while s.dac.len() > cap {
                s.dac.pop_front();
            }
        }
        drop(st);
        // Wake an ADC read waiting for its loopback source.
        self.stream_notify.notify_waiters();
    }

    /// Produce one ADC block of `n` samples. Called with the state lock held.
    fn produce_adc_block(&self, st: &mut AState, n: usize) -> Vec<u8> {
        let input_gain = st.input_gain as usize;
        let output_gain = st.output_gain as usize;
        let rate = rate_hz(st.rate_idx) as f64;

        // Volts model, consistent with the SERVED calibration page (real or
        // custom — the trims are read from it, see calpage.rs):
        //   DAC full-scale peak (volts) = outFS + 3 dB − dac_trim
        //   ADC full-scale peak (volts) = inFS − 6 dB + adc_trim
        // so the digital loopback gain is outFS − inFS + 9 − adc − dac, exactly
        // what a calibrated host expects to cancel back to `loopback_gain_db`.
        let db = |x: f32| 10f32.powf(x / 20.0);
        let out_fs = calpage::DAC_LEVELS_DBV[output_gain] as f32;
        let in_fs = calpage::ADC_LEVELS_DBV[input_gain] as f32;
        let page = &st.cal_page;
        let dac_fs = [
            db(out_fs + 3.0 - calpage::dac_trim_db(page, output_gain, 0)),
            db(out_fs + 3.0 - calpage::dac_trim_db(page, output_gain, 1)),
        ];
        let adc_fs = [
            db(in_fs - 6.0 + calpage::adc_trim_db(page, input_gain, 0)),
            db(in_fs - 6.0 + calpage::adc_trim_db(page, input_gain, 1)),
        ];

        let loop_gain = if self.opts.loopback {
            db(self.opts.loopback_gain_db)
        } else {
            0.0
        };
        // Polynomial distortion tuned so a full-scale sine shows the requested
        // harmonic levels: x² of amplitude A yields H2 = k2·A²/2, x³ yields
        // H3 = k3·A³/4.
        let k2 = self.opts.h2_dbc.map_or(0.0, |d| 2.0 * db(d));
        let k3 = self.opts.h3_dbc.map_or(0.0, |d| 4.0 * db(d));
        let noise_rms = db(self.opts.noise_dbfs);

        let generator = self.opts.generator;
        let gen_amp = generator.map_or(0.0f32, |g| std::f32::consts::SQRT_2 * db(g.level_dbv));
        let gen_step =
            generator.map_or(0.0f64, |g| g.freq_hz as f64 * std::f64::consts::TAU / rate);

        let s = st.stream.as_mut().expect("stream present");
        let mut out = Vec::with_capacity(n * 8);
        for _ in 0..n {
            let (dl, dr) = if s.delay > 0 {
                s.delay -= 1;
                (0.0, 0.0)
            } else {
                s.dac.pop_front().unwrap_or((0.0, 0.0))
            };

            // Input volts: DAC loopback + independent generator.
            let mut vl = dl * dac_fs[0] * loop_gain;
            let mut vr = dr * dac_fs[1] * loop_gain;
            if let Some(g) = generator {
                let tone = gen_amp * (s.phase as f32).sin();
                s.phase += gen_step;
                if s.phase > std::f64::consts::TAU {
                    s.phase -= std::f64::consts::TAU;
                }
                match g.channel {
                    GenChannel::Left => vl += tone,
                    GenChannel::Right => vr += tone,
                    GenChannel::Both => {
                        vl += tone;
                        vr += tone;
                    }
                }
            }

            // Digital domain: normalize, distort, add noise.
            let mut sample = |v: f32, ch: usize| -> i32 {
                let x = v / adc_fs[ch];
                let x = x + k2 * x * x + k3 * x * x * x;
                let x = x + noise_rms * gauss(&mut s.rng);
                (x.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32
            };
            let il = sample(vl, 0);
            let ir = sample(vr, 1);
            // Wire order is swapped: right sample first.
            out.extend_from_slice(&ir.to_le_bytes());
            out.extend_from_slice(&il.to_le_bytes());
        }
        s.emitted += n as u64;
        out
    }
}

/// Standard normal deviate from a xorshift64 state (Box-Muller, one branch).
fn gauss(state: &mut u64) -> f32 {
    let mut next = || {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        // Uniform in (0, 1].
        ((x >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    };
    let u1 = next();
    let u2 = next();
    ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
}

#[async_trait]
impl UsbBackend for Analyzer {
    fn summary(&self) -> DeviceSummary {
        self.identity.summary.clone()
    }

    async fn control(&self, setup: SetupPacket, _data_out: &[u8]) -> Result<Vec<u8>, Stall> {
        if let Some(handled) = self.identity.handle_standard(setup) {
            return handled;
        }
        // MS OS 2.0 descriptor-set fetch (Windows WinUSB auto-binding).
        if let Some(handled) = self.identity.handle_vendor(setup) {
            debug!("served the MS OS 2.0 descriptor set (WinUSB auto-binding)");
            return handled;
        }
        debug!(
            "unhandled control request bmRequestType=0x{:02X} bRequest=0x{:02X} — stalled",
            setup.bm_request_type, setup.b_request
        );
        Err(Stall)
    }

    async fn out_transfer(&self, ep: u8, data: &[u8]) -> Result<usize, Stall> {
        match ep & 0x7f {
            1 => {
                // Register frames are 5 bytes each; tolerate several per URB.
                if !data.len().is_multiple_of(5) {
                    warn!("register OUT of {} bytes (not a multiple of 5)", data.len());
                }
                for frame in data.chunks_exact(5) {
                    self.handle_frame(frame);
                }
                Ok(data.len())
            }
            2 => {
                self.handle_dac_block(data);
                Ok(data.len())
            }
            other => {
                warn!("OUT transfer on unknown endpoint 0x{other:02X}");
                Err(Stall)
            }
        }
    }

    async fn in_transfer(&self, ep: u8, len: usize) -> Result<Vec<u8>, Stall> {
        match ep & 0x7f {
            // Register-read replies: 4 bytes big-endian, short packet.
            1 => loop {
                // Register the waiter BEFORE checking the queue so a notify
                // arriving in between is not lost.
                let notified = self.reg_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(v) = self.st.lock().unwrap().replies.pop_front() {
                    return Ok(v.to_be_bytes()[..4.min(len)].to_vec());
                }
                notified.await;
            },
            // ADC stream: 16 KiB blocks, paced at the sample rate.
            2 => {
                let n = len / 8;
                if n == 0 {
                    return Ok(Vec::new());
                }
                loop {
                    // Wait for an active stream, then for the block's due time.
                    let notified = self.stream_notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    let due = {
                        let st = self.st.lock().unwrap();
                        st.stream.as_ref().map(|s| {
                            let elapsed =
                                (s.emitted + n as u64) as f64 / rate_hz(st.rate_idx) as f64;
                            s.started + Duration::from_secs_f64(elapsed)
                        })
                    };
                    let Some(due) = due else {
                        notified.await;
                        continue;
                    };
                    if self.opts.realtime {
                        tokio::time::sleep_until(due.into()).await;
                    }
                    // The host pre-queues DAC writes together with ADC reads;
                    // on real hardware the sample-rate pacing guarantees the
                    // loopback source has arrived by the time a block is
                    // produced. Emulate that: wait (bounded) until the DAC
                    // queue covers this block, then fall back to silence like
                    // a free-running ADC would. In realtime mode the block is
                    // already due, so only a short jitter grace is allowed —
                    // a real ADC never waits, and long waits here made the
                    // stream run visibly slower than the sample rate. The
                    // long deadline is for --no-realtime determinism only.
                    let grace = if self.opts.realtime {
                        Duration::from_millis(20)
                    } else {
                        Duration::from_millis(250)
                    };
                    let deadline = Instant::now() + grace;
                    loop {
                        let notified = self.stream_notify.notified();
                        tokio::pin!(notified);
                        notified.as_mut().enable();
                        let missing = {
                            let st = self.st.lock().unwrap();
                            match &st.stream {
                                Some(s) => n.saturating_sub(s.delay).saturating_sub(s.dac.len()),
                                None => 0, // stopped: bail to the outer loop
                            }
                        };
                        if missing == 0 {
                            break;
                        }
                        if Instant::now() >= deadline {
                            debug!(
                                "ADC block due but {missing} DAC samples missing — \
                                 substituting silence (host underran the DAC)"
                            );
                            break;
                        }
                        tokio::select! {
                            _ = notified => {}
                            _ = tokio::time::sleep_until(deadline.into()) => {}
                        }
                    }
                    let mut st = self.st.lock().unwrap();
                    if st.stream.is_none() {
                        // Stopped while we were pacing; wait for the next start.
                        continue;
                    }
                    return Ok(self.produce_adc_block(&mut st, n));
                }
            }
            other => {
                warn!("IN transfer on unknown endpoint 0x{other:02X}");
                Err(Stall)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyzer(noise_seed: Option<u64>) -> Analyzer {
        let opts = SimOptions {
            realtime: false,
            loopback: false,
            noise_dbfs: -60.0,
            noise_seed,
            ..SimOptions::default()
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        Analyzer::new(Arc::new(opts), 60, tx)
    }

    /// One start → block → stop cycle over the register bus, returning the
    /// raw ADC bytes (pure noise: no loopback, no generator).
    fn one_acquisition(a: &Analyzer, n: usize) -> Vec<u8> {
        a.handle_frame(&[reg::STREAM_CTRL, 0, 0, 0, 5]);
        let block = a.produce_adc_block(&mut a.st.lock().unwrap(), n);
        a.handle_frame(&[reg::STREAM_CTRL, 0, 0, 0, 0]);
        block
    }

    #[test]
    fn unseeded_streams_differ() {
        let a = analyzer(None);
        assert_ne!(one_acquisition(&a, 256), one_acquisition(&a, 256));
    }

    #[test]
    fn seeded_streams_replay() {
        let a = analyzer(Some(42));
        assert_eq!(one_acquisition(&a, 256), one_acquisition(&a, 256));
    }

    #[test]
    fn seed_zero_is_valid() {
        let a = analyzer(Some(0));
        let block = one_acquisition(&a, 256);
        let first = &block[..4];
        assert!(
            block.chunks_exact(4).any(|s| s != first),
            "seed 0 must still produce varying noise, not a stuck RNG"
        );
    }
}
