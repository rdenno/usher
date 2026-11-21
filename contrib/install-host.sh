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

if command -v setcap >/dev/null 2>&1; then
    echo "==> Granting cap_sys_admin+ep for DRM/KMS capture (dropped immediately after framebuffer open)"
    sudo setcap cap_sys_admin+ep "$BIN_DST" || echo "warn: setcap failed (KMS capture requires cap_sys_admin; Wayland portal fallback can use 'usher-host --setup-portal')" >&2
    getcap "$BIN_DST" || true
else
    echo "warn: 'setcap' not found (install libcap2-bin); KMS capture will fall back to Wayland portal ('usher-host --setup-portal') or X11" >&2
fi

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

echo "==> Checking host capture/encode environment (no ffmpeg/GStreamer required):"
for lib in libva.so.2 libnvidia-encode.so.1 libpipewire-0.3.so.0 libpulse-simple.so.0; do
    if ldconfig -p 2>/dev/null | grep -Fq "$lib"; then
        echo "  [ok] $lib (loaded at runtime when needed)"
    else
        echo "  [--] $lib not found in ldconfig (optional; OpenH264 software encode built in)"
    fi
done
for cmd in sway gamescope cage; do
    if command -v "$cmd" >/dev/null 2>&1; then
        echo "  [ok] headless compositor: $cmd -> $(command -v "$cmd")"
    fi
done
ls -l /dev/dri /dev/uinput 2>/dev/null || true

echo ""
echo "Done! Connect from any client with:"
echo "  usher <your-ssh-host-alias>"
echo "(Optional) To authorize Wayland ScreenCast portal without CAP_SYS_ADMIN, run locally once:"
echo "  usher-host --setup-portal"
