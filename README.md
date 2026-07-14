# virtual-qa40x

A virtual **QA402/QA403 audio analyzer**: a Rust USB/IP device server that
behaves like the real hardware — register bus, ADC/DAC streaming with a
configurable loopback, factory calibration page, telemetry, and a fake
firmware upgrade through an emulated NXP KBOOT bootloader.

Use it to develop and test QA40x host software without an analyzer on the
bench: attach the virtual device from Linux (`vhci-hcd`) or Windows
(usbip-win2) and point your app at it — including the official QA40x
application running in a VM.

> **Disclaimer — unofficial project.**
> This project is **not affiliated with, endorsed by, or supported by
> QuantAsylum** in any way. QA402, QA403 and QuantAsylum are trademarks of
> their respective owners, used here only to describe interoperability.
> The emulated behaviour was derived from observing USB traffic between the
> official application and the author's own analyzer, for interoperability
> purposes. This repository contains **no QuantAsylum firmware, software or
> other proprietary material**; the embedded calibration page is factory
> measurement data read from the author's own unit. Use at your own risk.

## Quick start

```sh
cargo build --release
./target/release/vqa40x                  # QA402, 16c0:4e37, USB/IP on port 3240
```

Attach from the client machine (the virtual device appears as real USB
hardware there):

* **Linux**
  ```sh
  sudo modprobe vhci-hcd
  sudo usbip attach -r <server-host> -b 1-1
  ```
* **Windows** — install [usbip-win2](https://github.com/vadimgrn/usbip-win2), then
  ```powershell
  usbip.exe attach -r <server-host> -b 1-1
  ```
  WinUSB binds automatically (the device exposes MS OS 2.0 descriptors), so
  the official app and libusb/nusb-based apps work without Zadig or an INF.
* **macOS** has no USB/IP client: run the server on the Mac and attach from a
  Linux or Windows VM.

`vqa40x --help` lists every option. The most useful:

| Option | Effect |
|---|---|
| `--model qa402\|qa403` | Emulated model (PID, sample rates) |
| `--vid` / `--pid` | USB ids (defaults match real units) |
| `--serial XXXX_XXXX` | USB serial; default: a random one per launch |
| `--fw-version <n>` | Firmware version reported by register 0x10 (real units: 60) |
| `--listen <addr:port>` | USB/IP listen address (default `0.0.0.0:3240`) |
| `--gen 1000:-10[:left]` | Independent sine at the input (Hz : dBV [: channel]) |
| `--noise-dbfs`, `--h2-dbc`, `--h3-dbc` | Noise floor and harmonic distortion |
| `--no-loopback`, `--loopback-gain-db` | DAC→ADC loopback (on by default, 0 dB) |
| `--latency-samples <n>` | Loopback round-trip latency |
| `--no-realtime` | Serve samples as fast as asked (CI/tests) instead of pacing at the sample rate |

Logs: `RUST_LOG=vqa40x=debug,info` shows every register operation.

## Firmware-update emulation

1. The host writes `0xDEADBEEF` then `0xCAFEBABE` to register 0x0F → the
   analyzer detaches and the same bus id re-exports an **NXP KBOOT HID
   bootloader** (`1fc9:0022`). Re-attach to talk to it.
2. The bootloader accepts `ReceiveSbFile` and validates the SB2.1 signature
   bytes. `--flash-secs 6` paces the data phase like a real erase/program
   cycle; `--save-firmware <dir>` writes every received image to disk.
3. On success the device settles 2 s, simulates an unplug (2 s with no device
   on the bus), then comes back as the analyzer. The reported version is
   resolved via `--registry <firmware-registry.json>` (sha256 lookup) or
   `--post-flash-version <n>`, else unchanged.

USB/IP clients do not re-attach by themselves. Run a retry loop —
`tools/auto-attach.ps1` (Windows) — before triggering the update:

```powershell
powershell -ExecutionPolicy Bypass -File tools\auto-attach.ps1 -Server <server-host>
```

Demo: make the official app (which bundles firmware v60) offer an upgrade:

```sh
vqa40x --fw-version 58 --post-flash-version 60 --flash-secs 6 --save-firmware ./flashes
```

## Calibration page

The device serves a real QA402 factory calibration page by default. The page
format is fully reverse-engineered (header, per-range trim records, 0xDEAD
markers, and a CRC-16/BUYPASS over the first 510 bytes — pages failing the
CRC are rejected by the official app as invalid calibration data; see
`vqa40x-core/src/calpage.rs` for the byte-level layout).

Alternatives to the embedded page:

```sh
vqa40x --cal-page random          # generate a valid page with realistic random trims
python3 tools/extract_calpage.py connect-capture.pcapng -o mypage.bin
vqa40x --cal-page mypage.bin      # serve another unit's page (CRC checked, warns if bad)
```

The audio engine reads its ADC/DAC trims from whatever page is served, so a
calibrated host always measures a loopback of exactly `--loopback-gain-db`.

## Architecture

| Crate | Role |
|---|---|
| `vqa40x-core` | Transport-agnostic device model: register bus, audio engine, calibration, KBOOT bootloader, persona switching |
| `vqa40x-usbip` | USB/IP device-side server (devlist/import, URB submit/unlink, per-endpoint ordering) |
| `vqa40x-cli` | The `vqa40x` binary |

Personas implement a small `UsbBackend` trait, so other transports (e.g. a
Linux FunctionFS/raw-gadget backend on a Pi Zero plugged into a real USB
port) can be added without touching the device model.

Tests: `cargo test`. The end-to-end test drives the full lifecycle through an
in-process USB/IP client: enumeration, registers, calibration readout,
loopback streaming (gain checked against the calibration model), bootloader
entry, fake flash, simulated replug, re-enumeration with the new version.

## Known limitations

* No isochronous transfers (the QA40x does not use them).
* KBOOT implements `ReceiveSbFile` and `Reset`; other commands answer
  `k_UnknownCommand`.
* One host at a time (like a physical device); a dead client is detected via
  TCP keepalive and the device becomes attachable again within ~30 s.

## License

[MIT](LICENSE). Unofficial project — see the disclaimer above.
