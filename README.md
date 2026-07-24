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

Prebuilt binaries for macOS (arm64), Linux (x86_64/arm64, static musl) and
Windows (x86_64/arm64) are on the
[releases page](https://github.com/GarageDeveloper/virtual-qa40x-rs/releases).

On macOS the downloaded binary is not notarized, so Gatekeeper blocks it
("cannot be opened"). Clear the quarantine flag once:

```sh
xattr -d com.apple.quarantine vqa40x
```

Or build from source:

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
| `--capability <word>` | Capability word served by register 0x1B (hex ok); default: the value captured on a real QA402 |
| `--capability2 <word>` | Second capability word, register 0x1C; default per model (QA402 0x02A35B03, QA403 0x7F31BD30) |
| `--listen <addr:port>` | USB/IP listen address (default `0.0.0.0:3240`) |
| `--gen 1000:-10[:left]` | Independent sine at the input (Hz : dBV [: channel]) |
| `--noise-dbfs`, `--h2-dbc`, `--h3-dbc` | Noise floor and harmonic distortion |
| `--noise-seed <n>` | Fixed noise seed: reproducible noise on every acquisition (default: fresh entropy each stream) |
| `--no-loopback`, `--loopback-gain-db` | DAC→ADC loopback (on by default, 0 dB) |
| `--latency-samples <n>` | Loopback round-trip latency |
| `--no-realtime` | Serve samples as fast as asked (CI/tests) instead of pacing at the sample rate |
| `--record <path>` | Record every USB exchange to a JSONL log (see below) |
| `--record-data full\|summary` | Keep stream payloads in the log, or only their lengths |

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

### Recovery mode (stuck in bootloader)

`--boot-bootloader` starts the device directly as the NXP KBOOT bootloader
(`1fc9:0022`), as a real unit does when a previous flash failed and left it in
DFU. This exercises the official app's recovery path — its hidden debug menu
(type `qa40x` in the app) → "Load default secure image (must be in HID mode)".
A successful flash boots the device into the analyzer.

```sh
vqa40x --boot-bootloader --post-flash-version 60
```

### Re-attaching

USB/IP clients do not re-attach by themselves. Run a retry loop —
`tools/auto-attach.ps1` (Windows) — before triggering the update:

```powershell
powershell -ExecutionPolicy Bypass -File tools\auto-attach.ps1 -Server <server-host>
```

Demo: make the official app (which bundles firmware v60) offer an upgrade:

```sh
vqa40x --fw-version 58 --post-flash-version 60 --flash-secs 6 --save-firmware ./flashes
```

## Multiple devices

Run a whole bench — several QA402s and QA403s at once, each with its own
busid and serial — from one JSON file, relaunchable identically:

```sh
vqa40x --config examples/devices.json
```

Each entry sets any subset of the per-device options; a fixed `serial`
reproduces across runs, while `"serial": "random"` (or omitting it) picks a
fresh one each launch. Attach each device by its busid
(`usbip attach -r <host> -b 1-2`). `vqa40x --dump-config` prints the JSON for
the current flags as a starting template.

```json
{
  "devices": [
    { "model": "qa402", "busid": "1-1", "serial": "C0FF_EE01" },
    { "model": "qa403", "busid": "1-2", "serial": "random" },
    { "model": "qa403", "busid": "1-3", "boot-bootloader": true }
  ]
}
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

## Recording USB exchanges

`--record <path>` captures everything the attached client sends and everything
the device answers — the exact traffic the official app (or any other host)
puts on the virtual wire — to a JSONL file: one JSON event per line, readable
as-is and complete enough to re-analyse or replay a session offline. Point it
at a file, or at an existing directory to get an auto-named `usb-<unix>.jsonl`.

```sh
vqa40x --record session.jsonl
```

The first line is a session header (`"format": "vqa40x-usb-log/1"`). Every
following event carries `n` (sequence number, contiguous from 1), `t` (seconds
since server start), `c` (connection id, one per attach) and a `kind`:

| `kind` | Meaning |
|---|---|
| `attach` / `detach` | A client imported the device (busid, peer, vid/pid, persona) / dropped it (reason) |
| `control` | EP0 control transfer: decoded SETUP (`request_name` for standard requests) + data stage |
| `reg_write` | EP1 register frame, decoded: `reg`, `reg_name`, u32 `value`, raw `hex` |
| `reg_read_req` / `reg_read` | Register read request and its 4-byte reply, paired by FIFO order |
| `dac_data` / `adc_data` | EP2 stream block: `len`, `frames` (int32 LE stereo, right first), payload as `data_b64` |
| `in_req` | An IN URB queued by the client (shows the host's read-ahead behaviour) |
| `out` / `in` | Raw transfers (bootloader HID reports, unknown endpoints), hex up to 64 bytes then base64 |
| `unlink` | The client cancelled a pending URB (`victim`, `outcome`) |

Register frames are decoded with the same register map the device model
implements; multi-frame URBs (several 5-byte frames in one transfer) produce
one event per frame sharing the URB's `seq`. During streaming the log grows by
roughly 1 MB per second of capture; `--record-data summary` drops the payloads
(lengths stay) when only the control flow matters.

The control flow reads directly with `jq`:

```sh
jq -c 'select(.kind | startswith("reg"))' session.jsonl   # the register bus
jq -c 'select(.kind == "control")' session.jsonl          # enumeration
```

and `data_b64` payloads decode back to samples in a few lines of Python
(`base64.b64decode`, then `struct.unpack("<ii", ...)` per frame — right
channel first on the wire, full scale 2^31).

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
