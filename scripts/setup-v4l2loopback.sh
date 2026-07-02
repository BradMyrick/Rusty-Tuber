#!/usr/bin/env bash
# Make the v4l2loopback virtual webcam persistent across reboots.
#
# Creates two system files so the module loads at boot with the options
# Rusty-Tuber expects (CAPTURE-only caps, a friendly card label, a fixed
# /dev/videoN), then loads it immediately if it isn't already present.
#
#   /etc/modules-load.d/v4l2loopback.conf   -> loads the module at boot
#   /etc/modprobe.d/v4l2loopback.conf       -> sets the options
#
# Idempotent: safe to re-run. Requires root (sudo). Tunables below.
#
# Usage:  sudo bash scripts/setup-v4l2loopback.sh
set -euo pipefail

VIDEO_NR="${VIDEO_NR:-2}"
CARD_LABEL="${CARD_LABEL:-Rusty-Tuber}"

if [ "$(id -u)" -ne 0 ]; then
    echo "This script writes to /etc — re-run with sudo:" >&2
    echo "  sudo bash scripts/setup-v4l2loopback.sh" >&2
    exit 1
fi

# Ensure the module package is present (Debian/Ubuntu). Other distros install
# v4l2loopback via their own package manager; this is a best-effort convenience.
if ! modinfo v4l2loopback >/dev/null 2>&1; then
    if command -v apt-get >/dev/null 2>&1; then
        echo "v4l2loopback module not found; installing v4l2loopback-dkms..."
        apt-get update
        apt-get install -y v4l2loopback-dkms v4l2loopback-utils
    else
        echo "ERROR: v4l2loopback module not installed and this isn't apt." >&2
        echo "       Install the v4l2loopback kernel module for your distro, then re-run." >&2
        exit 1
    fi
fi

LOAD_FILE="/etc/modules-load.d/v4l2loopback.conf"
OPTS_FILE="/etc/modprobe.d/v4l2loopback.conf"

echo "v4l2loopback" > "$LOAD_FILE"
echo "Wrote $LOAD_FILE (loads v4l2loopback at boot)"

echo "options v4l2loopback exclusive_caps=1 card_label=\"${CARD_LABEL}\" video_nr=${VIDEO_NR}" > "$OPTS_FILE"
echo "Wrote $OPTS_FILE (exclusive_caps=1, label=\"${CARD_LABEL}\", video_nr=${VIDEO_NR})"

# Load now if the device doesn't already exist, so this takes effect immediately.
DEV="/dev/video${VIDEO_NR}"
if [ -c "$DEV" ]; then
    echo "Already present: $DEV — no modprobe needed."
else
    echo "Loading module now..."
    modprobe v4l2loopback exclusive_caps=1 "card_label=${CARD_LABEL}" "video_nr=${VIDEO_NR}"
    if [ -c "$DEV" ]; then
        echo "Created $DEV."
    else
        echo "WARNING: $DEV did not appear — check dmesg | grep v4l2loopback" >&2
    fi
fi

cat <<EOF

Done. On every boot $DEV will come back automatically with these options.
Point Rusty-Tuber at it via [webcam].device = "$DEV" (or leave it blank to
auto-detect). If $VIDEO_NR collides with another device, re-run with a
different number:  sudo VIDEO_NR=3 bash scripts/setup-v4l2loopback.sh
EOF
