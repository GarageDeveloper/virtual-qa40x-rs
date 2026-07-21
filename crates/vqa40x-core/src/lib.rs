//! Transport-agnostic QuantAsylum QA402/QA403 device model.
//!
//! The behaviour emulated here is derived from USB captures of the official
//! QuantAsylum app talking to a real QA402 (see the host project's
//! `docs/qa402-usb-capture-analysis.md` and `docs/captures/regbus-analysis.md`):
//!
//! * **Register bus** — bulk EP1. Host→device OUT frames are 5 bytes
//!   `[reg:1][value:4 big-endian]`; a read sets bit 7 of the register byte and
//!   the 4-byte reply comes back on bulk IN. Covers the comm-test/keepalive
//!   echo (0x00), input/output gain, sample rate, stream control, telemetry,
//!   firmware version, serial, capability word, calibration readout and the
//!   bootloader-entry magic.
//! * **Audio streaming** — bulk EP2, 16 KiB transfers of interleaved int32
//!   little-endian stereo (right sample first on the wire). The fake ADC loops
//!   the fake DAC back with configurable latency, gain, distortion, noise and
//!   an optional independent generator, using the same volts model as the
//!   factory calibration page it serves.
//! * **Firmware update** — writing `0xDEADBEEF` then `0xCAFEBABE` to register
//!   0x0F "reboots" the device into an emulated NXP MCUBOOT/KBOOT HID
//!   bootloader (VID 0x1FC9 PID 0x0022) that accepts `ReceiveSbFile` and then
//!   "reboots" back into the analyzer, optionally with a new firmware version.
//!
//! The model is exposed through the [`backend::UsbBackend`] trait so any
//! transport (USB/IP today, Linux gadget/FunctionFS later) can present it as a
//! real USB device.

pub mod analyzer;
pub mod backend;
pub mod bootloader;
pub mod calpage;
pub mod identity;
pub mod options;
pub mod registry;
pub mod rng;
pub mod sim;

pub use options::{GenChannel, GenSpec, Model, SimOptions};
pub use sim::Simulator;
