#!/usr/bin/env python3
"""Extract the QA40x factory calibration page from a USB pcapng capture.

The official app reads the page at connect: CAL_PAGE_SELECT (0x0D = 0x10),
then a burst of CALIBRATION (0x19) reads, 4 bytes each. On the wire each
reply carries a page word big-endian; the host-side page is reconstructed by
reversing each 4-byte group (confirmed: the f32 trims then decode at the
documented record offsets).

Usage:
  extract_calpage.py CAPTURE.pcapng [-o page.bin]

Prints the decoded ADC/DAC trims as a sanity check and writes the first
512-byte page (multiple page reads in one capture are compared).
"""
import argparse
import struct
import subprocess
import sys

TSHARK = "/Applications/Wireshark.app/Contents/MacOS/tshark"

ADC_OFFSETS = [24, 36, 48, 60, 72, 84, 96, 108]  # 0..42 dBV, right at +6
DAC_OFFSETS = [120, 132, 144, 156]  # -12/-2/8/18 dBV, right at +6
ADC_LEVELS = [0, 6, 12, 18, 24, 30, 36, 42]
DAC_LEVELS = [-12, -2, 8, 18]


def load_cal_words(path):
    """All 4-byte replies to 0x19 read requests, in capture order (hex)."""
    out = subprocess.run(
        [TSHARK, "-r", path, "-Y", "usb.capdata", "-T", "fields",
         "-e", "usb.endpoint_address", "-e", "usb.capdata"],
        capture_output=True, text=True, check=True,
    ).stdout
    words = []
    pending_read = None
    for line in out.splitlines():
        p = line.split("\t")
        if len(p) < 2:
            continue
        ep, data = p[0], p[1]
        if ep == "0x01" and len(data) == 10:
            reg = int(data[0:2], 16)
            pending_read = (reg & 0x7F) if reg & 0x80 else None
        elif ep == "0x81" and len(data) == 8:
            if pending_read == 0x19:
                words.append(data)
            pending_read = None
    return words


def decode_trims(page):
    """(offset, level, left_db, right_db) for every record, host-app parse."""
    rows = []
    for offs, levels, kind in ((ADC_OFFSETS, ADC_LEVELS, "ADC"),
                               (DAC_OFFSETS, DAC_LEVELS, "DAC")):
        for off, lvl in zip(offs, levels):
            l = struct.unpack_from("<f", page, off + 2)[0]
            r = struct.unpack_from("<f", page, off + 8)[0]
            rows.append((kind, off, lvl, l, r))
    return rows


def plausible(rows):
    return all(abs(v) < 20 and v == v for _, _, _, l, r in rows for v in (l, r))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("capture")
    ap.add_argument("-o", "--out", default="calpage.bin")
    args = ap.parse_args()

    words = load_cal_words(args.capture)
    if len(words) < 128:
        sys.exit(f"only {len(words)} calibration reads in the capture (need 128)")
    print(f"{len(words)} calibration words ({len(words) // 128} page read(s))")

    pages_raw = [words[i:i + 128] for i in range(0, len(words) - 127, 128)]
    # Wire words are big-endian; the host page reverses each 4-byte group.
    pages = [b"".join(bytes.fromhex(w)[::-1] for w in p) for p in pages_raw]

    for i, p in enumerate(pages[1:], 2):
        print(f"page {i} identical to page 1: {p == pages[0]}")

    page = pages[0]
    rows = decode_trims(page)
    order = "reversed-per-word (host order)"
    if not plausible(rows):
        # Fallback: raw wire order.
        page = b"".join(bytes.fromhex(w) for w in pages_raw[0])
        rows = decode_trims(page)
        order = "raw wire order"
    if not plausible(rows):
        sys.exit("trims do not decode in either byte order — layout mismatch?")

    print(f"byte order: {order}")
    for kind, off, lvl, l, r in rows:
        print(f"  {kind} {lvl:+3d} dBV @ {off:3d}: L {l:+7.3f} dB   R {r:+7.3f} dB")

    with open(args.out, "wb") as f:
        f.write(page)
    print(f"wrote {len(page)} bytes to {args.out}")


if __name__ == "__main__":
    main()
