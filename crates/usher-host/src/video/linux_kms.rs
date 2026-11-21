use super::color::{CpuFrame, PixelFormat};
use super::drm_probe::DrmProbe;
use anyhow::{Context, Result, anyhow, bail};
use drm::{ClientCapability, Device as _};
use drm::control::{Device as ControlDevice, crtc, framebuffer, plane};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

const CAP_SYS_ADMIN_BIT: u32 = 21;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_START: u64 = 0;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

// DRM fourcc codes (little-endian ASCII)
const FOURCC_XR24: u32 = u32::from_le_bytes(*b"XR24");
const FOURCC_AR24: u32 = u32::from_le_bytes(*b"AR24");
const FOURCC_XB24: u32 = u32::from_le_bytes(*b"XB24");
const FOURCC_AB24: u32 = u32::from_le_bytes(*b"AB24");
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

#[repr(C)]
struct CapUserHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CapUserData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

pub fn parse_cap_eff_has_sys_admin(proc_status: &str) -> bool {
    for line in proc_status.lines() {
        if let Some(rest) = line.strip_prefix("CapEff:") {
            let hex = rest.trim();
            if let Ok(val) = u64::from_str_radix(hex, 16) {
                return (val & (1u64 << CAP_SYS_ADMIN_BIT)) != 0;
            }
        }
    }
    false
}

pub fn has_cap_sys_admin() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .map(|s| parse_cap_eff_has_sys_admin(&s))
        .unwrap_or(false)
}

/// Drops `CAP_SYS_ADMIN` from effective/permitted/inheritable sets and locks `PR_SET_NO_NEW_PRIVS`.
pub fn drop_cap_sys_admin() -> Result<()> {
    let had_cap = has_cap_sys_admin();
    let mut hdr = CapUserHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapUserData::default(); 2];
    let rc = unsafe { libc::syscall(libc::SYS_capget, &raw mut hdr, data.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("SYS_capget failed");
    }

    let mask = !(1u32 << CAP_SYS_ADMIN_BIT);
    let needs_drop = (data[0].effective | data[0].permitted | data[0].inheritable)
        & (1u32 << CAP_SYS_ADMIN_BIT)
        != 0;
    if needs_drop {
        data[0].effective &= mask;
        data[0].permitted &= mask;
        data[0].inheritable &= mask;
        let rc_set = unsafe { libc::syscall(libc::SYS_capset, &raw mut hdr, data.as_mut_ptr()) };
        if rc_set != 0 {
            return Err(std::io::Error::last_os_error()).context("SYS_capset failed");
        }
    }

    unsafe {
        let _ = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }

    if had_cap {
        tracing::info!("dropped CAP_SYS_ADMIN after KMS framebuffer setup");
    }
    Ok(())
}

struct DrmCard(File);

impl DrmCard {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .with_context(|| format!("opening DRM device {}", path.display()))?;
        Ok(Self(file))
    }
}

impl AsFd for DrmCard {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for DrmCard {}
impl ControlDevice for DrmCard {}

pub struct ExportedPlane {
    pub fd: OwnedFd,
    pub pitch: u32,
    pub offset: u32,
}

pub struct DmaBufFrame {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<ExportedPlane>,
}

struct MmapGuard {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Drop for MmapGuard {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            unsafe {
                let _ = libc::munmap(self.ptr, self.len);
            }
        }
    }
}

fn dma_buf_sync(fd: &OwnedFd, start: bool) {
    let mut sync = DmaBufSync {
        flags: if start {
            DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ
        } else {
            DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ
        },
    };
    unsafe {
        let _ = libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC, &raw mut sync);
    }
}

impl DmaBufFrame {
    pub fn read_cpu_frame(&self) -> Result<CpuFrame> {
        if self.modifier != 0 && self.modifier != DRM_FORMAT_MOD_INVALID {
            bail!(
                "KMS DMA-BUF uses GPU modifier 0x{:016x} (non-linear tiled/compressed); use VA-API zero-copy encode or Portal/X11 capture",
                self.modifier
            );
        }
        let plane0 = self
            .planes
            .first()
            .ok_or_else(|| anyhow!("DMA-BUF frame has no planes"))?;

        let format = match self.fourcc {
            FOURCC_XR24 => PixelFormat::Bgrx,
            FOURCC_AR24 => PixelFormat::Bgra,
            FOURCC_XB24 => PixelFormat::Rgbx,
            FOURCC_AB24 => PixelFormat::Rgba,
            FOURCC_NV12 => PixelFormat::Nv12,
            other => {
                let b = other.to_le_bytes();
                let name = String::from_utf8_lossy(&b);
                bail!("unsupported KMS framebuffer pixel format '{name}' (0x{other:08x})");
            }
        };

        if format == PixelFormat::Nv12 {
            let p0_end = plane0.offset as usize + plane0.pitch as usize * self.height as usize;
            let len0 = fd_map_len(&plane0.fd, p0_end);
            dma_buf_sync(&plane0.fd, true);
            let ptr0 = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len0,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    plane0.fd.as_raw_fd(),
                    0,
                )
            };
            if ptr0 == libc::MAP_FAILED {
                dma_buf_sync(&plane0.fd, false);
                return Err(std::io::Error::last_os_error()).context("mmap DMA-BUF plane 0");
            }
            let _guard0 = MmapGuard {
                ptr: ptr0,
                len: len0,
            };

            let stride = plane0.pitch as usize;
            let h = self.height as usize;
            let mut out = vec![0u8; stride * (h + h / 2)];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    ptr0.cast::<u8>().add(plane0.offset as usize),
                    out.as_mut_ptr(),
                    stride * h,
                );
            }

            if let Some(plane1) = self.planes.get(1) {
                let uv_h = h / 2;
                let p1_end = plane1.offset as usize + plane1.pitch as usize * uv_h;
                let len1 = fd_map_len(&plane1.fd, p1_end);
                dma_buf_sync(&plane1.fd, true);
                let ptr1 = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        len1,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        plane1.fd.as_raw_fd(),
                        0,
                    )
                };
                if ptr1 == libc::MAP_FAILED {
                    dma_buf_sync(&plane1.fd, false);
                    dma_buf_sync(&plane0.fd, false);
                    return Err(std::io::Error::last_os_error()).context("mmap DMA-BUF plane 1");
                }
                let _guard1 = MmapGuard {
                    ptr: ptr1,
                    len: len1,
                };
                for cy in 0..uv_h {
                    let copy_w = (plane1.pitch as usize).min(stride);
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            ptr1.cast::<u8>()
                                .add(plane1.offset as usize + cy * plane1.pitch as usize),
                            out.as_mut_ptr().add((h + cy) * stride),
                            copy_w,
                        );
                    }
                }
                dma_buf_sync(&plane1.fd, false);
            } else {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        ptr0.cast::<u8>().add(plane0.offset as usize + stride * h),
                        out.as_mut_ptr().add(stride * h),
                        stride * (h / 2),
                    );
                }
            }
            dma_buf_sync(&plane0.fd, false);
            return Ok(CpuFrame {
                width: self.width,
                height: self.height,
                stride,
                format,
                data: out,
            });
        }

        let need = plane0.offset as usize + plane0.pitch as usize * self.height as usize;
        let map_len = fd_map_len(&plane0.fd, need);
        dma_buf_sync(&plane0.fd, true);
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                plane0.fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            dma_buf_sync(&plane0.fd, false);
            return Err(std::io::Error::last_os_error())
                .context("mmap KMS linear DMA-BUF failed");
        }
        let _guard = MmapGuard { ptr, len: map_len };
        let byte_len = plane0.pitch as usize * self.height as usize;
        let mut data = vec![0u8; byte_len];
        unsafe {
            std::ptr::copy_nonoverlapping(
                ptr.cast::<u8>().add(plane0.offset as usize),
                data.as_mut_ptr(),
                byte_len,
            );
        }
        dma_buf_sync(&plane0.fd, false);

        Ok(CpuFrame {
            width: self.width,
            height: self.height,
            stride: plane0.pitch as usize,
            format,
            data,
        })
    }
}

fn fd_map_len(fd: &OwnedFd, min_bytes: usize) -> usize {
    let off = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
    if off > 0 && (off as usize) >= min_bytes {
        off as usize
    } else {
        min_bytes
    }
}

fn is_primary_plane(card: &DrmCard, plane_handle: plane::Handle) -> bool {
    let Ok(props) = card.get_properties(plane_handle) else {
        return false;
    };
    for (prop_handle, raw_val) in &props {
        if let Ok(info) = card.get_property(*prop_handle)
            && info.name().to_str() == Ok("type")
            && let drm::control::property::Value::Enum(Some(ev)) =
                info.value_type().convert_value(*raw_val)
            && ev.name().to_str() == Ok("Primary")
        {
            return true;
        }
    }
    false
}

struct ActiveScanout {
    crtc: crtc::Handle,
    primary_plane: Option<plane::Handle>,
    size: (u16, u16),
    current_fb: framebuffer::Handle,
}

fn find_active_scanout(card: &DrmCard, display_index: u8) -> Result<ActiveScanout> {
    let _ = card.set_client_capability(ClientCapability::UniversalPlanes, true);
    let res = card
        .resource_handles()
        .context("querying DRM mode resources")?;
    let planes = card.plane_handles().unwrap_or_default();

    let mut active: Vec<ActiveScanout> = Vec::new();
    for &crtc_h in res.crtcs() {
        let Ok(cinfo) = card.get_crtc(crtc_h) else {
            continue;
        };
        let mut size = cinfo.mode().map(|m| m.size()).unwrap_or((0, 0));
        let mut fb = cinfo.framebuffer();
        let mut primary_plane = None;

        for &plane_h in &planes {
            let Ok(pinfo) = card.get_plane(plane_h) else {
                continue;
            };
            if pinfo.crtc() != Some(crtc_h) {
                continue;
            }
            if is_primary_plane(card, plane_h) {
                primary_plane = Some(plane_h);
                if fb.is_none() {
                    fb = pinfo.framebuffer();
                }
                break;
            }
            if primary_plane.is_none() && pinfo.framebuffer().is_some() {
                primary_plane = Some(plane_h);
                if fb.is_none() {
                    fb = pinfo.framebuffer();
                }
            }
        }

        if let Some(fb_h) = fb {
            if (size.0 == 0 || size.1 == 0)
                && let Ok(fb2) = card.get_planar_framebuffer(fb_h)
            {
                let (w, h) = fb2.size();
                size = (w as u16, h as u16);
            }
            active.push(ActiveScanout {
                crtc: crtc_h,
                primary_plane,
                size,
                current_fb: fb_h,
            });
        }
    }

    if active.is_empty() {
        bail!("no active CRTC with an attached KMS framebuffer found");
    }
    let idx = (display_index as usize).min(active.len() - 1);
    Ok(active.swap_remove(idx))
}

fn current_fb_id(
    card: &DrmCard,
    crtc_h: crtc::Handle,
    primary_plane: Option<plane::Handle>,
) -> Option<framebuffer::Handle> {
    if let Some(plane_h) = primary_plane
        && let Ok(pinfo) = card.get_plane(plane_h)
        && pinfo.framebuffer().is_some()
    {
        return pinfo.framebuffer();
    }
    card.get_crtc(crtc_h).ok().and_then(|c| c.framebuffer())
}

fn export_fb_dmabuf(card: &DrmCard, fb_h: framebuffer::Handle) -> Result<DmaBufFrame> {
    match card.get_planar_framebuffer(fb_h) {
        Ok(planar) => {
            let (width, height) = planar.size();
            let fourcc = planar.pixel_format() as u32;
            let modifier = planar
                .modifier()
                .map(u64::from)
                .unwrap_or(DRM_FORMAT_MOD_INVALID);
            let buffers = planar.buffers();
            let pitches = planar.pitches();
            let offsets = planar.offsets();

            let mut planes = Vec::new();
            let mut to_close = HashSet::new();
            for i in 0..4 {
                let Some(buf_h) = buffers[i] else {
                    continue;
                };
                to_close.insert(u32::from(buf_h));
                match card.buffer_to_prime_fd(buf_h, drm::CLOEXEC) {
                    Ok(fd) => {
                        planes.push(ExportedPlane {
                            fd,
                            pitch: pitches[i],
                            offset: offsets[i],
                        });
                    }
                    Err(err) => {
                        for raw_h in to_close {
                            if let Some(bh) = drm::control::from_u32(raw_h) {
                                let _ = card.close_buffer(bh);
                            }
                        }
                        return Err(err).context("DRM prime_handle_to_fd failed");
                    }
                }
            }
            for raw_h in to_close {
                if let Some(bh) = drm::control::from_u32(raw_h) {
                    let _ = card.close_buffer(bh);
                }
            }
            if planes.is_empty() {
                bail!(
                    "DRM_IOCTL_MODE_GETFB2 returned no GEM handles (CAP_SYS_ADMIN required on non-master fd)"
                );
            }
            Ok(DmaBufFrame {
                width,
                height,
                fourcc,
                modifier,
                planes,
            })
        }
        Err(_) => {
            let info = card
                .get_framebuffer(fb_h)
                .context("DRM_IOCTL_MODE_GETFB failed")?;
            let buf_h = info.buffer().ok_or_else(|| {
                anyhow!(
                    "DRM_IOCTL_MODE_GETFB returned null GEM handle (CAP_SYS_ADMIN required)"
                )
            })?;
            let fd_res = card.buffer_to_prime_fd(buf_h, drm::CLOEXEC);
            let _ = card.close_buffer(buf_h);
            let fd = fd_res.context("DRM prime_handle_to_fd failed")?;
            let fourcc = match (info.depth(), info.bpp()) {
                (32, 32) => FOURCC_AR24,
                _ => FOURCC_XR24,
            };
            let (width, height) = info.size();
            Ok(DmaBufFrame {
                width,
                height,
                fourcc,
                modifier: 0,
                planes: vec![ExportedPlane {
                    fd,
                    pitch: info.pitch(),
                    offset: 0,
                }],
            })
        }
    }
}

/// Read-only KMS readiness probe for `--doctor` and auto backend selection.
/// Never drops `CAP_SYS_ADMIN`.
pub fn probe_kms_status(probe: &DrmProbe) -> std::result::Result<String, String> {
    let Some(card_path) = probe.card_node.as_deref() else {
        return Err("no /dev/dri/card* DRM node found".into());
    };
    let card = DrmCard::open(card_path).map_err(|e| format!("cannot open {}: {e:#}", card_path.display()))?;
    let scanout = find_active_scanout(&card, 0)
        .map_err(|e| format!("{} has no active scanout framebuffer ({e:#})", card_path.display()))?;

    let priv_ok = has_cap_sys_admin() || unsafe { libc::geteuid() == 0 };
    if !priv_ok {
        return Err(format!(
            "{} active ({}x{}), but CAP_SYS_ADMIN is missing (run: sudo setcap cap_sys_admin+ep /usr/local/bin/usher-host)",
            card_path.display(),
            scanout.size.0,
            scanout.size.1
        ));
    }

    match card.get_planar_framebuffer(scanout.current_fb) {
        Ok(p) if p.buffers()[0].is_some() => Ok(format!(
            "{} ({}x{}, GETFB2+DMA-BUF ready)",
            card_path.display(),
            scanout.size.0,
            scanout.size.1
        )),
        Ok(_) => Err(format!(
            "{} GETFB2 returned null GEM handle (run: sudo setcap cap_sys_admin+ep /usr/local/bin/usher-host)",
            card_path.display()
        )),
        Err(_) => match card.get_framebuffer(scanout.current_fb) {
            Ok(fb) if fb.buffer().is_some() => Ok(format!(
                "{} ({}x{}, GETFB+DMA-BUF ready)",
                card_path.display(),
                scanout.size.0,
                scanout.size.1
            )),
            Ok(_) => Err(format!(
                "{} GETFB returned null GEM handle (missing CAP_SYS_ADMIN)",
                card_path.display()
            )),
            Err(e) => Err(format!("GETFB failed on {}: {e}", card_path.display())),
        },
    }
}

pub struct KmsCapturer {
    card: DrmCard,
    crtc: crtc::Handle,
    primary_plane: Option<plane::Handle>,
    cache: HashMap<u32, DmaBufFrame>,
    last_fb_id: u32,
}

impl KmsCapturer {
    /// Opens KMS capture for `display_index`, exports initial scanout DMA-BUF(s),
    /// and immediately drops `CAP_SYS_ADMIN`.
    pub fn open(card_path: &Path, display_index: u8) -> Result<Self> {
        let card = DrmCard::open(card_path)?;
        let scanout = find_active_scanout(&card, display_index)?;
        let mut cache = HashMap::new();
        let initial_fb_id = u32::from(scanout.current_fb);
        let initial = export_fb_dmabuf(&card, scanout.current_fb)?;
        tracing::info!(
            card = %card_path.display(),
            width = initial.width,
            height = initial.height,
            modifier = format_args!("0x{:x}", initial.modifier),
            planes = initial.planes.len(),
            "KMS scanout DMA-BUF exported"
        );
        cache.insert(initial_fb_id, initial);

        // Catch immediate page-flip backbuffers briefly before dropping CAP_SYS_ADMIN.
        for _ in 0..8 {
            std::thread::sleep(Duration::from_millis(5));
            if let Some(fb_h) = current_fb_id(&card, scanout.crtc, scanout.primary_plane) {
                let id = u32::from(fb_h);
                if !cache.contains_key(&id)
                    && let Ok(dmabuf) = export_fb_dmabuf(&card, fb_h)
                {
                    cache.insert(id, dmabuf);
                }
            }
        }

        if let Err(err) = drop_cap_sys_admin() {
            tracing::warn!("failed to drop CAP_SYS_ADMIN: {err:#}");
        }

        Ok(Self {
            card,
            crtc: scanout.crtc,
            primary_plane: scanout.primary_plane,
            cache,
            last_fb_id: initial_fb_id,
        })
    }

    pub fn capture_frame(&mut self) -> Result<&DmaBufFrame> {
        if let Some(fb_h) = current_fb_id(&self.card, self.crtc, self.primary_plane) {
            let id = u32::from(fb_h);
            if self.cache.contains_key(&id) {
                self.last_fb_id = id;
            } else if let Ok(frame) = export_fb_dmabuf(&self.card, fb_h) {
                if self.cache.len() >= 16 {
                    let keep = self.last_fb_id;
                    self.cache.retain(|&k, _| k == keep);
                }
                self.cache.insert(id, frame);
                self.last_fb_id = id;
            }
        }
        self.cache
            .get(&self.last_fb_id)
            .ok_or_else(|| anyhow!("no cached KMS DMA-BUF frame available"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cap_eff_sys_admin_bit() {
        let with_sys_admin = "Name:\tusher-host\nCapInh:\t0000000000000000\nCapEff:\t0000000000200000\n";
        assert!(parse_cap_eff_has_sys_admin(with_sys_admin));

        let full_root = "CapEff:\t000001ffffffffff\n";
        assert!(parse_cap_eff_has_sys_admin(full_root));

        let without_sys_admin = "CapEff:\t0000000000000000\nCapBnd:\t000001ffffffffff\n";
        assert!(!parse_cap_eff_has_sys_admin(without_sys_admin));
    }

    #[test]
    fn drop_cap_sys_admin_is_safe_when_unprivileged() {
        // Must succeed cleanly in CI / Docker containers without elevated capabilities.
        drop_cap_sys_admin().expect("drop_cap_sys_admin should handle unprivileged processes cleanly");
        assert!(!has_cap_sys_admin());
    }
}
