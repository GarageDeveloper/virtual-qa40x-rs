//! End-to-end test: a minimal USB/IP client drives the virtual QA40x through
//! the full lifecycle the real host app exercises — enumeration, register
//! bus, calibration page, loopback streaming, bootloader entry, fake firmware
//! flash, and re-enumeration with the new firmware version.

use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use vqa40x_core::{calpage, SimOptions, Simulator};

const DEVID: u32 = (1 << 16) | 2;

/// Minimal USB/IP client.
struct Client {
    sock: TcpStream,
    seq: u32,
    /// seqnum → expects IN data in the RET.
    pending_in: HashMap<u32, bool>,
    /// RETs read ahead of the one we were waiting for.
    stash: HashMap<u32, (i32, Vec<u8>)>,
}

impl Client {
    /// Connect and try to import the device; `None` if the import is refused
    /// (device attached elsewhere or virtually unplugged).
    async fn try_attach(addr: std::net::SocketAddr, busid: &str) -> Option<(Self, u16, u16)> {
        let mut sock = TcpStream::connect(addr).await.unwrap();
        // OP_REQ_IMPORT
        let mut req = Vec::new();
        req.extend_from_slice(&0x0111u16.to_be_bytes());
        req.extend_from_slice(&0x8003u16.to_be_bytes());
        req.extend_from_slice(&0u32.to_be_bytes());
        let mut b = [0u8; 32];
        b[..busid.len()].copy_from_slice(busid.as_bytes());
        req.extend_from_slice(&b);
        sock.write_all(&req).await.unwrap();

        let mut hdr = [0u8; 8];
        sock.read_exact(&mut hdr).await.unwrap();
        let status = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        if status != 0 {
            return None;
        }
        let mut dev = [0u8; 312];
        sock.read_exact(&mut dev).await.unwrap();
        let vid = u16::from_be_bytes([dev[300], dev[301]]);
        let pid = u16::from_be_bytes([dev[302], dev[303]]);
        Some((
            Self {
                sock,
                seq: 1,
                pending_in: HashMap::new(),
                stash: HashMap::new(),
            },
            vid,
            pid,
        ))
    }

    /// Connect and import the device; returns the client and (vid, pid).
    async fn attach(addr: std::net::SocketAddr, busid: &str) -> (Self, u16, u16) {
        Self::try_attach(addr, busid).await.expect("import refused")
    }

    /// Retry attaching until the device is importable (the auto-attach loop a
    /// real client runs), with a timeout.
    async fn attach_retry(addr: std::net::SocketAddr, busid: &str) -> (Self, u16, u16) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(r) = Self::try_attach(addr, busid).await {
                return r;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "device did not come back within 10 s"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn submit(
        &mut self,
        direction: u32,
        ep: u32,
        setup: [u8; 8],
        data: &[u8],
        in_len: usize,
    ) -> u32 {
        let seq = self.seq;
        self.seq += 1;
        let tlen = if direction == 0 { data.len() } else { in_len };
        let mut pkt = Vec::with_capacity(48 + data.len());
        pkt.extend_from_slice(&1u32.to_be_bytes()); // CMD_SUBMIT
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&DEVID.to_be_bytes());
        pkt.extend_from_slice(&direction.to_be_bytes());
        pkt.extend_from_slice(&ep.to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes()); // transfer_flags
        pkt.extend_from_slice(&(tlen as u32).to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes()); // start_frame
        pkt.extend_from_slice(&0u32.to_be_bytes()); // number_of_packets
        pkt.extend_from_slice(&0u32.to_be_bytes()); // interval
        pkt.extend_from_slice(&setup);
        if direction == 0 {
            pkt.extend_from_slice(data);
        }
        self.sock.write_all(&pkt).await.unwrap();
        self.pending_in.insert(seq, direction == 1);
        seq
    }

    /// Read RETs until `seq` completes; returns (status, data).
    async fn wait(&mut self, seq: u32) -> (i32, Vec<u8>) {
        if let Some(r) = self.stash.remove(&seq) {
            return r;
        }
        loop {
            let mut hdr = [0u8; 48];
            self.sock.read_exact(&mut hdr).await.unwrap();
            let cmd = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
            assert_eq!(cmd, 3, "expected RET_SUBMIT");
            let rseq = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            let status = i32::from_be_bytes([hdr[20], hdr[21], hdr[22], hdr[23]]);
            let actual = u32::from_be_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]) as usize;
            let is_in = self.pending_in.remove(&rseq).expect("known seq");
            let mut data = Vec::new();
            if is_in && actual > 0 {
                data = vec![0u8; actual];
                self.sock.read_exact(&mut data).await.unwrap();
            }
            if rseq == seq {
                return (status, data);
            }
            self.stash.insert(rseq, (status, data));
        }
    }

    async fn control_in(
        &mut self,
        bm: u8,
        req: u8,
        value: u16,
        index: u16,
        len: u16,
    ) -> (i32, Vec<u8>) {
        let mut setup = [0u8; 8];
        setup[0] = bm;
        setup[1] = req;
        setup[2..4].copy_from_slice(&value.to_le_bytes());
        setup[4..6].copy_from_slice(&index.to_le_bytes());
        setup[6..8].copy_from_slice(&len.to_le_bytes());
        let seq = self.submit(1, 0, setup, &[], len as usize).await;
        self.wait(seq).await
    }

    async fn control_out(&mut self, bm: u8, req: u8, value: u16, index: u16, data: &[u8]) -> i32 {
        let mut setup = [0u8; 8];
        setup[0] = bm;
        setup[1] = req;
        setup[2..4].copy_from_slice(&value.to_le_bytes());
        setup[4..6].copy_from_slice(&index.to_le_bytes());
        setup[6..8].copy_from_slice(&(data.len() as u16).to_le_bytes());
        let seq = self.submit(0, 0, setup, data, 0).await;
        self.wait(seq).await.0
    }

    async fn bulk_out(&mut self, ep: u32, data: &[u8]) -> i32 {
        let seq = self.submit(0, ep, [0; 8], data, 0).await;
        self.wait(seq).await.0
    }

    async fn bulk_in(&mut self, ep: u32, len: usize) -> (i32, Vec<u8>) {
        let seq = self.submit(1, ep, [0; 8], &[], len).await;
        self.wait(seq).await
    }

    /// Register write: 5-byte frame on EP1 OUT.
    async fn reg_write(&mut self, reg: u8, value: u32) {
        let mut frame = vec![reg];
        frame.extend_from_slice(&value.to_be_bytes());
        assert_eq!(self.bulk_out(1, &frame).await, 0);
    }

    /// Register read: write `reg|0x80`, read 4 bytes on EP1 IN.
    async fn reg_read(&mut self, reg: u8) -> u32 {
        let mut frame = vec![reg | 0x80];
        frame.extend_from_slice(&[0u8; 4]);
        assert_eq!(self.bulk_out(1, &frame).await, 0);
        let (status, data) = self.bulk_in(1, 512).await;
        assert_eq!(status, 0);
        assert_eq!(data.len(), 4);
        u32::from_be_bytes([data[0], data[1], data[2], data[3]])
    }

    /// Read until the server closes the connection (device "rebooted").
    async fn wait_eof(&mut self) {
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(Duration::from_secs(5), self.sock.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return,
                Ok(Ok(_)) => continue,
                Err(_) => panic!("server did not drop the connection"),
            }
        }
    }
}

fn encode_stereo_wire(samples: &[f32]) -> Vec<u8> {
    // Same value on both channels; wire order is right-then-left, i32 LE.
    let mut buf = Vec::with_capacity(samples.len() * 8);
    for &x in samples {
        let v = (x.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32;
        buf.extend_from_slice(&v.to_le_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

fn decode_left_channel(bytes: &[u8]) -> Vec<f32> {
    let mut left = Vec::with_capacity(bytes.len() / 8);
    let mut i = 0;
    while i + 8 <= bytes.len() {
        // Right sample first on the wire.
        let l = i32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]);
        left.push(l as f32 / 2_147_483_648.0);
        i += 8;
    }
    left
}

/// Frame a KBOOT OUT report (60 bytes): [id][0][len LE][payload][pad].
fn kboot_report(id: u8, payload: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; 60];
    r[0] = id;
    r[2] = (payload.len() & 0xff) as u8;
    r[3] = (payload.len() >> 8) as u8;
    r[4..4 + payload.len()].copy_from_slice(payload);
    r
}

const LATENCY: usize = 256;

#[tokio::test(flavor = "multi_thread")]
async fn full_device_lifecycle() {
    let _ = env_logger::builder().is_test(true).try_init();

    let fw_dir = std::env::temp_dir().join("vqa40x-e2e-firmware");
    let _ = std::fs::remove_dir_all(&fw_dir);
    let _ = std::fs::remove_file(&fw_dir);

    let opts = SimOptions {
        realtime: false,
        latency_samples: LATENCY,
        noise_dbfs: -200.0,
        post_flash_version: Some(61),
        save_firmware: Some(fw_dir.clone()),
        // Pinned: the default is random per construction.
        serial: "AB12_CD34".to_string(),
        ..SimOptions::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sim = Simulator::new(opts);
    tokio::spawn(vqa40x_usbip::serve(sim, listener));

    // ---- Attach: analyzer persona ---------------------------------------
    let (mut c, vid, pid) = Client::attach(addr, "1-1").await;
    assert_eq!((vid, pid), (0x16C0, 0x4E37), "QA402 identity");

    // Standard enumeration.
    let (st, dev) = c.control_in(0x80, 6, 0x0100, 0, 18).await;
    assert_eq!(st, 0);
    assert_eq!(dev.len(), 18);
    assert_eq!(u16::from_le_bytes([dev[8], dev[9]]), 0x16C0);
    assert_eq!(u16::from_le_bytes([dev[10], dev[11]]), 0x4E37);
    assert_eq!(
        c.control_out(0x00, 9, 1, 0, &[]).await,
        0,
        "SET_CONFIGURATION"
    );
    // Config descriptor: 1 interface, 4 bulk endpoints.
    let (st, cfg) = c.control_in(0x80, 6, 0x0200, 0, 255).await;
    assert_eq!(st, 0);
    assert_eq!(cfg[4], 1);
    // String descriptors (nusb serves product/serial from these).
    let (st, s) = c.control_in(0x80, 6, 0x0303, 0x0409, 255).await;
    assert_eq!(st, 0);
    let serial: String =
        char::decode_utf16(s[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])))
            .map(|c| c.unwrap())
            .collect();
    assert_eq!(serial, "AB12_CD34");
    // CLEAR_FEATURE(HALT) on an endpoint, as the host app does.
    assert_eq!(c.control_out(0x02, 1, 0, 0x81, &[]).await, 0);

    // MS OS 2.0 chain, as Windows walks it to auto-bind WinUSB:
    // bcdUSB 0x0201 → BOS → platform capability → vendor request → WINUSB.
    assert_eq!(
        u16::from_le_bytes([dev[2], dev[3]]),
        0x0201,
        "bcdUSB must be BOS-capable"
    );
    let (st, bos) = c.control_in(0x80, 6, 0x0F00, 0, 255).await;
    assert_eq!(st, 0, "BOS descriptor");
    assert_eq!(bos[1], 0x0F);
    let set_len = u16::from_le_bytes([bos[bos.len() - 4], bos[bos.len() - 3]]);
    let vendor_code = bos[bos.len() - 2];
    let (st, set) = c.control_in(0xC0, vendor_code, 0, 7, set_len).await;
    assert_eq!(st, 0, "MS OS 2.0 descriptor set");
    assert_eq!(set.len(), set_len as usize);
    let winusb = set.windows(6).any(|w| w == b"WINUSB");
    assert!(winusb, "descriptor set must carry the WINUSB compatible ID");

    // Comm test: write a pattern to register 0, read it back.
    c.reg_write(0x00, 0xA5A5_0FF0).await;
    assert_eq!(c.reg_read(0x00).await, 0xA5A5_0FF0);

    // Identity registers.
    assert_eq!(c.reg_read(0x10).await, 60, "firmware version");
    assert_eq!(c.reg_read(0x1D).await, 0xAB12_CD34, "serial register");
    assert_eq!(c.reg_read(0x1B).await, 0x4000_0040, "capability word");
    assert!(c.reg_read(0x11).await > 4000, "USB voltage telemetry");

    // Vendor connect writes.
    c.reg_write(0x08, 0).await; // stream stop
    c.reg_write(0x0A, 0).await; // unknown init
    c.reg_write(0x05, 7).await; // 42 dBV
    c.reg_write(0x09, 0).await; // 48 kHz

    // Calibration page: select, then 128 reads of 4 bytes.
    c.reg_write(0x0D, 0x10).await;
    let mut page = Vec::with_capacity(512);
    for _ in 0..128 {
        let v = c.reg_read(0x19).await;
        page.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(page.len(), 512);
    // The served page is the REAL QA402 page (embedded default), byte for
    // byte, as the host app reconstructs it.
    assert_eq!(
        page,
        calpage::REAL_QA402_PAGE,
        "served page == embedded real page"
    );
    // Parse exactly like the host app: f32 LE at record offset + 2; the real
    // trims are ~+8.75 dB (ADC) and ~−0.3 dB (DAC).
    let adc_db = f32::from_le_bytes([page[26], page[27], page[28], page[29]]);
    assert!(
        (8.0..9.5).contains(&adc_db),
        "ADC trim, 0 dBV left: {adc_db}"
    );
    let dac_db = f32::from_le_bytes([page[146], page[147], page[148], page[149]]);
    assert!(
        (-1.0..0.0).contains(&dac_db),
        "DAC trim, 8 dBV left: {dac_db}"
    );

    // ---- Loopback streaming ----------------------------------------------
    // 4096 samples of a 1 kHz sine at 0.5 FS, two 16 KiB blocks.
    let n = 4096;
    let sine: Vec<f32> = (0..n)
        .map(|i| 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / 48000.0).sin())
        .collect();
    let tx = encode_stereo_wire(&sine);
    assert_eq!(tx.len(), 2 * 16384);

    c.reg_write(0x08, 5).await; // stream start
                                // Pre-queue reads and writes interleaved, like the host app.
    let r1 = c.submit(1, 2, [0; 8], &[], 16384).await;
    let w1 = c.submit(0, 2, [0; 8], &tx[..16384], 0).await;
    let r2 = c.submit(1, 2, [0; 8], &[], 16384).await;
    let w2 = c.submit(0, 2, [0; 8], &tx[16384..], 0).await;
    let (st, rx1) = c.wait(r1).await;
    assert_eq!(st, 0);
    let (st, rx2) = c.wait(r2).await;
    assert_eq!(st, 0);
    assert_eq!(c.wait(w1).await.0, 0);
    assert_eq!(c.wait(w2).await.0, 0);
    c.reg_write(0x08, 0).await; // stream stop
    assert_eq!(c.reg_read(0x1E).await, 0x40, "post-stop stream status");

    let mut rx = rx1;
    rx.extend_from_slice(&rx2);
    let adc = decode_left_channel(&rx);
    assert_eq!(adc.len(), n);

    // Latency lead-in is silence.
    for &s in &adc[..LATENCY - 1] {
        assert!(s.abs() < 1e-5, "lead-in must be silent, got {s}");
    }
    // After the lead-in the DAC signal comes back with the volts-model gain:
    // out_FS − in_FS + 9 − adc_trim − dac_trim (42 dBV in, −12 dBV out from
    // the boot/connect defaults), with the trims parsed from the page WE READ
    // over the wire — the same closure a calibrated host applies.
    let g_db =
        -12.0 - 42.0 + 9.0 - calpage::adc_trim_db(&page, 7, 0) - calpage::dac_trim_db(&page, 0, 0);
    let g = 10f32.powf(g_db / 20.0);
    let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
    let span = 2048;
    let measured = rms(&adc[LATENCY..LATENCY + span]);
    let expected = rms(&sine[..span]) * g;
    let ratio = measured / expected;
    assert!(
        (ratio - 1.0).abs() < 0.02,
        "loopback gain off: measured {measured}, expected {expected} (ratio {ratio})"
    );

    // ---- Enter the bootloader ---------------------------------------------
    c.reg_write(0x0F, 0xDEAD_BEEF).await;
    c.reg_write(0x0F, 0xCAFE_BABE).await;
    c.wait_eof().await; // device detaches
    drop(c);

    // ---- Bootloader persona ------------------------------------------------
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (mut c, vid, pid) = Client::attach(addr, "1-1").await;
    assert_eq!((vid, pid), (0x1FC9, 0x0022), "NXP KBOOT identity");

    let (st, dev) = c.control_in(0x80, 6, 0x0100, 0, 18).await;
    assert_eq!(st, 0);
    assert_eq!(u16::from_le_bytes([dev[8], dev[9]]), 0x1FC9);
    assert_eq!(c.control_out(0x00, 9, 1, 0, &[]).await, 0);
    // HID report descriptor must be readable (hidraw needs it).
    let (st, rd) = c.control_in(0x81, 6, 0x2200, 0, 512).await;
    assert_eq!(st, 0);
    assert!(!rd.is_empty());

    // A fake SB2.1 image: "STMP" signature at offset 0x14.
    let mut image = [0u8; 100];
    image[0x14..0x18].copy_from_slice(b"STMP");

    // ReceiveSbFile command: tag 0x08, paramCount 1, byte count LE.
    let mut cmd = vec![0x08, 0x00, 0x00, 0x01];
    cmd.extend_from_slice(&(image.len() as u32).to_le_bytes());
    assert_eq!(c.bulk_out(1, &kboot_report(1, &cmd)).await, 0);

    // Initial ack.
    let (st, ack) = c.bulk_in(1, 64).await;
    assert_eq!(st, 0);
    assert_eq!(ack[0], 3, "response report id");
    assert_eq!(ack[4], 0xA0, "generic response tag");
    assert_eq!(u32::from_le_bytes([ack[8], ack[9], ack[10], ack[11]]), 0);

    // Stream the image, 28 payload bytes per data report.
    for chunk in image.chunks(28) {
        assert_eq!(c.bulk_out(1, &kboot_report(2, chunk)).await, 0);
    }

    // Final response: success.
    let (st, fin) = c.bulk_in(1, 64).await;
    assert_eq!(st, 0);
    assert_eq!(fin[4], 0xA0);
    assert_eq!(
        u32::from_le_bytes([fin[8], fin[9], fin[10], fin[11]]),
        0,
        "flash must succeed"
    );

    c.wait_eof().await; // flash settles (2 s), then the "cable is pulled"
    drop(c);

    // ---- Simulated unplug/replug, then the analyzer with new firmware ------
    // Right after the detach the device is in its 2 s cable-out window:
    // imports must be refused, like an empty USB port.
    assert!(
        Client::try_attach(addr, "1-1").await.is_none(),
        "attach must be refused during the simulated unplug window"
    );
    let (mut c, vid, pid) = Client::attach_retry(addr, "1-1").await;
    assert_eq!((vid, pid), (0x16C0, 0x4E37), "analyzer is back");
    assert_eq!(c.reg_read(0x10).await, 61, "post-flash firmware version");

    // The flashed image was saved to disk, byte-identical to what was sent.
    let saved: Vec<_> = std::fs::read_dir(&fw_dir)
        .expect("firmware save dir")
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(saved.len(), 1, "one flash, one saved image");
    assert_eq!(
        std::fs::read(&saved[0]).unwrap(),
        image,
        "saved image content"
    );
    let _ = std::fs::remove_dir_all(&fw_dir);
}

/// A USB device can only be plugged into one host: a second concurrent import
/// must be refused, and a detach must free the device again.
#[tokio::test(flavor = "multi_thread")]
async fn exclusive_attach() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sim = Simulator::new(SimOptions::default());
    tokio::spawn(vqa40x_usbip::serve(sim, listener));

    let (c1, _, _) = Client::attach(addr, "1-1").await;

    // Second import: refused (status 1 in the OP_REP_IMPORT header).
    let mut sock = TcpStream::connect(addr).await.unwrap();
    let mut req = Vec::new();
    req.extend_from_slice(&0x0111u16.to_be_bytes());
    req.extend_from_slice(&0x8003u16.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    let mut b = [0u8; 32];
    b[..3].copy_from_slice(b"1-1");
    req.extend_from_slice(&b);
    sock.write_all(&req).await.unwrap();
    let mut hdr = [0u8; 8];
    sock.read_exact(&mut hdr).await.unwrap();
    assert_eq!(
        u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]),
        1,
        "second import must be refused"
    );

    // Detach the first client; the device becomes importable again.
    drop(c1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_c2, vid, _) = Client::attach(addr, "1-1").await;
    assert_eq!(vid, 0x16C0);
}

#[tokio::test(flavor = "multi_thread")]
async fn devlist_reports_the_device() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sim = Simulator::new(SimOptions::default());
    tokio::spawn(vqa40x_usbip::serve(sim, listener));

    let mut sock = TcpStream::connect(addr).await.unwrap();
    let mut req = Vec::new();
    req.extend_from_slice(&0x0111u16.to_be_bytes());
    req.extend_from_slice(&0x8005u16.to_be_bytes()); // OP_REQ_DEVLIST
    req.extend_from_slice(&0u32.to_be_bytes());
    sock.write_all(&req).await.unwrap();

    let mut hdr = [0u8; 12];
    sock.read_exact(&mut hdr).await.unwrap();
    let count = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    assert_eq!(count, 1);
    let mut dev = [0u8; 312 + 4];
    sock.read_exact(&mut dev).await.unwrap();
    assert_eq!(u16::from_be_bytes([dev[300], dev[301]]), 0x16C0);
    let busid = String::from_utf8_lossy(&dev[256..288]);
    assert!(busid.starts_with("1-1"));
}
