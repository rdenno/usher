#!/usr/bin/env bash
set -euo pipefail

BIN_SRC="${1:-$(command -v usher-host || echo ./target/release/usher-host)}"
BIN_DST="/usr/local/bin/usher-host"
UDEV_RULE="/etc/udev/rules.d/99-usher-uinput.rules"
TARGET_USER="${SUDO_USER:-$USER}"

if [[ ! -x "$BIN_SRC" ]]; then
    echo "error: usher-host binary not found or not executable at: $BIN_SRC" >&2
    echo "Build it first with: cargo build --release -p usher-host" >&2
    exit 1
fi

echo "==> Installing usher-host to $BIN_DST"
sudo install -m 0755 "$BIN_SRC" "$BIN_DST"

echo "==> Installing udev rule to $UDEV_RULE"
sudo tee "$UDEV_RULE" >/dev/null <<'EOF'
KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
EOF
sudo modprobe uinput || true
sudo udevadm control --reload-rules
sudo udevadm trigger /dev/uinput || true

if ! id -nG "$TARGET_USER" | tr ' ' '\n' | grep -qx input; then
    echo "==> Adding $TARGET_USER to group 'input' (re-login required for group change)"
    sudo usermod -aG input "$TARGET_USER"
fi

echo "==> Checking host capture/encode tools:"
for cmd in ffmpeg gst-launch-1.0 pw-record sway gamescope cage; do
    if command -v "$cmd" >/dev/null 2>&1; then
        echo "  [ok] $cmd -> $(command -v "$cmd")"
    else
        echo "  [--] $cmd not found (optional depending on capture/encoder mode)"
    fi
done
ls -l /dev/dri /dev/uinput 2>/dev/null || true

echo ""
echo "Done! Connect from any client with:"
echo "  usher <your-ssh-host-alias>"
