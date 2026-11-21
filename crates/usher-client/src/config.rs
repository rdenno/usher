use crate::decoder::DecoderKind;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use usher_proto::adapt::DEFAULT_MAX_BITRATE_KBPS;
use usher_proto::handshake::{CodecPreference, QualityPreference};
use usher_proto::packet::DEFAULT_MTU;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResolutionMode {
    #[default]
    Auto,
    Native,
    MatchWindow,
    Fixed(u16, u16),
}

impl fmt::Display for ResolutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Auto => write!(f, "auto"),
            Self::Native => write!(f, "native"),
            Self::MatchWindow => write!(f, "match-window"),
            Self::Fixed(w, h) => write!(f, "{w}x{h}"),
        }
    }
}

impl FromStr for ResolutionMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let clean = s.trim().to_ascii_lowercase();
        match clean.as_str() {
            "auto" => Ok(Self::Auto),
            "native" => Ok(Self::Native),
            "match-window" | "match_window" | "window" => Ok(Self::MatchWindow),
            other => {
                let Some((w_str, h_str)) = other.split_once('x') else {
                    bail!("invalid resolution '{s}' (expected auto, native, match-window, or WxH)");
                };
                let w: u16 = w_str
                    .trim()
                    .parse()
                    .with_context(|| format!("invalid width in '{s}'"))?;
                let h: u16 = h_str
                    .trim()
                    .parse()
                    .with_context(|| format!("invalid height in '{s}'"))?;
                if w < 64 || h < 64 {
                    bail!("resolution {w}x{h} too small (minimum 64x64)");
                }
                Ok(Self::Fixed(w & !1, h & !1))
            }
        }
    }
}

impl Serialize for ResolutionMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ResolutionMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        ResolutionMode::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FpsSetting {
    #[default]
    Auto,
    Fixed(u16),
}

impl fmt::Display for FpsSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Auto => write!(f, "auto"),
            Self::Fixed(fps) => write!(f, "{fps}"),
        }
    }
}

impl FromStr for FpsSetting {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let clean = s.trim().to_ascii_lowercase();
        if clean == "auto" {
            return Ok(Self::Auto);
        }
        let val: u16 = clean
            .parse()
            .with_context(|| format!("invalid fps '{s}' (expected auto or 1..=240)"))?;
        if val == 0 || val > 240 {
            bail!("fps {val} out of range (1..=240)");
        }
        Ok(Self::Fixed(val))
    }
}

impl Serialize for FpsSetting {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match *self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Fixed(v) => serializer.serialize_u16(v),
        }
    }
}

impl<'de> Deserialize<'de> for FpsSetting {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct FpsVisitor;
        impl<'de> serde::de::Visitor<'de> for FpsVisitor {
            type Value = FpsSetting;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("'auto' or an integer frame rate between 1 and 240")
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                if (1..=240).contains(&v) {
                    Ok(FpsSetting::Fixed(v as u16))
                } else {
                    Err(E::custom(format!("fps {v} out of range (1..=240)")))
                }
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                if (1..=240).contains(&v) {
                    Ok(FpsSetting::Fixed(v as u16))
                } else {
                    Err(E::custom(format!("fps {v} out of range (1..=240)")))
                }
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                FpsSetting::from_str(v).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(FpsVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PointerMode {
    #[default]
    Absolute,
    Relative,
}

impl fmt::Display for PointerMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Absolute => write!(f, "absolute"),
            Self::Relative => write!(f, "relative"),
        }
    }
}

impl FromStr for PointerMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "abs" | "absolute" => Ok(Self::Absolute),
            "rel" | "relative" => Ok(Self::Relative),
            other => bail!("unsupported pointer_mode '{other}' (expected absolute or relative)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum TransportPreference {
    #[default]
    Auto,
    Udp,
    Tcp,
}

impl fmt::Display for TransportPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Auto => write!(f, "auto"),
            Self::Udp => write!(f, "udp"),
            Self::Tcp => write!(f, "tcp"),
        }
    }
}

impl FromStr for TransportPreference {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "udp" => Ok(Self::Udp),
            "tcp" => Ok(Self::Tcp),
            other => bail!("unsupported transport '{other}' (expected auto, udp, or tcp)"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DisplaySelector {
    Index(u8),
    Name(String),
}

impl Default for DisplaySelector {
    fn default() -> Self {
        Self::Index(0)
    }
}

impl fmt::Display for DisplaySelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index(i) => write!(f, "{i}"),
            Self::Name(name) => f.write_str(name),
        }
    }
}

impl FromStr for DisplaySelector {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            bail!("display selector cannot be empty");
        }
        if let Ok(idx) = trimmed.parse::<u8>() {
            Ok(Self::Index(idx))
        } else {
            Ok(Self::Name(trimmed.to_string()))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChromaPreference {
    #[default]
    Auto,
    #[serde(rename = "420", alias = "yuv420", alias = "4:2:0")]
    Yuv420,
    #[serde(rename = "444", alias = "yuv444", alias = "4:4:4")]
    Yuv444,
}

impl fmt::Display for ChromaPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Auto => f.write_str("auto"),
            Self::Yuv420 => f.write_str("420"),
            Self::Yuv444 => f.write_str("444"),
        }
    }
}

impl FromStr for ChromaPreference {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "420" | "4:2:0" | "yuv420" => Ok(Self::Yuv420),
            "444" | "4:4:4" | "yuv444" => Ok(Self::Yuv444),
            other => bail!("unsupported chroma '{other}' (expected auto, 420, or 444)"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostSettings {
    pub max_bitrate_kbps: Option<u32>,
    pub resolution: Option<ResolutionMode>,
    pub fps: Option<FpsSetting>,
    pub codec: Option<CodecPreference>,
    pub prefer: Option<QualityPreference>,
    pub chroma: Option<ChromaPreference>,
    pub audio: Option<bool>,
    pub mic: Option<bool>,
    pub clipboard: Option<bool>,
    pub display: Option<DisplaySelector>,
    pub pointer_mode: Option<PointerMode>,
    pub stats_overlay: Option<bool>,
    pub fullscreen: Option<bool>,
    pub capture: Option<String>,
    pub encoder: Option<String>,
    pub decoder: Option<DecoderKind>,
    pub headless: Option<String>,
    pub mtu: Option<u16>,
    pub fec: Option<u8>,
    pub remote_bin: Option<String>,
    pub bind_ip: Option<IpAddr>,
    pub ssh_opts: Option<Vec<String>>,
    pub no_adaptive: Option<bool>,
    pub no_system: Option<bool>,
    pub batch_mode: Option<bool>,
    pub transport: Option<TransportPreference>,
    pub auto_install: Option<bool>,
}

impl HostSettings {
    pub fn merge_from(&mut self, other: &HostSettings) {
        macro_rules! take_some {
            ($field:ident) => {
                if let Some(ref v) = other.$field {
                    self.$field = Some(v.clone());
                }
            };
        }
        take_some!(max_bitrate_kbps);
        take_some!(resolution);
        take_some!(fps);
        take_some!(codec);
        take_some!(prefer);
        take_some!(chroma);
        take_some!(audio);
        take_some!(mic);
        take_some!(display);
        take_some!(pointer_mode);
        take_some!(stats_overlay);
        take_some!(fullscreen);
        take_some!(clipboard);
        take_some!(capture);
        take_some!(encoder);
        take_some!(decoder);
        take_some!(headless);
        take_some!(mtu);
        take_some!(fec);
        take_some!(remote_bin);
        take_some!(bind_ip);
        take_some!(ssh_opts);
        take_some!(no_adaptive);
        take_some!(no_system);
        take_some!(batch_mode);
        take_some!(transport);
        take_some!(auto_install);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    #[serde(flatten)]
    pub defaults: HostSettings,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host: BTreeMap<String, HostSettings>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    pub max_bitrate_kbps: u32,
    pub explicit_bitrate_kbps: Option<u32>,
    pub resolution: ResolutionMode,
    pub fps: FpsSetting,
    pub codec: CodecPreference,
    pub prefer: QualityPreference,
    pub chroma: ChromaPreference,
    pub audio: bool,
    pub mic: bool,
    pub display: DisplaySelector,
    pub pointer_mode: PointerMode,
    pub stats_overlay: bool,
    pub fullscreen: bool,
    pub clipboard: bool,
    pub capture: String,
    pub encoder: String,
    pub decoder: DecoderKind,
    pub headless: String,
    pub mtu: u16,
    pub fec: u8,
    pub remote_bin: String,
    pub bind_ip: Option<IpAddr>,
    pub ssh_opts: Vec<String>,
    pub no_adaptive: bool,
    pub no_system: bool,
    pub batch_mode: bool,
    pub transport: TransportPreference,
    pub auto_install: Option<bool>,
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self {
            max_bitrate_kbps: DEFAULT_MAX_BITRATE_KBPS,
            explicit_bitrate_kbps: None,
            resolution: ResolutionMode::Auto,
            fps: FpsSetting::Auto,
            codec: CodecPreference::Auto,
            prefer: QualityPreference::Latency,
            chroma: ChromaPreference::Auto,
            audio: true,
            mic: false,
            clipboard: true,
            display: DisplaySelector::Index(0),
            pointer_mode: PointerMode::Absolute,
            stats_overlay: false,
            fullscreen: true,
            capture: "auto".to_string(),
            encoder: "auto".to_string(),
            decoder: DecoderKind::Auto,
            headless: "none".to_string(),
            mtu: DEFAULT_MTU as u16,
            fec: 10,
            remote_bin: "usher-host".to_string(),
            bind_ip: None,
            ssh_opts: Vec::new(),
            no_adaptive: false,
            no_system: false,
            batch_mode: false,
            transport: TransportPreference::Auto,
            auto_install: None,
        }
    }
}

impl EffectiveConfig {
    pub fn resolve(
        file: &ConfigFile,
        host_alias: Option<&str>,
        cli: &HostSettings,
        explicit_bitrate_kbps: Option<u32>,
    ) -> Self {
        let mut merged = file.defaults.clone();
        if let Some(alias) = host_alias
            && let Some(per_host) = file.host.get(alias)
        {
            merged.merge_from(per_host);
        }
        merged.merge_from(cli);

        let def = Self::default();
        Self {
            max_bitrate_kbps: merged.max_bitrate_kbps.unwrap_or(def.max_bitrate_kbps),
            explicit_bitrate_kbps,
            resolution: merged.resolution.unwrap_or(def.resolution),
            fps: merged.fps.unwrap_or(def.fps),
            codec: merged.codec.unwrap_or(def.codec),
            prefer: merged.prefer.unwrap_or(def.prefer),
            chroma: merged.chroma.unwrap_or(def.chroma),
            audio: merged.audio.unwrap_or(def.audio),
            mic: merged.mic.unwrap_or(def.mic),
            clipboard: merged.clipboard.unwrap_or(def.clipboard),
            display: merged.display.unwrap_or(def.display),
            pointer_mode: merged.pointer_mode.unwrap_or(def.pointer_mode),
            stats_overlay: merged.stats_overlay.unwrap_or(def.stats_overlay),
            fullscreen: merged.fullscreen.unwrap_or(def.fullscreen),
            capture: merged.capture.unwrap_or(def.capture),
            encoder: merged.encoder.unwrap_or(def.encoder),
            decoder: merged.decoder.unwrap_or(def.decoder),
            headless: merged.headless.unwrap_or(def.headless),
            mtu: merged.mtu.unwrap_or(def.mtu),
            fec: merged.fec.unwrap_or(def.fec),
            remote_bin: merged.remote_bin.unwrap_or(def.remote_bin),
            bind_ip: merged.bind_ip,
            ssh_opts: merged.ssh_opts.unwrap_or(def.ssh_opts),
            no_adaptive: merged.no_adaptive.unwrap_or(def.no_adaptive),
            no_system: merged.no_system.unwrap_or(def.no_system),
            batch_mode: merged.batch_mode.unwrap_or(def.batch_mode),
            transport: merged.transport.unwrap_or(def.transport),
            auto_install: merged.auto_install,
        }
    }
}

pub fn default_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("USHER_CONFIG")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA")
            && !appdata.is_empty()
        {
            return PathBuf::from(appdata).join("usher").join("config.toml");
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(profile)
                .join(".config")
                .join("usher")
                .join("config.toml");
        }
        PathBuf::from("usher-config.toml")
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return PathBuf::from(xdg).join("usher").join("config.toml");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".config")
                .join("usher")
                .join("config.toml");
        }
        PathBuf::from("usher-config.toml")
    }
}

pub fn load_config(path: &Path) -> Result<ConfigFile> {
    if !path.exists() {
        return Ok(ConfigFile::default());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing TOML config {}", path.display()))
}

pub fn save_host_settings(path: &Path, host_alias: &str, entry: &HostSettings) -> Result<()> {
    let mut cfg = load_config(path)?;
    let slot = cfg.host.entry(host_alias.to_string()).or_default();
    slot.merge_from(entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let rendered = toml::to_string_pretty(&cfg).context("serializing config.toml")?;
    std::fs::write(path, rendered).with_context(|| format!("writing config {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_defaults_global_host_and_cli_precedence() {
        let raw = r#"
max_bitrate_kbps = 45000
fps = 90
prefer = "quality"

[host.desktop]
max_bitrate_kbps = 80000
resolution = "2560x1440"
fps = 120
pointer_mode = "relative"
audio = false
display = 1
"#;
        let cfg: ConfigFile = toml::from_str(raw).unwrap();
        let cli = HostSettings {
            fps: Some(FpsSetting::Fixed(60)),
            ..Default::default()
        };
        let eff = EffectiveConfig::resolve(&cfg, Some("desktop"), &cli, None);
        assert_eq!(eff.max_bitrate_kbps, 80_000);
        assert_eq!(eff.resolution, ResolutionMode::Fixed(2560, 1440));
        assert_eq!(eff.fps, FpsSetting::Fixed(60)); // CLI beats [host.desktop]
        assert_eq!(eff.prefer, QualityPreference::Quality); // inherited from global
        assert_eq!(eff.pointer_mode, PointerMode::Relative);
        assert!(!eff.audio);
        assert_eq!(eff.display, DisplaySelector::Index(1));
        assert!(eff.fullscreen);

        let cli_windowed = HostSettings {
            fullscreen: Some(false),
            ..Default::default()
        };
        let eff_win = EffectiveConfig::resolve(&cfg, Some("desktop"), &cli_windowed, None);
        assert!(!eff_win.fullscreen);

        let eff_other = EffectiveConfig::resolve(&cfg, Some("laptop"), &HostSettings::default(), None);
        assert_eq!(eff_other.max_bitrate_kbps, 45_000);
        assert_eq!(eff_other.fps, FpsSetting::Fixed(90));
        assert_eq!(eff_other.resolution, ResolutionMode::Auto);
    }

    #[test]
    fn save_host_settings_preserves_globals_and_other_hosts() {
        let tmp = std::env::temp_dir().join(format!("usher-cfg-test-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        let initial = r#"
max_bitrate_kbps = 50000
stats_overlay = true

[host.server1]
codec = "hevc"
"#;
        std::fs::write(&tmp, initial).unwrap();

        let update = HostSettings {
            max_bitrate_kbps: Some(75_000),
            resolution: Some(ResolutionMode::MatchWindow),
            fps: Some(FpsSetting::Auto),
            codec: Some(CodecPreference::Av1),
            prefer: Some(QualityPreference::Latency),
            audio: Some(true),
            display: Some(DisplaySelector::Index(2)),
            pointer_mode: Some(PointerMode::Relative),
            stats_overlay: Some(false),
            ..Default::default()
        };
        save_host_settings(&tmp, "gaming-pc", &update).unwrap();

        let reloaded = load_config(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(reloaded.defaults.max_bitrate_kbps, Some(50_000));
        assert_eq!(reloaded.defaults.stats_overlay, Some(true));
        assert_eq!(
            reloaded.host.get("server1").and_then(|h| h.codec),
            Some(CodecPreference::Hevc)
        );
        let g = reloaded.host.get("gaming-pc").unwrap();
        assert_eq!(g.max_bitrate_kbps, Some(75_000));
        assert_eq!(g.resolution, Some(ResolutionMode::MatchWindow));
        assert_eq!(g.codec, Some(CodecPreference::Av1));
        assert_eq!(g.pointer_mode, Some(PointerMode::Relative));
    }

    #[test]
    fn parses_display_selector_chroma_and_mic_from_toml() {
        let raw = r#"
chroma = "444"
mic = true

[host.workstation]
display = "HDMI-1"

[host.tv]
display = 2
chroma = "420"
mic = false
"#;
        let cfg: ConfigFile = toml::from_str(raw).unwrap();
        let eff_ws = EffectiveConfig::resolve(&cfg, Some("workstation"), &HostSettings::default(), None);
        assert_eq!(eff_ws.display, DisplaySelector::Name("HDMI-1".into()));
        assert_eq!(eff_ws.chroma, ChromaPreference::Yuv444);
        assert!(eff_ws.mic);

        let eff_tv = EffectiveConfig::resolve(&cfg, Some("tv"), &HostSettings::default(), None);
        assert_eq!(eff_tv.display, DisplaySelector::Index(2));
        assert_eq!(eff_tv.chroma, ChromaPreference::Yuv420);
        assert!(!eff_tv.mic);
    }
}
