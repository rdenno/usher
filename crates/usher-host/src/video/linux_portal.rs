use super::color::{CpuFrame, PixelFormat};
use anyhow::{Context, Result, anyhow, bail};
use libloading::{Library, Symbol};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use zbus::export::futures_core::Stream;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_IFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

pub fn portal_token_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("usher").join("portal-token");
    }
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home)
            .join(".config")
            .join("usher")
            .join("portal-token");
    }
    PathBuf::from("usher-portal-token")
}

pub fn load_portal_token() -> Option<String> {
    let raw = std::fs::read_to_string(portal_token_path()).ok()?;
    let token = raw.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

pub fn save_portal_token(token: &str) -> Result<()> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        bail!("refusing to save empty ScreenCast restore_token");
    }
    let path = portal_token_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, format!("{trimmed}\n"))
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn pipewire_lib_available() -> bool {
    unsafe { Library::new("libpipewire-0.3.so.0") }.is_ok()
}

/// Non-intrusive readiness check for Wayland ScreenCast portal capture.
pub fn probe_portal_status() -> std::result::Result<String, String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err("WAYLAND_DISPLAY not set".into());
    }
    let Some(token) = load_portal_token() else {
        return Err(format!(
            "no saved restore_token at {} (approve once locally with: usher-host --setup-portal)",
            portal_token_path().display()
        ));
    };
    if !pipewire_lib_available() {
        return Err("libpipewire-0.3.so.0 not found".into());
    }
    Ok(format!(
        "ScreenCast portal ready (restore_token={}..., libpipewire-0.3.so.0 present)",
        token.chars().take(8).collect::<String>()
    ))
}

async fn next_signal_msg(stream: &mut zbus::proxy::SignalStream<'_>) -> Option<zbus::Message> {
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

async fn subscribe_portal_request(
    conn: &zbus::Connection,
    sanitized_sender: &str,
    handle_token: &str,
) -> Result<zbus::proxy::SignalStream<'static>> {
    let req_path = format!("/org/freedesktop/portal/desktop/request/{sanitized_sender}/{handle_token}");
    let proxy = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(conn)
        .destination(PORTAL_DEST)?
        .path(req_path)?
        .interface(REQUEST_IFACE)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    proxy
        .receive_signal("Response")
        .await
        .context("subscribing to portal Request.Response")
}

async fn await_portal_response(
    mut stream: zbus::proxy::SignalStream<'_>,
    timeout_secs: u64,
) -> Result<(u32, HashMap<String, OwnedValue>)> {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async move {
        next_signal_msg(&mut stream).await
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for ScreenCast portal response"))?
    .ok_or_else(|| anyhow!("portal Response stream closed"))?;

    let (code, results): (u32, HashMap<String, OwnedValue>) = msg
        .body()
        .deserialize()
        .context("decoding portal Response body")?;
    Ok((code, results))
}

fn new_token(prefix: &str, counter: &mut u32) -> String {
    *counter += 1;
    format!("{prefix}_{}_{}", std::process::id(), *counter)
}

async fn request_screencast_fd(
    allow_interactive: bool,
) -> Result<(OwnedFd, u32, Option<(u16, u16)>)> {
    let saved_token = load_portal_token();
    if !allow_interactive && saved_token.is_none() {
        bail!(
            "ScreenCast restore_token missing; refusing unattended portal prompt over SSH (run locally: usher-host --setup-portal)"
        );
    }

    let conn = zbus::Connection::session()
        .await
        .context("connecting to session D-Bus for ScreenCast portal")?;
    let sender = conn
        .unique_name()
        .ok_or_else(|| anyhow!("D-Bus connection missing unique name"))?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");

    let sc_proxy = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(&conn)
        .destination(PORTAL_DEST)?
        .path(PORTAL_PATH)?
        .interface(PORTAL_IFACE)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;

    let mut ctr = 0u32;

    // 1) CreateSession
    let handle_create = new_token("usher_req", &mut ctr);
    let session_token = new_token("usher_sess", &mut ctr);
    let create_sig = subscribe_portal_request(&conn, &sender, &handle_create).await?;

    let mut create_opts: HashMap<&str, Value<'_>> = HashMap::new();
    create_opts.insert("handle_token", Value::from(handle_create.as_str()));
    create_opts.insert("session_handle_token", Value::from(session_token.as_str()));
    let _: OwnedObjectPath = sc_proxy
        .call("CreateSession", &(create_opts))
        .await
        .context("portal ScreenCast.CreateSession failed")?;

    let (create_code, create_res) = await_portal_response(create_sig, 15).await?;
    if create_code != 0 {
        bail!("ScreenCast CreateSession failed with response code {create_code}");
    }
    let session_handle_str = create_res
        .get("session_handle")
        .and_then(|v| v.downcast_ref::<zbus::zvariant::Str>().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!("/org/freedesktop/portal/desktop/session/{sender}/{session_token}")
        });
    let session_obj = OwnedObjectPath::try_from(session_handle_str)?;

    // 2) SelectSources (subscribe BEFORE calling portal method)
    let handle_select = new_token("usher_sel", &mut ctr);
    let select_sig = subscribe_portal_request(&conn, &sender, &handle_select).await?;
    let mut select_opts: HashMap<&str, Value<'_>> = HashMap::new();
    select_opts.insert("handle_token", Value::from(handle_select.as_str()));
    select_opts.insert("types", Value::from(1u32)); // MONITOR
    select_opts.insert("multiple", Value::from(false));
    select_opts.insert("cursor_mode", Value::from(2u32)); // EMBEDDED
    select_opts.insert("persist_mode", Value::from(2u32)); // Persistent restore_token
    if let Some(ref tok) = saved_token {
        select_opts.insert("restore_token", Value::from(tok.as_str()));
    }
    let _: OwnedObjectPath = sc_proxy
        .call("SelectSources", &(&session_obj, select_opts))
        .await
        .context("portal ScreenCast.SelectSources failed")?;
    let (sel_code, _) = await_portal_response(select_sig, 15).await?;
    if sel_code != 0 {
        bail!("ScreenCast SelectSources failed with response code {sel_code}");
    }

    // 3) Start (subscribe BEFORE calling portal method)
    let handle_start = new_token("usher_start", &mut ctr);
    let start_sig = subscribe_portal_request(&conn, &sender, &handle_start).await?;
    let mut start_opts: HashMap<&str, Value<'_>> = HashMap::new();
    start_opts.insert("handle_token", Value::from(handle_start.as_str()));
    let _: OwnedObjectPath = sc_proxy
        .call("Start", &(&session_obj, "", start_opts))
        .await
        .context("portal ScreenCast.Start failed")?;
    let start_timeout = if allow_interactive { 120 } else { 15 };
    let (start_code, start_res) = await_portal_response(start_sig, start_timeout).await?;
    if start_code != 0 {
        bail!("ScreenCast Start cancelled or denied (code={start_code})");
    }

    if let Some(new_tok) = start_res
        .get("restore_token")
        .and_then(|v| v.downcast_ref::<zbus::zvariant::Str>().ok())
    {
        if let Err(err) = save_portal_token(new_tok.as_str()) {
            tracing::warn!("failed to persist ScreenCast restore_token: {err:#}");
        } else {
            tracing::debug!("persisted updated ScreenCast restore_token");
        }
    } else if allow_interactive && saved_token.is_none() {
        tracing::warn!(
            "portal did not return a restore_token (desktop portal may not support persistent ScreenCast tokens)"
        );
    }

    // Parse streams: a(ua{sv}) -> [(node_id, props)]
    type PortalStreams = Vec<(u32, HashMap<String, OwnedValue>)>;
    let streams_val = start_res
        .get("streams")
        .ok_or_else(|| anyhow!("ScreenCast Start response contained no streams"))?;
    let streams = PortalStreams::try_from(streams_val.clone())
        .context("parsing ScreenCast streams array")?;
    let (node_id, props) = streams
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("ScreenCast Start returned empty stream list"))?;

    let stream_size = props
        .get("size")
        .and_then(|v| <(i32, i32)>::try_from(v.clone()).ok())
        .and_then(|(w, h)| {
            if w >= 64 && h >= 64 {
                Some((w as u16, h as u16))
            } else {
                None
            }
        });

    // 4) OpenPipeWireRemote
    let open_opts: HashMap<&str, Value<'_>> = HashMap::new();
    let pw_fd: zbus::zvariant::OwnedFd = sc_proxy
        .call("OpenPipeWireRemote", &(&session_obj, open_opts))
        .await
        .context("portal ScreenCast.OpenPipeWireRemote failed")?;
    let owned_fd = OwnedFd::from(pw_fd);

    Ok((owned_fd, node_id, stream_size))
}

pub async fn run_setup_portal() -> Result<()> {
    println!("Opening xdg-desktop-portal ScreenCast permission prompt...");
    println!("Select the monitor/screen you want Usher to capture and allow persistent access.");
    let (_fd, node_id, size) = request_screencast_fd(true).await?;
    let tok_path = portal_token_path();
    println!(
        "[OK]   Portal ScreenCast authorized (node_id={node_id}, size={:?})",
        size
    );
    if load_portal_token().is_some() {
        println!("[OK]   Saved persistent restore_token to {}", tok_path.display());
    } else {
        println!("[WARN] Desktop portal did not issue a persistent restore_token.");
    }
    Ok(())
}

// ---------- Pure-Rust SPA Pod builder/parser & runtime libpipewire-0.3.so.0 loader ----------

const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_INT: u32 = 4;
const SPA_TYPE_CHOICE: u32 = 14;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_RECTANGLE: u32 = 10;
const SPA_TYPE_FRACTION: u32 = 11;

const SPA_CHOICE_ENUM: u32 = 3;
const SPA_CHOICE_RANGE: u32 = 1;

const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_PARAM_FORMAT: u32 = 4;
const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;

const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_VIDEO_FORMAT: u32 = 0x20001;
const SPA_FORMAT_VIDEO_SIZE: u32 = 0x20003;
const SPA_FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;

const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;

const SPA_VIDEO_FORMAT_BGRX: u32 = 7;
const SPA_VIDEO_FORMAT_BGRA: u32 = 12;
const SPA_VIDEO_FORMAT_RGBX: u32 = 8;
const SPA_VIDEO_FORMAT_RGBA: u32 = 11;
const SPA_VIDEO_FORMAT_NV12: u32 = 23;

#[derive(Default)]
struct PodBuilder {
    buf: Vec<u8>,
}

impl PodBuilder {
    fn push_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_ne_bytes());
    }

    fn push_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_ne_bytes());
    }

    fn pad8(&mut self) {
        while !self.buf.len().is_multiple_of(8) {
            self.buf.push(0);
        }
    }

    fn push_prop_id(&mut self, key: u32, val: u32) {
        self.push_u32(key);
        self.push_u32(0); // flags
        self.push_u32(4); // pod size
        self.push_u32(SPA_TYPE_ID);
        self.push_u32(val);
        self.pad8();
    }

    fn push_prop_choice_enum_id(&mut self, key: u32, default: u32, alts: &[u32]) {
        self.push_u32(key);
        self.push_u32(0);
        let choice_body_size = 8 + 8 + (1 + alts.len() as u32) * 4;
        self.push_u32(choice_body_size);
        self.push_u32(SPA_TYPE_CHOICE);
        self.push_u32(SPA_CHOICE_ENUM);
        self.push_u32(0);
        self.push_u32(4); // child_size
        self.push_u32(SPA_TYPE_ID);
        self.push_u32(default);
        for &a in alts {
            self.push_u32(a);
        }
        self.pad8();
    }

    fn push_prop_choice_range_rect(
        &mut self,
        key: u32,
        def: (u32, u32),
        min: (u32, u32),
        max: (u32, u32),
    ) {
        self.push_u32(key);
        self.push_u32(0);
        // spa_pod_choice_body: type(4)+flags(4) + child_pod(size=8,type=RECT)+3*rect(24) = 40
        self.push_u32(40);
        self.push_u32(SPA_TYPE_CHOICE);
        self.push_u32(SPA_CHOICE_RANGE);
        self.push_u32(0);
        self.push_u32(8);
        self.push_u32(SPA_TYPE_RECTANGLE);
        for (w, h) in [def, min, max] {
            self.push_u32(w);
            self.push_u32(h);
        }
        self.pad8();
    }

    fn push_prop_choice_range_frac(&mut self, key: u32, fps: i32) {
        self.push_u32(key);
        self.push_u32(0);
        self.push_u32(40);
        self.push_u32(SPA_TYPE_CHOICE);
        self.push_u32(SPA_CHOICE_RANGE);
        self.push_u32(0);
        self.push_u32(8);
        self.push_u32(SPA_TYPE_FRACTION);
        for (n, d) in [(fps, 1), (0, 1), (240, 1)] {
            self.push_i32(n);
            self.push_i32(d);
        }
        self.pad8();
    }
}

pub fn build_enum_format_pod(width: u16, height: u16, fps: u16) -> Vec<u8> {
    let mut inner = PodBuilder::default();
    inner.push_u32(SPA_TYPE_OBJECT_FORMAT);
    inner.push_u32(SPA_PARAM_ENUM_FORMAT);
    inner.push_prop_id(SPA_FORMAT_MEDIA_TYPE, SPA_MEDIA_TYPE_VIDEO);
    inner.push_prop_id(SPA_FORMAT_MEDIA_SUBTYPE, SPA_MEDIA_SUBTYPE_RAW);
    inner.push_prop_choice_enum_id(
        SPA_FORMAT_VIDEO_FORMAT,
        SPA_VIDEO_FORMAT_BGRX,
        &[
            SPA_VIDEO_FORMAT_BGRX,
            SPA_VIDEO_FORMAT_BGRA,
            SPA_VIDEO_FORMAT_RGBX,
            SPA_VIDEO_FORMAT_RGBA,
            SPA_VIDEO_FORMAT_NV12,
        ],
    );
    inner.push_prop_choice_range_rect(
        SPA_FORMAT_VIDEO_SIZE,
        (width.max(64) as u32, height.max(64) as u32),
        (16, 16),
        (7680, 4320),
    );
    inner.push_prop_choice_range_frac(SPA_FORMAT_VIDEO_FRAMERATE, fps.max(1) as i32);

    let mut pod = Vec::with_capacity(8 + inner.buf.len());
    pod.extend_from_slice(&(inner.buf.len() as u32).to_ne_bytes());
    pod.extend_from_slice(&SPA_TYPE_OBJECT.to_ne_bytes());
    pod.extend_from_slice(&inner.buf);
    pod
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedVideoFormat {
    pub pixel_format: PixelFormat,
    pub width: u32,
    pub height: u32,
}

fn read_u32_ne(bytes: &[u8], off: usize) -> Option<u32> {
    let slice: [u8; 4] = bytes.get(off..off + 4)?.try_into().ok()?;
    Some(u32::from_ne_bytes(slice))
}

fn parse_spa_format_pod_bytes(bytes: &[u8]) -> Option<NegotiatedVideoFormat> {
    if bytes.len() < 16 {
        return None;
    }
    let body_size = read_u32_ne(bytes, 0)? as usize;
    let pod_type = read_u32_ne(bytes, 4)?;
    if pod_type != SPA_TYPE_OBJECT || 8 + body_size > bytes.len() {
        return None;
    }
    let obj = &bytes[8..8 + body_size];
    let obj_type = read_u32_ne(obj, 0)?;
    if obj_type != SPA_TYPE_OBJECT_FORMAT {
        return None;
    }

    let mut pos = 8usize;
    let mut pix_fmt: Option<PixelFormat> = None;
    let mut width = 0u32;
    let mut height = 0u32;

    while pos + 16 <= obj.len() {
        let key = read_u32_ne(obj, pos)?;
        let _flags = read_u32_ne(obj, pos + 4)?;
        let val_size = read_u32_ne(obj, pos + 8)? as usize;
        let val_type = read_u32_ne(obj, pos + 12)?;
        let val_off = pos + 16;
        if val_off + val_size > obj.len() {
            break;
        }
        let val_bytes = &obj[val_off..val_off + val_size];

        match key {
            SPA_FORMAT_VIDEO_FORMAT => {
                let raw_id = if val_type == SPA_TYPE_ID || val_type == SPA_TYPE_INT {
                    read_u32_ne(val_bytes, 0)
                } else if val_type == SPA_TYPE_CHOICE && val_size >= 20 {
                    read_u32_ne(val_bytes, 16)
                } else {
                    None
                };
                if let Some(id) = raw_id {
                    pix_fmt = match id {
                        SPA_VIDEO_FORMAT_BGRX => Some(PixelFormat::Bgrx),
                        SPA_VIDEO_FORMAT_BGRA => Some(PixelFormat::Bgra),
                        SPA_VIDEO_FORMAT_RGBX => Some(PixelFormat::Rgbx),
                        SPA_VIDEO_FORMAT_RGBA => Some(PixelFormat::Rgba),
                        SPA_VIDEO_FORMAT_NV12 => Some(PixelFormat::Nv12),
                        _ => None,
                    };
                }
            }
            SPA_FORMAT_VIDEO_SIZE => {
                if val_type == SPA_TYPE_RECTANGLE && val_size >= 8 {
                    width = read_u32_ne(val_bytes, 0).unwrap_or(0);
                    height = read_u32_ne(val_bytes, 4).unwrap_or(0);
                } else if val_type == SPA_TYPE_CHOICE && val_size >= 24 {
                    width = read_u32_ne(val_bytes, 16).unwrap_or(0);
                    height = read_u32_ne(val_bytes, 20).unwrap_or(0);
                }
            }
            _ => {}
        }

        let total = 16 + val_size;
        let padded = (total + 7) & !7;
        pos += padded;
    }

    if let Some(pixel_format) = pix_fmt
        && width >= 16
        && height >= 16
    {
        Some(NegotiatedVideoFormat {
            pixel_format,
            width,
            height,
        })
    } else {
        None
    }
}

#[allow(dead_code)]
#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

#[allow(dead_code)]
#[repr(C)]
struct SpaData {
    r#type: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

#[allow(dead_code)]
#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut c_void,
    datas: *mut SpaData,
}

#[allow(dead_code)]
#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
    requested: u64,
}

#[allow(dead_code)]
#[repr(C)]
struct PwStreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const u32)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    process: Option<unsafe extern "C" fn(*mut c_void)>,
    drained: Option<unsafe extern "C" fn(*mut c_void)>,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Option<unsafe extern "C" fn(*mut c_void, u64)>,
}

type FnPwInit = unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char);
type FnPwThreadLoopNew = unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void;
type FnPwThreadLoopDestroy = unsafe extern "C" fn(*mut c_void);
type FnPwThreadLoopGetLoop = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnPwThreadLoopStart = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnPwThreadLoopStop = unsafe extern "C" fn(*mut c_void);
type FnPwThreadLoopLock = unsafe extern "C" fn(*mut c_void);
type FnPwThreadLoopUnlock = unsafe extern "C" fn(*mut c_void);
type FnPwContextNew = unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void;
type FnPwContextDestroy = unsafe extern "C" fn(*mut c_void);
type FnPwContextConnectFd = unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void;
type FnPwCoreDisconnect = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnPwPropertiesNewString = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type FnPwStreamNew =
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void;
type FnPwStreamDestroy = unsafe extern "C" fn(*mut c_void);
type FnPwStreamAddListener = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    *const PwStreamEvents,
    *mut c_void,
);
type FnPwStreamConnect =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const u32, u32) -> c_int;
type FnPwStreamDequeueBuffer = unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer;
type FnPwStreamQueueBuffer = unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int;

struct PipeWireApi {
    _lib: Library,
    pw_init: FnPwInit,
    pw_thread_loop_new: FnPwThreadLoopNew,
    pw_thread_loop_destroy: FnPwThreadLoopDestroy,
    pw_thread_loop_get_loop: FnPwThreadLoopGetLoop,
    pw_thread_loop_start: FnPwThreadLoopStart,
    pw_thread_loop_stop: FnPwThreadLoopStop,
    pw_thread_loop_lock: FnPwThreadLoopLock,
    pw_thread_loop_unlock: FnPwThreadLoopUnlock,
    pw_context_new: FnPwContextNew,
    pw_context_destroy: FnPwContextDestroy,
    pw_context_connect_fd: FnPwContextConnectFd,
    pw_core_disconnect: FnPwCoreDisconnect,
    pw_properties_new_string: FnPwPropertiesNewString,
    pw_stream_new: FnPwStreamNew,
    pw_stream_destroy: FnPwStreamDestroy,
    pw_stream_add_listener: FnPwStreamAddListener,
    pw_stream_connect: FnPwStreamConnect,
    pw_stream_dequeue_buffer: FnPwStreamDequeueBuffer,
    pw_stream_queue_buffer: FnPwStreamQueueBuffer,
}

unsafe impl Send for PipeWireApi {}
unsafe impl Sync for PipeWireApi {}

impl PipeWireApi {
    fn load() -> Result<Arc<Self>> {
        let lib = unsafe { Library::new("libpipewire-0.3.so.0") }
            .context("loading libpipewire-0.3.so.0 at runtime")?;
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let s: Symbol<'_, $ty> = unsafe { lib.get($name) }
                    .with_context(|| format!("missing symbol {:?}", $name))?;
                *s
            }};
        }
        Ok(Arc::new(Self {
            pw_init: sym!(b"pw_init\0", FnPwInit),
            pw_thread_loop_new: sym!(b"pw_thread_loop_new\0", FnPwThreadLoopNew),
            pw_thread_loop_destroy: sym!(b"pw_thread_loop_destroy\0", FnPwThreadLoopDestroy),
            pw_thread_loop_get_loop: sym!(b"pw_thread_loop_get_loop\0", FnPwThreadLoopGetLoop),
            pw_thread_loop_start: sym!(b"pw_thread_loop_start\0", FnPwThreadLoopStart),
            pw_thread_loop_stop: sym!(b"pw_thread_loop_stop\0", FnPwThreadLoopStop),
            pw_thread_loop_lock: sym!(b"pw_thread_loop_lock\0", FnPwThreadLoopLock),
            pw_thread_loop_unlock: sym!(b"pw_thread_loop_unlock\0", FnPwThreadLoopUnlock),
            pw_context_new: sym!(b"pw_context_new\0", FnPwContextNew),
            pw_context_destroy: sym!(b"pw_context_destroy\0", FnPwContextDestroy),
            pw_context_connect_fd: sym!(b"pw_context_connect_fd\0", FnPwContextConnectFd),
            pw_core_disconnect: sym!(b"pw_core_disconnect\0", FnPwCoreDisconnect),
            pw_properties_new_string: sym!(b"pw_properties_new_string\0", FnPwPropertiesNewString),
            pw_stream_new: sym!(b"pw_stream_new\0", FnPwStreamNew),
            pw_stream_destroy: sym!(b"pw_stream_destroy\0", FnPwStreamDestroy),
            pw_stream_add_listener: sym!(b"pw_stream_add_listener\0", FnPwStreamAddListener),
            pw_stream_connect: sym!(b"pw_stream_connect\0", FnPwStreamConnect),
            pw_stream_dequeue_buffer: sym!(b"pw_stream_dequeue_buffer\0", FnPwStreamDequeueBuffer),
            pw_stream_queue_buffer: sym!(b"pw_stream_queue_buffer\0", FnPwStreamQueueBuffer),
            _lib: lib,
        }))
    }
}

struct CallbackState {
    api: Arc<PipeWireApi>,
    stream: *mut c_void,
    format: Mutex<Option<NegotiatedVideoFormat>>,
    frame_tx: SyncSender<CpuFrame>,
    error: Mutex<Option<String>>,
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: c_int,
    state: c_int,
    err: *const c_char,
) {
    if data.is_null() {
        return;
    }
    let st = unsafe { &*data.cast::<CallbackState>() };
    // PW_STREAM_STATE_ERROR = -1
    if state < 0 && !err.is_null() {
        let msg = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
        if let Ok(mut g) = st.error.lock() {
            *g = Some(msg);
        }
    }
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const u32) {
    if data.is_null() || param.is_null() || id != SPA_PARAM_FORMAT {
        return;
    }
    let st = unsafe { &*data.cast::<CallbackState>() };
    let body_size = unsafe { *param } as usize;
    let total_size = 8 + body_size;
    if total_size > 16 && total_size < 65536 {
        let bytes = unsafe { std::slice::from_raw_parts(param.cast::<u8>(), total_size) };
        if let Some(fmt) = parse_spa_format_pod_bytes(bytes) {
            tracing::info!(
                width = fmt.width,
                height = fmt.height,
                pixel_format = ?fmt.pixel_format,
                "PipeWire portal video format negotiated"
            );
            if let Ok(mut g) = st.format.lock() {
                *g = Some(fmt);
            }
        }
    }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    let st = unsafe { &*data.cast::<CallbackState>() };
    if st.stream.is_null() {
        return;
    }
    let mut last_buf: *mut PwBuffer = std::ptr::null_mut();
    loop {
        let b = unsafe { (st.api.pw_stream_dequeue_buffer)(st.stream) };
        if b.is_null() {
            break;
        }
        if !last_buf.is_null() {
            unsafe { (st.api.pw_stream_queue_buffer)(st.stream, last_buf) };
        }
        last_buf = b;
    }
    if last_buf.is_null() {
        return;
    }

    let fmt = st.format.lock().ok().and_then(|g| *g);
    if let Some(fmt) = fmt {
        let spa_buf = unsafe { (*last_buf).buffer };
        if !spa_buf.is_null() && unsafe { (*spa_buf).n_datas } >= 1 {
            let spa_data = unsafe { &*(*spa_buf).datas };
            let stride = if !spa_data.chunk.is_null() && unsafe { (*spa_data.chunk).stride } > 0 {
                unsafe { (*spa_data.chunk).stride as usize }
            } else if fmt.pixel_format == PixelFormat::Nv12 {
                fmt.width as usize
            } else {
                fmt.width as usize * 4
            };
            let min_bytes = if fmt.pixel_format == PixelFormat::Nv12 {
                stride * (fmt.height as usize + fmt.height as usize / 2)
            } else {
                stride * fmt.height as usize
            };
            let offset = if !spa_data.chunk.is_null() {
                unsafe { (*spa_data.chunk).offset as usize }
            } else {
                0
            };

            if !spa_data.data.is_null() && spa_data.maxsize as usize >= offset + min_bytes {
                let mut out = vec![0u8; min_bytes];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        spa_data.data.cast::<u8>().add(offset),
                        out.as_mut_ptr(),
                        min_bytes,
                    );
                }
                let _ = st.frame_tx.try_send(CpuFrame {
                    width: fmt.width,
                    height: fmt.height,
                    stride,
                    format: fmt.pixel_format,
                    data: out,
                });
            } else if spa_data.fd >= 0 {
                let raw_fd = spa_data.fd as c_int;
                let map_off = spa_data.mapoffset as usize + offset;
                let map_len = (spa_data.maxsize as usize).max(map_off + min_bytes);
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        map_len,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        raw_fd,
                        0,
                    )
                };
                if ptr != libc::MAP_FAILED {
                    let mut out = vec![0u8; min_bytes];
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            ptr.cast::<u8>().add(map_off),
                            out.as_mut_ptr(),
                            min_bytes,
                        );
                        let _ = libc::munmap(ptr, map_len);
                    }
                    let _ = st.frame_tx.try_send(CpuFrame {
                        width: fmt.width,
                        height: fmt.height,
                        stride,
                        format: fmt.pixel_format,
                        data: out,
                    });
                }
            }
        }
    }

    unsafe { (st.api.pw_stream_queue_buffer)(st.stream, last_buf) };
}

pub struct PipeWireCapturer {
    api: Arc<PipeWireApi>,
    thread_loop: *mut c_void,
    stream: *mut c_void,
    core: *mut c_void,
    context: *mut c_void,
    _listener_hook: Box<[u64; 16]>,
    _events: Box<PwStreamEvents>,
    state: Box<CallbackState>,
    frame_rx: Receiver<CpuFrame>,
    last_frame: Option<CpuFrame>,
}

unsafe impl Send for PipeWireCapturer {}

impl PipeWireCapturer {
    pub fn open_from_portal(width: u16, height: u16, fps: u16) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building helper runtime for ScreenCast portal")?;
        let (pw_fd, node_id, size) = rt.block_on(request_screencast_fd(false))?;
        let req_w = size.map(|s| s.0).unwrap_or(width);
        let req_h = size.map(|s| s.1).unwrap_or(height);

        let api = PipeWireApi::load()?;
        unsafe { (api.pw_init)(std::ptr::null_mut(), std::ptr::null_mut()) };

        let loop_name = CString::new("usher-pw-capture")?;
        let thread_loop = unsafe { (api.pw_thread_loop_new)(loop_name.as_ptr(), std::ptr::null()) };
        if thread_loop.is_null() {
            bail!("pw_thread_loop_new returned null");
        }
        let pw_loop = unsafe { (api.pw_thread_loop_get_loop)(thread_loop) };
        let context = unsafe { (api.pw_context_new)(pw_loop, std::ptr::null_mut(), 0) };
        if context.is_null() {
            unsafe { (api.pw_thread_loop_destroy)(thread_loop) };
            bail!("pw_context_new returned null");
        }

        let core =
            unsafe { (api.pw_context_connect_fd)(context, pw_fd.into_raw_fd(), std::ptr::null_mut(), 0) };
        if core.is_null() {
            unsafe {
                (api.pw_context_destroy)(context);
                (api.pw_thread_loop_destroy)(thread_loop);
            }
            bail!("pw_context_connect_fd failed");
        }

        let props_c = CString::new("media.type=Video\nmedia.category=Capture\nmedia.role=Screen")?;
        let props = unsafe { (api.pw_properties_new_string)(props_c.as_ptr()) };
        let stream_name = CString::new("usher-screencast")?;
        let stream = unsafe { (api.pw_stream_new)(core, stream_name.as_ptr(), props) };
        if stream.is_null() {
            unsafe {
                (api.pw_core_disconnect)(core);
                (api.pw_context_destroy)(context);
                (api.pw_thread_loop_destroy)(thread_loop);
            }
            bail!("pw_stream_new returned null");
        }

        let (frame_tx, frame_rx) = sync_channel(2);
        let state = Box::new(CallbackState {
            api: api.clone(),
            stream,
            format: Mutex::new(None),
            frame_tx,
            error: Mutex::new(None),
        });
        let events = Box::new(PwStreamEvents {
            version: 2,
            destroy: None,
            state_changed: Some(on_state_changed),
            control_info: None,
            io_changed: None,
            param_changed: Some(on_param_changed),
            add_buffer: None,
            remove_buffer: None,
            process: Some(on_process),
            drained: None,
            command: None,
            trigger_done: None,
        });
        let mut hook = Box::new([0u64; 16]);
        let state_ptr = (&*state as *const CallbackState).cast_mut().cast::<c_void>();
        unsafe {
            (api.pw_stream_add_listener)(
                stream,
                hook.as_mut_ptr().cast(),
                &*events,
                state_ptr,
            );
        }

        let mut format_pod = build_enum_format_pod(req_w, req_h, fps);
        let mut param_ptrs: [*const u32; 1] = [format_pod.as_mut_ptr().cast()];
        // SPA_DIRECTION_INPUT = 0, flags = AUTOCONNECT(1) | MAP_BUFFERS(4)
        let rc = unsafe {
            (api.pw_stream_connect)(stream, 0, node_id, 0x5, param_ptrs.as_mut_ptr(), 1)
        };
        if rc < 0 {
            unsafe {
                (api.pw_stream_destroy)(stream);
                (api.pw_core_disconnect)(core);
                (api.pw_context_destroy)(context);
                (api.pw_thread_loop_destroy)(thread_loop);
            }
            bail!("pw_stream_connect(node_id={node_id}) failed with {rc}");
        }

        if unsafe { (api.pw_thread_loop_start)(thread_loop) } != 0 {
            unsafe {
                (api.pw_stream_destroy)(stream);
                (api.pw_core_disconnect)(core);
                (api.pw_context_destroy)(context);
                (api.pw_thread_loop_destroy)(thread_loop);
            }
            bail!("pw_thread_loop_start failed");
        }

        tracing::info!(
            node_id,
            req_w,
            req_h,
            "PipeWire runtime ScreenCast capture active"
        );

        Ok(Self {
            api,
            thread_loop,
            stream,
            core,
            context,
            _listener_hook: hook,
            _events: events,
            state,
            frame_rx,
            last_frame: None,
        })
    }

    pub fn capture_frame(&mut self) -> Result<CpuFrame> {
        if let Ok(mut g) = self.state.error.lock()
            && let Some(err) = g.take()
        {
            bail!("PipeWire stream error: {err}");
        }
        while let Ok(f) = self.frame_rx.try_recv() {
            self.last_frame = Some(f);
        }
        if let Some(ref f) = self.last_frame {
            return Ok(f.clone());
        }
        let first = self
            .frame_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .context("timed out waiting for initial PipeWire portal frame")?;
        self.last_frame = Some(first.clone());
        Ok(first)
    }
}

impl Drop for PipeWireCapturer {
    fn drop(&mut self) {
        unsafe {
            (self.api.pw_thread_loop_stop)(self.thread_loop);
            (self.api.pw_thread_loop_lock)(self.thread_loop);
            (self.api.pw_stream_destroy)(self.stream);
            (self.api.pw_core_disconnect)(self.core);
            (self.api.pw_context_destroy)(self.context);
            (self.api.pw_thread_loop_unlock)(self.thread_loop);
            (self.api.pw_thread_loop_destroy)(self.thread_loop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spa_enum_format_pod_is_well_formed() {
        let pod = build_enum_format_pod(1920, 1080, 60);
        assert!(pod.len() >= 16);
        assert!(pod.len().is_multiple_of(8));
        let body_size = u32::from_ne_bytes(pod[0..4].try_into().unwrap()) as usize;
        let pod_type = u32::from_ne_bytes(pod[4..8].try_into().unwrap());
        assert_eq!(pod_type, SPA_TYPE_OBJECT);
        assert_eq!(8 + body_size, pod.len());
    }

    #[test]
    fn parses_spa_negotiated_format_pod() {
        let mut inner = PodBuilder::default();
        inner.push_u32(SPA_TYPE_OBJECT_FORMAT);
        inner.push_u32(SPA_PARAM_FORMAT);
        inner.push_prop_id(SPA_FORMAT_MEDIA_TYPE, SPA_MEDIA_TYPE_VIDEO);
        inner.push_prop_id(SPA_FORMAT_MEDIA_SUBTYPE, SPA_MEDIA_SUBTYPE_RAW);
        inner.push_prop_id(SPA_FORMAT_VIDEO_FORMAT, SPA_VIDEO_FORMAT_NV12);
        // Rectangle property
        inner.push_u32(SPA_FORMAT_VIDEO_SIZE);
        inner.push_u32(0);
        inner.push_u32(8);
        inner.push_u32(SPA_TYPE_RECTANGLE);
        inner.push_u32(2560);
        inner.push_u32(1440);
        inner.pad8();

        let mut pod = Vec::new();
        pod.extend_from_slice(&(inner.buf.len() as u32).to_ne_bytes());
        pod.extend_from_slice(&SPA_TYPE_OBJECT.to_ne_bytes());
        pod.extend_from_slice(&inner.buf);

        let parsed = parse_spa_format_pod_bytes(&pod).expect("valid SPA format pod");
        assert_eq!(
            parsed,
            NegotiatedVideoFormat {
                pixel_format: PixelFormat::Nv12,
                width: 2560,
                height: 1440,
            }
        );
    }
}
