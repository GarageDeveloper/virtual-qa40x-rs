//! FunctionFS descriptor + strings blobs (V2 format).
//!
//! FunctionFS expects the userspace daemon to write, to `ep0`, a descriptor
//! blob and a strings blob describing the function's interface(s) and
//! endpoints — everything the config descriptor holds *except* the 9-byte
//! configuration header (device/config/string descriptors and enumeration are
//! handled by the kernel composite core from the configfs gadget attributes).
//!
//! This module builds those two blobs. It is plain byte assembly with no OS
//! dependency, so it is unit-tested on any platform.

// `interrupt`/`XFER_INTERRUPT` are for the bootloader (HID) persona, wired in a
// later milestone; keep them without tripping `-D warnings`.
#![allow(dead_code)]

/// `FUNCTIONFS_DESCRIPTORS_MAGIC_V2`
const DESCRIPTORS_MAGIC_V2: u32 = 2;
/// `FUNCTIONFS_STRINGS_MAGIC`
const STRINGS_MAGIC: u32 = 2;
/// `functionfs_flags`: full-speed and high-speed descriptors are present.
const HAS_FS_DESC: u32 = 1;
const HAS_HS_DESC: u32 = 2;

/// USB transfer-type bits for `bmAttributes`.
pub const XFER_BULK: u8 = 0x02;
pub const XFER_INTERRUPT: u8 = 0x03;

/// One endpoint of the function. Full-speed and high-speed advertise
/// different max packet sizes (bulk is 64 at FS but 512 at HS); interrupt
/// endpoints keep 64 at both.
#[derive(Clone, Copy)]
pub struct Endpoint {
    pub address: u8,
    pub attributes: u8,
    pub fs_max_packet: u16,
    pub hs_max_packet: u16,
    pub interval: u8,
}

impl Endpoint {
    pub fn bulk(address: u8) -> Self {
        Self {
            address,
            attributes: XFER_BULK,
            fs_max_packet: 64,
            hs_max_packet: 512,
            interval: 0,
        }
    }

    pub fn interrupt(address: u8, interval: u8) -> Self {
        Self {
            address,
            attributes: XFER_INTERRUPT,
            fs_max_packet: 64,
            hs_max_packet: 64,
            interval,
        }
    }
}

/// The analyzer persona's four bulk endpoints: register bus (0x01/0x81) and
/// audio stream (0x02/0x82).
pub fn analyzer_endpoints() -> Vec<Endpoint> {
    vec![
        Endpoint::bulk(0x01),
        Endpoint::bulk(0x81),
        Endpoint::bulk(0x02),
        Endpoint::bulk(0x82),
    ]
}

fn interface_descriptor(class: (u8, u8, u8), num_endpoints: u8) -> [u8; 9] {
    [
        9,
        4, // bLength, INTERFACE
        0,
        0, // bInterfaceNumber, bAlternateSetting
        num_endpoints,
        class.0,
        class.1,
        class.2,
        0, // iInterface (device strings come from configfs)
    ]
}

fn endpoint_descriptor(addr: u8, attributes: u8, max_packet: u16, interval: u8) -> [u8; 7] {
    [
        7,
        5, // bLength, ENDPOINT
        addr,
        attributes,
        (max_packet & 0xff) as u8,
        (max_packet >> 8) as u8,
        interval,
    ]
}

/// One alternate-speed descriptor set: interface + optional class descriptor
/// (e.g. HID) + endpoints at the given speed's max packet size.
fn speed_descriptors(
    class: (u8, u8, u8),
    class_descriptor: &[u8],
    endpoints: &[Endpoint],
    high_speed: bool,
) -> (u32, Vec<u8>) {
    let mut out = Vec::new();
    out.extend_from_slice(&interface_descriptor(class, endpoints.len() as u8));
    out.extend_from_slice(class_descriptor);
    for ep in endpoints {
        let mp = if high_speed {
            ep.hs_max_packet
        } else {
            ep.fs_max_packet
        };
        out.extend_from_slice(&endpoint_descriptor(
            ep.address,
            ep.attributes,
            mp,
            ep.interval,
        ));
    }
    // Descriptor count = interface + class descriptor (0 or 1) + endpoints.
    let count = 1 + (!class_descriptor.is_empty()) as u32 + endpoints.len() as u32;
    (count, out)
}

/// Build the FunctionFS V2 descriptor blob for a single interface with the
/// given endpoints (and optional interface-class descriptor such as a HID
/// descriptor placed between the interface and its endpoints).
pub fn build_descriptors(
    class: (u8, u8, u8),
    class_descriptor: &[u8],
    endpoints: &[Endpoint],
) -> Vec<u8> {
    let (fs_count, fs) = speed_descriptors(class, class_descriptor, endpoints, false);
    let (hs_count, hs) = speed_descriptors(class, class_descriptor, endpoints, true);

    let mut body = Vec::new();
    body.extend_from_slice(&fs_count.to_le_bytes());
    body.extend_from_slice(&hs_count.to_le_bytes());
    body.extend_from_slice(&fs);
    body.extend_from_slice(&hs);

    let total = 12 + body.len();
    let mut blob = Vec::with_capacity(total);
    blob.extend_from_slice(&DESCRIPTORS_MAGIC_V2.to_le_bytes());
    blob.extend_from_slice(&(total as u32).to_le_bytes());
    blob.extend_from_slice(&(HAS_FS_DESC | HAS_HS_DESC).to_le_bytes());
    blob.extend_from_slice(&body);
    blob
}

/// Build the (empty) FunctionFS strings blob: the analyzer's interface has no
/// string, and the device-level manufacturer/product/serial strings come from
/// the configfs gadget, so no per-function strings are needed.
pub fn build_strings() -> Vec<u8> {
    let mut blob = Vec::with_capacity(16);
    blob.extend_from_slice(&STRINGS_MAGIC.to_le_bytes());
    blob.extend_from_slice(&16u32.to_le_bytes()); // length
    blob.extend_from_slice(&0u32.to_le_bytes()); // str_count
    blob.extend_from_slice(&0u32.to_le_bytes()); // lang_count
    blob
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_blob_is_well_formed() {
        let eps = analyzer_endpoints();
        let blob = build_descriptors((0xff, 0, 0), &[], &eps);

        // Header: magic, length == blob length, flags = FS|HS.
        assert_eq!(u32::from_le_bytes(blob[0..4].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(blob[4..8].try_into().unwrap()) as usize,
            blob.len()
        );
        assert_eq!(u32::from_le_bytes(blob[8..12].try_into().unwrap()), 3);

        // Counts: 1 interface + 4 endpoints per speed.
        assert_eq!(u32::from_le_bytes(blob[12..16].try_into().unwrap()), 5);
        assert_eq!(u32::from_le_bytes(blob[16..20].try_into().unwrap()), 5);

        // Two speeds × (9-byte interface + 4 × 7-byte endpoint) = 2 × 37.
        assert_eq!(blob.len(), 20 + 2 * (9 + 4 * 7));

        // First interface descriptor, vendor class, 4 endpoints.
        assert_eq!(&blob[20..24], &[9, 4, 0, 0]);
        assert_eq!(blob[24], 4);
        assert_eq!(blob[25], 0xff);

        // FS bulk endpoints carry max packet 64; HS carries 512.
        let fs_ep0 = &blob[29..36];
        assert_eq!(fs_ep0[2], 0x01);
        assert_eq!(u16::from_le_bytes([fs_ep0[4], fs_ep0[5]]), 64);
        let hs_iface = 20 + (9 + 4 * 7);
        let hs_ep0 = &blob[hs_iface + 9..hs_iface + 16];
        assert_eq!(u16::from_le_bytes([hs_ep0[4], hs_ep0[5]]), 512);
    }

    #[test]
    fn strings_blob_is_16_bytes() {
        assert_eq!(build_strings().len(), 16);
    }
}
