<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/logo-light.svg">
    <img src="assets/logo-light.svg" width="204" alt="Usher">
  </picture>
  <h3><code>ssh</code> with a screen.</h3>
  <p>If you can <code>ssh my-host</code>, you can <code>usher my-host</code>.<br>Nothing to install as a service, nothing to pair, nothing listening when you're not connected.</p>
</div>

---

```bash
usher my-host
```

For couch or TV streaming, use Moonlight. Usher is for machines you already SSH into: workstations, desktops, and home servers over LAN, Tailscale, ZeroTier, or WireGuard.

When you run `usher my-host`, the client invokes your existing OpenSSH config (`~/.ssh/config`), launches `usher-host` over `ssh -T`, exchanges a fresh 256-bit ChaCha20-Poly1305 session key on stdout, and streams hardware-encoded video (`HEVC`, `AV1`, or `H.264`, falling back to OpenH264), Opus audio, clipboard text, keyboard/mouse, and Xbox 360 gamepad input over encrypted UDP. When you close the window or press `Ctrl+Alt+Q`, `usher-host` tears down its virtual devices and exits—leaving zero background processes and zero open ports.

What Usher deliberately skips: phone/tablet/TV clients, PIN or QR pairing flows, HDR, controller rumble, and game library launchers. Desktop streaming, SSH ergonomics, and predictable teardown come first.

---

## Demo & Latency

- **Demo clip:** `[pending capture: 15–30s clip of typing usher desktop and interacting with a remote desktop session]`
- **End-to-end latency vs. Moonlight (`240 fps` camera test):** `[pending 240 fps capture measurement]`
  - Methodology: film the host monitor and laptop client side-by-side at `240 fps` while running a millisecond stopwatch/cursor-flash test over the same Wi-Fi/Tailscale link, then report median (`p50`) and `p95` glass-to-glass frame deltas honestly side-by-side with Moonlight + Sunshine.

---

## How Usher Compares

| Parameter | Usher | Moonlight + Sunshine | Windows RDP | Parsec |
| :--- | :--- | :--- | :--- | :--- |
| **Background daemon & open ports** | **None.** Spawns per SSH session; exits cleanly on disconnect | Always-on Sunshine service listening on HTTPS/RTSP/UDP ports | Always-on TermService on TCP/UDP 3389 | Always-on background daemon |
| **Client platforms & TV/mobile** | **Desktop only** (Windows, macOS, Linux). *Use Moonlight for couch/TV/mobile.* | **Excellent:** Windows, macOS, Linux, iOS, Android, tvOS, Xbox, smart TVs | Microsoft clients across desktop & mobile | Desktop, Web, Android |
| **Color, HDR & maturity** | SDR (`HEVC`/`AV1`/`H.264`), newer project | **Mature frame pacing, HDR, mobile/TV ecosystem** | Office UI tuned (GDI/RemoteFX, ClearType) | Low-latency gaming focus |
| **Auth & security model** | **OpenSSH keys** (`~/.ssh/config`) + ephemeral per-session ChaCha20-Poly1305 | TLS pairing with 4-digit PIN + Sunshine web UI | Windows account / NLA / Kerberos | Account / cloud auth |
| **Network & mesh VPNs** | Binds strictly to `SSH_CONNECTION` overlay/private IP (`<= 1200` B MTU) | LAN by default; remote via UPnP, port forwarding, or manual VPN | LAN / VPN | Proprietary WAN hole-punching & relays |
| **Best fit** | Machines you already SSH into; zero persistent host footprint | Living-room TV, mobile/tablet, HDR game streaming | Windows Pro/Enterprise IT admin | Account-managed screen sharing |

---

## Requirements by OS

### Windows
- **Client & Host binaries (`usher.exe`, `usher-host.exe`):** Zero external runtime dependencies on modern Windows (uses native DXGI Desktop Duplication, Windows Media Foundation `NVENC`/`AMF`/`QSV`/`DX12` encode & DXVA decode, WASAPI audio, and Win32 `SendInput`, with built-in OpenH264 software fallback). Optional `ViGEmBus` enables virtual Xbox 360 gamepads; optional `ffmpeg` acts as a fallback.
- **SSH Hosts:** Enable OpenSSH Server (`sshd`). For Administrator accounts, `usher-host` creates an ephemeral `NT AUTHORITY\SYSTEM` worker (cleaned up immediately on exit) to capture the secure `Winlogon` Lock Screen and UAC prompts. Pass `--no-system` (`no_system = true` in `config.toml`) to restrict the worker to `InteractiveToken` only (cannot capture Lock Screen or UAC). See [SECURITY.md](SECURITY.md) for the full security model.

### Linux Host (`usher-host`)
- **Zero Link-Time C Dependencies:** Linux release tarballs and `.deb` packages are built against `manylinux_2_28` (`glibc >= 2.28`) for `x86_64` and `aarch64`. Native KMS/DRM capture, runtime-loaded VA-API & NVENC hardware encoders (`libva.so.2`, `libnvidia-encode.so.1`), PipeWire audio (`libpipewire-0.3.so.0`), and PulseAudio virtual microphone (`libpulse-simple.so.0`) are all loaded dynamically at runtime via `dlopen` with built-in OpenH264 software fallback (plus optional `ffmpeg` / `gst-launch-1.0` backends).
- **Session, Ephemeral Headless Wayland & Permissions:** Works against an active `X11`/`Wayland` session, direct KMS/DRM console (`kmsgrab`), or `--capture testsrc`. Pass `--headless auto|sway|gamescope|cage` (or trailing command `usher my-host -- sway` / `usher my-host -- /path/to/app`) to spawn an isolated ephemeral Wayland compositor on connect and tear it down on disconnect. Grant `/dev/uinput` (`input` group + [`contrib/99-usher-uinput.rules`](contrib/99-usher-uinput.rules)) and `video`/`render` access (`./contrib/install-host.sh`, optionally `sudo setcap cap_sys_admin+ep ~/.local/bin/usher-host` for unprivileged KMS/DRM capture). Run `usher --doctor <host>` to inspect backend selection.

### macOS Client (`usher`)
- Native VideoToolbox hardware decoding (`HEVC`, `H.264`), built-in OpenH264 fallback, and `wgpu` Metal rendering on Apple Silicon (`arm64`).

---

## Quickstart

### 1. Install

| Platform | Install / Update |
| :--- | :--- |
| **Windows (PowerShell)** | Client (default):<br>`irm https://raw.githubusercontent.com/rdenno/usher/main/contrib/install-windows.ps1 \| iex`<br>Host or Both:<br>`$env:USHER_ROLE='both'; irm https://raw.githubusercontent.com/rdenno/usher/main/contrib/install-windows.ps1 \| iex` |
| **Windows (Installer / Package Managers)** | Run **`usher-setup-windows-x86_64.exe`** from **Releases**, or use the Scoop / Winget manifests in [`contrib/packaging`](contrib/packaging). |
| **Linux (`.deb` / tarball)** | Install `usher_<version>_amd64.deb` / `arm64.deb` from **Releases**, or extract `usher-linux-<arch>.tar.gz` and run [`./contrib/install-host.sh`](contrib/install-host.sh) once on the host. See [`contrib/packaging/aur`](contrib/packaging/aur) for Arch PKGBUILD. |
| **macOS** | Extract `usher-macos-aarch64.tar.gz` from **Releases**, or see [`contrib/packaging/homebrew`](contrib/packaging/homebrew). |
| **Cargo / From Source** | `cargo install usher-client usher-host` or `cargo build --release --workspace` |

> On Windows, open a new terminal after a first-time install (or refresh `PATH`: `$env:PATH = [Environment]::GetEnvironmentVariable("Path","User") + ";" + [Environment]::GetEnvironmentVariable("Path","Machine")`).

### 2. Configure SSH (`~/.ssh/config`)

```sshconfig
Host desktop
    HostName 100.88.12.34
    User alice
    IdentityFile ~/.ssh/id_ed25519
```

OpenSSH arguments pass straight through (`usher -p 2222 -J bastion -i ~/.ssh/key desktop`), and interactive SSH prompts (YubiKey / FIDO touch, passphrases, `ssh-agent` confirmations) work out of the box (pass `--batch-mode` if you want non-interactive `BatchMode=yes`). If UDP cannot punch through a firewall or bastion, Usher automatically falls back to tunneling encrypted frames over the existing SSH stdio stream (`--transport auto|udp|tcp`). If `usher-host` is missing on the remote machine, Usher can inspect `~/.local/bin` or upload/install the matching binary over SSH (`--auto-install` / `--no-auto-install`).

### 3. Connect & Diagnose

```bash
usher desktop
usher -J bastion desktop -- bash -lc "steam -tenfoot"
usher --list-displays desktop
usher --display HDMI-1 desktop
usher --doctor desktop
usher --local --capture testsrc
usher --local --capture testsrc --no-audio --no-input --no-clipboard --smoke-frames 5
```

---

## Configuration (`config.toml`)

Optional defaults live at `~/.config/usher/config.toml` (Linux/macOS) or `%APPDATA%\usher\config.toml` (Windows). Press `Ctrl+Alt+S` -> **Save for `<host>`** mid-stream to persist per-host defaults:

```toml
max_bitrate_kbps = 60000
prefer = "latency"
chroma = "auto"

[host.desktop]
max_bitrate_kbps = 80000
resolution = "match-window"
fps = 120
codec = "auto"
chroma = "444"
display = "DP-1"
pointer_mode = "absolute"
mic = false
```

---

## Controls & Hotkeys

| Shortcut / Input | Action |
| :--- | :--- |
| **`Ctrl+Alt+S`** | Toggle the hidden **Settings & Live Stats Overlay** |
| **`Ctrl+Alt+G`** | Toggle **Absolute** pointer / **Locked Relative** mouse |
| **`Ctrl+Alt+F`** | Toggle borderless fullscreen |
| **`Ctrl+Alt+Q`** | Release all held remote inputs, disconnect, and tear down `usher-host` |
| **Separate Cursor Plane** | Host sends cursor shape (`Cursor` plane) so the client draws the pointer locally with 0 ms delay in Absolute mode (pass `--no-cursor-plane` to bake into host video) |
| **Drag & Drop** | Drop files or folders onto the Usher window to transfer via `scp` into the remote user's `Downloads/` directory |
| **Microphone (`--mic`)** | Optional local microphone passthrough to an ephemeral host virtual mic (off by default) |
| **Gamepads** | Xbox 360 pass-through (`ViGEmBus` on Windows hosts, `/dev/uinput` on Linux hosts) |
| **Clipboard** | Bidirectional UTF-8 clipboard sync (disable with `--no-clipboard` / `clipboard = false`) |

---

## Key Flags

| Flag | Default | Notes |
| :--- | :--- | :--- |
| `--max-bitrate-kbps` | `60000` | Adaptive bitrate ceiling in kbps |
| `--resolution` | `auto` | `auto` (fit host aspect inside client monitor), `native`, `match-window`, or `WxH` |
| `--fps` | `auto` | `auto` or `1..=240` |
| `--codec` | `auto` | `auto` (`hevc` -> `av1` -> `h264`), `h264`, `hevc`, `av1` |
| `--chroma` | `auto` | `auto` (4:4:4 when host NVENC + client support it, else 4:2:0), `420`, `444` (plus 350 ms static-screen crisp IDR refresh) |
| `--prefer` | `latency` | `latency` or `quality` |
| `--list-displays` | `false` | Enumerate available host displays (`index: name (WxH)`) and exit |
| `--display` | `0` | Host monitor index or connector/display name from `--list-displays` |
| `--transport` | `auto` | `auto` (UDP with automatic SSH stdio TCP tunnel fallback), `udp`, or `tcp` |
| `--headless` | `none` | Ephemeral remote Wayland session (`none`, `auto`, `sway`, `gamescope`, `cage`; auto-enabled with trailing `-- <cmd>`) |
| `--windowed` | `false` | Start in a window instead of borderless fullscreen (`Ctrl+Alt+F` toggles) |
| `--capture` / `--encoder` / `--decoder` | `auto` | Backend overrides (`dxgi`, `drm`, `pipewire`, `x11`, `testsrc`, `nvenc`, `vaapi`, `openh264`, `ffmpeg`, `native`) |
| `--no-cursor-plane` | `false` | Disable separate local cursor rendering and bake pointer into host video |
| `--mic` | `false` | Enable client-to-host virtual microphone passthrough (off by default) |
| `--view-only` / `--no-input` | `false` | Disable keyboard/mouse/gamepad forwarding and file drop uploads |
| `--no-clipboard` / `--no-audio` | `false` | Disable clipboard sync or audio streaming |
| `--batch-mode` | `false` | Enable OpenSSH `BatchMode=yes` (off by default so hardware keys & passphrase prompts work) |
| `--auto-install` / `--no-auto-install` | prompt | Automatically install `usher-host` over SSH when missing on remote target |
| `--no-system` | `false` | Windows host: run console worker as `InteractiveToken` only (never `NT AUTHORITY\SYSTEM`) |
| `--doctor` | `false` | Test SSH, codec support, Tailscale/ZeroTier P2P vs. relay, `ViGEmBus`, and UDP RTT |
| `--smoke-frames <N>` | none | Headless connect/decode check for `N` frames (used in CI; exits `0` on success) |

---

## Security & Workspace

See [SECURITY.md](SECURITY.md) for the full cryptographic design, nonce/anti-replay guarantees, Windows `SYSTEM` scope (`--no-system`), and `cargo-fuzz` targets.

| Crate | Purpose |
| :--- | :--- |
| [`crates/usher-proto`](crates/usher-proto) | Framing, BLAKE3 KDF, ChaCha20-Poly1305 AEAD, 128-packet replay window, XOR-FEC, AIMD controller |
| [`crates/usher-host`](crates/usher-host) | `usher-host` binary (Windows Session 0->1 DXGI/MF/WASAPI/`SendInput`; Linux DRM/PipeWire/X11/`uinput`) |
| [`crates/usher-client`](crates/usher-client) | `usher` binary (OpenSSH bootstrapper, `config.toml`, `Ctrl+Alt+S` overlay, `wgpu` shader, MF/VT/OpenH264 decode) |
