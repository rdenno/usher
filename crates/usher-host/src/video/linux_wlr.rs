use super::color::{CpuFrame, PixelFormat};
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(test)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::ptr::NonNull;

const WL_DISPLAY_ID: u32 = 1;
const WL_DISPLAY_OP_SYNC: u16 = 0;
const WL_DISPLAY_OP_GET_REGISTRY: u16 = 1;
const WL_DISPLAY_EV_ERROR: u16 = 0;
const WL_DISPLAY_EV_DELETE_ID: u16 = 1;

const WL_REGISTRY_OP_BIND: u16 = 0;
const WL_REGISTRY_EV_GLOBAL: u16 = 0;

const WL_CALLBACK_EV_DONE: u16 = 0;

const WL_SHM_OP_CREATE_POOL: u16 = 0;
const WL_SHM_POOL_OP_CREATE_BUFFER: u16 = 0;
const WL_SHM_POOL_OP_DESTROY: u16 = 1;
const WL_BUFFER_OP_DESTROY: u16 = 0;

const WL_SHM_FORMAT_ARGB8888: u32 = 0;
const WL_SHM_FORMAT_XRGB8888: u32 = 1;
const WL_SHM_FORMAT_ABGR8888: u32 = 0x3432_4241;
const WL_SHM_FORMAT_XBGR8888: u32 = 0x3432_4258;

const WLR_MGR_OP_CAPTURE_OUTPUT: u16 = 0;
const WLR_MGR_OP_DESTROY: u16 = 2;

const WLR_FRAME_OP_COPY: u16 = 0;
const WLR_FRAME_OP_DESTROY: u16 = 1;

const WLR_FRAME_EV_BUFFER: u16 = 0;
const WLR_FRAME_EV_FLAGS: u16 = 1;
const WLR_FRAME_EV_READY: u16 = 2;
const WLR_FRAME_EV_FAILED: u16 = 3;
const WLR_FRAME_EV_BUFFER_DONE: u16 = 6;
const WLR_FRAME_FLAG_Y_INVERT: u32 = 1;

pub(crate) mod wire {
    use anyhow::{Context, Result, bail};
    use std::collections::VecDeque;
    use std::ffi::CString;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};

    #[derive(Debug, Clone)]
    pub struct GlobalInfo {
        pub name: u32,
        pub interface: String,
        pub version: u32,
    }

    #[derive(Debug, Clone)]
    pub struct RawEvent {
        pub obj_id: u32,
        pub opcode: u16,
        pub payload: Vec<u8>,
    }

    pub fn resolve_wayland_socket_path(
        runtime_dir: Option<&Path>,
        wayland_display: Option<&str>,
    ) -> Option<PathBuf> {
        let wd = wayland_display
            .map(str::to_string)
            .or_else(|| std::env::var("WAYLAND_DISPLAY").ok())
            .filter(|s| !s.is_empty())?;
        let wd_path = Path::new(&wd);
        if wd_path.is_absolute() {
            return Some(wd_path.to_path_buf());
        }
        let rt = runtime_dir
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
            .filter(|p| !p.as_os_str().is_empty())?;
        Some(rt.join(wd_path))
    }

    pub fn create_memfd(name: &str, size: usize) -> Result<OwnedFd> {
        let cname = CString::new(name).unwrap_or_else(|_| CString::new("usher-wl").unwrap());
        let fd = unsafe { libc::memfd_create(cname.as_ptr().cast::<libc::c_char>(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("memfd_create failed");
        }
        // Wrap immediately in OwnedFd so any error closes cleanly.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        use std::os::fd::AsRawFd;
        if unsafe { libc::ftruncate(owned.as_raw_fd(), size as libc::off_t) } < 0 {
            return Err(std::io::Error::last_os_error()).context("ftruncate memfd failed");
        }
        Ok(owned)
    }

    pub struct MsgBuilder {
        buf: Vec<u8>,
        fds: Vec<RawFd>,
    }

    impl MsgBuilder {
        pub fn new(obj_id: u32, opcode: u16) -> Self {
            let mut buf = Vec::with_capacity(32);
            buf.extend_from_slice(&obj_id.to_ne_bytes());
            buf.extend_from_slice(&(opcode as u32).to_ne_bytes());
            Self {
                buf,
                fds: Vec::new(),
            }
        }

        pub fn push_u32(&mut self, v: u32) {
            self.buf.extend_from_slice(&v.to_ne_bytes());
        }

        pub fn push_i32(&mut self, v: i32) {
            self.buf.extend_from_slice(&v.to_ne_bytes());
        }

        pub fn push_str(&mut self, s: &str) {
            let bytes = s.as_bytes();
            let len_with_nul = bytes.len() + 1;
            self.push_u32(len_with_nul as u32);
            self.buf.extend_from_slice(bytes);
            self.buf.push(0);
            while !self.buf.len().is_multiple_of(4) {
                self.buf.push(0);
            }
        }

        pub fn push_fd(&mut self, fd: RawFd) {
            self.fds.push(fd);
        }

        pub fn finish(mut self) -> (Vec<u8>, Vec<RawFd>) {
            let size = self.buf.len() as u32;
            let opcode = u32::from_ne_bytes(self.buf[4..8].try_into().unwrap()) & 0xffff;
            let word1 = (size << 16) | opcode;
            self.buf[4..8].copy_from_slice(&word1.to_ne_bytes());
            (self.buf, self.fds)
        }
    }

    pub fn read_str(payload: &[u8], offset: &mut usize) -> Result<String> {
        if *offset + 4 > payload.len() {
            bail!("truncated Wayland string length");
        }
        let len_with_nul =
            u32::from_ne_bytes(payload[*offset..*offset + 4].try_into().unwrap()) as usize;
        *offset += 4;
        if len_with_nul == 0 {
            return Ok(String::new());
        }
        let padded = len_with_nul.next_multiple_of(4);
        if *offset + padded > payload.len() {
            bail!("truncated Wayland string payload");
        }
        let bytes = &payload[*offset..*offset + len_with_nul - 1];
        *offset += padded;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    pub fn read_u32(payload: &[u8], offset: &mut usize) -> Result<u32> {
        if *offset + 4 > payload.len() {
            bail!("truncated Wayland u32");
        }
        let v = u32::from_ne_bytes(payload[*offset..*offset + 4].try_into().unwrap());
        *offset += 4;
        Ok(v)
    }

    #[repr(C)]
    struct CtrlBuf {
        words: [u64; 8],
    }

    pub struct WaylandConn {
        stream: UnixStream,
        rx_buf: Vec<u8>,
        next_id: u32,
        free_ids: VecDeque<u32>,
        pending_events: VecDeque<RawEvent>,
        pub registry_id: u32,
        pub globals: Vec<GlobalInfo>,
    }

    impl WaylandConn {
        pub fn from_stream(stream: UnixStream) -> Result<Self> {
            let mut conn = Self {
                stream,
                rx_buf: Vec::with_capacity(4096),
                next_id: 2,
                free_ids: VecDeque::new(),
                pending_events: VecDeque::new(),
                registry_id: 0,
                globals: Vec::new(),
            };
            conn.discover_globals()?;
            Ok(conn)
        }

        pub fn connect(socket_path: &Path) -> Result<Self> {
            let stream = UnixStream::connect(socket_path)
                .with_context(|| format!("connecting to Wayland socket {}", socket_path.display()))?;
            Self::from_stream(stream)
        }

        pub fn alloc_id(&mut self) -> u32 {
            if let Some(reused) = self.free_ids.pop_front() {
                reused
            } else {
                let id = self.next_id;
                self.next_id += 1;
                id
            }
        }

        pub fn send(&mut self, msg: MsgBuilder) -> Result<()> {
            use std::os::fd::AsRawFd;
            let (data, fds) = msg.finish();
            let fd_sock = self.stream.as_raw_fd();
            let mut sent = 0usize;
            let mut fds_sent = false;
            while sent < data.len() {
                let mut iov = libc::iovec {
                    iov_base: data[sent..].as_ptr() as *mut libc::c_void,
                    iov_len: data.len() - sent,
                };
                let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
                hdr.msg_iov = &mut iov;
                hdr.msg_iovlen = 1;

                let mut cbuf = CtrlBuf { words: [0; 8] };
                if !fds.is_empty() && !fds_sent {
                    let fd_bytes = std::mem::size_of_val(fds.as_slice());
                    let cmsg_space = unsafe { libc::CMSG_SPACE(fd_bytes as _) } as usize;
                    let max_ctrl = std::mem::size_of::<CtrlBuf>();
                    if cmsg_space > max_ctrl {
                        bail!("too many SCM_RIGHTS file descriptors");
                    }
                    hdr.msg_control = cbuf.words.as_mut_ptr().cast::<libc::c_void>();
                    hdr.msg_controllen = cmsg_space as _;
                    unsafe {
                        let cmsg = libc::CMSG_FIRSTHDR(&hdr);
                        (*cmsg).cmsg_level = libc::SOL_SOCKET;
                        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as _) as _;
                        std::ptr::copy_nonoverlapping(
                            fds.as_ptr().cast::<u8>(),
                            libc::CMSG_DATA(cmsg),
                            fd_bytes,
                        );
                    }
                    fds_sent = true;
                }

                let rc = unsafe { libc::sendmsg(fd_sock, &hdr, libc::MSG_NOSIGNAL) };
                if rc < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err).context("sendmsg on Wayland socket failed");
                }
                if rc == 0 {
                    bail!("Wayland socket closed on sendmsg");
                }
                sent += rc as usize;
            }
            Ok(())
        }

        pub fn bind_global(&mut self, global: &GlobalInfo, version: u32) -> Result<u32> {
            let new_id = self.alloc_id();
            let mut msg = MsgBuilder::new(self.registry_id, super::WL_REGISTRY_OP_BIND);
            msg.push_u32(global.name);
            msg.push_str(&global.interface);
            msg.push_u32(version);
            msg.push_u32(new_id);
            self.send(msg)?;
            Ok(new_id)
        }

        pub fn roundtrip(&mut self) -> Result<()> {
            let cb_id = self.alloc_id();
            let mut msg = MsgBuilder::new(super::WL_DISPLAY_ID, super::WL_DISPLAY_OP_SYNC);
            msg.push_u32(cb_id);
            self.send(msg)?;

            loop {
                let ev = self.next_event()?;
                if ev.obj_id == cb_id && ev.opcode == super::WL_CALLBACK_EV_DONE {
                    return Ok(());
                }
                self.pending_events.push_back(ev);
            }
        }

        pub fn next_event(&mut self) -> Result<RawEvent> {
            loop {
                if let Some(ev) = self.pending_events.pop_front() {
                    return Ok(ev);
                }
                if let Some(ev) = self.pop_buffered_event()? {
                    return Ok(ev);
                }
                self.fill_rx()?;
            }
        }

        pub fn next_event_nonqueued(&mut self) -> Result<RawEvent> {
            loop {
                if let Some(ev) = self.pop_buffered_event()? {
                    return Ok(ev);
                }
                self.fill_rx()?;
            }
        }

        fn fill_rx(&mut self) -> Result<()> {
            use std::io::Read;
            let mut chunk = [0u8; 4096];
            loop {
                match self.stream.read(&mut chunk) {
                    Ok(0) => bail!("Wayland compositor closed connection (EOF)"),
                    Ok(n) => {
                        self.rx_buf.extend_from_slice(&chunk[..n]);
                        return Ok(());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e).context("reading from Wayland socket"),
                }
            }
        }

        fn pop_buffered_event(&mut self) -> Result<Option<RawEvent>> {
            loop {
                if self.rx_buf.len() < 8 {
                    return Ok(None);
                }
                let obj_id = u32::from_ne_bytes(self.rx_buf[0..4].try_into().unwrap());
                let word1 = u32::from_ne_bytes(self.rx_buf[4..8].try_into().unwrap());
                let size = (word1 >> 16) as usize;
                let opcode = (word1 & 0xffff) as u16;
                if size < 8 || !size.is_multiple_of(4) {
                    bail!("malformed Wayland event header (obj={obj_id}, size={size})");
                }
                if self.rx_buf.len() < size {
                    return Ok(None);
                }
                let payload = self.rx_buf[8..size].to_vec();
                self.rx_buf.drain(..size);

                if obj_id == super::WL_DISPLAY_ID {
                    match opcode {
                        super::WL_DISPLAY_EV_ERROR => {
                            let mut off = 0;
                            let err_obj = read_u32(&payload, &mut off)?;
                            let code = read_u32(&payload, &mut off)?;
                            let msg = read_str(&payload, &mut off)?;
                            bail!(
                                "Wayland protocol error on object {err_obj} (code {code}): {msg}"
                            );
                        }
                        super::WL_DISPLAY_EV_DELETE_ID => {
                            let mut off = 0;
                            if let Ok(deleted) = read_u32(&payload, &mut off) {
                                self.free_ids.push_back(deleted);
                            }
                            continue;
                        }
                        _ => {}
                    }
                }
                return Ok(Some(RawEvent {
                    obj_id,
                    opcode,
                    payload,
                }));
            }
        }

        fn discover_globals(&mut self) -> Result<()> {
            let reg_id = self.alloc_id();
            self.registry_id = reg_id;
            let mut reg_msg =
                MsgBuilder::new(super::WL_DISPLAY_ID, super::WL_DISPLAY_OP_GET_REGISTRY);
            reg_msg.push_u32(reg_id);
            self.send(reg_msg)?;

            let cb_id = self.alloc_id();
            let mut sync_msg = MsgBuilder::new(super::WL_DISPLAY_ID, super::WL_DISPLAY_OP_SYNC);
            sync_msg.push_u32(cb_id);
            self.send(sync_msg)?;

            loop {
                let ev = self.next_event_nonqueued()?;
                if ev.obj_id == reg_id && ev.opcode == super::WL_REGISTRY_EV_GLOBAL {
                    let mut off = 0;
                    let name = read_u32(&ev.payload, &mut off)?;
                    let interface = read_str(&ev.payload, &mut off)?;
                    let version = read_u32(&ev.payload, &mut off)?;
                    self.globals.push(GlobalInfo {
                        name,
                        interface,
                        version,
                    });
                } else if ev.obj_id == cb_id && ev.opcode == super::WL_CALLBACK_EV_DONE {
                    break;
                } else {
                    self.pending_events.push_back(ev);
                }
            }
            Ok(())
        }
    }
}

pub use wire::resolve_wayland_socket_path;

/// Non-intrusive readiness check for `zwlr_screencopy_manager_v1` over Wayland.
pub fn probe_wlr_screencopy(
    runtime_dir: Option<&Path>,
    wayland_display: Option<&str>,
) -> std::result::Result<String, String> {
    let Some(sock) = resolve_wayland_socket_path(runtime_dir, wayland_display) else {
        return Err("WAYLAND_DISPLAY not set".into());
    };
    let conn = wire::WaylandConn::connect(&sock)
        .map_err(|e| format!("Wayland connect ({}) failed: {e}", sock.display()))?;
    let has_shm = conn.globals.iter().any(|g| g.interface == "wl_shm");
    let outputs = conn
        .globals
        .iter()
        .filter(|g| g.interface == "wl_output")
        .count();
    let mgr_ver = conn
        .globals
        .iter()
        .find(|g| g.interface == "zwlr_screencopy_manager_v1")
        .map(|g| g.version);
    if !has_shm {
        return Err("compositor lacks wl_shm".into());
    }
    if outputs == 0 {
        return Err("compositor reports 0 wl_output devices".into());
    }
    let Some(ver) = mgr_ver else {
        return Err("compositor does not advertise zwlr_screencopy_manager_v1".into());
    };
    Ok(format!(
        "wlr-screencopy v{ver} ready ({outputs} output(s), socket={})",
        sock.display()
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShmSpec {
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

struct MappedShmBuffer {
    spec: ShmSpec,
    pool_id: u32,
    buffer_id: u32,
    _file: File,
    ptr: NonNull<u8>,
    map_len: usize,
}

unsafe impl Send for MappedShmBuffer {}

impl MappedShmBuffer {
    fn create(conn: &mut wire::WaylandConn, shm_id: u32, spec: ShmSpec) -> Result<Self> {
        let len = (spec.stride as usize)
            .checked_mul(spec.height as usize)
            .context("shm buffer size overflow")?;
        if len == 0 {
            bail!("zero-sized wlr screencopy buffer");
        }
        let fd: OwnedFd = wire::create_memfd("usher-wlr-shm", len)?;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap shm memfd failed");
        }
        let non_null = NonNull::new(ptr.cast::<u8>()).context("mmap returned null")?;
        let file = File::from(fd);

        let pool_id = conn.alloc_id();
        let mut create_pool = wire::MsgBuilder::new(shm_id, WL_SHM_OP_CREATE_POOL);
        create_pool.push_u32(pool_id);
        create_pool.push_fd(file.as_raw_fd());
        create_pool.push_i32(len as i32);
        conn.send(create_pool)?;

        let buffer_id = conn.alloc_id();
        let mut create_buf = wire::MsgBuilder::new(pool_id, WL_SHM_POOL_OP_CREATE_BUFFER);
        create_buf.push_u32(buffer_id);
        create_buf.push_i32(0);
        create_buf.push_i32(spec.width as i32);
        create_buf.push_i32(spec.height as i32);
        create_buf.push_i32(spec.stride as i32);
        create_buf.push_u32(spec.format);
        conn.send(create_buf)?;

        Ok(Self {
            spec,
            pool_id,
            buffer_id,
            _file: file,
            ptr: non_null,
            map_len: len,
        })
    }

    fn destroy(self, conn: &mut wire::WaylandConn) {
        let _ = conn.send(wire::MsgBuilder::new(self.buffer_id, WL_BUFFER_OP_DESTROY));
        let _ = conn.send(wire::MsgBuilder::new(self.pool_id, WL_SHM_POOL_OP_DESTROY));
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.map_len) }
    }
}

impl Drop for MappedShmBuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast::<libc::c_void>(), self.map_len);
        }
    }
}

pub struct WlrScreencopyCapturer {
    conn: wire::WaylandConn,
    shm_id: u32,
    output_id: u32,
    mgr_id: u32,
    mgr_version: u32,
    shm_buf: Option<MappedShmBuffer>,
}

impl WlrScreencopyCapturer {
    pub fn open(socket_path: &Path, display_index: u8) -> Result<Self> {
        let conn = wire::WaylandConn::connect(socket_path)?;
        Self::from_conn(conn, display_index)
    }

    #[cfg(test)]
    pub fn from_stream(stream: UnixStream, display_index: u8) -> Result<Self> {
        let conn = wire::WaylandConn::from_stream(stream)?;
        Self::from_conn(conn, display_index)
    }

    fn from_conn(mut conn: wire::WaylandConn, display_index: u8) -> Result<Self> {
        let shm_global = conn
            .globals
            .iter()
            .find(|g| g.interface == "wl_shm")
            .cloned()
            .context("Wayland compositor does not advertise wl_shm")?;
        let mgr_global = conn
            .globals
            .iter()
            .find(|g| g.interface == "zwlr_screencopy_manager_v1")
            .cloned()
            .context("Wayland compositor does not advertise zwlr_screencopy_manager_v1")?;
        let outputs: Vec<_> = conn
            .globals
            .iter()
            .filter(|g| g.interface == "wl_output")
            .cloned()
            .collect();
        if outputs.is_empty() {
            bail!("Wayland compositor exposes 0 wl_output globals");
        }
        let chosen_output = &outputs[(display_index as usize).min(outputs.len() - 1)];

        let shm_id = conn.bind_global(&shm_global, 1)?;
        let output_id = conn.bind_global(chosen_output, chosen_output.version.min(4))?;
        let mgr_version = mgr_global.version.min(3);
        let mgr_id = conn.bind_global(&mgr_global, mgr_version)?;
        conn.roundtrip()?;

        Ok(Self {
            conn,
            shm_id,
            output_id,
            mgr_id,
            mgr_version,
            shm_buf: None,
        })
    }

    pub fn capture_frame(&mut self) -> Result<CpuFrame> {
        let frame_id = self.conn.alloc_id();
        let mut cap_msg = wire::MsgBuilder::new(self.mgr_id, WLR_MGR_OP_CAPTURE_OUTPUT);
        cap_msg.push_u32(frame_id);
        cap_msg.push_i32(1); // overlay_cursor = 1
        cap_msg.push_u32(self.output_id);
        self.conn.send(cap_msg)?;

        let mut chosen_spec: Option<ShmSpec> = None;
        let mut y_invert = false;
        let need_buffer_done = self.mgr_version >= 3;

        loop {
            let ev = self.conn.next_event()?;
            if ev.obj_id != frame_id {
                continue;
            }
            match ev.opcode {
                WLR_FRAME_EV_BUFFER => {
                    let mut off = 0;
                    let fmt = wire::read_u32(&ev.payload, &mut off)?;
                    let width = wire::read_u32(&ev.payload, &mut off)?;
                    let height = wire::read_u32(&ev.payload, &mut off)?;
                    let stride = wire::read_u32(&ev.payload, &mut off)?;
                    let is_supported = matches!(
                        fmt,
                        WL_SHM_FORMAT_ARGB8888
                            | WL_SHM_FORMAT_XRGB8888
                            | WL_SHM_FORMAT_ABGR8888
                            | WL_SHM_FORMAT_XBGR8888
                    );
                    if is_supported || chosen_spec.is_none() {
                        chosen_spec = Some(ShmSpec {
                            width,
                            height,
                            stride,
                            format: fmt,
                        });
                    }
                    if !need_buffer_done && is_supported {
                        break;
                    }
                }
                WLR_FRAME_EV_BUFFER_DONE => break,
                WLR_FRAME_EV_FLAGS => {
                    let mut off = 0;
                    let flags = wire::read_u32(&ev.payload, &mut off)?;
                    y_invert = (flags & WLR_FRAME_FLAG_Y_INVERT) != 0;
                }
                WLR_FRAME_EV_FAILED => {
                    let _ = self
                        .conn
                        .send(wire::MsgBuilder::new(frame_id, WLR_FRAME_OP_DESTROY));
                    bail!("wlr_screencopy frame failed before copy");
                }
                _ => {}
            }
        }

        let Some(spec) = chosen_spec else {
            let _ = self
                .conn
                .send(wire::MsgBuilder::new(frame_id, WLR_FRAME_OP_DESTROY));
            bail!("wlr_screencopy compositor offered no SHM buffer specification");
        };

        let pixel_fmt = match spec.format {
            WL_SHM_FORMAT_ARGB8888 | WL_SHM_FORMAT_XRGB8888 => PixelFormat::Bgra,
            WL_SHM_FORMAT_ABGR8888 | WL_SHM_FORMAT_XBGR8888 => PixelFormat::Rgba,
            other => {
                let _ = self
                    .conn
                    .send(wire::MsgBuilder::new(frame_id, WLR_FRAME_OP_DESTROY));
                bail!("unsupported wlr_screencopy WL_SHM format {other:#x}");
            }
        };

        if self.shm_buf.as_ref().is_none_or(|b| b.spec != spec) {
            if let Some(old) = self.shm_buf.take() {
                old.destroy(&mut self.conn);
            }
            self.shm_buf = Some(MappedShmBuffer::create(&mut self.conn, self.shm_id, spec)?);
        }

        let buf_id = self.shm_buf.as_ref().expect("shm buffer initialized").buffer_id;
        let mut copy_msg = wire::MsgBuilder::new(frame_id, WLR_FRAME_OP_COPY);
        copy_msg.push_u32(buf_id);
        self.conn.send(copy_msg)?;

        loop {
            let ev = self.conn.next_event()?;
            if ev.obj_id != frame_id {
                continue;
            }
            match ev.opcode {
                WLR_FRAME_EV_FLAGS => {
                    let mut off = 0;
                    let flags = wire::read_u32(&ev.payload, &mut off)?;
                    y_invert = (flags & WLR_FRAME_FLAG_Y_INVERT) != 0;
                }
                WLR_FRAME_EV_READY => break,
                WLR_FRAME_EV_FAILED => {
                    let _ = self
                        .conn
                        .send(wire::MsgBuilder::new(frame_id, WLR_FRAME_OP_DESTROY));
                    bail!("wlr_screencopy copy failed");
                }
                _ => {}
            }
        }

        let _ = self
            .conn
            .send(wire::MsgBuilder::new(frame_id, WLR_FRAME_OP_DESTROY));

        let buf = self.shm_buf.as_ref().expect("shm buffer valid");
        let raw = buf.as_slice();
        let row_bytes = (spec.width as usize) * 4;
        let stride = spec.stride as usize;
        let mut data = vec![0u8; row_bytes * spec.height as usize];
        for y in 0..spec.height as usize {
            let src_y = if y_invert {
                spec.height as usize - 1 - y
            } else {
                y
            };
            let src_off = src_y * stride;
            let dst_off = y * row_bytes;
            data[dst_off..dst_off + row_bytes]
                .copy_from_slice(&raw[src_off..src_off + row_bytes]);
        }

        Ok(CpuFrame {
            width: spec.width,
            height: spec.height,
            stride: row_bytes,
            format: pixel_fmt,
            data,
        })
    }
}

impl Drop for WlrScreencopyCapturer {
    fn drop(&mut self) {
        if let Some(buf) = self.shm_buf.take() {
            buf.destroy(&mut self.conn);
        }
        let _ = self
            .conn
            .send(wire::MsgBuilder::new(self.mgr_id, WLR_MGR_OP_DESTROY));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    fn read_wire_msg(stream: &mut UnixStream) -> (u32, u16, Vec<u8>, Option<OwnedFd>) {
        let mut hdr_buf = [0u8; 8];
        let mut cbuf = [0u64; 8];
        let mut iov = libc::iovec {
            iov_base: hdr_buf.as_mut_ptr().cast::<libc::c_void>(),
            iov_len: 8,
        };
        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        msghdr.msg_iov = &mut iov;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = cbuf.as_mut_ptr().cast::<libc::c_void>();
        msghdr.msg_controllen = std::mem::size_of_val(&cbuf) as _;

        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msghdr, 0) };
        assert_eq!(n, 8, "expected 8-byte Wayland message header");

        let mut received_fd = None;
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msghdr);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                {
                    let mut raw_fd: RawFd = -1;
                    std::ptr::copy_nonoverlapping(
                        libc::CMSG_DATA(cmsg),
                        (&mut raw_fd as *mut RawFd).cast::<u8>(),
                        std::mem::size_of::<RawFd>(),
                    );
                    if raw_fd >= 0 {
                        received_fd = Some(OwnedFd::from_raw_fd(raw_fd));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msghdr, cmsg);
            }
        }

        let obj_id = u32::from_ne_bytes(hdr_buf[0..4].try_into().unwrap());
        let word1 = u32::from_ne_bytes(hdr_buf[4..8].try_into().unwrap());
        let size = (word1 >> 16) as usize;
        let opcode = (word1 & 0xffff) as u16;
        let mut payload = vec![0u8; size - 8];
        if !payload.is_empty() {
            use std::io::Read;
            stream.read_exact(&mut payload).unwrap();
        }
        (obj_id, opcode, payload, received_fd)
    }

    fn send_wire_event(stream: &mut UnixStream, msg: wire::MsgBuilder) {
        let (bytes, _) = msg.finish();
        stream.write_all(&bytes).unwrap();
    }

    #[test]
    fn wlr_screencopy_captures_frame_over_mock_wayland_socket() {
        let (client_sock, mut server_sock) = UnixStream::pair().unwrap();

        let server = std::thread::spawn(move || {
            // 1. Initial discovery: get_registry + sync
            let (obj1, op1, pay1, _) = read_wire_msg(&mut server_sock);
            assert_eq!((obj1, op1), (WL_DISPLAY_ID, WL_DISPLAY_OP_GET_REGISTRY));
            let reg_id = u32::from_ne_bytes(pay1[0..4].try_into().unwrap());

            let (obj2, op2, pay2, _) = read_wire_msg(&mut server_sock);
            assert_eq!((obj2, op2), (WL_DISPLAY_ID, WL_DISPLAY_OP_SYNC));
            let cb_id = u32::from_ne_bytes(pay2[0..4].try_into().unwrap());

            for (name, iface, ver) in [
                (1u32, "wl_shm", 1u32),
                (2u32, "wl_output", 4u32),
                (3u32, "zwlr_screencopy_manager_v1", 3u32),
            ] {
                let mut g = wire::MsgBuilder::new(reg_id, WL_REGISTRY_EV_GLOBAL);
                g.push_u32(name);
                g.push_str(iface);
                g.push_u32(ver);
                send_wire_event(&mut server_sock, g);
            }
            let mut done = wire::MsgBuilder::new(cb_id, WL_CALLBACK_EV_DONE);
            done.push_u32(1);
            send_wire_event(&mut server_sock, done);

            // 2. Expect 3x bind + sync
            let (_, _, _, _) = read_wire_msg(&mut server_sock); // bind shm
            let (_, _, _, _) = read_wire_msg(&mut server_sock); // bind output
            let (_, _, bind_mgr_pay, _) = read_wire_msg(&mut server_sock); // bind mgr
            let mut off = 4;
            let _iface = wire::read_str(&bind_mgr_pay, &mut off).unwrap();
            let _ver = wire::read_u32(&bind_mgr_pay, &mut off).unwrap();
            let mgr_id = wire::read_u32(&bind_mgr_pay, &mut off).unwrap();

            let (_, op_s, sync_pay, _) = read_wire_msg(&mut server_sock);
            assert_eq!(op_s, WL_DISPLAY_OP_SYNC);
            let cb2 = u32::from_ne_bytes(sync_pay[0..4].try_into().unwrap());
            let mut done2 = wire::MsgBuilder::new(cb2, WL_CALLBACK_EV_DONE);
            done2.push_u32(2);
            send_wire_event(&mut server_sock, done2);

            // Frame 1 (normal orientation) & Frame 2 (Y_INVERT)
            let mut pool_fd: Option<OwnedFd> = None;
            for pass in 0..2 {
                let (obj, op, cap_pay, _) = read_wire_msg(&mut server_sock);
                assert_eq!((obj, op), (mgr_id, WLR_MGR_OP_CAPTURE_OUTPUT));
                let frame_id = u32::from_ne_bytes(cap_pay[0..4].try_into().unwrap());

                let mut ev_buf = wire::MsgBuilder::new(frame_id, WLR_FRAME_EV_BUFFER);
                ev_buf.push_u32(WL_SHM_FORMAT_ARGB8888);
                ev_buf.push_u32(2); // width
                ev_buf.push_u32(2); // height
                ev_buf.push_u32(8); // stride
                send_wire_event(&mut server_sock, ev_buf);

                let ev_done = wire::MsgBuilder::new(frame_id, WLR_FRAME_EV_BUFFER_DONE);
                send_wire_event(&mut server_sock, ev_done);

                if pass == 0 {
                    // First capture creates pool + buffer
                    let (_, op_p, _, fd) = read_wire_msg(&mut server_sock);
                    assert_eq!(op_p, WL_SHM_OP_CREATE_POOL);
                    pool_fd = fd;
                    let (_, op_b, _, _) = read_wire_msg(&mut server_sock);
                    assert_eq!(op_b, WL_SHM_POOL_OP_CREATE_BUFFER);
                }

                let (obj_c, op_c, _, _) = read_wire_msg(&mut server_sock);
                assert_eq!((obj_c, op_c), (frame_id, WLR_FRAME_OP_COPY));

                // Write 2 rows of 2 BGRA pixels into client's memfd
                let fd = pool_fd.as_ref().unwrap().as_raw_fd();
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        16,
                        libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        fd,
                        0,
                    )
                };
                assert_ne!(ptr, libc::MAP_FAILED);
                let pixels: [u8; 16] = [
                    // Row 0: blue, green
                    255, 0, 0, 255, 0, 255, 0, 255, // Row 1: red, white
                    0, 0, 255, 255, 255, 255, 255, 255,
                ];
                unsafe {
                    std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr.cast::<u8>(), 16);
                    libc::munmap(ptr, 16);
                }

                let mut ev_flags = wire::MsgBuilder::new(frame_id, WLR_FRAME_EV_FLAGS);
                ev_flags.push_u32(if pass == 1 { WLR_FRAME_FLAG_Y_INVERT } else { 0 });
                send_wire_event(&mut server_sock, ev_flags);

                let mut ev_ready = wire::MsgBuilder::new(frame_id, WLR_FRAME_EV_READY);
                ev_ready.push_u32(0);
                ev_ready.push_u32(1);
                ev_ready.push_u32(0);
                send_wire_event(&mut server_sock, ev_ready);

                let (obj_d, op_d, _, _) = read_wire_msg(&mut server_sock);
                assert_eq!((obj_d, op_d), (frame_id, WLR_FRAME_OP_DESTROY));
            }

            // Drain Drop destroys
            let _ = read_wire_msg(&mut server_sock);
        });

        let mut cap = WlrScreencopyCapturer::from_stream(client_sock, 0).unwrap();
        let f1 = cap.capture_frame().unwrap();
        assert_eq!((f1.width, f1.height, f1.format), (2, 2, PixelFormat::Bgra));
        assert_eq!(&f1.data[0..4], &[255, 0, 0, 255]); // Row 0 = blue
        assert_eq!(&f1.data[8..12], &[0, 0, 255, 255]); // Row 1 = red

        let f2 = cap.capture_frame().unwrap();
        assert_eq!(&f2.data[0..4], &[0, 0, 255, 255]); // Y_INVERT puts Row 1 first
        assert_eq!(&f2.data[8..12], &[255, 0, 0, 255]);

        drop(cap);
        server.join().unwrap();
    }
}
