# Running as a real USB device (Linux gadget)

`vqa40x-gadget` presents the virtual analyzer as a **real USB device** through
the Linux USB gadget stack (FunctionFS). Run it on a small Linux board with a
USB **device** controller (UDC) and plug that board into the machine that
should see the QA40x — including a **Mac**, which has no USB/IP client but
happily enumerates a physical USB device.

```
  ┌──────────────┐   USB cable    ┌───────────────────────────┐
  │  Mac / PC    │◀──────────────▶│  Raspberry Pi (this binary)│
  │ nusb, libusb │  real USB dev  │  vqa40x-gadget → FunctionFS│
  │ official app │                │  = a real QA40x on the bus │
  └──────────────┘                └───────────────────────────┘
```

## Hardware

Any Linux board whose USB port can act as a device/OTG works:

| Board | Port to use | Notes |
|---|---|---|
| Pi Zero / Zero 2 W | the **USB** (not PWR) micro-USB | OTG-capable, ideal |
| Pi 4 | the **USB-C** power port | supports peripheral mode |
| Pi 5 | USB-C | peripheral mode |
| BeagleBone, many others | OTG port | any `dwc2`/UDC-capable port |

Enable the device-mode controller (Raspberry Pi OS):

```sh
# /boot/firmware/config.txt
dtoverlay=dwc2
# /boot/firmware/cmdline.txt  — add after rootwait:
modules-load=dwc2
```

Reboot, then check a UDC exists:

```sh
ls /sys/class/udc      # e.g. 3f980000.usb  (non-empty = ready)
```

## Run

Build for the Pi (or grab the `aarch64-unknown-linux-musl` release binary),
then:

```sh
sudo tools/gadget-setup.sh           # QA402 identity; pass VID PID SERIAL to change
sudo ./vqa40x-gadget                 # writes descriptors, binds the UDC

# tidy up when done
sudo tools/gadget-teardown.sh
```

Plug the board into the Mac/PC: a **QA402 Audio Analyzer** (`16c0:4e37`)
appears on the USB bus. On macOS `nusb`/`libusb` apps (e.g. a Tauri GUI) and
on Windows the official app (WinUSB) can talk to it — no USB/IP, no drivers.

Options mirror the server where relevant: `--model qa402|qa403`,
`--serial XXXX_XXXX` (default random per launch), `--fw-version`, and
`--udc <name>` if the board has more than one controller.

## What works today

The **analyzer persona** is fully driven over FunctionFS: register bus,
telemetry, calibration page, and calibrated ADC/DAC loopback streaming — the
same core as the USB/IP server, so behaviour is identical.

## Not yet on the gadget path

* **Firmware-update emulation.** Switching to the NXP bootloader persona means
  re-declaring the device with a different interface (HID) and re-binding the
  UDC — a configfs reconfigure between personas. The descriptor building for
  it is in place (`descriptors::interrupt`), but the switch orchestration is
  not wired yet. Use the USB/IP server for the flash demo for now.
* **MS OS 2.0 descriptors.** The gadget does not emit them, so Windows
  auto-WinUSB-bind is not automatic on this path (macOS/libusb don't need it).
  They can be added via configfs `os_desc` later.
