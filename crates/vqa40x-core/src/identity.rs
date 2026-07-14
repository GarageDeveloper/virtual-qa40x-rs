//! USB descriptors + standard-request handling shared by both personas.

use crate::backend::{DeviceSummary, SetupPacket, Stall, UsbSpeed};

/// Standard request codes.
const GET_STATUS: u8 = 0;
const CLEAR_FEATURE: u8 = 1;
const SET_FEATURE: u8 = 3;
const SET_ADDRESS: u8 = 5;
const GET_DESCRIPTOR: u8 = 6;
const GET_CONFIGURATION: u8 = 8;
const SET_CONFIGURATION: u8 = 9;
const GET_INTERFACE: u8 = 10;
const SET_INTERFACE: u8 = 11;

/// Descriptor types (wValue high byte).
const DT_DEVICE: u8 = 1;
const DT_CONFIGURATION: u8 = 2;
const DT_STRING: u8 = 3;
const DT_DEVICE_QUALIFIER: u8 = 6;
const DT_BOS: u8 = 0x0F;
const DT_HID_REPORT: u8 = 0x22;

/// Vendor request code Windows uses to fetch our MS OS 2.0 descriptor set
/// (announced in the BOS platform capability, any value works).
const MSOS20_VENDOR_CODE: u8 = 0x20;
/// wIndex of the MS OS 2.0 descriptor-set request.
const MSOS20_DESCRIPTOR_INDEX: u16 = 0x07;

/// Microsoft OS 2.0 support: lets Windows bind WinUSB to the vendor interface
/// automatically (no Zadig/INF), like real hardware that ships MS OS
/// descriptors. Windows reads the BOS descriptor (requires bcdUSB >= 0x0201),
/// finds the platform capability, then issues a vendor control request to
/// fetch the descriptor set with the `WINUSB` compatible ID.
pub struct MsOs20 {
    pub vendor_code: u8,
    pub bos: Vec<u8>,
    pub descriptor_set: Vec<u8>,
}

/// Static USB identity of a persona: descriptors and strings.
pub struct Identity {
    pub summary: DeviceSummary,
    pub device_descriptor: Vec<u8>,
    pub config_descriptor: Vec<u8>,
    /// Index 0 is the manufacturer; string descriptor indices are 1-based.
    pub strings: Vec<String>,
    /// HID report descriptor (bootloader persona only).
    pub report_descriptor: Option<Vec<u8>>,
    /// MS OS 2.0 descriptors (analyzer persona: auto-binds WinUSB on Windows).
    pub msos20: Option<MsOs20>,
}

impl Identity {
    /// Handle the standard (and HID-descriptor) control requests every persona
    /// answers the same way. Returns `None` if the request is not one of them
    /// (persona-specific handler takes over).
    pub fn handle_standard(&self, setup: SetupPacket) -> Option<Result<Vec<u8>, Stall>> {
        let recipient = setup.bm_request_type & 0x1f;
        let is_standard = setup.bm_request_type & 0x60 == 0;

        if !is_standard {
            return None;
        }

        let reply = |data: Vec<u8>| {
            let n = data.len().min(setup.w_length as usize);
            Some(Ok(data[..n].to_vec()))
        };

        match setup.b_request {
            GET_DESCRIPTOR if setup.is_in() => {
                let dtype = (setup.w_value >> 8) as u8;
                let index = (setup.w_value & 0xff) as u8;
                match dtype {
                    DT_DEVICE => reply(self.device_descriptor.clone()),
                    DT_CONFIGURATION => reply(self.config_descriptor.clone()),
                    DT_STRING => match self.string_descriptor(index) {
                        Some(d) => reply(d),
                        None => Some(Err(Stall)),
                    },
                    DT_BOS => match &self.msos20 {
                        Some(ms) => reply(ms.bos.clone()),
                        None => Some(Err(Stall)),
                    },
                    DT_DEVICE_QUALIFIER => {
                        if self.summary.speed == UsbSpeed::High {
                            reply(self.device_qualifier())
                        } else {
                            Some(Err(Stall))
                        }
                    }
                    DT_HID_REPORT => match &self.report_descriptor {
                        Some(d) => reply(d.clone()),
                        None => Some(Err(Stall)),
                    },
                    _ => Some(Err(Stall)),
                }
            }
            GET_STATUS if setup.is_in() => reply(vec![0, 0]),
            GET_CONFIGURATION if setup.is_in() => reply(vec![self.summary.configuration_value]),
            GET_INTERFACE if setup.is_in() => reply(vec![0]),
            SET_CONFIGURATION | SET_INTERFACE | SET_ADDRESS => Some(Ok(Vec::new())),
            // CLEAR_FEATURE(ENDPOINT_HALT) — the host app clears halts on all
            // four endpoints at connect; accept as a no-op.
            CLEAR_FEATURE | SET_FEATURE if recipient == 2 => Some(Ok(Vec::new())),
            CLEAR_FEATURE | SET_FEATURE => Some(Ok(Vec::new())),
            _ => None,
        }
    }

    fn string_descriptor(&self, index: u8) -> Option<Vec<u8>> {
        if index == 0 {
            // Supported language: US English.
            return Some(vec![4, DT_STRING, 0x09, 0x04]);
        }
        let s = self.strings.get(index as usize - 1)?;
        let utf16: Vec<u16> = s.encode_utf16().collect();
        let mut d = Vec::with_capacity(2 + utf16.len() * 2);
        d.push((2 + utf16.len() * 2) as u8);
        d.push(DT_STRING);
        for u in utf16 {
            d.extend_from_slice(&u.to_le_bytes());
        }
        Some(d)
    }

    /// Handle the MS OS 2.0 vendor request (descriptor-set fetch). Returns
    /// `None` for anything else.
    pub fn handle_vendor(&self, setup: SetupPacket) -> Option<Result<Vec<u8>, Stall>> {
        let ms = self.msos20.as_ref()?;
        // Vendor request, device-to-host, device recipient.
        if setup.bm_request_type == 0xC0
            && setup.b_request == ms.vendor_code
            && setup.w_index == MSOS20_DESCRIPTOR_INDEX
        {
            let n = ms.descriptor_set.len().min(setup.w_length as usize);
            return Some(Ok(ms.descriptor_set[..n].to_vec()));
        }
        None
    }

    fn device_qualifier(&self) -> Vec<u8> {
        let d = &self.device_descriptor;
        vec![
            10,
            DT_DEVICE_QUALIFIER,
            d[2],
            d[3], // bcdUSB
            d[4],
            d[5],
            d[6], // class/subclass/protocol
            d[7], // bMaxPacketSize0
            1,    // bNumConfigurations
            0,
        ]
    }
}

fn device_descriptor(
    bcd_usb: u16,
    class: (u8, u8, u8),
    vid: u16,
    pid: u16,
    bcd_device: u16,
    n_strings: u8,
) -> Vec<u8> {
    let i_mfr = if n_strings >= 1 { 1 } else { 0 };
    let i_prod = if n_strings >= 2 { 2 } else { 0 };
    let i_serial = if n_strings >= 3 { 3 } else { 0 };
    vec![
        18,
        DT_DEVICE,
        (bcd_usb & 0xff) as u8,
        (bcd_usb >> 8) as u8,
        class.0,
        class.1,
        class.2,
        64, // bMaxPacketSize0
        (vid & 0xff) as u8,
        (vid >> 8) as u8,
        (pid & 0xff) as u8,
        (pid >> 8) as u8,
        (bcd_device & 0xff) as u8,
        (bcd_device >> 8) as u8,
        i_mfr,
        i_prod,
        i_serial,
        1, // bNumConfigurations
    ]
}

fn endpoint_descriptor(addr: u8, attributes: u8, max_packet: u16, interval: u8) -> [u8; 7] {
    [
        7,
        5, // ENDPOINT
        addr,
        attributes,
        (max_packet & 0xff) as u8,
        (max_packet >> 8) as u8,
        interval,
    ]
}

/// Interface GUID advertised through the MS OS 2.0 `DeviceInterfaceGUIDs`
/// registry property (fixed, so the virtual device is stable across runs).
const DEVICE_INTERFACE_GUID: &str = "{D696BFEB-1734-417D-8A04-86D091193D30}";

fn utf16le(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2 + 2);
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&[0, 0]); // NUL terminator
    out
}

/// Build the BOS descriptor + MS OS 2.0 descriptor set that make Windows bind
/// WinUSB automatically (compatible ID `WINUSB` + a DeviceInterfaceGUIDs
/// registry property, the layout libusb/WinUSB documentation recommends).
fn msos20(vendor_code: u8) -> MsOs20 {
    // --- Descriptor set ---------------------------------------------------
    // Feature: compatible ID "WINUSB" (applies to the whole non-composite
    // device, i.e. our single vendor interface).
    let mut compat = Vec::with_capacity(20);
    compat.extend_from_slice(&20u16.to_le_bytes()); // wLength
    compat.extend_from_slice(&3u16.to_le_bytes()); // MS_OS_20_FEATURE_COMPATBLE_ID
    compat.extend_from_slice(b"WINUSB\0\0"); // CompatibleID
    compat.extend_from_slice(&[0u8; 8]); // SubCompatibleID

    // Feature: registry property DeviceInterfaceGUIDs (REG_MULTI_SZ).
    let name = utf16le("DeviceInterfaceGUIDs");
    let mut data = utf16le(DEVICE_INTERFACE_GUID);
    data.extend_from_slice(&[0, 0]); // MULTI_SZ: extra NUL ends the list
    let mut regprop = Vec::new();
    let regprop_len = 2 + 2 + 2 + 2 + name.len() + 2 + data.len();
    regprop.extend_from_slice(&(regprop_len as u16).to_le_bytes()); // wLength
    regprop.extend_from_slice(&4u16.to_le_bytes()); // MS_OS_20_FEATURE_REG_PROPERTY
    regprop.extend_from_slice(&7u16.to_le_bytes()); // REG_MULTI_SZ
    regprop.extend_from_slice(&(name.len() as u16).to_le_bytes());
    regprop.extend_from_slice(&name);
    regprop.extend_from_slice(&(data.len() as u16).to_le_bytes());
    regprop.extend_from_slice(&data);

    let total = 10 + compat.len() + regprop.len();
    let mut set = Vec::with_capacity(total);
    set.extend_from_slice(&10u16.to_le_bytes()); // wLength (set header)
    set.extend_from_slice(&0u16.to_le_bytes()); // MS_OS_20_SET_HEADER_DESCRIPTOR
    set.extend_from_slice(&0x0603_0000u32.to_le_bytes()); // Windows 8.1+
    set.extend_from_slice(&(total as u16).to_le_bytes()); // wTotalLength
    set.extend_from_slice(&compat);
    set.extend_from_slice(&regprop);

    // --- BOS with the MS OS 2.0 platform capability -------------------------
    let mut bos = Vec::with_capacity(33);
    bos.extend_from_slice(&[5, DT_BOS]);
    bos.extend_from_slice(&33u16.to_le_bytes()); // wTotalLength
    bos.push(1); // bNumDeviceCaps
    bos.extend_from_slice(&[
        28,   // bLength
        0x10, // DEVICE CAPABILITY
        0x05, // PLATFORM
        0,    // bReserved
        // MS OS 2.0 platform capability UUID
        // {D8DD60DF-4589-4CC7-9CD2-659D9E648A9F}, first three fields LE.
        0xDF, 0x60, 0xDD, 0xD8, 0x89, 0x45, 0xC7, 0x4C, 0x9C, 0xD2, 0x65, 0x9D, 0x9E, 0x64, 0x8A,
        0x9F,
    ]);
    bos.extend_from_slice(&0x0603_0000u32.to_le_bytes()); // dwWindowsVersion
    bos.extend_from_slice(&(total as u16).to_le_bytes()); // wMSOSDescriptorSetTotalLength
    bos.push(vendor_code);
    bos.push(0); // bAltEnumCode

    MsOs20 {
        vendor_code,
        bos,
        descriptor_set: set,
    }
}

/// Analyzer persona: vendor-specific interface with the four bulk endpoints
/// the QA40x exposes (0x01/0x81 register bus, 0x02/0x82 audio stream).
pub fn analyzer_identity(vid: u16, pid: u16, product: &str, serial: &str) -> Identity {
    let class = (0u8, 0u8, 0u8); // class defined at interface level
    let iface_class = (0xffu8, 0x00u8, 0x00u8);

    let mut config = Vec::new();
    // Configuration descriptor header, wTotalLength patched below.
    config.extend_from_slice(&[9, DT_CONFIGURATION, 0, 0, 1, 1, 0, 0x80, 250]);
    config.extend_from_slice(&[
        9,
        4, // INTERFACE
        0,
        0, // number, alternate
        4, // bNumEndpoints
        iface_class.0,
        iface_class.1,
        iface_class.2,
        0,
    ]);
    for ep in [0x01u8, 0x81, 0x02, 0x82] {
        config.extend_from_slice(&endpoint_descriptor(ep, 0x02 /* bulk */, 512, 0));
    }
    let total = config.len() as u16;
    config[2] = (total & 0xff) as u8;
    config[3] = (total >> 8) as u8;

    Identity {
        summary: DeviceSummary {
            vid,
            pid,
            bcd_device: 0x0100,
            device_class: class.0,
            device_subclass: class.1,
            device_protocol: class.2,
            configuration_value: 1,
            num_configurations: 1,
            speed: UsbSpeed::High,
            interfaces: vec![iface_class],
        },
        // bcdUSB 0x0201: BOS-capable, so Windows fetches the MS OS 2.0
        // descriptors and binds WinUSB without an INF/Zadig.
        device_descriptor: device_descriptor(0x0201, class, vid, pid, 0x0100, 3),
        config_descriptor: config,
        strings: vec![
            "QuantAsylum".to_string(),
            product.to_string(),
            serial.to_string(),
        ],
        report_descriptor: None,
        msos20: Some(msos20(MSOS20_VENDOR_CODE)),
    }
}

/// KBOOT HID report descriptor: vendor-defined usage page with the four
/// numbered reports the NXP bootloader uses (1 = command OUT, 2 = data OUT,
/// 3 = response IN, 4 = data IN), 59 payload bytes each (60-byte reports
/// including the report id, as confirmed by the flash captures).
fn kboot_report_descriptor() -> Vec<u8> {
    let mut d = vec![
        0x06, 0x00, 0xff, // Usage Page (Vendor Defined 0xFF00)
        0x09, 0x01, // Usage (1)
        0xa1, 0x01, // Collection (Application)
    ];
    let mut report = |id: u8, input: bool| {
        d.extend_from_slice(&[
            0x85,
            id, // Report ID
            0x09,
            id, // Usage
            0x15,
            0x00, // Logical Minimum (0)
            0x26,
            0xff,
            0x00, // Logical Maximum (255)
            0x75,
            0x08, // Report Size (8)
            0x95,
            59, // Report Count (59)
            if input { 0x81 } else { 0x91 },
            0x02, // Input/Output (Data,Var,Abs)
        ]);
    };
    report(1, false);
    report(2, false);
    report(3, true);
    report(4, true);
    d.push(0xc0); // End Collection
    d
}

/// Bootloader persona: the NXP MCUBOOT/KBOOT HID device (0x1FC9:0x0022) the
/// QA40x re-enumerates as after the register-0x0F magic.
pub fn bootloader_identity() -> Identity {
    let report_desc = kboot_report_descriptor();

    let mut config = Vec::new();
    config.extend_from_slice(&[9, DT_CONFIGURATION, 0, 0, 1, 1, 0, 0x80, 50]);
    config.extend_from_slice(&[
        9, 4, // INTERFACE
        0, 0, // number, alternate
        2, // bNumEndpoints
        3, 0, 0, // HID class, no subclass/protocol
        0,
    ]);
    // HID descriptor.
    config.extend_from_slice(&[
        9,
        0x21, // HID
        0x11,
        0x01, // bcdHID 1.11
        0,    // country
        1,    // one class descriptor
        DT_HID_REPORT,
        (report_desc.len() & 0xff) as u8,
        (report_desc.len() >> 8) as u8,
    ]);
    config.extend_from_slice(&endpoint_descriptor(0x81, 0x03 /* interrupt */, 64, 1));
    config.extend_from_slice(&endpoint_descriptor(0x01, 0x03, 64, 1));
    let total = config.len() as u16;
    config[2] = (total & 0xff) as u8;
    config[3] = (total >> 8) as u8;

    Identity {
        summary: DeviceSummary {
            vid: 0x1FC9,
            pid: 0x0022,
            bcd_device: 0x0100,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            configuration_value: 1,
            num_configurations: 1,
            speed: UsbSpeed::Full,
            interfaces: vec![(3, 0, 0)],
        },
        device_descriptor: device_descriptor(0x0200, (0, 0, 0), 0x1FC9, 0x0022, 0x0100, 2),
        config_descriptor: config,
        strings: vec![
            "NXP Semiconductor Inc.".to_string(),
            "USB COMPOSITE DEVICE".to_string(),
        ],
        report_descriptor: Some(report_desc),
        // HID binds by class on every OS — no MS OS descriptors needed.
        msos20: None,
    }
}
