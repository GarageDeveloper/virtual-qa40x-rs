//! Linux FunctionFS runner: drives the analyzer persona over a mounted
//! FunctionFS instance, so a Raspberry Pi (or any Linux gadget host) plugged
//! into the target machine presents a real QA40x on its USB port.
//!
//! Flow: write the descriptor + strings blobs to `ep0`, bind the UDC (start
//! enumeration), then on the ENABLE event open the endpoint files and pump
//! each one through the shared [`UsbBackend`]:
//!
//! ```text
//!   ep1  (0x01 OUT)  host → device : register writes   → out_transfer(1)
//!   ep2  (0x81 IN)   device → host : register replies  ← in_transfer(1)
//!   ep3  (0x02 OUT)  host → device : DAC audio         → out_transfer(2)
//!   ep4  (0x82 IN)   device → host : ADC audio         ← in_transfer(2)
//! ```
//!
//! Endpoint I/O is blocking (one OS thread each); the async backend methods
//! run on a shared Tokio runtime via `Handle::block_on`. Standard USB control
//! requests are answered by the kernel from the configfs gadget attributes, so
//! ep0 only handles lifecycle events here.

use crate::descriptors;
use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use vqa40x_core::backend::UsbBackend;

/// FunctionFS event types (`usb_functionfs_event_type`).
const FUNCTIONFS_BIND: u8 = 0;
const FUNCTIONFS_UNBIND: u8 = 1;
const FUNCTIONFS_ENABLE: u8 = 2;
const FUNCTIONFS_DISABLE: u8 = 3;
const FUNCTIONFS_SETUP: u8 = 4;
const FUNCTIONFS_SUSPEND: u8 = 5;
const FUNCTIONFS_RESUME: u8 = 6;

/// `struct usb_functionfs_event` is 12 bytes: an 8-byte `usb_ctrlrequest`
/// union member, then `type` + 3 pad bytes.
const EVENT_SIZE: usize = 12;

pub struct GadgetPaths {
    /// FunctionFS mount point (holds ep0, ep1, …).
    pub ffs_dir: PathBuf,
    /// configfs gadget directory (holds the `UDC` file).
    pub gadget_dir: PathBuf,
    /// UDC device name to bind (e.g. the sole entry of /sys/class/udc).
    pub udc: String,
}

/// Run the analyzer persona until interrupted. Blocks the calling thread.
pub fn run(paths: GadgetPaths, backend: Arc<dyn UsbBackend>, handle: Handle) -> Result<()> {
    let ep0_path = paths.ffs_dir.join("ep0");
    let mut ep0 = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&ep0_path)
        .with_context(|| format!("open {} (is FunctionFS mounted?)", ep0_path.display()))?;

    // Declare the function: descriptors then strings.
    let eps = descriptors::analyzer_endpoints();
    ep0.write_all(&descriptors::build_descriptors((0xff, 0, 0), &[], &eps))
        .context("write FunctionFS descriptors to ep0")?;
    ep0.write_all(&descriptors::build_strings())
        .context("write FunctionFS strings to ep0")?;
    info!("FunctionFS descriptors written; binding UDC {}", paths.udc);

    // Bind the UDC to start enumeration.
    let udc_file = paths.gadget_dir.join("UDC");
    std::fs::write(&udc_file, paths.udc.as_bytes())
        .with_context(|| format!("bind UDC via {}", udc_file.display()))?;
    info!("UDC bound — waiting for the host to enumerate");

    let mut pumps_started = false;
    let mut buf = [0u8; EVENT_SIZE * 8];
    loop {
        let n = ep0.read(&mut buf).context("read ep0 events")?;
        if n == 0 {
            continue;
        }
        for evt in buf[..n].chunks_exact(EVENT_SIZE) {
            // Layout: [setup:8][type:1][pad:3].
            let event_type = evt[8];
            match event_type {
                FUNCTIONFS_BIND => info!("FunctionFS: BIND"),
                FUNCTIONFS_UNBIND => info!("FunctionFS: UNBIND"),
                FUNCTIONFS_ENABLE => {
                    info!("FunctionFS: ENABLE (host configured the device)");
                    if !pumps_started {
                        start_pumps(&paths.ffs_dir, &backend, &handle)?;
                        pumps_started = true;
                    }
                }
                FUNCTIONFS_DISABLE => info!("FunctionFS: DISABLE"),
                FUNCTIONFS_SUSPEND => debug!("FunctionFS: SUSPEND"),
                FUNCTIONFS_RESUME => debug!("FunctionFS: RESUME"),
                FUNCTIONFS_SETUP => {
                    // Standard requests are answered by the kernel; anything
                    // forwarded here is unexpected for the analyzer. Stall it
                    // by halting ep0 (best effort).
                    let bm = evt[0];
                    warn!("unexpected ep0 SETUP (bmRequestType 0x{bm:02X}) — stalling");
                    stall_ep0(&ep0);
                }
                other => warn!("FunctionFS: unknown event type {other}"),
            }
        }
    }
}

/// Open the four endpoint files and spawn one blocking pump thread each. The
/// files appear once the interface is enabled; endpoint order matches the
/// descriptor order (ep1..ep4).
fn start_pumps(ffs_dir: &Path, backend: &Arc<dyn UsbBackend>, handle: &Handle) -> Result<()> {
    // (file name, endpoint address, direction)
    let map = [
        ("ep1", 0x01u8, Dir::Out),
        ("ep2", 0x81u8, Dir::In),
        ("ep3", 0x02u8, Dir::Out),
        ("ep4", 0x82u8, Dir::In),
    ];
    for (name, addr, dir) in map {
        let path = ffs_dir.join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(0)
            .open(&path)
            .with_context(|| format!("open endpoint {}", path.display()))?;
        let backend = backend.clone();
        let handle = handle.clone();
        std::thread::Builder::new()
            .name(format!("pump-{name}"))
            .spawn(move || match dir {
                Dir::Out => pump_out(file, addr, backend, handle),
                Dir::In => pump_in(file, addr, backend, handle),
            })
            .context("spawn endpoint pump")?;
    }
    info!("endpoint pumps started");
    Ok(())
}

enum Dir {
    In,
    Out,
}

/// Host → device: read transfers from the endpoint file and hand them to the
/// backend. `read` blocks until the host sends a transfer.
fn pump_out(mut file: File, addr: u8, backend: Arc<dyn UsbBackend>, handle: Handle) {
    // Register frames are 5 bytes; audio blocks are 16 KiB. One buffer covers
    // both (the kernel returns the actual transfer size per read).
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => {
                if let Err(e) = handle.block_on(backend.out_transfer(addr, &buf[..n])) {
                    debug!("out_transfer(0x{addr:02X}) rejected: {e:?}");
                }
            }
            Err(e) => {
                error!("endpoint 0x{addr:02X} read error: {e} — pump exiting");
                return;
            }
        }
    }
}

/// Device → host: ask the backend for the next transfer and write it to the
/// endpoint file. `in_transfer` blocks until data is available (a register
/// reply is queued, or a paced ADC block is due), and `write_all` blocks until
/// the host reads it.
fn pump_in(mut file: File, addr: u8, backend: Arc<dyn UsbBackend>, handle: Handle) {
    // The requested length hints the backend: 4 bytes for a register reply,
    // one USB buffer for audio.
    let want = if addr & 0x7f == 1 { 4 } else { 16 * 1024 };
    loop {
        match handle.block_on(backend.in_transfer(addr, want)) {
            Ok(data) if data.is_empty() => continue,
            Ok(data) => {
                if let Err(e) = file.write_all(&data) {
                    error!("endpoint 0x{addr:02X} write error: {e} — pump exiting");
                    return;
                }
            }
            Err(e) => {
                debug!("in_transfer(0x{addr:02X}) stalled: {e:?}");
            }
        }
    }
}

/// Halt ep0 to signal a protocol stall for an unhandled SETUP. FunctionFS
/// stalls the control endpoint when userspace issues I/O in the wrong
/// direction; a zero-length read is the conventional way to do it.
fn stall_ep0(ep0: &File) {
    use std::os::unix::io::AsRawFd;
    // SAFETY: a 0-length read on the ep0 fd; FunctionFS interprets it as a
    // stall of the pending control transfer.
    unsafe {
        libc::read(ep0.as_raw_fd(), std::ptr::null_mut(), 0);
    }
}
