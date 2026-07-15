//! Multi-device environment config (JSON).
//!
//! Describes a set of virtual devices so a whole bench can be launched — and
//! relaunched identically — from one file. Every field is optional with a
//! sensible default; a device with a fixed `serial` reproduces exactly across
//! runs, while `serial: "random"` (or omitting it) picks a fresh one each
//! launch. Example: `examples/devices.json`.

use serde::{Deserialize, Serialize};

/// One device in the environment. All fields optional; see the CLI flags of
/// the same name for their meaning.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct DeviceSpec {
    /// "qa402" or "qa403".
    pub model: Option<String>,
    /// USB/IP bus id; auto-assigned "1-N" per position if omitted. Unique.
    pub busid: Option<String>,
    /// Fixed serial (e.g. "AB12_CD34"), or "random"/"dynamic"/omitted for a
    /// fresh one each launch.
    pub serial: Option<String>,
    /// USB vendor id, hex ("0x16C0") or decimal. Defaults to 0x16C0.
    pub vid: Option<String>,
    /// USB product id; defaults to the model's real PID.
    pub pid: Option<String>,
    /// Firmware version served by register 0x10.
    pub fw_version: Option<u32>,
    /// Boot straight into the NXP KBOOT bootloader (recovery mode).
    pub boot_bootloader: Option<bool>,
    /// Version reported after a fake flash (when not resolved via a registry).
    pub post_flash_version: Option<u32>,
    /// Simulated flash-write seconds.
    pub flash_secs: Option<f32>,
    /// Path to a firmware-registry.json for sha256 → version lookup.
    pub registry: Option<String>,
    /// Directory to save every received firmware image.
    pub save_firmware: Option<String>,
    /// Calibration page: "real" (default), "random", or a 512-byte file path.
    pub cal_page: Option<String>,
    /// Pace ADC at the sample rate (default true).
    pub realtime: Option<bool>,
    /// Loopback round-trip latency, in samples.
    pub latency_samples: Option<usize>,
    /// ADC noise floor, dBFS.
    pub noise_dbfs: Option<f32>,
    /// DAC→ADC loopback enabled (default true).
    pub loopback: Option<bool>,
    /// Extra loopback gain, dB.
    pub loopback_gain_db: Option<f32>,
    /// 2nd-harmonic distortion, dBc.
    pub h2_dbc: Option<f32>,
    /// 3rd-harmonic distortion, dBc.
    pub h3_dbc: Option<f32>,
    /// Independent input generator, "FREQ:LEVEL_DBV[:left|right|both]".
    pub gen: Option<String>,
}

/// The environment file: a list of devices.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvConfig {
    pub devices: Vec<DeviceSpec>,
}

impl EnvConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read config {path:?}: {e}"))?;
        let cfg: EnvConfig = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("invalid config {path:?}: {e}"))?;
        if cfg.devices.is_empty() {
            anyhow::bail!("config {path:?} lists no devices");
        }
        Ok(cfg)
    }
}
