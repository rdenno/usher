<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/logo-light.svg">
    <img src="assets/logo-light.svg" width="260" alt="Usher">
  </picture>
  <p><strong>U</strong>DP + <strong>S</strong>S<strong>H</strong> = <strong>E</strong>phemeral <strong>R</strong>emote desktop</p>
  <p>Zero-daemon, SSH-bootstrapped remote desktop &amp; game streaming over Tailscale, ZeroTier, or WireGuard.</p>
  <p>Zero open ports when idle &middot; Adaptive bitrate &amp; FEC &middot; Direct ChaCha20-Poly1305 UDP</p>
</div>

---

```bash
usher my-host
```

With zero flags, `usher my-host` matches your monitor & refresh rate, picks the best mutual hardware codec (`HEVC`/`AV1` when both ends support it, otherwise `H.264`), and adapts bitrate & FEC automatically to link conditions.

- **SSH key auth only**: Reuses `~/.ssh/config` aliases and public keys with no background server, PINs, or accounts.
- **Cut-fast, grow-slow adaptive transport**: 500 ms client network reports steer bitrate (up to 60 Mbps by default) and dynamic XOR-FEC parity within `<= 1200`-byte mesh-safe UDP datagrams.
- **Hidden in-stream overlay (`Ctrl+Alt+S`)**: Tweak bitrate ceiling, resolution, FPS, codec, latency/quality tuning, audio, monitor, pointer mode, or HUD live and save per-host defaults.
- **Zero-state teardown**: Closing the window or dropping SSH exits `usher-host`, destroys virtual devices, and closes the UDP port.

---

## How Usher Compares

| Parameter | Usher | Moonlight + Sunshine | Windows RDP | Parsec / RustDesk |
| :--- | :--- | :--- | :--- | :--- |
| **Background daemon & open ports** | **None.** Spawns per SSH session and closes everything on exit | Always-on Sunshine service listening on HTTPS/RTSP/UDP ports | Always-on TermService listening on TCP/UDP 3389 | Always-on background daemon + relay / broker agents |
| **Latency & gaming** | **Low** (hardware HEVC/AV1/H.264, `ull` CBR, adaptive FEC, gamepad + relative mouse) | **Very low** (hardware stream, mature frame pacing & HDR) | **Medium** (office UI focused; no raw relative mouse or gamepads) | **Low–medium** (good gaming on Parsec; RustDesk tuned for support) |
| **Auth & security model** | **OpenSSH public-key only** (`BatchMode=yes`) + per-session ChaCha20-Poly1305 | TLS pairing with 4-digit PIN + Sunshine web login | Windows account / NLA / Kerberos | Vendor account/cloud auth (or custom self-hosted ID server) |
| **Network & mesh VPNs** | **Mesh-first** (binds strictly to `SSH_CONNECTION` overlay/private IP, `<= 1200` B MTU) | LAN by default; remote requires UPnP/port-forwarding or manual VPN setup | LAN/VPN; exposing 3389 publicly is unsafe | Built-in WAN hole-punching & vendor relays |
| **Setup & portability** | **Single static binary** (`usher` / `usher-host`), reuses `~/.ssh/config` | Host installer + web UI pairing + virtual display/gamepad drivers | Built into Windows Pro (none on Linux/Home hosts) | Full app installer + account or server config |
| **Best fit** | Ad-hoc, SSH-first remote desktop & gaming over Tailscale / ZeroTier | Dedicated living-room / high-FPS game streaming rigs | Multi-monitor Windows office administration | Team screen sharing, third-party support, zero-SSH access |

---

## Quickstart

### 1. Install

| Platform | Install / Update |
| :--- | :--- |
| **Windows (One-liner)** | Client (default):<br>`irm https://raw.githubusercontent.com/rdenno/usher/main/contrib/install-windows.ps1 \| iex`<br>Host / Both (also works for updates):<br>`$env:USHER_ROLE='host'; irm https://raw.githubusercontent.com/rdenno/usher/main/contrib/install-windows.ps1 \| iex`<br>`$env:USHER_ROLE='both'; irm https://raw.githubusercontent.com/rdenno/usher/main/contrib/install-windows.ps1 \| iex` |
| **Windows (Installer)** | Run **`usher-setup-windows-x86_64.exe`** from **Releases** (installs Client/Host to `%LOCALAPPDATA%\Programs\Usher` and updates User `PATH`; re-run newer releases to upgrade in place while keeping settings). |
| **Linux** | Extract `usher-linux-x86_64.tar.gz` (`usher` + `usher-host`) and run [`./contrib/install-host.sh`](contrib/install-host.sh) once on the host for `/dev/uinput` permissions. |
| **macOS** | Extract `usher-macos-aarch64.tar.gz` (`usher` client). |
| **From Source** | `cargo build --release --workspace` |

> After a fresh Windows install, open a **new** PowerShell window (or refresh `PATH` in your current shell with `$env:PATH = [Environment]::GetEnvironmentVariable("Path","User") + ";" + [Environment]::GetEnvironmentVariable("Path","Machine")`).

### 2. Configure SSH (`~/.ssh/config`)

```sshconfig
Host desktop
    HostName 100.88.12.34
    User alice
    IdentityFile ~/.ssh/id_ed25519
```

- **Windows hosts**: Enable OpenSSH Server (`sshd`), authorize your key (`C:\ProgramData\ssh\administrators_authorized_keys` for admin users or `%USERPROFILE%\.ssh\authorized_keys`), and select **Host** or **Client + Host** in the installer. `usher-host` automatically attaches to the active console session—capturing the Lock Screen (`Winlogon`) and UAC prompts as `NT AUTHORITY\SYSTEM` for Administrator SSH accounts and seamlessly following unlock transitions (`Session 2 -> Session 1`) without dropping the stream.

### 3. Connect

```bash
usher desktop
```

Single-machine loopback check without SSH:

```bash
usher --local --capture testsrc
```

---

## Configuration (`config.toml`)

Optional defaults live at `~/.config/usher/config.toml` (Linux/macOS) or `%APPDATA%\usher\config.toml` (Windows), using the same option names as CLI flags (CLI flags always win). Press `Ctrl+Alt+S` → **Save for `<host>`** mid-stream to write a host section automatically:

```toml
max_bitrate_kbps = 60000
prefer = "latency"

[host.desktop]
max_bitrate_kbps = 80000
resolution = "match-window"
fps = 120
codec = "auto"
pointer_mode = "relative"
stats_overlay = true
```

---

## Controls & Hotkeys

| Shortcut / Input | Action |
| :--- | :--- |
| **`Ctrl+Alt+S`** | Toggle the hidden **Settings & Live Stats Overlay** |
| **`Ctrl+Alt+G`** | Toggle **Absolute** desktop pointer / **Locked Relative** raw mouse (3D games) |
| **`Ctrl+Alt+F`** | Toggle borderless fullscreen |
| **`Ctrl+Alt+Q`** | Disconnect and tear down the remote host session |
| **Gamepads** | Polled via `gilrs` and mapped to a virtual Xbox 360 pad on Linux hosts |

---

## How It Works

1. **Bootstrap (`ssh -T`)**: `usher` probes client monitor/codec support and launches `usher-host` over OpenSSH public-key auth (`BatchMode=yes`).
2. **Ephemeral Bind & Handshake**: `usher-host` reads `SSH_CONNECTION`, binds an ephemeral UDP port strictly on the overlay/private IP (never `0.0.0.0`), generates a 256-bit CSPRNG key, and emits `USHER_HELLO {...}` on stdout.
3. **Adaptive Encrypted UDP**: Encrypted ChaCha20-Poly1305 video, XOR-FEC parity, 48 kHz Opus audio, and input events stream over `<= 1200`-byte packets while 500 ms client reports tune bitrate and FEC live.
4. **Zero-State Teardown**: SSH `EOF`/`SIGHUP` or `Ctrl+Alt+Q` shuts down encoders, virtual devices, and sockets cleanly.

---

## Options

| Flag | Default | Values / Notes |
| :--- | :--- | :--- |
| `--max-bitrate-kbps` | `60000` | Adaptive bitrate ceiling in kbps (client/config/host) |
| `--resolution` | `auto` | `auto` (client monitor), `native` (host monitor), `match-window`, or `WxH` |
| `--fps` | `auto` | `auto` (client refresh rate) or `1..=240` |
| `--codec` | `auto` | `auto` (negotiates `hevc`/`av1`/`h264`), `h264`, `hevc`, `av1` |
| `--prefer` | `latency` | `latency` (lowest delay + higher min FEC) or `quality` |
| `--display` | `0` | Multi-monitor host display index |
| `--stats-overlay` | `false` | Show compact top-left live stats HUD by default |
| `--capture` | `auto` | `auto`, `drm`, `pipewire`, `x11`, `dxgi`, `testsrc` |
| `--encoder` | `auto` | `auto`, `vaapi`, `nvenc`, `amf`, `qsv`, `openh264` |
| `--mtu` / `--fec` | `1200` / `10` | Outer UDP packet cap (`<= 1200`) and initial XOR parity % (`0..=50`) |

> Keep peer-to-peer UDP hole punching active (`tailscale status` showing a direct peer rather than `relay`); `usher` warns if RTT rises above `60 ms`.

---

## Workspace

| Crate | Purpose |
| :--- | :--- |
| [`crates/usher-proto`](crates/usher-proto) | Wire framing, ChaCha20-Poly1305 AEAD, anti-replay, striped XOR-FEC, AIMD `AdaptiveController`, and codecs |
| [`crates/usher-host`](crates/usher-host) | `usher-host` binary (Linux DRM/PipeWire/headless + `uinput`, Windows Session 0→1 `ddagrab`/DXGI + WASAPI + `SendInput`) |
| [`crates/usher-client`](crates/usher-client) | `usher` binary (SSH bootstrapper, `config.toml`, `Ctrl+Alt+S` overlay, `wgpu` BT.709 renderer, `winit`/`gilrs`/`cpal`) |
