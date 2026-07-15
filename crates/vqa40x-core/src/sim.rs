//! The simulator: owns the current persona (analyzer or bootloader), the
//! persisted state that survives "reboots" (firmware version), and the
//! persona-switch choreography of a fake firmware update:
//!
//! ```text
//! analyzer --0x0F magic--> [detach] bootloader --ReceiveSbFile ok--> [detach] analyzer (new fw)
//! ```
//!
//! Transports watch the `generation` channel: when it bumps, the currently
//! attached device is gone (the unit "rebooted") and the connection must be
//! dropped so the host sees an unplug, exactly like real hardware.

use crate::analyzer::Analyzer;
use crate::backend::UsbBackend;
use crate::bootloader::Bootloader;
use crate::options::SimOptions;
use log::{info, warn};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Events emitted by the personas.
#[derive(Debug)]
pub enum SimEvent {
    /// Analyzer received the 0x0F two-magic sequence.
    EnterBootloader,
    /// Bootloader accepted a full SB2.1 image.
    FlashCompleted { image: Vec<u8> },
    /// Bootloader received a Reset command (no flash).
    BootloaderReset,
}

struct SimInner {
    opts: Arc<SimOptions>,
    fw_version: Mutex<u32>,
    /// Number of completed fake flashes (for logs / curiosity).
    flash_count: Mutex<u32>,
    current: Mutex<Arc<dyn UsbBackend>>,
    generation: watch::Sender<u64>,
    events_tx: mpsc::UnboundedSender<SimEvent>,
    imported: AtomicBool,
    /// Simulated "cable out" window: while true, imports are refused as if no
    /// device were plugged in.
    unplugged: AtomicBool,
}

/// Handle shared by transports. Cheap to clone.
#[derive(Clone)]
pub struct Simulator {
    inner: Arc<SimInner>,
}

impl Simulator {
    pub fn new(opts: SimOptions) -> Self {
        let opts = Arc::new(opts);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (generation, _) = watch::channel(0u64);

        // A unit whose last flash failed boots stuck in the DFU bootloader;
        // `boot_bootloader` reproduces that so the official app's recovery
        // flow can be tested. Otherwise boot into the analyzer.
        let initial: Arc<dyn UsbBackend> = if opts.boot_bootloader {
            info!("booting into the NXP KBOOT bootloader (1fc9:0022), awaiting a firmware image");
            Arc::new(Bootloader::new(opts.clone(), events_tx.clone()))
        } else {
            Arc::new(Analyzer::new(
                opts.clone(),
                opts.fw_version,
                events_tx.clone(),
            ))
        };

        let inner = Arc::new(SimInner {
            fw_version: Mutex::new(opts.fw_version),
            flash_count: Mutex::new(0),
            current: Mutex::new(initial),
            generation,
            events_tx,
            imported: AtomicBool::new(false),
            unplugged: AtomicBool::new(false),
            opts,
        });

        let sim = Self { inner };
        tokio::spawn(sim.clone().event_loop(events_rx));
        sim
    }

    /// The USB/IP bus id this device is exported under.
    pub fn busid(&self) -> &str {
        &self.inner.opts.busid
    }

    /// The currently attached persona.
    pub fn current(&self) -> Arc<dyn UsbBackend> {
        self.inner.current.lock().unwrap().clone()
    }

    /// Subscribe to persona switches ("reboots"). When the value changes, the
    /// device a transport was serving is gone.
    pub fn generation(&self) -> watch::Receiver<u64> {
        self.inner.generation.subscribe()
    }

    /// Current firmware version (register 0x10 of the analyzer persona).
    pub fn fw_version(&self) -> u32 {
        *self.inner.fw_version.lock().unwrap()
    }

    /// Whether the device is in a simulated "unplugged" window (e.g. the
    /// user-replug pause after a firmware flash). Imports must be refused.
    pub fn is_unplugged(&self) -> bool {
        self.inner.unplugged.load(Ordering::SeqCst)
    }

    /// Exclusive-attach guard: a USB device can only be plugged into one host.
    /// Returns false if some connection already imported the device.
    pub fn try_import(&self) -> bool {
        !self.inner.imported.swap(true, Ordering::SeqCst)
    }

    pub fn release_import(&self) {
        self.inner.imported.store(false, Ordering::SeqCst);
    }

    async fn event_loop(self, mut rx: mpsc::UnboundedReceiver<SimEvent>) {
        while let Some(ev) = rx.recv().await {
            match ev {
                SimEvent::EnterBootloader => {
                    // Give the transport a beat to flush the RET of the second
                    // magic write before the device "detaches".
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    info!("persona switch: analyzer -> NXP KBOOT bootloader (1fc9:0022)");
                    self.switch(Arc::new(Bootloader::new(
                        self.inner.opts.clone(),
                        self.inner.events_tx.clone(),
                    )));
                }
                SimEvent::FlashCompleted { image } => {
                    let sha = sha256_hex(&image);
                    info!("flash received: {} bytes, sha256 {sha}", image.len());
                    self.save_flashed_image(&image, &sha);
                    let new_version = self.resolve_flashed_version(&image);
                    {
                        let mut fw = self.inner.fw_version.lock().unwrap();
                        match new_version {
                            Some(v) => {
                                info!("fake flash complete — firmware version {} -> {v}", *fw);
                                *fw = v;
                            }
                            None => {
                                warn!(
                                    "fake flash complete — image unknown and no \
                                     post-flash version configured; keeping version {}",
                                    *fw
                                );
                            }
                        }
                        *self.inner.flash_count.lock().unwrap() += 1;
                    }
                    // Simulate the user unplugging and replugging the unit
                    // after a flash: let the host read the final KBOOT
                    // response and settle (2 s), "pull the cable" (detach +
                    // imports refused), wait 2 s, then the analyzer is
                    // plugged back in.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    self.inner.unplugged.store(true, Ordering::SeqCst);
                    let fw = self.fw_version();
                    info!("flash settled — simulating unplug (2 s cable-out window)");
                    self.switch(Arc::new(Analyzer::new(
                        self.inner.opts.clone(),
                        fw,
                        self.inner.events_tx.clone(),
                    )));
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    self.inner.unplugged.store(false, Ordering::SeqCst);
                    info!("simulated replug — analyzer (firmware v{fw}) is attachable again");
                }
                SimEvent::BootloaderReset => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let fw = self.fw_version();
                    info!("persona switch: bootloader -> analyzer (reset, firmware v{fw})");
                    self.switch(Arc::new(Analyzer::new(
                        self.inner.opts.clone(),
                        fw,
                        self.inner.events_tx.clone(),
                    )));
                }
            }
        }
    }

    fn switch(&self, backend: Arc<dyn UsbBackend>) {
        *self.inner.current.lock().unwrap() = backend;
        self.inner
            .generation
            .send_modify(|g| *g = g.wrapping_add(1));
    }

    /// Persist the image received by the fake bootloader (wire image as sent
    /// by the host app — 2 bytes zeroed at size−4 if it followed the vendor
    /// path). Best-effort: a write failure must not break the flash flow.
    fn save_flashed_image(&self, image: &[u8], sha: &str) {
        let Some(dir) = &self.inner.opts.save_firmware else {
            return;
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!(
                "could not create the firmware save dir {}: {e}",
                dir.display()
            );
            return;
        }
        let n = *self.inner.flash_count.lock().unwrap() + 1;
        let path = dir.join(format!("qa40x-flash-{n}-{}.sb", &sha[..8]));
        match std::fs::write(&path, image) {
            Ok(()) => info!("flashed image saved to {}", path.display()),
            Err(e) => warn!(
                "could not save the flashed image to {}: {e}",
                path.display()
            ),
        }
    }

    /// Decide the post-flash firmware version: registry lookup by image
    /// sha256 first, then the configured fallback.
    fn resolve_flashed_version(&self, image: &[u8]) -> Option<u32> {
        if let Some(registry) = &self.inner.opts.registry {
            if let Some(v) = registry.version_of(image) {
                info!("image found in the firmware registry: version {v}");
                return Some(v);
            }
            info!(
                "image not in the firmware registry (expected: the vendor app zeroes \
                 2 bytes at size-4 before sending)"
            );
        }
        self.inner.opts.post_flash_version
    }
}
