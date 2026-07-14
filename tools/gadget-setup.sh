#!/bin/sh
# Create the configfs USB gadget skeleton + FunctionFS mount for vqa40x, then
# leave it UNBOUND. Run `vqa40x-gadget` afterwards: it writes the descriptors
# and binds the UDC. Root required. See GADGET.md.
#
# Usage: sudo tools/gadget-setup.sh [VID] [PID] [SERIAL]
set -eu

VID="${1:-0x16c0}"
PID="${2:-0x4e37}"          # QA402; use 0x4e39 for QA403
SERIAL="${3:-VQA0_0001}"
NAME="vqa40x"
G="/sys/kernel/config/usb_gadget/$NAME"
FFS="/dev/ffs-$NAME"

modprobe libcomposite 2>/dev/null || true

if [ -d "$G" ]; then
    echo "gadget already exists at $G — run gadget-teardown.sh first" >&2
    exit 1
fi

mkdir -p "$G"
echo "$VID" > "$G/idVendor"
echo "$PID" > "$G/idProduct"
echo 0x0100 > "$G/bcdDevice"
echo 0x0200 > "$G/bcdUSB"

mkdir -p "$G/strings/0x409"
echo "QuantAsylum"          > "$G/strings/0x409/manufacturer"
echo "QA40x Audio Analyzer" > "$G/strings/0x409/product"
echo "$SERIAL"              > "$G/strings/0x409/serialnumber"

mkdir -p "$G/functions/ffs.$NAME"
mkdir -p "$G/configs/c.1/strings/0x409"
echo "vqa40x" > "$G/configs/c.1/strings/0x409/configuration"
echo 250      > "$G/configs/c.1/MaxPower"
ln -s "$G/functions/ffs.$NAME" "$G/configs/c.1/"

mkdir -p "$FFS"
mount -t functionfs "$NAME" "$FFS"

echo "Gadget ready. Available UDC(s): $(ls /sys/class/udc 2>/dev/null || echo none)"
echo "Now run:  sudo vqa40x-gadget --ffs-dir $FFS --gadget-dir $G"
