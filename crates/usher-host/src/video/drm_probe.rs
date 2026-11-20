use std::path::PathBuf;
use usher_proto::handshake::DisplayInfo;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Amd,
    Intel,
    Nvidia,
    Unknown,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Default)]
pub struct DrmProbe {
    pub render_node: Option<PathBuf>,
    pub card_node: Option<PathBuf>,
    pub vendor: Option<GpuVendor>,
    pub active_mode: Option<(u16, u16)>,
    pub displays: Vec<DisplayInfo>,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_vendor_hex(raw: &str) -> GpuVendor {
    match raw.trim().to_ascii_lowercase().as_str() {
        "0x1002" => GpuVendor::Amd,
        "0x8086" => GpuVendor::Intel,
        "0x10de" => GpuVendor::Nvidia,
        _ => GpuVendor::Unknown,
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_drm_mode(raw: &str) -> Option<(u16, u16)> {
    let first = raw.lines().next()?.trim();
    let (w_str, h_part) = first.split_once('x')?;
    let h_str: String = h_part.chars().take_while(|c| c.is_ascii_digit()).collect();
    let w = w_str.parse::<u16>().ok()?;
    let h = h_str.parse::<u16>().ok()?;
    if w >= 64 && h >= 64 {
        Some((w & !1, h & !1))
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
pub fn probe_drm() -> DrmProbe {
    use std::fs;
    use std::path::Path;

    let mut probe = DrmProbe::default();

    let Ok(dri_entries) = fs::read_dir("/dev/dri") else {
        return probe;
    };

    let mut cards: Vec<PathBuf> = Vec::new();
    let mut renders: Vec<PathBuf> = Vec::new();
    for entry in dri_entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("renderD") {
            renders.push(entry.path());
        } else if name_str.starts_with("card") {
            cards.push(entry.path());
        }
    }
    cards.sort();
    renders.sort();
    probe.card_node = cards.into_iter().next();
    probe.render_node = renders.into_iter().next();

    if let Ok(sys_entries) = fs::read_dir("/sys/class/drm") {
        let mut names: Vec<String> = sys_entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();

        for name in &names {
            if !name.contains('-') && name.starts_with("card") {
                let vendor_path = Path::new("/sys/class/drm").join(name).join("device/vendor");
                if let Ok(raw) = fs::read_to_string(vendor_path) {
                    let v = parse_vendor_hex(&raw);
                    if v != GpuVendor::Unknown {
                        probe.vendor = Some(v);
                        break;
                    }
                }
            }
        }

        for name in &names {
            if name.contains('-') && name.starts_with("card") {
                let base = Path::new("/sys/class/drm").join(name);
                let status = fs::read_to_string(base.join("status")).unwrap_or_default();
                if status.trim() == "connected"
                    && let Ok(modes) = fs::read_to_string(base.join("modes"))
                    && let Some(mode) = parse_drm_mode(&modes)
                {
                    if probe.active_mode.is_none() {
                        probe.active_mode = Some(mode);
                    }
                    if let Ok(idx) = u8::try_from(probe.displays.len()) {
                        probe.displays.push(DisplayInfo {
                            index: idx,
                            name: name.clone(),
                            width: mode.0,
                            height: mode.1,
                        });
                    }
                }
            }
        }
    }

    probe
}

#[cfg(not(target_os = "linux"))]
pub fn probe_drm() -> DrmProbe {
    DrmProbe::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vendor_and_drm_modes() {
        assert_eq!(parse_vendor_hex("0x1002\n"), GpuVendor::Amd);
        assert_eq!(parse_vendor_hex("0x8086"), GpuVendor::Intel);
        assert_eq!(parse_vendor_hex("0x10de"), GpuVendor::Nvidia);
        assert_eq!(parse_vendor_hex("0x9999"), GpuVendor::Unknown);

        assert_eq!(parse_drm_mode("2560x1440\n1920x1080\n"), Some((2560, 1440)));
        assert_eq!(parse_drm_mode("1920x1080i"), Some((1920, 1080)));
        assert_eq!(parse_drm_mode(""), None);
    }
}
