//! Simulator configuration.

use crate::registry::FirmwareRegistry;

/// Emulated analyzer model. The USB protocol is shared; the PID, the product
/// string and the supported sample rates differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    Qa402,
    Qa403,
}

impl Model {
    pub fn default_pid(&self) -> u16 {
        match self {
            Model::Qa402 => 0x4E37,
            Model::Qa403 => 0x4E39,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Model::Qa402 => "QA402",
            Model::Qa403 => "QA403",
        }
    }

    pub fn product_string(&self) -> String {
        format!("{} Audio Analyzer", self.name())
    }

    /// Highest valid sample-rate index for register 9 (0=48k, 1=96k, 2=192k,
    /// 3=384k QA403-only).
    pub fn max_rate_index(&self) -> u32 {
        match self {
            Model::Qa402 => 2,
            Model::Qa403 => 3,
        }
    }
}

/// Which channel(s) the independent input generator drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenChannel {
    Left,
    Right,
    Both,
}

/// An independent sine "present at the analyzer input", in absolute volts
/// (level in dBV RMS), regardless of what the DAC plays. Useful to test
/// acquire-only paths of the host app.
#[derive(Debug, Clone, Copy)]
pub struct GenSpec {
    pub freq_hz: f32,
    pub level_dbv: f32,
    pub channel: GenChannel,
}

/// A random serial in the real units' format (`XXXX_XXXX`, hex digits).
/// Used as the default so the virtual device never impersonates a specific
/// physical unit; pass an explicit serial to pin it.
pub fn random_serial() -> String {
    let v = crate::rng::entropy_seed() as u32;
    format!("{:04X}_{:04X}", v >> 16, v & 0xFFFF)
}

/// Everything configurable about the virtual device.
#[derive(Debug, Clone)]
pub struct SimOptions {
    pub model: Model,
    /// USB vendor id of the analyzer persona (default 0x16C0).
    pub vid: u16,
    /// USB product id of the analyzer persona (default per model).
    pub pid: u16,
    /// USB serial string, e.g. "AB12_CD34". Register 0x1D returns the first 8
    /// hex digits packed as a u32 (underscores ignored), matching hardware.
    /// Defaults to a random one (see [`random_serial`]).
    pub serial: String,
    /// Firmware build number served by register 0x10 at boot (real units: 60).
    pub fw_version: u32,
    /// Version to report after a fake flash when the image is not found in the
    /// firmware registry. `None` keeps the current version.
    pub post_flash_version: Option<u32>,
    /// Optional sha256→version map (the host project's firmware-registry.json).
    /// Note: the vendor app zeroes 2 bytes at size−4 before sending, so wire
    /// images usually do NOT match the registry's embedded-image hashes;
    /// `post_flash_version` is the practical knob.
    pub registry: Option<FirmwareRegistry>,

    /// Pace ADC data at the configured sample rate like real hardware. Off =
    /// serve blocks as fast as the host asks (fast tests).
    pub realtime: bool,
    /// DAC→ADC loopback round-trip latency, in samples.
    pub latency_samples: usize,
    /// ADC noise floor in dBFS RMS (Gaussian). `-200` ≈ disabled.
    pub noise_dbfs: f32,
    /// Loop the DAC output back into the ADC (the "cable in loopback" setup).
    pub loopback: bool,
    /// Extra gain applied in the loopback path, in dB (0 = ideal cable).
    pub loopback_gain_db: f32,
    /// 2nd-harmonic distortion level in dBc for a full-scale sine, `None` = clean.
    pub h2_dbc: Option<f32>,
    /// 3rd-harmonic distortion level in dBc for a full-scale sine, `None` = clean.
    pub h3_dbc: Option<f32>,
    /// Independent generator at the ADC input.
    pub generator: Option<GenSpec>,

    /// USB/IP bus id this device is exported under.
    pub busid: String,

    /// Directory where every image received by the fake bootloader is saved
    /// (created if needed), as `qa40x-flash-<n>-<sha256:8>.sb`.
    pub save_firmware: Option<std::path::PathBuf>,

    /// Custom 512-byte calibration page (e.g. extracted from another unit
    /// with tools/extract_calpage.py). `None` serves the embedded real QA402
    /// page. The audio engine reads its trims from the served page either way.
    pub cal_page: Option<[u8; 512]>,

    /// Boot straight into the NXP KBOOT bootloader persona (`1fc9:0022`),
    /// waiting for a firmware image — as a real unit does when a previous
    /// flash failed and left it stuck in DFU. Lets the official app's recovery
    /// path ("Load default secure image, must be in HID mode") be tested. On a
    /// successful flash the device boots into the analyzer as usual.
    pub boot_bootloader: bool,

    /// Simulated flash-write time: the bootloader paces the ReceiveSbFile
    /// data phase over roughly this many seconds (0.0 = instant). A real
    /// flash takes a few seconds; the host app's progress bar follows.
    pub flash_secs: f32,
}

impl Default for SimOptions {
    fn default() -> Self {
        Self {
            model: Model::Qa402,
            vid: 0x16C0,
            pid: Model::Qa402.default_pid(),
            serial: random_serial(),
            fw_version: 60,
            post_flash_version: None,
            registry: None,
            realtime: true,
            latency_samples: 1200,
            noise_dbfs: -140.0,
            loopback: true,
            loopback_gain_db: 0.0,
            h2_dbc: None,
            h3_dbc: None,
            generator: None,
            busid: "1-1".to_string(),
            save_firmware: None,
            cal_page: None,
            flash_secs: 0.0,
            boot_bootloader: false,
        }
    }
}

impl SimOptions {
    /// Register 0x1D value: the serial string's hex digits packed as a u32
    /// (e.g. "AB12_CD34" → 0xAB12CD34). A serial without 8 hex digits falls
    /// back to a stable hash of the string, so 0x1D still identifies it.
    pub fn serial_u32(&self) -> u32 {
        let hex: String = self
            .serial
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .take(8)
            .collect();
        if hex.len() == 8 {
            if let Ok(v) = u32::from_str_radix(&hex, 16) {
                return v;
            }
        }
        self.serial.bytes().fold(0x811C_9DC5u32, |h, b| {
            (h ^ b as u32).wrapping_mul(0x0100_0193)
        })
    }
}
