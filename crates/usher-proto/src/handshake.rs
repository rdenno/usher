use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

pub const HELLO_PREFIX: &str = "USHER_HELLO ";
pub const DISPLAYS_PREFIX: &str = "USHER_DISPLAYS ";
pub const PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

impl VideoCodec {
    pub fn to_u8(self) -> u8 {
        match self {
            Self::H264 => 0,
            Self::Hevc => 1,
            Self::Av1 => 2,
        }
    }

    pub fn from_u8(val: u8) -> Result<Self> {
        match val {
            0 => Ok(Self::H264),
            1 => Ok(Self::Hevc),
            2 => Ok(Self::Av1),
            other => bail!("unknown VideoCodec byte {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecPreference {
    #[default]
    Auto,
    H264,
    Hevc,
    Av1,
}

impl CodecPreference {
    pub fn to_reconfigure_u8(self) -> u8 {
        match self {
            Self::Auto => 1,
            Self::H264 => 2,
            Self::Hevc => 3,
            Self::Av1 => 4,
        }
    }

    pub fn from_reconfigure_u8(val: u8) -> Result<Option<Self>> {
        match val {
            0 => Ok(None),
            1 => Ok(Some(Self::Auto)),
            2 => Ok(Some(Self::H264)),
            3 => Ok(Some(Self::Hevc)),
            4 => Ok(Some(Self::Av1)),
            other => bail!("unknown CodecPreference reconfigure byte {other}"),
        }
    }
}

impl fmt::Display for CodecPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::H264 => write!(f, "h264"),
            Self::Hevc => write!(f, "hevc"),
            Self::Av1 => write!(f, "av1"),
        }
    }
}

impl FromStr for CodecPreference {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "h264" | "h.264" | "avc" => Ok(Self::H264),
            "hevc" | "h265" | "h.265" => Ok(Self::Hevc),
            "av1" => Ok(Self::Av1),
            other => bail!("unsupported codec preference: {other}"),
        }
    }
}

impl From<VideoCodec> for CodecPreference {
    fn from(c: VideoCodec) -> Self {
        match c {
            VideoCodec::H264 => Self::H264,
            VideoCodec::Hevc => Self::Hevc,
            VideoCodec::Av1 => Self::Av1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QualityPreference {
    #[default]
    Latency,
    Quality,
}

impl QualityPreference {
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Latency => 0,
            Self::Quality => 1,
        }
    }

    pub fn from_u8(val: u8) -> Result<Self> {
        match val {
            0 => Ok(Self::Latency),
            1 => Ok(Self::Quality),
            other => bail!("unknown QualityPreference byte {other}"),
        }
    }

    pub fn to_reconfigure_u8(self) -> u8 {
        match self {
            Self::Latency => 1,
            Self::Quality => 2,
        }
    }

    pub fn from_reconfigure_u8(val: u8) -> Result<Option<Self>> {
        match val {
            0 => Ok(None),
            1 => Ok(Some(Self::Latency)),
            2 => Ok(Some(Self::Quality)),
            other => bail!("unknown QualityPreference reconfigure byte {other}"),
        }
    }
}

impl fmt::Display for QualityPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Latency => write!(f, "latency"),
            Self::Quality => write!(f, "quality"),
        }
    }
}

impl FromStr for QualityPreference {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "latency" | "low-latency" => Ok(Self::Latency),
            "quality" => Ok(Self::Quality),
            other => bail!("unsupported prefer mode: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChromaSubsampling {
    #[default]
    Yuv420,
    Yuv444,
}

impl ChromaSubsampling {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yuv420 => "420",
            Self::Yuv444 => "444",
        }
    }

    pub fn from_arg(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "420" | "4:2:0" | "yuv420" => Ok(Self::Yuv420),
            "444" | "4:4:4" | "yuv444" => Ok(Self::Yuv444),
            other => bail!("invalid chroma subsampling '{other}' (expected 420 or 444)"),
        }
    }
}

impl fmt::Display for ChromaSubsampling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Yuv420 => f.write_str("4:2:0"),
            Self::Yuv444 => f.write_str("4:4:4"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub index: u8,
    pub name: String,
    pub width: u16,
    pub height: u16,
}

/// Scales `(src_w, src_h)` to fit within `(max_w, max_h)` while preserving the host's
/// aspect ratio and never upscaling beyond `(src_w, src_h)`. Outputs even dimensions `>= 64`.
pub fn fit_preserving_aspect(src_w: u16, src_h: u16, max_w: u16, max_h: u16) -> (u16, u16) {
    let sw = (src_w.max(64) as u32) & !1;
    let sh = (src_h.max(64) as u32) & !1;
    let mw = (max_w.max(64) as u32) & !1;
    let mh = (max_h.max(64) as u32) & !1;

    if sw <= mw && sh <= mh {
        return (sw as u16, sh as u16);
    }

    let (w, h) = if (sw as u64) * (mh as u64) >= (mw as u64) * (sh as u64) {
        let w = mw;
        let h = ((sh as u64 * mw as u64 + sw as u64 / 2) / sw as u64).max(64) as u32;
        (w, h)
    } else {
        let h = mh;
        let w = ((sw as u64 * mh as u64 + sh as u64 / 2) / sh as u64).max(64) as u32;
        (w, h)
    };

    (
        ((w.clamp(64, mw)) as u16) & !1,
        ((h.clamp(64, mh)) as u16) & !1,
    )
}

impl fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoCodec::H264 => write!(f, "h264"),
            VideoCodec::Hevc => write!(f, "hevc"),
            VideoCodec::Av1 => write!(f, "av1"),
        }
    }
}

impl FromStr for VideoCodec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h264" | "h.264" | "avc" => Ok(VideoCodec::H264),
            "hevc" | "h265" | "h.265" => Ok(VideoCodec::Hevc),
            "av1" => Ok(VideoCodec::Av1),
            other => bail!("unsupported video codec: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloMessage {
    pub version: u8,
    pub bind_ip: IpAddr,
    pub port: u16,
    pub session_key: String,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub codec: VideoCodec,
    pub audio: bool,
    pub audio_sample_rate: u32,
    pub audio_channels: u8,
    pub mtu: u16,
    #[serde(default)]
    pub max_bitrate_kbps: u32,
    #[serde(default)]
    pub bitrate_kbps: u32,
    #[serde(default)]
    pub fec_percent: u8,
    #[serde(default)]
    pub display_index: u8,
    #[serde(default)]
    pub displays: Vec<DisplayInfo>,
    #[serde(default)]
    pub prefer: QualityPreference,
    #[serde(default)]
    pub supported_codecs: Vec<VideoCodec>,
    #[serde(default)]
    pub host_version: String,
    #[serde(default)]
    pub video_backend: String,
    #[serde(default)]
    pub gamepad_backend: String,
    #[serde(default)]
    pub hi_res_scroll: bool,
    #[serde(default)]
    pub capture_diagnostics: Vec<String>,
    #[serde(default)]
    pub tcp_tunnel: bool,
    #[serde(default)]
    pub chroma: ChromaSubsampling,
    #[serde(default)]
    pub cursor_plane: bool,
    #[serde(default)]
    pub mic_enabled: bool,
}

impl HelloMessage {
    pub fn decode_session_key(&self) -> Result<[u8; 32]> {
        let bytes = BASE64_STD
            .decode(self.session_key.trim())
            .context("invalid base64 session_key")?;
        if bytes.len() != 32 {
            bail!(
                "invalid session_key length: expected 32 bytes, got {}",
                bytes.len()
            );
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        Ok(key)
    }

    pub fn encode_session_key(key: &[u8; 32]) -> String {
        BASE64_STD.encode(key)
    }
}

pub fn encode_hello_line(msg: &HelloMessage) -> Result<String> {
    let json = serde_json::to_string(msg).context("failed to serialize HelloMessage")?;
    Ok(format!("{HELLO_PREFIX}{json}"))
}

pub fn encode_displays_line(displays: &[DisplayInfo]) -> Result<String> {
    let json = serde_json::to_string(displays).context("failed to serialize displays list")?;
    Ok(format!("{DISPLAYS_PREFIX}{json}"))
}

pub fn parse_displays_line(line: &str) -> Option<Result<Vec<DisplayInfo>>> {
    let trimmed = line.trim();
    let json_part = trimmed.strip_prefix(DISPLAYS_PREFIX)?;
    Some(serde_json::from_str(json_part).map_err(Into::into))
}

/// Resolves a `--display` selector (numeric index or case-insensitive display name/substring)
/// against enumerated displays.
pub fn resolve_display_selector(displays: &[DisplayInfo], selector: &str) -> Option<u8> {
    let sel = selector.trim();
    if sel.is_empty() {
        return None;
    }
    if let Ok(idx) = sel.parse::<u8>() {
        if displays.is_empty() || displays.iter().any(|d| d.index == idx) {
            return Some(idx);
        }
        return None;
    }
    if let Some(d) = displays.iter().find(|d| d.name.eq_ignore_ascii_case(sel)) {
        return Some(d.index);
    }
    let lower = sel.to_ascii_lowercase();
    displays
        .iter()
        .find(|d| d.name.to_ascii_lowercase().contains(&lower))
        .map(|d| d.index)
}

/// Encodes an ephemeral session command array into a shell-safe base64 JSON payload.
pub fn encode_session_cmd_b64(cmd: &[String]) -> String {
    let json = serde_json::to_vec(cmd).expect("serializing session command slice");
    BASE64_STD.encode(json)
}

/// Decodes a base64 JSON array produced by [`encode_session_cmd_b64`].
pub fn decode_session_cmd_b64(encoded: &str) -> Result<Vec<String>> {
    let raw = BASE64_STD
        .decode(encoded.trim())
        .context("decoding base64 --session-cmd-b64 payload")?;
    let cmd: Vec<String> =
        serde_json::from_slice(&raw).context("parsing JSON array in --session-cmd-b64 payload")?;
    if cmd.is_empty() {
        bail!("--session-cmd-b64 command must not be empty");
    }
    Ok(cmd)
}

/// Parses a line from the SSH host's stdout.
/// Returns `None` if the line does not carry the `USHER_HELLO ` handshake prefix
/// (allowing unrelated login MOTD / shell noise before the handshake).
pub fn parse_hello_line(line: &str) -> Option<Result<HelloMessage>> {
    let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
    let json_part = trimmed.strip_prefix(HELLO_PREFIX)?;
    Some(parse_and_validate_hello(json_part))
}

fn parse_and_validate_hello(json_part: &str) -> Result<HelloMessage> {
    let msg: HelloMessage =
        serde_json::from_str(json_part).context("malformed USHER_HELLO JSON")?;
    if msg.version != PROTOCOL_VERSION {
        bail!(
            "unsupported protocol version {} (expected {})",
            msg.version,
            PROTOCOL_VERSION
        );
    }
    if msg.port == 0 {
        bail!("invalid UDP port 0 in handshake");
    }
    if msg.mtu < 512 {
        bail!("MTU {} is too small (minimum 512)", msg.mtu);
    }
    if msg.width == 0 || msg.height == 0 || msg.fps == 0 {
        bail!("invalid video geometry {}x{}@{}fps", msg.width, msg.height, msg.fps);
    }
    let _ = msg.decode_session_key()?;
    Ok(msg)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshConnection {
    pub client_ip: IpAddr,
    pub client_port: u16,
    pub server_ip: IpAddr,
    pub server_port: u16,
}

/// Parses OpenSSH's `SSH_CONNECTION` environment variable:
/// `<client_ip> <client_port> <server_ip> <server_port>`.
pub fn parse_ssh_connection(val: &str) -> Result<SshConnection> {
    let parts: Vec<&str> = val.split_whitespace().collect();
    if parts.len() != 4 {
        bail!(
            "invalid SSH_CONNECTION format: expected 4 whitespace-separated fields, got {}",
            parts.len()
        );
    }
    let client_ip = parse_ip_stripping_zone(parts[0])
        .with_context(|| format!("invalid SSH_CONNECTION client IP '{}'", parts[0]))?;
    let client_port = parts[1]
        .parse::<u16>()
        .with_context(|| format!("invalid SSH_CONNECTION client port '{}'", parts[1]))?;
    let server_ip = parse_ip_stripping_zone(parts[2])
        .with_context(|| format!("invalid SSH_CONNECTION server IP '{}'", parts[2]))?;
    let server_port = parts[3]
        .parse::<u16>()
        .with_context(|| format!("invalid SSH_CONNECTION server port '{}'", parts[3]))?;

    Ok(SshConnection {
        client_ip,
        client_port,
        server_ip,
        server_port,
    })
}

fn parse_ip_stripping_zone(raw: &str) -> Result<IpAddr> {
    let without_zone = raw.split('%').next().unwrap_or(raw);
    IpAddr::from_str(without_zone).map_err(Into::into)
}

/// Returns true for private mesh overlay addresses (Tailscale 100.64.0.0/10,
/// ZeroTier/WireGuard RFC1918, IPv6 ULA fc00::/7), link-local, and loopback.
/// Rejects public WAN addresses and unspecified `0.0.0.0` / `::`.
pub fn is_overlay_or_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_overlay_or_private_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_overlay_or_private_v4(mapped);
            }
            is_overlay_or_private_v6(v6)
        }
    }
}

fn is_overlay_or_private_v4(v4: Ipv4Addr) -> bool {
    if v4.is_unspecified() {
        return false;
    }
    if v4.is_loopback() || v4.is_private() || v4.is_link_local() {
        return true;
    }
    let octets = v4.octets();
    // Tailscale / CGNAT RFC 6598: 100.64.0.0/10
    octets[0] == 100 && (octets[1] & 0b1100_0000) == 64
}

fn is_overlay_or_private_v6(v6: Ipv6Addr) -> bool {
    if v6.is_unspecified() {
        return false;
    }
    if v6.is_loopback() {
        return true;
    }
    let seg0 = v6.segments()[0];
    // Unique Local Addresses fc00::/7 (Tailscale IPv6 uses fd7a:115c:a1e0::/48)
    let is_ula = (seg0 & 0xfe00) == 0xfc00;
    // Link-local unicast fe80::/10
    let is_link_local = (seg0 & 0xffc0) == 0xfe80;
    is_ula || is_link_local
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrip_ignores_noise() {
        let key = [42u8; 32];
        let msg = HelloMessage {
            version: PROTOCOL_VERSION,
            bind_ip: "100.88.12.34".parse().unwrap(),
            port: 51842,
            session_key: HelloMessage::encode_session_key(&key),
            width: 1920,
            height: 1080,
            fps: 120,
            codec: VideoCodec::H264,
            audio: true,
            audio_sample_rate: 48_000,
            audio_channels: 2,
            mtu: 1200,
            max_bitrate_kbps: 60_000,
            bitrate_kbps: 25_000,
            fec_percent: 10,
            display_index: 0,
            displays: vec![DisplayInfo {
                index: 0,
                name: "Primary".into(),
                width: 1920,
                height: 1080,
            }],
            prefer: QualityPreference::Latency,
            supported_codecs: vec![VideoCodec::Hevc, VideoCodec::H264],
            host_version: "0.4.0".into(),
            video_backend: "Win(NativeDxgiMf)".into(),
            gamepad_backend: "ViGEmBus".into(),
            hi_res_scroll: true,
            capture_diagnostics: vec!["selected: dxgi + mf".into()],
            tcp_tunnel: true,
            chroma: ChromaSubsampling::Yuv444,
            cursor_plane: true,
            mic_enabled: true,
        };
        let encoded = encode_hello_line(&msg).unwrap();
        assert!(parse_hello_line("Welcome to Ubuntu 24.04 LTS").is_none());
        assert!(parse_hello_line("").is_none());

        let parsed = parse_hello_line(&format!("{encoded}\r\n"))
            .expect("should detect prefix")
            .expect("should parse valid hello");
        assert_eq!(parsed, msg);
        assert_eq!(parsed.decode_session_key().unwrap(), key);

        // Legacy v1 payload without extended fields must still deserialize via serde defaults.
        let legacy = format!(
            "{{\"version\":1,\"bind_ip\":\"100.88.12.34\",\"port\":51842,\"session_key\":\"{}\",\"width\":1280,\"height\":720,\"fps\":60,\"codec\":\"h264\",\"audio\":true,\"audio_sample_rate\":48000,\"audio_channels\":2,\"mtu\":1200}}",
            HelloMessage::encode_session_key(&key)
        );
        let parsed_legacy = parse_hello_line(&format!("{HELLO_PREFIX}{legacy}"))
            .expect("prefix")
            .expect("valid legacy hello");
        assert_eq!(parsed_legacy.max_bitrate_kbps, 0);
        assert_eq!(parsed_legacy.prefer, QualityPreference::Latency);
        assert!(!parsed_legacy.hi_res_scroll);
        assert!(parsed_legacy.capture_diagnostics.is_empty());
        assert!(!parsed_legacy.tcp_tunnel);
        assert_eq!(parsed_legacy.chroma, ChromaSubsampling::Yuv420);
        assert!(!parsed_legacy.cursor_plane);
        assert!(!parsed_legacy.mic_enabled);
    }

    #[test]
    fn displays_line_and_selector_work() {
        let displays = vec![
            DisplayInfo {
                index: 0,
                name: "DISPLAY1 (Primary)".into(),
                width: 1920,
                height: 1080,
            },
            DisplayInfo {
                index: 1,
                name: "DP-2".into(),
                width: 2560,
                height: 1440,
            },
        ];
        let line = encode_displays_line(&displays).unwrap();
        let decoded = parse_displays_line(&format!("{line}\n"))
            .expect("prefix")
            .expect("valid displays json");
        assert_eq!(decoded, displays);
        assert!(parse_displays_line("unrelated line").is_none());

        assert_eq!(resolve_display_selector(&displays, "0"), Some(0));
        assert_eq!(resolve_display_selector(&displays, "1"), Some(1));
        assert_eq!(resolve_display_selector(&displays, "2"), None);
        assert_eq!(resolve_display_selector(&displays, "dp-2"), Some(1));
        assert_eq!(resolve_display_selector(&displays, "primary"), Some(0));
        assert_eq!(resolve_display_selector(&displays, ""), None);
        assert_eq!(resolve_display_selector(&[], "3"), Some(3));
        assert_eq!(
            ChromaSubsampling::from_arg("4:4:4").unwrap(),
            ChromaSubsampling::Yuv444
        );
        assert_eq!(
            ChromaSubsampling::from_arg("420").unwrap(),
            ChromaSubsampling::Yuv420
        );
    }

    #[test]
    fn session_cmd_b64_roundtrip() {
        let cmd = vec![
            "firefox".to_string(),
            "--new-window".to_string(),
            "https://example.com/?a=1&b=two words".to_string(),
        ];
        let encoded = encode_session_cmd_b64(&cmd);
        assert!(!encoded.contains(' '));
        assert!(!encoded.contains('&'));
        assert_eq!(decode_session_cmd_b64(&encoded).unwrap(), cmd);
        let empty_b64 = BASE64_STD.encode(b"[]");
        assert!(decode_session_cmd_b64(&empty_b64).is_err());
    }

    #[test]
    fn parse_ssh_connection_v4_and_v6() {
        let v4 = parse_ssh_connection("100.64.0.10 54321 100.64.0.20 22").unwrap();
        assert_eq!(v4.client_ip, "100.64.0.10".parse::<IpAddr>().unwrap());
        assert_eq!(v4.client_port, 54321);
        assert_eq!(v4.server_ip, "100.64.0.20".parse::<IpAddr>().unwrap());
        assert_eq!(v4.server_port, 22);

        let v6 = parse_ssh_connection("fd7a:115c:a1e0::1 40000 fd7a:115c:a1e0::2 22").unwrap();
        assert_eq!(
            v6.server_ip,
            "fd7a:115c:a1e0::2".parse::<IpAddr>().unwrap()
        );
        assert!(parse_ssh_connection("bad").is_err());
    }

    #[test]
    fn overlay_and_private_ip_policy() {
        // Tailscale CGNAT 100.64.0.0/10
        assert!(is_overlay_or_private_ip("100.64.0.1".parse().unwrap()));
        assert!(is_overlay_or_private_ip("100.127.255.254".parse().unwrap()));
        assert!(!is_overlay_or_private_ip("100.63.255.254".parse().unwrap()));
        assert!(!is_overlay_or_private_ip("100.128.0.1".parse().unwrap()));

        // RFC1918, loopback, link-local, Tailscale IPv6 ULA
        assert!(is_overlay_or_private_ip("10.147.20.5".parse().unwrap()));
        assert!(is_overlay_or_private_ip("172.16.0.5".parse().unwrap()));
        assert!(is_overlay_or_private_ip("192.168.1.50".parse().unwrap()));
        assert!(is_overlay_or_private_ip("127.0.0.1".parse().unwrap()));
        assert!(is_overlay_or_private_ip("::1".parse().unwrap()));
        assert!(is_overlay_or_private_ip("fd7a:115c:a1e0::1234".parse().unwrap()));

        // Unspecified and public WAN IPs rejected
        assert!(!is_overlay_or_private_ip("0.0.0.0".parse().unwrap()));
        assert!(!is_overlay_or_private_ip("::".parse().unwrap()));
        assert!(!is_overlay_or_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_overlay_or_private_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_overlay_or_private_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn fit_preserving_aspect_handles_mismatched_monitors() {
        assert_eq!(fit_preserving_aspect(1920, 1080, 1920, 1200), (1920, 1080));
        assert_eq!(fit_preserving_aspect(3840, 2160, 1920, 1200), (1920, 1080));
        assert_eq!(fit_preserving_aspect(2560, 1600, 1920, 1080), (1728, 1080));
        assert_eq!(fit_preserving_aspect(3440, 1440, 1920, 1080), (1920, 804));
        assert_eq!(fit_preserving_aspect(1920, 1080, 1280, 720), (1280, 720));
    }
}
