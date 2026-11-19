<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/logo-light.svg">
    <img src="assets/logo-light.svg" width="260" alt="Usher">
  </picture>
  <p><strong>U</strong>DP + <strong>S</strong>S<strong>H</strong> = <strong>E</strong>phemeral <strong>R</strong>emote desktop</p>
  <p>Zero-daemon, SSH-bootstrapped remote desktop &amp; game streaming over Tailscale, ZeroTier, or WireGuard.</p>
  <p>Zero open ports when idle &middot; No pairing PINs or accounts &middot; Direct ChaCha20-Poly1305 UDP + XOR-FEC</p>
</div>

---

```bash
usher my-host
```

- **SSH key auth only**: Reuses your existing `~/.ssh/config` aliases and public keys with no background server or manual pairing.
- **Mesh-native UDP**: Caps packets at `<= 1200` bytes for Tailscale/ZeroTier MTU safety and recovers dropped packets with striped XOR-FEC instead of TCP retransmits.
- **Hardware capture & encode**: Linux (DRM/PipeWire + VAAPI/NVENC, ephemeral headless `sway`/`gamescope`/`cage`) and Windows (DXGI/`ddagrab` + NVENC/AMF/QSV or built-in OpenH264).
- **Zero-state teardown**: Closing the client window or dropping SSH exits `usher-host`, destroys virtual input/display devices, and closes the UDP port.

---

## Quickstart

### 1. Install

Download the release artifact for your platform from **Releases** (or build from source):

| Platform | Install / Update |
| :--- | :--- |
| **Windows** | Run **`usher-setup-windows-x86_64.exe`** (installs Client/Host to `%LOCALAPPDATA%\Programs\Usher` and updates User `PATH`; re-run newer releases to upgrade in place while keeping settings). Portable `usher-windows-x86_64.zip` and [`contrib/install-windows.ps1`](contrib/install-windows.ps1) are also available. |
| **Linux** | Extract `usher-linux-x86_64.tar.gz` (`usher` + `usher-host`) and run [`./contrib/install-host.sh`](contrib/install-host.sh) once on the host for `/dev/uinput` permissions. |
| **macOS** | Extract `usher-macos-aarch64.tar.gz` (`usher` client). |
| **From Source** | `cargo build --release --workspace` |

> After a fresh Windows install, open a **new** PowerShell window (or refresh your current session with `$env:PATH = [Environment]::GetEnvironmentVariable("Path","User") + ";" + [Environment]::GetEnvironmentVariable("Path","Machine")`).

### 2. Configure SSH (`~/.ssh/config`)

Point a host alias at your desktop's Tailscale, ZeroTier, or WireGuard IP:

```sshconfig
Host desktop
    HostName 100.88.12.34
    User alice
    IdentityFile ~/.ssh/id_ed25519
```

- **Windows hosts**: Enable OpenSSH Server (`sshd`), authorize your key (`C:\ProgramData\ssh\administrators_authorized_keys` for admin users or `%USERPROFILE%\.ssh\authorized_keys`), and select **Host** or **Client + Host** in the installer. `usher-host` automatically bridges Session 0 SSH shells into your active desktop session (`InteractiveToken`).

### 3. Connect

```bash
usher desktop
```

Or run a single-machine loopback sanity check without SSH:

```bash
usher --local --capture testsrc
```

---

## Controls & Hotkeys

Window title bar shows live `fps`, `RTT`, `Mbps`, `FEC` recoveries, and pointer mode.

| Shortcut / Input | Action |
| :--- | :--- |
| **`Ctrl+Alt+G`** | Toggle **Absolute** desktop pointer / **Locked Relative** raw mouse (3D games) |
| **`Ctrl+Alt+F`** | Toggle borderless fullscreen |
| **`Ctrl+Alt+Q`** | Disconnect and tear down the remote host session |
| **Gamepads** | Polled automatically via `gilrs` and forwarded as a virtual Xbox 360 pad on Linux hosts |

---

## How It Works

1. **Bootstrap (`ssh -T`)**: `usher` runs `usher-host` over OpenSSH public-key auth (`BatchMode=yes`).
2. **Ephemeral Bind & Handshake**: `usher-host` reads `SSH_CONNECTION`, binds an ephemeral UDP port **strictly on the overlay/private IP** (never `0.0.0.0`), generates a 256-bit CSPRNG session key, and prints one `USHER_HELLO {...}` JSON line to stdout.
3. **Direct Encrypted UDP**: Video frames, striped XOR-FEC parity, 48 kHz Opus audio, and input events flow over `<= 1200`-byte UDP datagrams encrypted with **ChaCha20-Poly1305**, directional BLAKE3 subkeys, and a 128-packet anti-replay window.
4. **Zero-State Teardown**: SSH `EOF`/`SIGHUP` or `Ctrl+Alt+Q` shuts down pipelines, virtual devices, and sockets immediately.

---

## Options

| Flag | Default | Values / Notes |
| :--- | :--- | :--- |
| `--capture` | `auto` | `auto`, `drm`, `pipewire`, `x11`, `dxgi` (Windows), `testsrc` |
| `--encoder` | `auto` | `auto`, `vaapi`, `nvenc`, `amf`, `qsv`, `openh264` |
| `--headless` | `none` | Linux ephemeral `WLR_BACKENDS=headless`: `none`, `sway`, `gamescope`, `cage` |
| `--codec` | `h264` | `h264`, `hevc`, `av1` |
| `--width` / `--height` | `0` | `0` auto-detects connected display resolution (or `1280x720`) |
| `--fps` | `60` | Target frame rate |
| `--bitrate-kbps` | `20000` | CBR target bitrate |
| `--mtu` | `1200` | Outer UDP packet cap (`<= 1200` recommended for Tailscale/ZeroTier) |
| `--fec` | `10` | XOR parity percentage (`0..=50`) for packet-loss recovery |

> Keep peer-to-peer UDP hole punching active (`tailscale status` showing a direct peer rather than `relay`); `usher` warns if RTT rises above `60 ms`.

---

## Workspace

| Crate | Purpose |
| :--- | :--- |
| [`crates/usher-proto`](crates/usher-proto) | Wire framing, ChaCha20-Poly1305 AEAD, anti-replay, striped XOR-FEC, MTU fragmenter, Annex-B & audio/input codecs |
| [`crates/usher-host`](crates/usher-host) | `usher-host` binary (Linux DRM/PipeWire/headless + `uinput`, Windows Session 0→1 `ddagrab`/DXGI + WASAPI + `SendInput`) |
| [`crates/usher-client`](crates/usher-client) | `usher` binary (SSH bootstrapper, `wgpu` BT.709 renderer, `winit` input, `gilrs` gamepad, `cpal` + Opus audio) |
