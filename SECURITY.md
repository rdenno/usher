# Security Model (`usher`)

Usher (`v0.4.3+`) uses an ephemeral, SSH-bootstrapped session architecture with zero listening daemons and zero persistent credentials when idle. This document explains what is trusted, how the UDP transport is secured, how Windows Session 0->1 elevation works (and how to disable it), and how network parsers are fuzzed.

## 1. Threat Model & Trust Boundaries

- **Authentication & Authorization = OpenSSH (`sshd`):** Usher has no PINs, pairing database, or TLS CA. A client is authorized to stream and send input if and only if it can authenticate over SSH and execute `usher-host`.
- **Single-Use Session Key:** On startup, `usher-host` draws a fresh 256-bit key (`session_key`) from the operating system CSPRNG (`rand::rngs::SysRng` -> `getrandom` / `BCryptGenRandom`) and prints a single JSON handshake line (`USHER_HELLO {...}`) on SSH stdout. Keys exist only in process memory for that session and are never written to disk.
- **No Wildcard Exposure:** `usher-host` inspects `SSH_CONNECTION` and binds its UDP socket strictly to the server's SSH endpoint IP (e.g. Tailscale `100.64.0.0/10`, ZeroTier, WireGuard, LAN RFC 1918, `fd7a:...` ULA, or loopback). Non-private / public IPs are refused unless `--allow-public` is explicitly passed.
- **Peer Locking:** Before sending video/audio/clipboard data, `usher-host` waits for an authenticated control packet (`ClientHello`) that decrypts cleanly under the session key, then locks its UDP destination to that source `SocketAddr` (and logs a warning if its IP differs from `SSH_CONNECTION`'s client IP).

## 2. Wire Cryptography & Anti-Replay

Implemented in [`crates/usher-proto/src/crypto.rs`](crates/usher-proto/src/crypto.rs) and [`crates/usher-proto/src/packet.rs`](crates/usher-proto/src/packet.rs):

- **Directional Key Separation:** From the 32-byte `session_key`, `SessionCrypto::new` derives two independent 256-bit keys using BLAKE3 key derivation:
  - Host -> Client: `blake3::derive_key("usher-v1-host-to-client", session_key)`
  - Client -> Host: `blake3::derive_key("usher-v1-client-to-host", session_key)`
  Distinct directional keys ensure packets from one peer can never be reflected back to it.
- **Cipher & Framing:** Every UDP datagram is encrypted with **`ChaCha20-Poly1305`** (IETF 96-bit nonce, 128-bit Poly1305 tag):
  - **Outer Header (12 bytes, authenticated as AEAD AAD):** `[magic: "US" (2B)][version<<4 | packet_kind (1B)][reserved: 0 (1B)][seq: u64 LE (8B)]`
  - **Ciphertext + 16-byte Tag** bounded inside `<= 1200` bytes total (`max_plaintext_len(mtu)`).
- **Nonce Construction & Nonce Uniqueness:**
  - `nonce = [0, 0, 0, 0, seq_le[0..8]]`.
  - `tx_seq` starts at `0`, increments strictly monotonically per encrypted packet, and bails before `u64::MAX` wraps. Because every session generates a fresh CSPRNG `session_key`, `(key, nonce)` never repeats across sessions or packets.
- **128-Packet Sliding Anti-Replay Window:**
  - `ReplayWindow` (`highest_seq: u64`, `bitmap: u128`) rejects duplicate sequences and packets older than `highest_seq - 127`.
  - Crucially, **`ReplayWindow::record_accepted(seq)` is called ONLY after `ChaCha20-Poly1305` decryption/authentication succeeds**. Unauthenticated spoofed packets cannot shift `highest_seq` or mark valid in-flight packets as replayed.
- **Safe Teardown Input Release:** On disconnect (`Ctrl+Alt+Q`), focus loss, SSH EOF, or network timeout, both client and host drain and release any held keys, modifiers (`Alt`/`Meta` before `Ctrl`/`Shift`), mouse buttons, and gamepad state before exiting.

## 3. Windows `NT AUTHORITY\SYSTEM` Console Bridge (`--no-system` Opt-Out)

- **Why it exists:** When SSHing into Windows, `sshd` runs the shell in non-interactive **Session 0**, which cannot access `DXGI` Desktop Duplication, `WASAPI`, or `SendInput`, and standard user tokens cannot read the secure **`Winlogon` desktop** (Lock Screen or UAC prompts).
- **Default behavior (Administrator SSH accounts):** [`crates/usher-host/src/windows_session.rs`](crates/usher-host/src/windows_session.rs) spawns a short-lived scheduled task as `NT AUTHORITY\SYSTEM` (`S-1-5-18`) in Session 0 whose only job (`--windows-system-launcher`) is calling `CreateProcessAsUserW` with a duplicated primary token targeting `active_console_session_id()` on `WinSta0\Default`, immediately deleting the scheduled task via `ScheduledTaskGuard` (`Drop`) before the session even streams. The Session 0 host communicates with the Session 1 worker over an ephemeral random-named pipe (`\\.\pipe\usher-bridge-<16 hex>`).
- **What `SYSTEM` grants:** Local Administrator SSH users can unlock a locked Windows PC and click UAC elevation dialogs remotely. Non-admin SSH accounts automatically fall back to `InteractiveToken` (current desktop only; no lock-screen/UAC access).
- **How to opt out (`--no-system`):** If you never want `usher-host` to spawn a `SYSTEM` task even when SSHed in as an Administrator, pass `--no-system` on the CLI or set `no_system = true` in `~/.config/usher/config.toml` (`%APPDATA%\usher\config.toml`). The host will skip `SYSTEM` creation completely and run the worker with `InteractiveToken` only (sacrificing Lock Screen and UAC capture/control).

## 4. Network Parser Fuzzing (`cargo-fuzz`)

All untrusted network and IPC decoders live in `usher-proto` (`#![forbid(unsafe_code)]` / pure safe Rust slice/byte parsing with explicit bounds checks and errors instead of panics). Continuous fuzzing harnesses live under [`fuzz/`](fuzz):

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run packet
cargo +nightly fuzz run fragment
cargo +nightly fuzz run fec
cargo +nightly fuzz run handshake
cargo +nightly fuzz run input_clipboard
```

A deterministic pseudo-random stress suite (`crates/usher-proto/tests/fuzz_smoke.rs`) also runs under normal `cargo test --workspace` on every commit and CI build.

## 5. Reporting Vulnerabilities

Please report security issues privately via GitHub Security Advisories (`https://github.com/rdenno/usher/security/advisories/new`) or open a minimal report without leaking exploit keys/targets if non-sensitive.
