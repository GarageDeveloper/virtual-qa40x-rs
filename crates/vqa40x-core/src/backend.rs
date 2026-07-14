//! The transport-facing device abstraction.
//!
//! A transport (USB/IP server, Linux gadget, …) drives the current persona
//! through this trait: control transfers on EP0, bulk/interrupt OUT and IN on
//! numbered endpoints. Endpoint addresses carry the direction bit (0x81 = EP1
//! IN), matching USB conventions.

use async_trait::async_trait;

/// A control-transfer SETUP packet.
#[derive(Debug, Clone, Copy)]
pub struct SetupPacket {
    pub bm_request_type: u8,
    pub b_request: u8,
    pub w_value: u16,
    pub w_index: u16,
    pub w_length: u16,
}

impl SetupPacket {
    pub fn parse(bytes: &[u8; 8]) -> Self {
        Self {
            bm_request_type: bytes[0],
            b_request: bytes[1],
            w_value: u16::from_le_bytes([bytes[2], bytes[3]]),
            w_index: u16::from_le_bytes([bytes[4], bytes[5]]),
            w_length: u16::from_le_bytes([bytes[6], bytes[7]]),
        }
    }

    pub fn is_in(&self) -> bool {
        self.bm_request_type & 0x80 != 0
    }
}

/// The endpoint refuses the transfer (host sees a STALL / -EPIPE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall;

/// USB speeds as encoded by USB/IP (`usb_device_speed` in the Linux kernel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbSpeed {
    Full = 2,
    High = 3,
}

/// Static identity of the current persona, for the transport's enumeration
/// (USB/IP devlist/import needs it outside of descriptor parsing).
#[derive(Debug, Clone)]
pub struct DeviceSummary {
    pub vid: u16,
    pub pid: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub num_configurations: u8,
    pub speed: UsbSpeed,
    /// (class, subclass, protocol) per interface.
    pub interfaces: Vec<(u8, u8, u8)>,
}

/// One USB device persona (analyzer or bootloader).
#[async_trait]
pub trait UsbBackend: Send + Sync {
    fn summary(&self) -> DeviceSummary;

    /// Handle a control transfer. For IN requests the returned bytes are the
    /// data stage (already sized to the device's intent; the transport
    /// truncates to `w_length`). For OUT requests `data_out` is the data stage
    /// and the return value is empty.
    async fn control(&self, setup: SetupPacket, data_out: &[u8]) -> Result<Vec<u8>, Stall>;

    /// Host→device transfer on OUT endpoint `ep` (address without direction
    /// bit is `ep & 0x7f`; this is called with e.g. 0x01, 0x02). Returns the
    /// number of bytes accepted.
    async fn out_transfer(&self, ep: u8, data: &[u8]) -> Result<usize, Stall>;

    /// Device→host transfer on IN endpoint `ep` (e.g. 0x81, 0x82). May block
    /// until data is available; the transport cancels it on URB unlink or
    /// disconnect. `len` is the host's buffer size.
    async fn in_transfer(&self, ep: u8, len: usize) -> Result<Vec<u8>, Stall>;
}
