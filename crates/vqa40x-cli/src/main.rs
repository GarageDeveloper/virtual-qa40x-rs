//! `vqa40x` — a virtual QuantAsylum QA402/QA403 exported over USB/IP.

use anyhow::{bail, Context, Result};
use clap::Parser;
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
    #[arg(long, value_parser = parse_gen)]
    gen: Option<GenSpec>,

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

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let registry = match &args.registry {
        Some(path) => {
            let r = FirmwareRegistry::load(path)
                .map_err(|e| anyhow::anyhow!("failed to load registry {path:?}: {e}"))?;
            if r.is_empty() {
                bail!("registry {path:?} contains no usable sha256->version entries");
            }
            info!("firmware registry loaded: {} image hashes", r.len());
            Some(r)
        }
        None => None,
    };

    let cal_page = match args.cal_page.as_deref() {
        Some("random") => {
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let page = vqa40x_core::calpage::generate_page(seed);
            info!(
                "serving a generated calibration page (ADC L0 {:+.3} dB, DAC L0 {:+.3} dB)",
                vqa40x_core::calpage::adc_trim_db(&page, 0, 0),
                vqa40x_core::calpage::dac_trim_db(&page, 0, 0),
            );
            Some(page)
        }
        Some(path) => {
            let path = PathBuf::from(path);
            let bytes = std::fs::read(&path)
                .with_context(|| format!("cannot read calibration page {path:?}"))?;
            let page: [u8; 512] = bytes.as_slice().try_into().map_err(|_| {
                anyhow::anyhow!(
                    "calibration page {path:?} is {} bytes, expected exactly 512",
                    bytes.len()
                )
            })?;
            if !vqa40x_core::calpage::page_crc_ok(&page) {
                log::warn!(
                    "calibration page {} fails its CRC-16 — the official app will \
                     reject it as invalid",
                    path.display()
                );
            }
            info!("serving custom calibration page from {}", path.display());
            Some(page)
        }
        None => None,
    };

    let opts = SimOptions {
        model: args.model,
        vid: args.vid,
        pid: args.pid.unwrap_or_else(|| args.model.default_pid()),
        serial: args
            .serial
            .unwrap_or_else(vqa40x_core::options::random_serial),
        fw_version: args.fw_version,
        post_flash_version: args.post_flash_version,
        registry,
        realtime: !args.no_realtime,
        latency_samples: args.latency_samples,
        noise_dbfs: args.noise_dbfs,
        loopback: !args.no_loopback,
        loopback_gain_db: args.loopback_gain_db,
        h2_dbc: args.h2_dbc,
        h3_dbc: args.h3_dbc,
        generator: args.gen,
        busid: args.busid,
        save_firmware: args.save_firmware,
        cal_page,
        flash_secs: args.flash_secs,
    };

    info!(
        "virtual {} — vid:pid {:04x}:{:04x}, serial {}, firmware v{}",
        opts.model.name(),
        opts.vid,
        opts.pid,
        opts.serial,
        opts.fw_version
    );

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("cannot bind {}", args.listen))?;
    let addr = listener.local_addr()?;
    info!("USB/IP server listening on {addr} (busid {})", opts.busid);
    info!(
        "attach (Linux):   sudo usbip attach -r <this-host> -b {}",
        opts.busid
    );
    info!(
        "attach (Windows): usbip.exe attach -r <this-host> -b {}",
        opts.busid
    );

    let sim = Simulator::new(opts);
    tokio::select! {
        r = vqa40x_usbip::serve(sim, listener) => r.context("server error")?,
        _ = tokio::signal::ctrl_c() => info!("interrupted — shutting down"),
    }
    Ok(())
}
