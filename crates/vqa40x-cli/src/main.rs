//! `vqa40x` — a virtual QuantAsylum QA402/QA403 exported over USB/IP.

mod config;

use anyhow::{bail, Context, Result};
use clap::Parser;
use config::{DeviceSpec, EnvConfig};
use log::info;
use std::net::SocketAddr;
use std::path::PathBuf;
use vqa40x_core::registry::FirmwareRegistry;
use vqa40x_core::{GenChannel, GenSpec, Model, SimOptions, Simulator};

#[derive(Parser, Debug)]
#[command(
    name = "vqa40x",
    version,
    about = "Virtual QA402/QA403 audio analyzer (USB/IP device server).\n\
             Unofficial project, not affiliated with or endorsed by QuantAsylum.",
    after_help = "\
ATTACHING THE DEVICE
  Linux:    sudo modprobe vhci-hcd
            sudo usbip attach -r <this-host> -b <busid>
  Windows:  usbip-win2: usbip.exe attach -r <this-host> -b <busid>
  macOS:    no USB/IP client exists; run the server here and attach from a
            Linux/Windows VM (e.g. the Parallels VM used for USB captures).

FIRMWARE UPDATE EMULATION
  Writing 0xDEADBEEF then 0xCAFEBABE to register 0x0F detaches the analyzer
  and re-exports an NXP KBOOT HID bootloader (1fc9:0022) under the same busid;
  re-attach to talk to it. After a successful ReceiveSbFile the device detaches
  again and comes back as the analyzer (re-attach once more)."
)]
struct Args {
    /// Emulated model.
    #[arg(long, default_value = "qa402", value_parser = parse_model)]
    model: Model,

    /// USB vendor id (hex or decimal, e.g. 0x16C0).
    #[arg(long, value_parser = parse_u16, default_value = "0x16C0")]
    vid: u16,

    /// USB product id. Defaults to the model's real PID (QA402 0x4E37, QA403 0x4E39).
    #[arg(long, value_parser = parse_u16)]
    pid: Option<u16>,

    /// USB serial string; register 0x1D serves its hex digits packed as u32.
    /// Default: a random one per launch, in the real units' XXXX_XXXX format.
    /// Pin it (e.g. --serial AB12_CD34) for a stable identity across runs.
    #[arg(long)]
    serial: Option<String>,

    /// Firmware build number served by register 0x10 (real units: 60).
    #[arg(long, default_value_t = 60)]
    fw_version: u32,

    /// Firmware version reported after a fake flash (when the image is not in
    /// the registry). Without it the version is unchanged by a flash.
    #[arg(long)]
    post_flash_version: Option<u32>,

    /// Path to a firmware-registry.json (sha256 -> version) to identify
    /// flashed images.
    #[arg(long)]
    registry: Option<PathBuf>,

    /// Listen address of the USB/IP server.
    #[arg(long, default_value = "0.0.0.0:3240")]
    listen: SocketAddr,

    /// USB/IP bus id the device is exported under.
    #[arg(long, default_value = "1-1")]
    busid: String,

    /// Serve ADC data as fast as the host asks instead of pacing it at the
    /// sample rate like real hardware.
    #[arg(long)]
    no_realtime: bool,

    /// DAC->ADC loopback latency, in samples.
    #[arg(long, default_value_t = 1200)]
    latency_samples: usize,

    /// ADC noise floor in dBFS RMS.
    #[arg(long, default_value_t = -140.0, allow_hyphen_values = true)]
    noise_dbfs: f32,

    /// Fixed noise RNG seed: every stream restart replays the identical noise
    /// sequence (reproducible runs). Default: fresh entropy per acquisition.
    #[arg(long)]
    noise_seed: Option<u64>,

    /// Disable the DAC->ADC loopback (open input).
    #[arg(long)]
    no_loopback: bool,

    /// Extra loopback gain in dB (0 = ideal cable).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    loopback_gain_db: f32,

    /// 2nd-harmonic distortion injected in the ADC path, in dBc at full scale
    /// (e.g. -80).
    #[arg(long, allow_hyphen_values = true)]
    h2_dbc: Option<f32>,

    /// 3rd-harmonic distortion, in dBc at full scale.
    #[arg(long, allow_hyphen_values = true)]
    h3_dbc: Option<f32>,

    /// Independent generator at the ADC input: "FREQ_HZ:LEVEL_DBV[:left|right|both]"
    /// (e.g. "1000:-10" or "1000:-10:left").
    #[arg(long)]
    gen: Option<String>,

    /// Directory where every firmware image received by the fake bootloader
    /// is saved (created if needed), as qa40x-flash-<n>-<sha8>.sb.
    #[arg(long)]
    save_firmware: Option<PathBuf>,

    /// Calibration page to serve: a 512-byte file (extract one from a capture
    /// with tools/extract_calpage.py), or "random" to generate a valid page
    /// with realistic randomized trims. Default: the embedded real QA402 page.
    #[arg(long)]
    cal_page: Option<String>,

    /// Simulate the flash-write time: pace the bootloader's ReceiveSbFile
    /// data phase over roughly this many seconds (e.g. 6). Default: instant.
    #[arg(long, default_value_t = 0.0)]
    flash_secs: f32,

    /// Boot straight into the NXP KBOOT bootloader (1fc9:0022), awaiting a
    /// firmware image — reproduces a unit stuck after a failed flash so the
    /// official app's recovery ("Load default secure image, must be in HID
    /// mode") can be exercised.
    #[arg(long)]
    boot_bootloader: bool,

    /// Load a multi-device environment from a JSON file (see
    /// examples/devices.json). Each device gets its own busid and serial;
    /// this overrides the single-device flags above.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Print the JSON config equivalent to the current settings and exit — a
    /// starting template for --config.
    #[arg(long)]
    dump_config: bool,
}

fn parse_model(s: &str) -> Result<Model, String> {
    match s.to_ascii_lowercase().as_str() {
        "qa402" => Ok(Model::Qa402),
        "qa403" => Ok(Model::Qa403),
        _ => Err("expected qa402 or qa403".into()),
    }
}

fn parse_u16(s: &str) -> Result<u16, String> {
    let s = s.trim();
    let r = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16)
    } else {
        s.parse()
    };
    r.map_err(|e| e.to_string())
}

fn parse_gen(s: &str) -> Result<GenSpec, String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err("expected FREQ_HZ:LEVEL_DBV[:left|right|both]".into());
    }
    let freq_hz: f32 = parts[0].parse().map_err(|_| "invalid frequency")?;
    let level_dbv: f32 = parts[1].parse().map_err(|_| "invalid level")?;
    let channel = match parts.get(2).map(|c| c.to_ascii_lowercase()) {
        None => GenChannel::Both,
        Some(c) if c == "left" => GenChannel::Left,
        Some(c) if c == "right" => GenChannel::Right,
        Some(c) if c == "both" => GenChannel::Both,
        Some(c) => return Err(format!("unknown channel {c:?}")),
    };
    Ok(GenSpec {
        freq_hz,
        level_dbv,
        channel,
    })
}

/// A random serial in the real units' format when the spec is absent or asks
/// for "random"/"dynamic"; otherwise the fixed serial verbatim.
fn resolve_serial(spec: Option<&str>) -> String {
    match spec {
        None => vqa40x_core::options::random_serial(),
        Some(s) if s.eq_ignore_ascii_case("random") || s.eq_ignore_ascii_case("dynamic") => {
            vqa40x_core::options::random_serial()
        }
        Some(s) => s.to_string(),
    }
}

/// Load the firmware registry named by a spec string (a path), if any.
fn load_registry(spec: Option<&str>) -> Result<Option<FirmwareRegistry>> {
    let Some(path) = spec else { return Ok(None) };
    let r = FirmwareRegistry::load(std::path::Path::new(path))
        .map_err(|e| anyhow::anyhow!("failed to load registry {path:?}: {e}"))?;
    if r.is_empty() {
        bail!("registry {path:?} contains no usable sha256->version entries");
    }
    Ok(Some(r))
}

/// Resolve a `cal_page` spec: `None`/"real" → embedded page, "random" →
/// generated, otherwise a 512-byte file.
fn load_cal_page(spec: Option<&str>) -> Result<Option<[u8; 512]>> {
    match spec {
        None => Ok(None),
        Some(s) if s.eq_ignore_ascii_case("real") => Ok(None),
        Some(s) if s.eq_ignore_ascii_case("random") => {
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            Ok(Some(vqa40x_core::calpage::generate_page(seed)))
        }
        Some(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("cannot read calibration page {path:?}"))?;
            let page: [u8; 512] = bytes.as_slice().try_into().map_err(|_| {
                anyhow::anyhow!(
                    "calibration page {path:?} is {} bytes, expected exactly 512",
                    bytes.len()
                )
            })?;
            if !vqa40x_core::calpage::page_crc_ok(&page) {
                log::warn!(
                    "calibration page {path} fails its CRC-16 — the official app will \
                     reject it as invalid"
                );
            }
            Ok(Some(page))
        }
    }
}

/// Build the runtime options for one device from its spec. `index` seeds the
/// auto-assigned busid ("1-N") when the spec omits one.
fn spec_to_options(spec: &DeviceSpec, index: usize) -> Result<SimOptions> {
    let model = match spec
        .model
        .as_deref()
        .unwrap_or("qa402")
        .to_ascii_lowercase()
        .as_str()
    {
        "qa402" => Model::Qa402,
        "qa403" => Model::Qa403,
        other => bail!("device {index}: unknown model {other:?} (expected qa402 or qa403)"),
    };
    let vid = match spec.vid.as_deref() {
        Some(s) => parse_u16(s).map_err(|e| anyhow::anyhow!("device {index}: vid: {e}"))?,
        None => 0x16C0,
    };
    let pid = match spec.pid.as_deref() {
        Some(s) => parse_u16(s).map_err(|e| anyhow::anyhow!("device {index}: pid: {e}"))?,
        None => model.default_pid(),
    };
    let generator = match spec.gen.as_deref() {
        Some(s) => Some(parse_gen(s).map_err(|e| anyhow::anyhow!("device {index}: gen: {e}"))?),
        None => None,
    };
    Ok(SimOptions {
        model,
        vid,
        pid,
        serial: resolve_serial(spec.serial.as_deref()),
        fw_version: spec.fw_version.unwrap_or(60),
        post_flash_version: spec.post_flash_version,
        registry: load_registry(spec.registry.as_deref())?,
        realtime: spec.realtime.unwrap_or(true),
        latency_samples: spec.latency_samples.unwrap_or(1200),
        noise_dbfs: spec.noise_dbfs.unwrap_or(-140.0),
        noise_seed: spec.noise_seed,
        loopback: spec.loopback.unwrap_or(true),
        loopback_gain_db: spec.loopback_gain_db.unwrap_or(0.0),
        h2_dbc: spec.h2_dbc,
        h3_dbc: spec.h3_dbc,
        generator,
        busid: spec
            .busid
            .clone()
            .unwrap_or_else(|| format!("1-{}", index + 1)),
        save_firmware: spec.save_firmware.as_ref().map(PathBuf::from),
        cal_page: load_cal_page(spec.cal_page.as_deref())?,
        flash_secs: spec.flash_secs.unwrap_or(0.0),
        boot_bootloader: spec.boot_bootloader.unwrap_or(false),
    })
}

/// The single-device spec described by the top-level flags.
fn args_to_spec(a: &Args) -> DeviceSpec {
    DeviceSpec {
        model: Some(a.model.name().to_ascii_lowercase()),
        busid: Some(a.busid.clone()),
        serial: a.serial.clone(),
        vid: Some(format!("0x{:04X}", a.vid)),
        pid: a.pid.map(|p| format!("0x{p:04X}")),
        fw_version: Some(a.fw_version),
        boot_bootloader: Some(a.boot_bootloader),
        post_flash_version: a.post_flash_version,
        flash_secs: Some(a.flash_secs),
        registry: a.registry.as_ref().map(|p| p.display().to_string()),
        save_firmware: a.save_firmware.as_ref().map(|p| p.display().to_string()),
        cal_page: a.cal_page.clone(),
        realtime: Some(!a.no_realtime),
        latency_samples: Some(a.latency_samples),
        noise_dbfs: Some(a.noise_dbfs),
        noise_seed: a.noise_seed,
        loopback: Some(!a.no_loopback),
        loopback_gain_db: Some(a.loopback_gain_db),
        h2_dbc: a.h2_dbc,
        h3_dbc: a.h3_dbc,
        gen: a.gen.clone(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    // Device specs: from the environment file, or the single-device flags.
    let specs: Vec<DeviceSpec> = match &args.config {
        Some(path) => EnvConfig::load(path)?.devices,
        None => vec![args_to_spec(&args)],
    };

    if args.dump_config {
        let env = EnvConfig {
            devices: specs.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&env)?);
        return Ok(());
    }

    // Build one Simulator per device, checking busid uniqueness.
    let mut options = Vec::with_capacity(specs.len());
    let mut seen = std::collections::HashSet::new();
    for (i, spec) in specs.iter().enumerate() {
        let opts = spec_to_options(spec, i)?;
        if !seen.insert(opts.busid.clone()) {
            bail!(
                "duplicate busid {:?} — each device needs a unique busid",
                opts.busid
            );
        }
        options.push(opts);
    }

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("cannot bind {}", args.listen))?;
    let addr = listener.local_addr()?;
    info!(
        "USB/IP server listening on {addr} — {} device(s)",
        options.len()
    );

    let devices: Vec<Simulator> = options
        .into_iter()
        .map(|opts| {
            let persona = if opts.boot_bootloader {
                "bootloader"
            } else {
                "analyzer"
            };
            info!(
                "  [{}] {} {:04x}:{:04x} serial {} fw v{} ({persona})",
                opts.busid,
                opts.model.name(),
                opts.vid,
                opts.pid,
                opts.serial,
                opts.fw_version,
            );
            Simulator::new(opts)
        })
        .collect();
    info!("attach: usbip attach -r <this-host> -b <busid>  (see busids above)");

    tokio::select! {
        r = vqa40x_usbip::serve_many(devices, listener) => r.context("server error")?,
        _ = tokio::signal::ctrl_c() => info!("interrupted — shutting down"),
    }
    Ok(())
}
