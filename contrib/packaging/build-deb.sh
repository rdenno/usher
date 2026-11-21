#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
    echo "usage: $0 <version> <arch: amd64|arm64> <bin_dir> <out_dir>" >&2
    exit 2
fi

VERSION="${1#v}"
ARCH="$2"
BIN_DIR="$3"
OUT_DIR="$4"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$OUT_DIR"
mkdir -p \
    "$STAGE/DEBIAN" \
    "$STAGE/usr/bin" \
    "$STAGE/lib/udev/rules.d" \
    "$STAGE/usr/share/doc/usher"

install -m 0755 "$BIN_DIR/usher" "$STAGE/usr/bin/usher"
install -m 0755 "$BIN_DIR/usher-host" "$STAGE/usr/bin/usher-host"
install -m 0644 "$REPO_ROOT/contrib/99-usher-uinput.rules" "$STAGE/lib/udev/rules.d/99-usher-uinput.rules"
install -m 0644 "$REPO_ROOT/README.md" "$STAGE/usr/share/doc/usher/README.md"
install -m 0644 "$REPO_ROOT/SECURITY.md" "$STAGE/usr/share/doc/usher/SECURITY.md"

cat >"$STAGE/DEBIAN/control" <<EOF
Package: usher
Version: ${VERSION}
Section: net
Priority: optional
Architecture: ${ARCH}
Maintainer: rdenno <https://github.com/rdenno/usher>
Depends: libc6 (>= 2.28), libasound2, libudev1, libx11-6, libxkbcommon0
Recommends: openssh-client, libcap2-bin
Homepage: https://github.com/rdenno/usher
Description: Zero-daemon, SSH-bootstrapped remote desktop client and host
 Usher launches an ephemeral host over OpenSSH, streams hardware-encoded
 video and Opus audio over ChaCha20-Poly1305 encrypted UDP with adaptive
 bitrate and XOR-FEC, and tears everything down cleanly on disconnect.
EOF

cat >"$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules >/dev/null 2>&1 || true
    udevadm trigger /dev/uinput >/dev/null 2>&1 || true
fi
if command -v setcap >/dev/null 2>&1 && [ -x /usr/bin/usher-host ]; then
    setcap cap_sys_admin+ep /usr/bin/usher-host >/dev/null 2>&1 || true
fi
EOF
chmod 0755 "$STAGE/DEBIAN/postinst"

OUT_DEB="$OUT_DIR/usher_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$STAGE" "$OUT_DEB"
echo "Built $OUT_DEB"
