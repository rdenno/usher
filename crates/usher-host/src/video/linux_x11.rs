use super::color::{CpuFrame, PixelFormat};
use anyhow::{Context, Result, anyhow, bail};
use std::ptr::NonNull;
use usher_proto::handshake::DisplayInfo;
use x11rb::connection::Connection as _;
use x11rb::protocol::randr;
use x11rb::protocol::shm;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

const X11_Z_PIXMAP: u8 = 2;

#[derive(Debug, Clone)]
struct MonitorRect {
    info: DisplayInfo,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

fn enumerate_monitors(conn: &RustConnection, screen_num: usize) -> Vec<MonitorRect> {
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let mut out = Vec::new();

    if randr::query_version(conn, 1, 5)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some()
        && let Ok(cookie) = randr::get_monitors(conn, root, true)
        && let Ok(reply) = cookie.reply()
    {
        for (idx, m) in reply.monitors.into_iter().enumerate() {
            let name = conn
                .get_atom_name(m.name)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("X11-{}", idx + 1));
            if m.width >= 64
                && m.height >= 64
                && let Ok(idx_u8) = u8::try_from(out.len())
            {
                out.push(MonitorRect {
                    info: DisplayInfo {
                        index: idx_u8,
                        name,
                        width: m.width & !1,
                        height: m.height & !1,
                    },
                    x: m.x,
                    y: m.y,
                    width: m.width,
                    height: m.height,
                });
            }
        }
    }

    if out.is_empty() {
        out.push(MonitorRect {
            info: DisplayInfo {
                index: 0,
                name: "X11-Root".to_string(),
                width: (screen.width_in_pixels.max(64)) & !1,
                height: (screen.height_in_pixels.max(64)) & !1,
            },
            x: 0,
            y: 0,
            width: screen.width_in_pixels,
            height: screen.height_in_pixels,
        });
    }

    out
}

/// Probes X11 displays and MIT-SHM availability without mutating state.
pub fn probe_x11_displays() -> std::result::Result<(Vec<DisplayInfo>, bool), String> {
    let display = std::env::var("DISPLAY").unwrap_or_default();
    if display.is_empty() {
        return Err("DISPLAY not set".into());
    }
    let (conn, screen_num) =
        RustConnection::connect(None).map_err(|e| format!("X11 connect({display}) failed: {e}"))?;
    let monitors = enumerate_monitors(&conn, screen_num);
    let shm_ok = shm::query_version(&conn)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some();
    Ok((monitors.into_iter().map(|m| m.info).collect(), shm_ok))
}

struct ShmSegment {
    shmseg: shm::Seg,
    ptr: NonNull<u8>,
    len: usize,
}

unsafe impl Send for ShmSegment {}

pub struct X11Capturer {
    conn: RustConnection,
    root: xproto::Window,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    shm: Option<ShmSegment>,
}

impl X11Capturer {
    pub fn open(display_index: u8) -> Result<Self> {
        let x11_display = std::env::var("DISPLAY").unwrap_or_default();
        if x11_display.is_empty() {
            bail!("DISPLAY environment variable is not set");
        }
        let (conn, screen_num) = RustConnection::connect(None)
            .with_context(|| format!("connecting to X11 DISPLAY={x11_display}"))?;
        let root = conn.setup().roots[screen_num].root;
        let monitors = enumerate_monitors(&conn, screen_num);
        let mon = monitors
            .get(display_index as usize)
            .or_else(|| monitors.first())
            .ok_or_else(|| anyhow!("no X11 monitor found"))?
            .clone();

        let shm = try_init_shm(&conn, mon.width, mon.height);
        tracing::info!(
            x11_display = %x11_display,
            monitor = %mon.info.name,
            x = mon.x,
            y = mon.y,
            width = mon.width,
            height = mon.height,
            mit_shm = shm.is_some(),
            "X11 pure-Rust screen capture active"
        );

        Ok(Self {
            conn,
            root,
            x: mon.x,
            y: mon.y,
            width: mon.width,
            height: mon.height,
            shm,
        })
    }

    pub fn capture_frame(&mut self) -> Result<CpuFrame> {
        let byte_len = self.width as usize * self.height as usize * 4;
        if let Some(ref seg) = self.shm {
            let reply = shm::get_image(
                &self.conn,
                self.root,
                self.x,
                self.y,
                self.width,
                self.height,
                u32::MAX,
                X11_Z_PIXMAP,
                seg.shmseg,
                0,
            )
            .context("sending XShmGetImage")?
            .reply()
            .context("awaiting XShmGetImage reply")?;

            let copy_len = (reply.size as usize).min(seg.len).min(byte_len);
            let mut data = vec![0u8; byte_len];
            unsafe {
                std::ptr::copy_nonoverlapping(seg.ptr.as_ptr(), data.as_mut_ptr(), copy_len);
            }
            return Ok(CpuFrame {
                width: self.width as u32,
                height: self.height as u32,
                stride: self.width as usize * 4,
                format: PixelFormat::Bgrx,
                data,
            });
        }

        let reply = self
            .conn
            .get_image(
                xproto::ImageFormat::Z_PIXMAP,
                self.root,
                self.x,
                self.y,
                self.width,
                self.height,
                u32::MAX,
            )
            .context("sending X11 GetImage")?
            .reply()
            .context("awaiting X11 GetImage reply")?;

        if reply.data.len() < byte_len {
            bail!(
                "X11 GetImage returned {}B, expected {byte_len}B",
                reply.data.len()
            );
        }
        Ok(CpuFrame {
            width: self.width as u32,
            height: self.height as u32,
            stride: self.width as usize * 4,
            format: PixelFormat::Bgrx,
            data: reply.data,
        })
    }
}

impl Drop for X11Capturer {
    fn drop(&mut self) {
        if let Some(seg) = self.shm.take() {
            let _ = shm::detach(&self.conn, seg.shmseg);
            let _ = self.conn.flush();
            unsafe {
                let _ = libc::shmdt(seg.ptr.as_ptr().cast());
            }
        }
    }
}

fn try_init_shm(conn: &RustConnection, width: u16, height: u16) -> Option<ShmSegment> {
    let ok = shm::query_version(conn).ok()?.reply().ok()?;
    let size = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    if size == 0 {
        return None;
    }

    let shmid = unsafe {
        libc::shmget(
            libc::IPC_PRIVATE,
            size,
            libc::IPC_CREAT | 0o600,
        )
    };
    if shmid < 0 {
        return None;
    }

    let addr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
    // Mark segment for removal immediately so even a crash cleans up kernel SHM after detach.
    unsafe {
        let _ = libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
    }
    if addr.is_null() || addr == (-1isize as *mut libc::c_void) {
        return None;
    }

    let Some(ptr) = NonNull::new(addr.cast::<u8>()) else {
        unsafe {
            let _ = libc::shmdt(addr);
        }
        return None;
    };

    let Ok(shmseg) = conn.generate_id() else {
        unsafe {
            let _ = libc::shmdt(addr);
        }
        return None;
    };

    if let Err(err) = shm::attach(conn, shmseg, shmid as u32, false)
        .ok()
        .and_then(|c| c.check().ok())
        .ok_or_else(|| anyhow!("XShmAttach rejected (MIT-SHM v{}.{})", ok.major_version, ok.minor_version))
    {
        tracing::debug!("MIT-SHM unavailable, falling back to core XGetImage: {err:#}");
        unsafe {
            let _ = libc::shmdt(addr);
        }
        return None;
    }

    Some(ShmSegment {
        shmseg,
        ptr,
        len: size,
    })
}
