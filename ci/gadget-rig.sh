#!/usr/bin/env bash
# Tier-1 test rig: a real USB Audio device, in software, with no hardware.
#
# `dummy_hcd` provides a paired virtual UDC and host controller on the same machine. Bind the
# kernel's own `f_uac1` gadget to the UDC through configfs and the host side enumerates a genuine
# USB Audio Class device — real descriptors, real alternate settings, real isochronous transfers —
# that this repo's crates can drive end to end.
#
# ⚠ UNVERIFIED AS SHIPPED. Whether a given machine can run this depends on its kernel config, and
# distro kernels frequently ship `# CONFIG_USB_DUMMY_HCD is not set` because it is a development
# driver. `./ci/gadget-rig.sh check` reports what the running kernel actually has, without needing
# root, and is the first thing to run.
#
# If the check fails there are three options, in order of preference:
#   1. A CI VM with a purpose-built kernel (CONFIG_USB_DUMMY_HCD=m, CONFIG_USB_CONFIGFS_F_UAC1=y).
#   2. A privileged container on a host whose kernel has the modules — the modules load into the
#      HOST kernel, so a container only helps if the host already has them.
#   3. usbip + vhci_hcd with a simulated device. Less faithful (the transfers cross a socket) but
#      it is still a real bus with real descriptors.
#
# Usage:
#   ./ci/gadget-rig.sh check     # report kernel support; no root needed, no changes
#   sudo ./ci/gadget-rig.sh up   # create the gadget
#   sudo ./ci/gadget-rig.sh down # tear it down
set -euo pipefail

GADGET_NAME="${GADGET_NAME:-usbiso}"
CONFIGFS="/sys/kernel/config/usb_gadget"
GADGET="$CONFIGFS/$GADGET_NAME"

# The Linux Foundation's gadget vendor id and a multifunction composite product id. Not a real
# product; nothing here should ever be mistaken for one.
VID="0x1d6b"
PID="0x0104"

# The device the tests expect: 2 channels, 16-bit, 48 kHz in both directions. Deliberately NOT the
# DualSense's 4-channel layout — the point of a synthetic rig is to be a second, independent shape,
# so a parser that only ever worked by coincidence on one device fails here.
P_CHMASK="${P_CHMASK:-3}"      # playback channel mask: front left + front right
P_SRATE="${P_SRATE:-48000}"
P_SSIZE="${P_SSIZE:-2}"        # bytes per sample
C_CHMASK="${C_CHMASK:-3}"
C_SRATE="${C_SRATE:-48000}"
C_SSIZE="${C_SSIZE:-2}"

kernel_config() {
  if [ -r /proc/config.gz ]; then
    zcat /proc/config.gz
  elif [ -r "/boot/config-$(uname -r)" ]; then
    cat "/boot/config-$(uname -r)"
  fi
}

have_module() {
  modinfo "$1" >/dev/null 2>&1 || [ -d "/sys/module/${1//-/_}" ]
}

cmd_check() {
  local ok=0
  echo "kernel: $(uname -r)"

  echo "== kernel config =="
  local cfg
  cfg="$(kernel_config || true)"
  if [ -z "$cfg" ]; then
    echo "  (kernel config not readable; falling back to module probing)"
  else
    for opt in CONFIG_USB_DUMMY_HCD CONFIG_USB_CONFIGFS CONFIG_USB_CONFIGFS_F_UAC1 \
               CONFIG_USB_CONFIGFS_F_UAC2 CONFIG_USB_GADGET; do
      printf '  %-32s %s\n' "$opt" "$(grep -E "^($opt=|# $opt )" <<<"$cfg" || echo 'not set')"
    done
  fi

  echo "== modules =="
  for m in dummy_hcd usb_f_uac1 usb_f_uac2 libcomposite; do
    if have_module "$m"; then
      printf '  %-16s present\n' "$m"
    else
      printf '  %-16s MISSING\n' "$m"
      [ "$m" = usb_f_uac2 ] || ok=1
    fi
  done

  echo "== verdict =="
  if [ "$ok" -eq 0 ]; then
    echo "  This kernel can run the tier-1 rig: sudo $0 up"
  else
    echo "  This kernel CANNOT run the tier-1 rig (see the header of this script for the"
    echo "  fallbacks). Tier-0 tests still cover all parsing and packet arithmetic:"
    echo "      cargo test --workspace"
    return 1
  fi
}

cmd_up() {
  [ "$(id -u)" -eq 0 ] || { echo "must be root" >&2; exit 1; }

  modprobe libcomposite
  modprobe dummy_hcd
  mountpoint -q /sys/kernel/config || mount -t configfs none /sys/kernel/config

  [ -d "$GADGET" ] && { echo "$GADGET already exists; run '$0 down' first" >&2; exit 1; }

  mkdir -p "$GADGET"
  echo "$VID" > "$GADGET/idVendor"
  echo "$PID" > "$GADGET/idProduct"
  mkdir -p "$GADGET/strings/0x409"
  echo "0123456789" > "$GADGET/strings/0x409/serialnumber"
  echo "usb-iso"    > "$GADGET/strings/0x409/manufacturer"
  echo "UAC1 test gadget" > "$GADGET/strings/0x409/product"

  mkdir -p "$GADGET/configs/c.1/strings/0x409"
  echo "uac1" > "$GADGET/configs/c.1/strings/0x409/configuration"
  echo 250    > "$GADGET/configs/c.1/MaxPower"

  mkdir -p "$GADGET/functions/uac1.usb0"
  local f="$GADGET/functions/uac1.usb0"
  # Newer kernels renamed these; write what exists rather than failing on the other spelling.
  for pair in "p_chmask:$P_CHMASK" "p_srate:$P_SRATE" "p_ssize:$P_SSIZE" \
              "c_chmask:$C_CHMASK" "c_srate:$C_SRATE" "c_ssize:$C_SSIZE"; do
    local key="${pair%%:*}" value="${pair#*:}"
    [ -w "$f/$key" ] && echo "$value" > "$f/$key"
  done

  ln -s "$f" "$GADGET/configs/c.1/" 2>/dev/null || true

  # Binding to the UDC is what makes the host side enumerate.
  local udc
  udc="$(ls /sys/class/udc | head -1)"
  [ -n "$udc" ] || { echo "no UDC available (is dummy_hcd loaded?)" >&2; exit 1; }
  echo "$udc" > "$GADGET/UDC"

  echo "gadget up on UDC $udc as $VID:$PID"
  sleep 1
  echo "host side now sees:"
  lsusb -d "${VID#0x}:${PID#0x}" || true
  echo
  echo "drive it with:  cargo run -p iso-probe -- list"
  echo "run the rig test with:  USB_ISO_GADGET=1 cargo test -p uac-host --test gadget -- --ignored"
}

cmd_down() {
  [ "$(id -u)" -eq 0 ] || { echo "must be root" >&2; exit 1; }
  [ -d "$GADGET" ] || { echo "no gadget to remove"; return 0; }
  echo "" > "$GADGET/UDC" 2>/dev/null || true
  rm -f "$GADGET/configs/c.1/uac1.usb0"
  rmdir "$GADGET/configs/c.1/strings/0x409" 2>/dev/null || true
  rmdir "$GADGET/configs/c.1" 2>/dev/null || true
  rmdir "$GADGET/functions/uac1.usb0" 2>/dev/null || true
  rmdir "$GADGET/strings/0x409" 2>/dev/null || true
  rmdir "$GADGET" 2>/dev/null || true
  echo "gadget down"
}

case "${1:-check}" in
  check) cmd_check ;;
  up)    cmd_up ;;
  down)  cmd_down ;;
  *)     echo "usage: $0 {check|up|down}" >&2; exit 1 ;;
esac
