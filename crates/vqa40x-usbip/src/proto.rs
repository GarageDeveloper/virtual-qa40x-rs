//! USB/IP wire protocol (device side), protocol version 0x0111.
//!
//! Reference: Linux `Documentation/usb/usbip_protocol.rst` and
//! `drivers/usb/usbip/`. All header fields are big-endian.

pub const USBIP_VERSION: u16 = 0x0111;

pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;

pub const USBIP_CMD_SUBMIT: u32 = 0x0000_0001;
pub const USBIP_CMD_UNLINK: u32 = 0x0000_0002;
pub const USBIP_RET_SUBMIT: u32 = 0x0000_0003;
pub const USBIP_RET_UNLINK: u32 = 0x0000_0004;

pub const DIR_OUT: u32 = 0;
pub const DIR_IN: u32 = 1;

/// URB completion status codes (Linux errno, negated).
pub const ST_OK: i32 = 0;
pub const ST_EPIPE: i32 = -32; // endpoint stall
pub const ST_ECONNRESET: i32 = -104; // unlinked

pub const BUS_NUM: u32 = 1;
pub const DEV_NUM: u32 = 2;

/// The fixed 48-byte URB header shared by CMD/RET SUBMIT/UNLINK.
#[derive(Debug, Clone, Copy, Default)]
pub struct UrbHeader {
    pub command: u32,
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    /// The 28 command-specific bytes after the 20-byte basic header.
    pub rest: [u8; 28],
}

impl UrbHeader {
    pub fn parse(buf: &[u8; 48]) -> Self {
        let u32_at = |o: usize| u32::from_be_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
        let mut rest = [0u8; 28];
        rest.copy_from_slice(&buf[20..48]);
        Self {
            command: u32_at(0),
            seqnum: u32_at(4),
            devid: u32_at(8),
            direction: u32_at(12),
            ep: u32_at(16),
            rest,
        }
    }

    pub fn u32_rest(&self, index: usize) -> u32 {
        let o = index * 4;
        u32::from_be_bytes([
            self.rest[o],
            self.rest[o + 1],
            self.rest[o + 2],
            self.rest[o + 3],
        ])
    }

    /// CMD_SUBMIT: transfer_buffer_length.
    pub fn transfer_length(&self) -> usize {
        self.u32_rest(1) as usize
    }

    /// CMD_SUBMIT: the 8 setup bytes.
    pub fn setup(&self) -> [u8; 8] {
        let mut s = [0u8; 8];
        s.copy_from_slice(&self.rest[20..28]);
        s
    }

    /// CMD_UNLINK: seqnum of the URB to unlink.
    pub fn unlink_seqnum(&self) -> u32 {
        self.u32_rest(0)
    }
}

/// Encode a RET_SUBMIT (header + optional IN payload).
pub fn ret_submit(seqnum: u32, status: i32, data: Option<&[u8]>, out_actual: usize) -> Vec<u8> {
    let actual = data.map_or(out_actual, |d| d.len());
    let mut buf = Vec::with_capacity(48 + data.map_or(0, |d| d.len()));
    buf.extend_from_slice(&USBIP_RET_SUBMIT.to_be_bytes());
    buf.extend_from_slice(&seqnum.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes()); // devid
    buf.extend_from_slice(&0u32.to_be_bytes()); // direction
    buf.extend_from_slice(&0u32.to_be_bytes()); // ep
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&(actual as u32).to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes()); // start_frame
    buf.extend_from_slice(&0u32.to_be_bytes()); // number_of_packets (non-iso)
    buf.extend_from_slice(&0u32.to_be_bytes()); // error_count
    buf.extend_from_slice(&[0u8; 8]); // padding
    if let Some(d) = data {
        buf.extend_from_slice(d);
    }
    buf
}

/// Encode a RET_UNLINK.
pub fn ret_unlink(seqnum: u32, status: i32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(48);
    buf.extend_from_slice(&USBIP_RET_UNLINK.to_be_bytes());
    buf.extend_from_slice(&seqnum.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&status.to_be_bytes());
    buf.extend_from_slice(&[0u8; 24]);
    buf
}

/// The 312-byte `usbip_usb_device` block used by OP_REP_DEVLIST / OP_REP_IMPORT.
pub fn usb_device_block(
    busid: &str,
    summary: &vqa40x_core::backend::DeviceSummary,
    n_interfaces_field: u8,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(312);
    let mut path = [0u8; 256];
    let p = format!("/sys/devices/virtual/usbip/{busid}");
    path[..p.len().min(255)].copy_from_slice(&p.as_bytes()[..p.len().min(255)]);
    buf.extend_from_slice(&path);
    let mut bus = [0u8; 32];
    bus[..busid.len().min(31)].copy_from_slice(&busid.as_bytes()[..busid.len().min(31)]);
    buf.extend_from_slice(&bus);
    buf.extend_from_slice(&BUS_NUM.to_be_bytes());
    buf.extend_from_slice(&DEV_NUM.to_be_bytes());
    buf.extend_from_slice(&(summary.speed as u32).to_be_bytes());
    buf.extend_from_slice(&summary.vid.to_be_bytes());
    buf.extend_from_slice(&summary.pid.to_be_bytes());
    buf.extend_from_slice(&summary.bcd_device.to_be_bytes());
    buf.push(summary.device_class);
    buf.push(summary.device_subclass);
    buf.push(summary.device_protocol);
    buf.push(summary.configuration_value);
    buf.push(summary.num_configurations);
    buf.push(n_interfaces_field);
    buf
}

pub fn op_header(code: u16, status: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8);
    buf.extend_from_slice(&USBIP_VERSION.to_be_bytes());
    buf.extend_from_slice(&code.to_be_bytes());
    buf.extend_from_slice(&status.to_be_bytes());
    buf
}

/// devid as sent in OP_REP_IMPORT-importing clients' CMD headers.
pub fn devid() -> u32 {
    (BUS_NUM << 16) | DEV_NUM
}
