//! USB/IP device-side server for the virtual QA40x.
//!
//! Exports one or more simulated devices, each under its own `busid`. The
//! DEVLIST reply lists every device; IMPORT selects one by busid. Attach from:
//!
//! * **Linux**: `sudo modprobe vhci-hcd && sudo usbip attach -r <host> -b 1-1`
//! * **Windows**: usbip-win2 (`usbip.exe attach -r <host> -b 1-1`)
//!
//! When a device "reboots" (bootloader entry, end of a fake flash) its
//! connection is dropped so the client sees a real unplug, and the next attach
//! enumerates the new persona.

pub mod proto;
pub mod record;

use log::{debug, info, warn};
use proto::*;
use record::{ConnRecorder, Recorder};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vqa40x_core::backend::{SetupPacket, Stall, UsbBackend};
use vqa40x_core::Simulator;

/// Serve one simulated device (convenience wrapper over [`serve_many`]).
pub async fn serve(sim: Simulator, listener: TcpListener) -> std::io::Result<()> {
    serve_many(vec![sim], listener).await
}

/// Serve a set of simulated devices on an already-bound listener, forever.
/// Each device is exported under its own busid; busids must be unique.
pub async fn serve_many(devices: Vec<Simulator>, listener: TcpListener) -> std::io::Result<()> {
    serve_many_recorded(devices, listener, None).await
}

/// [`serve_many`] with an optional USB exchange recorder ([`record`]): every
/// URB of every attached client is captured to its JSONL log.
pub async fn serve_many_recorded(
    devices: Vec<Simulator>,
    listener: TcpListener,
    recorder: Option<Arc<Recorder>>,
) -> std::io::Result<()> {
    let devices = Arc::new(devices);
    loop {
        let (socket, peer) = listener.accept().await?;
        // Debug level: an auto-attach retry loop probes every few hundred ms,
        // which would flood the log at info.
        debug!("connection from {peer}");
        let devices = devices.clone();
        let recorder = recorder.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(devices, socket, recorder).await {
                debug!("connection {peer} ended: {e}");
            } else {
                debug!("connection {peer} closed");
            }
        });
    }
}

async fn handle_connection(
    devices: Arc<Vec<Simulator>>,
    mut socket: TcpStream,
    recorder: Option<Arc<Recorder>>,
) -> std::io::Result<()> {
    socket.set_nodelay(true)?;
    // Detect dead peers (killed VM, dropped network): a vanished client must
    // release the single-attach import instead of holding the device forever.
    {
        let sock = socket2::SockRef::from(&socket);
        let ka = socket2::TcpKeepalive::new()
            .with_time(std::time::Duration::from_secs(10))
            .with_interval(std::time::Duration::from_secs(5));
        sock.set_tcp_keepalive(&ka)?;
    }

    // --- Operation phase: DEVLIST or IMPORT ---------------------------------
    let mut op = [0u8; 8];
    socket.read_exact(&mut op).await?;
    let code = u16::from_be_bytes([op[2], op[3]]);

    match code {
        OP_REQ_DEVLIST => {
            // List every present device (an "unplugged" one — e.g. mid
            // post-flash replug — shows no device, like an empty port).
            let exported: Vec<&Simulator> = devices.iter().filter(|d| !d.is_unplugged()).collect();
            let mut reply = op_header(OP_REP_DEVLIST, 0);
            reply.extend_from_slice(&(exported.len() as u32).to_be_bytes());
            for d in exported {
                let backend = d.current();
                let summary = backend.summary();
                reply.extend_from_slice(&usb_device_block(
                    d.busid(),
                    &summary,
                    summary.interfaces.len() as u8,
                ));
                for (class, subclass, protocol) in &summary.interfaces {
                    reply.extend_from_slice(&[*class, *subclass, *protocol, 0]);
                }
            }
            socket.write_all(&reply).await?;
            Ok(())
        }
        OP_REQ_IMPORT => {
            let mut busid = [0u8; 32];
            socket.read_exact(&mut busid).await?;
            let requested = String::from_utf8_lossy(&busid)
                .trim_end_matches('\0')
                .to_string();

            let Some(sim) = devices.iter().find(|d| d.busid() == requested) else {
                warn!("import for unknown busid {requested:?}");
                socket.write_all(&op_header(OP_REP_IMPORT, 1)).await?;
                return Ok(());
            };
            if sim.is_unplugged() {
                // Simulated cable-out window (post-flash user replug).
                debug!("import refused: {requested} is (virtually) unplugged");
                socket.write_all(&op_header(OP_REP_IMPORT, 1)).await?;
                return Ok(());
            }
            if !sim.try_import() {
                // Normal with an auto-attach loop running while attached.
                debug!("import refused: {requested} already attached to another client");
                socket.write_all(&op_header(OP_REP_IMPORT, 1)).await?;
                return Ok(());
            }

            let backend = sim.current();
            let summary = backend.summary();
            let mut reply = op_header(OP_REP_IMPORT, 0);
            reply.extend_from_slice(&usb_device_block(
                sim.busid(),
                &summary,
                summary.interfaces.len() as u8,
            ));
            socket.write_all(&reply).await?;
            info!(
                "device {:04x}:{:04x} attached (busid {})",
                summary.vid,
                summary.pid,
                sim.busid()
            );

            let crec = recorder
                .as_ref()
                .map(|r| r.connection(sim.busid(), socket.peer_addr().ok(), &summary));
            let result = urb_phase(sim, backend, socket, crec).await;
            sim.release_import();
            result
        }
        other => {
            warn!("unknown USB/IP op code 0x{other:04X}");
            Ok(())
        }
    }
}

/// A blocking IN request handed to a per-endpoint worker.
struct InJob {
    seqnum: u32,
    ep_addr: u8,
    len: usize,
}

/// An OUT transfer handed to a per-endpoint worker. OUT is workered too:
/// a paced endpoint (EP3 front-panel I2S accepts one block per 42.7 ms)
/// must not stall the URB loop, or every register URB queued behind it
/// inherits the pacing delay — the host app's short keepalive/telemetry
/// timeouts then expire and it declares the device gone. Real USB runs
/// endpoints concurrently; only per-endpoint ordering is guaranteed.
struct OutJob {
    seqnum: u32,
    ep_addr: u8,
    data: Vec<u8>,
}

/// Authority deciding who replies for a seqnum: pending URBs own a
/// cancellation token; completion and unlink race through this map.
type Pending = Arc<Mutex<HashMap<u32, CancellationToken>>>;

async fn urb_phase(
    sim: &Simulator,
    backend: Arc<dyn UsbBackend>,
    socket: TcpStream,
    crec: Option<Arc<ConnRecorder>>,
) -> std::io::Result<()> {
    let (mut rd, mut wr) = socket.into_split();

    // Single writer task: RET packets from everywhere, in queue order.
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let conn_token = CancellationToken::new();
    let writer_token = conn_token.clone();
    let writer = tokio::spawn(async move {
        while let Some(pkt) = rx.recv().await {
            if wr.write_all(&pkt).await.is_err() {
                writer_token.cancel();
                break;
            }
        }
    });

    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    // Per-endpoint-address FIFO workers, so completions keep submission
    // order per endpoint while endpoints stay concurrent with each other
    // (see [`OutJob`] for why OUT must not run inline in this loop).
    let mut in_workers: HashMap<u8, mpsc::UnboundedSender<InJob>> = HashMap::new();
    let mut out_workers: HashMap<u8, mpsc::UnboundedSender<OutJob>> = HashMap::new();

    // The device "reboots" (persona switch) → drop the connection.
    let mut generation = sim.generation();

    let result = loop {
        let mut hdr = [0u8; 48];
        let read = tokio::select! {
            r = rd.read_exact(&mut hdr) => r,
            _ = generation.changed() => {
                info!("device rebooted — detaching client");
                if let Some(rec) = &crec {
                    rec.detach("device_rebooted");
                }
                break Ok(());
            }
            _ = conn_token.cancelled() => {
                if let Some(rec) = &crec {
                    rec.detach("connection_closed");
                }
                break Ok(());
            }
        };
        if let Err(e) = read {
            // Client detached (usbip detach / vhci teardown / dead peer).
            info!("client detached ({})", e.kind());
            if let Some(rec) = &crec {
                rec.detach("client_detached");
            }
            break if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(e)
            };
        }

        let h = UrbHeader::parse(&hdr);
        match h.command {
            USBIP_CMD_SUBMIT => {
                let tlen = h.transfer_length();
                let mut out_data = Vec::new();
                if h.direction == DIR_OUT && tlen > 0 {
                    out_data = vec![0u8; tlen];
                    rd.read_exact(&mut out_data).await?;
                }

                if h.ep == 0 {
                    handle_control(&backend, &h, &out_data, &tx, &crec).await;
                } else if h.direction == DIR_OUT {
                    let ep_addr = (h.ep & 0x0f) as u8;
                    let token = CancellationToken::new();
                    pending.lock().unwrap().insert(h.seqnum, token);
                    let worker = out_workers.entry(ep_addr).or_insert_with(|| {
                        spawn_out_worker(
                            backend.clone(),
                            tx.clone(),
                            pending.clone(),
                            conn_token.clone(),
                            crec.clone(),
                        )
                    });
                    let _ = worker.send(OutJob {
                        seqnum: h.seqnum,
                        ep_addr,
                        data: out_data,
                    });
                } else {
                    // Blocking IN: dispatch to the endpoint's FIFO worker.
                    let ep_addr = 0x80 | (h.ep & 0x0f) as u8;
                    if let Some(rec) = &crec {
                        rec.in_req(h.seqnum, ep_addr, tlen);
                    }
                    let token = CancellationToken::new();
                    pending.lock().unwrap().insert(h.seqnum, token);
                    let worker = in_workers.entry(ep_addr).or_insert_with(|| {
                        spawn_in_worker(
                            backend.clone(),
                            tx.clone(),
                            pending.clone(),
                            conn_token.clone(),
                            crec.clone(),
                        )
                    });
                    let _ = worker.send(InJob {
                        seqnum: h.seqnum,
                        ep_addr,
                        len: tlen,
                    });
                }
            }
            USBIP_CMD_UNLINK => {
                let victim = h.unlink_seqnum();
                let token = pending.lock().unwrap().remove(&victim);
                if let Some(rec) = &crec {
                    rec.unlink(h.seqnum, victim, token.is_some());
                }
                match token {
                    Some(t) => {
                        t.cancel();
                        debug!("unlinked pending URB seq {victim}");
                        let _ = tx.send(ret_unlink(h.seqnum, ST_ECONNRESET));
                    }
                    None => {
                        // Already completed — per protocol, report 0.
                        let _ = tx.send(ret_unlink(h.seqnum, ST_OK));
                    }
                }
            }
            other => {
                warn!("unknown URB command 0x{other:08X} — closing");
                if let Some(rec) = &crec {
                    rec.detach("protocol_error");
                }
                break Ok(());
            }
        }
    };

    conn_token.cancel();
    drop(in_workers);
    drop(out_workers);
    drop(tx);
    let _ = writer.await;
    result
}

/// One FIFO worker per IN endpoint: preserves completion order while letting
/// unlink/disconnect cancel a blocked transfer.
fn spawn_in_worker(
    backend: Arc<dyn UsbBackend>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    pending: Pending,
    conn_token: CancellationToken,
    crec: Option<Arc<ConnRecorder>>,
) -> mpsc::UnboundedSender<InJob> {
    let (job_tx, mut job_rx) = mpsc::unbounded_channel::<InJob>();
    tokio::spawn(async move {
        while let Some(job) = job_rx.recv().await {
            let token = match pending.lock().unwrap().get(&job.seqnum) {
                Some(t) => t.clone(),
                // Unlinked before we got to it.
                None => continue,
            };
            tokio::select! {
                r = backend.in_transfer(job.ep_addr, job.len) => {
                    // Whoever removes the seqnum from the map replies.
                    if pending.lock().unwrap().remove(&job.seqnum).is_some() {
                        if let Some(rec) = &crec {
                            rec.in_result(job.seqnum, job.ep_addr, &r);
                        }
                        let pkt = match r {
                            Ok(data) => {
                                let n = data.len().min(job.len);
                                ret_submit(job.seqnum, ST_OK, Some(&data[..n]), 0)
                            }
                            Err(Stall) => ret_submit(job.seqnum, ST_EPIPE, Some(&[]), 0),
                        };
                        let _ = tx.send(pkt);
                    }
                }
                _ = token.cancelled() => { /* RET_UNLINK already sent */ }
                _ = conn_token.cancelled() => break,
            }
        }
    });
    job_tx
}

/// One FIFO worker per OUT endpoint: transfers complete in submission order
/// per endpoint, and a paced endpoint (I2S) never delays the others.
fn spawn_out_worker(
    backend: Arc<dyn UsbBackend>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    pending: Pending,
    conn_token: CancellationToken,
    crec: Option<Arc<ConnRecorder>>,
) -> mpsc::UnboundedSender<OutJob> {
    let (job_tx, mut job_rx) = mpsc::unbounded_channel::<OutJob>();
    tokio::spawn(async move {
        while let Some(job) = job_rx.recv().await {
            let token = match pending.lock().unwrap().get(&job.seqnum) {
                Some(t) => t.clone(),
                // Unlinked before we got to it.
                None => continue,
            };
            tokio::select! {
                r = backend.out_transfer(job.ep_addr, &job.data) => {
                    // Whoever removes the seqnum from the map replies.
                    if pending.lock().unwrap().remove(&job.seqnum).is_some() {
                        let status = match r {
                            Ok(_) => ST_OK,
                            Err(Stall) => ST_EPIPE,
                        };
                        if let Some(rec) = &crec {
                            rec.out(job.seqnum, job.ep_addr, &job.data, status != ST_OK);
                        }
                        let _ = tx.send(ret_submit(job.seqnum, status, None, job.data.len()));
                    }
                }
                _ = token.cancelled() => { /* RET_UNLINK already sent */ }
                _ = conn_token.cancelled() => break,
            }
        }
    });
    job_tx
}

async fn handle_control(
    backend: &Arc<dyn UsbBackend>,
    h: &UrbHeader,
    out_data: &[u8],
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    crec: &Option<Arc<ConnRecorder>>,
) {
    let setup = SetupPacket::parse(&h.setup());
    let result = backend.control(setup, out_data).await;
    if let Some(rec) = crec {
        rec.control(h.seqnum, &setup, out_data, &result);
    }
    let pkt = match (h.direction == DIR_IN, result) {
        (true, Ok(data)) => {
            let n = data.len().min(h.transfer_length());
            ret_submit(h.seqnum, ST_OK, Some(&data[..n]), 0)
        }
        (false, Ok(_)) => ret_submit(h.seqnum, ST_OK, None, out_data.len()),
        (true, Err(Stall)) => ret_submit(h.seqnum, ST_EPIPE, Some(&[]), 0),
        (false, Err(Stall)) => ret_submit(h.seqnum, ST_EPIPE, None, 0),
    };
    let _ = tx.send(pkt);
}
