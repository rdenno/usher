# Usher

**Zero-daemon, SSH-bootstrapped, ultra-low-latency remote desktop and game streamer.**

Usher streams video, audio, keyboard/mouse, and gamepad input over direct UDP on your private mesh network (Tailscale, ZeroTier, or WireGuard) using **only** your existing OpenSSH key and `~/.ssh/config` host aliases. No background daemons, no pairing PINs, no accounts, and zero open ports when idle.

```bash
usher my-host
```

---

## How It Works

1. **Bootstrap (`ssh -T`)**: `usher` launches a single non-interactive SSH command (`usher-host`) over public-key auth (`BatchMode=yes`, no password prompts).
2. **Ephemeral Bind & Handshake**: `usher-host` reads `SSH_CONNECTION`, binds an ephemeral high UDP port **strictly on the overlay/private IP** (never `0.0.0.0`), generates a 256-bit CSPRNG session key, and prints one JSON line to stdout (`USHER_HELLO {...}`).
3. **Direct Encrypted UDP**: The client connects directly to `<overlay_ip>:<port>`. All video, XOR-FEC parity, Opus audio, input, and control datagrams are capped at `<= 1200` bytes (safe inside Tailscale's 1280-byte MTU without fragmentation) and authenticated/encrypted with **ChaCha20-Poly1305** + directional BLAKE3 subkeys + a 128-packet sliding anti-replay window.
4. **Zero-State Teardown**: Closing the window (`Ctrl+Alt+Q`) or dropping the SSH connection immediately sends `EOF`/`SIGHUP` to `usher-host`, destroying its virtual display/input devices, closing the UDP socket, and exiting.

---

## Quickstart

### 1. SSH Config (`~/.ssh/config`)

Point your host alias at its Tailscale / ZeroTier / WireGuard IP (or MagicDNS name):

```sshconfig
Host desktop
    HostName 100.88.12.34
    User alice
    IdentityFile ~/.ssh/id_ed25519
```

### 2. Install & Run

#### Option A: Prebuilt GitHub Releases (Recommended for Client Laptop)

Download the release bundle for your OS from the GitHub Releases page (`usher-windows-x86_64.zip`, `usher-linux-x86_64.tar.gz`, `usher-macos-aarch64.tar.gz`), or run the Windows installer script via GitHub CLI (`gh`):

```powershell
# Windows client laptop (downloads latest release & adds usher.exe to User PATH):
.\contrib\install-windows.ps1 -Role client

# Windows host PC (installs usher-host.exe, checks sshd, sets UDP firewall rule):
.\contrib\install-windows.ps1 -Role host

# Or build from source locally instead of downloading a release:
.\contrib\install-windows.ps1 -Role both -FromSource
```

#### Option B: Build from Source

```bash
cargo build --release --workspace
```

On a **Linux host** (one-time setup for `/dev/uinput` virtual keyboard/mouse/Xbox 360 gamepad):

```bash
./contrib/install-host.sh
```

On a **Windows host**:
- Ensure OpenSSH Server (`sshd`) is running and your client's public key is authorized (`C:\ProgramData\ssh\administrators_authorized_keys` for admin accounts or `%USERPROFILE%\.ssh\authorized_keys`).
- Ensure `usher-host.exe` is in your system/user `PATH` (`contrib/install-windows.ps1 -Role host` handles installation, User `PATH`, firewall rule, and prints a ready-to-paste `~/.ssh/config` block).
- When `ssh` connects to Windows OpenSSH Server (Session 0), `usher-host.exe` automatically bridges into your active interactive desktop session via Windows Task Scheduler (`InteractiveToken`) and a local named pipe to run `ddagrab` + NVENC/AMF/QSV (or built-in DXGI Desktop Duplication + OpenH264), WASAPI loopback Opus audio, and Win32 `SendInput`, tearing down immediately when the SSH connection drops.

Then connect from Windows, macOS, or Linux:

```bash
usher desktop
```

### Local Loopback Test (No SSH Required)

Test the full pipeline (OpenH264 test pattern + Opus audio + ChaCha20-Poly1305 UDP + XOR-FEC + `wgpu` window) locally on one machine:

```bash
usher --local --capture testsrc
```

---

## Host Options

| Flag | Default | Description |
| :--- | :--- | :--- |
| `--capture` | `auto` | `auto`, `drm` (Linux kmsgrab), `pipewire` (Wayland), `x11`, `dxgi` (Windows Desktop Duplication / `ddagrab`), or `testsrc` |
| `--encoder` | `auto` | `auto`, `vaapi` (Linux AMD/Intel DMA-BUF), `nvenc` (Linux/Windows NVIDIA), `amf` (Windows AMD), `qsv` (Windows Intel), or `openh264` |
| `--headless` | `none` | Linux only: spin up an ephemeral `WLR_BACKENDS=headless` compositor (`sway`, `gamescope`, `cage`) for the duration of the session |
| `--codec` | `h264` | `h264`, `hevc`, or `av1` |
| `--width` / `--height` | `0` (auto) | `0` auto-detects connected display resolution (DRM on Linux, primary monitor on Windows) or defaults to `1280x720` |
| `--fps` | `60` | Target frame rate |
| `--bitrate-kbps` | `20000` | CBR target video bitrate |
| `--mtu` | `1200` | Outer UDP packet cap (recommended `<= 1200` for Tailscale/ZeroTier) |
| `--fec` | `10` | Striped XOR parity percentage (`0..=50`) for single/burst packet-loss recovery without retransmit latency |

> **Tip for Tailscale/ZeroTier:** Make sure peer-to-peer UDP hole punching is active (`tailscale status` shows direct IP rather than `relay`). If RTT exceeds `60 ms`, `usher` logs a warning that traffic may be hitting a DERP relay.

---

## Client Controls & Hotkeys

Live telemetry (`fps`, `RTT`, `Mbps`, `FEC` recoveries, pointer mode) is shown in the window title bar.

- **`Ctrl+Alt+G`**: Toggle between **Absolute** pointer (desktop) and **Locked Relative** raw mouse (3D games)
- **`Ctrl+Alt+F`**: Toggle borderless fullscreen
- **`Ctrl+Alt+Q`**: Disconnect and tear down the remote session cleanly
- **Gamepads**: Automatically polled via `gilrs` and mapped to a virtual Xbox 360 controller on the Linux host

---

## Workspace Crates

- `crates/usher-proto`: Wire envelope, directional ChaCha20-Poly1305 AEAD + replay protection, striped XOR-FEC, MTU-bounded frame fragmenter/reassembler, Annex-B AU splitter, input & audio codecs, and `USHER_HELLO` / `SSH_CONNECTION` parsers.
- `crates/usher-host` (`usher-host`): SSH-bootstrapped host supporting Linux (DRM/KMS + VAAPI, NVENC, PipeWire Wayland + ephemeral headless `sway`/`gamescope`/`cage`, `/dev/uinput` virtual devices, PipeWire Opus audio), Windows (Session 0 -> Session 1 SSH bridge, `ddagrab` NVENC/AMF/QSV or built-in DXGI Desktop Duplication + OpenH264, WASAPI loopback Opus audio, Win32 `SendInput`), and built-in cross-platform `testsrc`.
- `crates/usher-client` (`usher`): Zero-config SSH bootstrapper + `wgpu` BT.709 YUV420P GPU renderer + `winit` input capture + `gilrs` gamepad + `cpal` / pure-Rust Opus playback (`rusty-opus`).
