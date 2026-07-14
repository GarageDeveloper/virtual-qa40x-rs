//! `vqa40x-gadget` — present the virtual QA40x as a REAL USB device through the
//! Linux USB gadget stack (FunctionFS). Run it on a Linux board with a
//! USB device controller (Raspberry Pi Zero/4/5 in OTG mode, etc.) plugged
//! into the machine that should see the analyzer.
//!
//! Set up the configfs gadget + FunctionFS mount with `tools/gadget-setup.sh`
//! first, then run this binary; see GADGET.md.

use clap::Parser;

#[cfg(target_os = "linux")]
mod descriptors;
#[cfg(target_os = "linux")]
mod runner;

// Descriptor building is portable; keep it compiled (and tested) everywhere.
#[cfg(not(target_os = "linux"))]
#[path = "descriptors.rs"]
mod descriptors;

#[derive(Parser, Debug)]
#[command(
    name = "vqa40x-gadget",
    version,
    about = "Present the virtual QA40x as a real USB device via Linux FunctionFS.\n\
             Unofficial project, not affiliated with or endorsed by QuantAsylum."
)]
struct Args {
    /// FunctionFS mount point (holds ep0, ep1, …). See tools/gadget-setup.sh.
    #[arg(long, default_value = "/dev/ffs-vqa40x")]
    ffs_dir: std::path::PathBuf,

    /// configfs gadget directory (holds the UDC file).
    #[arg(long, default_value = "/sys/kernel/config/usb_gadget/vqa40x")]
    gadget_dir: std::path::PathBuf,

    /// UDC device to bind. Defaults to the sole entry under /sys/class/udc.
    #[arg(long)]
    udc: Option<String>,

    /// USB serial string served in register 0x1D (default: random per launch).
    #[arg(long)]
    serial: Option<String>,

    /// Firmware build number served by register 0x10.
    #[arg(long, default_value_t = 60)]
    fw_version: u32,

    /// Emulated model.
    #[arg(long, default_value = "qa402")]
    model: String,
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use vqa40x_core::{Model, SimOptions, Simulator};

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let model = match args.model.to_ascii_lowercase().as_str() {
        "qa402" => Model::Qa402,
        "qa403" => Model::Qa403,
        other => bail!("unknown model {other:?} (expected qa402 or qa403)"),
    };

    let udc = match args.udc {
        Some(u) => u,
        None => detect_udc().context("auto-detect UDC")?,
    };

    let opts = SimOptions {
        model,
        serial: args
            .serial
            .unwrap_or_else(vqa40x_core::options::random_serial),
        fw_version: args.fw_version,
        ..SimOptions::default()
    };
    log::info!(
        "virtual {} — serial {}, firmware v{}, UDC {udc}",
        opts.model.name(),
        opts.serial,
        opts.fw_version
    );

    // The gadget currently drives the analyzer persona (see GADGET.md for the
    // bootloader/flash persona, which needs a configfs reconfigure on switch).
    let runtime = tokio::runtime::Runtime::new()?;
    let handle = runtime.handle().clone();
    let backend = runtime.block_on(async {
        let sim = Simulator::new(opts);
        sim.current()
    });

    let paths = runner::GadgetPaths {
        ffs_dir: args.ffs_dir,
        gadget_dir: args.gadget_dir,
        udc,
    };
    runner::run(paths, backend, handle)
}

/// The single UDC exposed by the board (the common case: one device
/// controller). Errors if there are zero or several — pass `--udc` then.
#[cfg(target_os = "linux")]
fn detect_udc() -> anyhow::Result<String> {
    use anyhow::bail;
    let mut udcs: Vec<String> = std::fs::read_dir("/sys/class/udc")?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    match udcs.len() {
        0 => {
            bail!("no UDC under /sys/class/udc — is the board in USB device/OTG mode (e.g. dwc2)?")
        }
        1 => Ok(udcs.pop().unwrap()),
        _ => bail!("multiple UDCs found ({udcs:?}); pass --udc <name>"),
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    let _ = Args::parse();
    eprintln!(
        "vqa40x-gadget only runs on Linux (it uses the USB gadget/FunctionFS \
         stack). Run it on a Linux board with a USB device controller — see \
         GADGET.md. On other systems, use the USB/IP server (the `vqa40x` binary)."
    );
    std::process::exit(1);
}
