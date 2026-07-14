#!/bin/sh
# Unbind and remove the vqa40x USB gadget created by gadget-setup.sh.
# Root required. Safe to run repeatedly.
set -u

NAME="vqa40x"
G="/sys/kernel/config/usb_gadget/$NAME"
FFS="/dev/ffs-$NAME"

# Unbind the UDC (ignore if already unbound).
[ -f "$G/UDC" ] && echo "" > "$G/UDC" 2>/dev/null || true

# Unlink the function from the config, then remove directories in order.
rm -f "$G/configs/c.1/ffs.$NAME" 2>/dev/null || true
umount "$FFS" 2>/dev/null || true
rmdir "$FFS" 2>/dev/null || true
rmdir "$G/configs/c.1/strings/0x409" 2>/dev/null || true
rmdir "$G/configs/c.1" 2>/dev/null || true
rmdir "$G/functions/ffs.$NAME" 2>/dev/null || true
rmdir "$G/strings/0x409" 2>/dev/null || true
rmdir "$G" 2>/dev/null || true

echo "vqa40x gadget removed."
