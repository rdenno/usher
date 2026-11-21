use crate::config::TransportPreference;
use crate::decoder::{DecoderKind, FrameSink, Yuv420Frame, create_decoder};
use crate::ssh::{HostConnection, StdioTunnelIo};
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UdpSocket;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::AudioPacket;
use usher_proto::clipboard::{ClipboardReassembler, packetize_clipboard_text};
use usher_proto::crypto::{Role, SessionCrypto};
use usher_proto::cursor::{CursorPacket, CursorShape, CursorShapeReassembler};
use usher_proto::fragment::{CompletedFrame, FRAME_FLAG_KEYFRAME, FrameReassembler};
use usher_proto::handshake::{ChromaSubsampling, VideoCodec};
use usher_proto::input::InputEvent;
use usher_proto::packet::{
    ControlMessage, PacketKind, PingPayload, PongPayload, STREAM_STATUS_FLAG_STEPPED_DOWN,
    TUNNEL_MAGIC, decode_tunnel_frame_len, encode_tunnel_frame, max_plaintext_len, open_packet,
    seal_packet,
};

pub const TRANSPORT_UDP: u8 = 0;
pub const TRANSPORT_SSH_TCP: u8 = 1;

#[derive(Debug, Default)]
pub struct Telemetry {
    pub fps_x10: AtomicU32,
    pub rtt_us: AtomicU32,
    pub kbps: AtomicU32,
    pub fec_recovered: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub loss_after_fec_bps: AtomicU32,
    pub fec_recovery_bps: AtomicU32,
    pub target_bitrate_kbps: AtomicU32,
    pub max_bitrate_kbps: AtomicU32,
    pub fec_percent: AtomicU8,
    pub active_codec: AtomicU8,
    pub active_width: AtomicU16,
    pub active_height: AtomicU16,
    pub active_fps: AtomicU16,
    pub active_display: AtomicU8,
    pub active_prefer: AtomicU8,
    pub audio_enabled: AtomicBool,
    pub resolution_stepped_down: AtomicBool,
    pub chroma_444: AtomicBool,
    pub decoded_frames: AtomicU64,
    pub transport_mode: AtomicU8,
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    NewFrame,
    StreamStatusChanged,
    CursorUpdated,
    Toast(String),
    Shutdown,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteCursorState {
    pub has_pos: bool,
    pub visible: bool,
    pub norm_x: f32,
    pub norm_y: f32,
    pub shape_id: u32,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub rgba: Vec<u8>,
    last_pos_seq: u32,
}

impl RemoteCursorState {
    pub fn update_position(&mut self, seq: u32, visible: bool, x_norm: u16, y_norm: u16) -> bool {
        let is_newer = !self.has_pos
            || (seq != self.last_pos_seq && seq.wrapping_sub(self.last_pos_seq) < 0x8000_0000);
        if !is_newer {
            return false;
        }
        self.has_pos = true;
        self.last_pos_seq = seq;
        self.visible = visible;
        self.norm_x = x_norm as f32 / 65535.0;
        self.norm_y = y_norm as f32 / 65535.0;
        true
    }

    pub fn update_shape(&mut self, shape: CursorShape) {
        self.shape_id = shape.shape_id;
        self.width = shape.width;
        self.height = shape.height;
        self.hotspot_x = shape.hotspot_x;
        self.hotspot_y = shape.hotspot_y;
        self.rgba = shape.rgba;
    }

    pub fn has_shape(&self) -> bool {
        self.shape_id != 0 && self.width > 0 && self.height > 0 && !self.rgba.is_empty()
    }
}

fn emit_app_event(
    proxy: &Option<winit::event_loop::EventLoopProxy<AppEvent>>,
    ev: AppEvent,
) {
    if let Some(p) = proxy {
        let _ = p.send_event(ev);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DecoderSpec {
    codec: VideoCodec,
    width: u16,
    height: u16,
    chroma: ChromaSubsampling,
}

pub struct LatestSlot<T> {
    inner: Mutex<Option<T>>,
}

impl<T> LatestSlot<T> {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(None),
        })
    }

    /// Stores `val`, returning `true` if a previous unconsumed item was overwritten.
    pub fn push(&self, val: T) -> bool {
        let mut guard = self.inner.lock().expect("slot lock poisoned");
        guard.replace(val).is_some()
    }

    pub fn try_take(&self) -> Option<T> {
        self.inner.lock().expect("slot lock poisoned").take()
    }

    pub fn close(&self) {
        *self.inner.lock().expect("slot lock poisoned") = None;
    }
}

struct EncodedQueue {
    inner: Mutex<VecDeque<CompletedFrame>>,
    cv: Condvar,
    closed: AtomicBool,
    max_cap: usize,
}

impl EncodedQueue {
    fn new(max_cap: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::with_capacity(max_cap)),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
            max_cap: max_cap.max(1),
        })
    }

    /// Enqueues an encoded frame, evicting the oldest non-keyframe if full.
    /// Returns `true` if a queued frame had to be dropped.
    fn push(&self, val: CompletedFrame) -> bool {
        let mut guard = self.inner.lock().expect("encoded queue lock poisoned");
        let mut evicted = false;
        while guard.len() >= self.max_cap {
            let idx = guard
                .iter()
                .position(|f| (f.flags & FRAME_FLAG_KEYFRAME) == 0)
                .unwrap_or(0);
            guard.remove(idx);
            evicted = true;
        }
        guard.push_back(val);
        self.cv.notify_one();
        evicted
    }

    fn wait_take(&self) -> Option<CompletedFrame> {
        let mut guard = self.inner.lock().expect("encoded queue lock poisoned");
        loop {
            if let Some(v) = guard.pop_front() {
                return Some(v);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            guard = self.cv.wait(guard).expect("encoded queue condvar poisoned");
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cv.notify_all();
    }
}

pub struct ClientTransportHandles {
    pub input_tx: mpsc::Sender<InputEvent>,
    pub control_tx: mpsc::Sender<ControlMessage>,
    pub decoded_frame_slot: Arc<LatestSlot<Yuv420Frame>>,
    pub telemetry: Arc<Telemetry>,
}

enum ClientLinkRx {
    Udp {
        socket: Arc<UdpSocket>,
        initial_opened: Option<(PacketKind, Vec<u8>)>,
    },
    Tcp {
        rx: mpsc::Receiver<Vec<u8>>,
    },
}

enum ClientLinkTx {
    Udp(Arc<UdpSocket>),
    Tcp(Option<ChildStdin>),
}

impl ClientLinkTx {
    fn is_tcp(&self) -> bool {
        matches!(self, Self::Tcp(_))
    }

    async fn send_datagram(&mut self, dgram: &[u8]) {
        match self {
            Self::Udp(socket) => {
                let _ = socket.send(dgram).await;
            }
            Self::Tcp(Some(stdin)) => {
                if let Ok(framed) = encode_tunnel_frame(dgram)
                    && stdin.write_all(&framed).await.is_ok()
                {
                    let _ = stdin.flush().await;
                }
            }
            Self::Tcp(None) => {}
        }
    }

    fn close(&mut self) {
        if let Self::Tcp(ref mut stdin) = *self {
            drop(stdin.take());
        }
    }
}

pub async fn activate_stdio_tunnel(
    mut io: StdioTunnelIo,
    client_hello_pkt: &[u8],
) -> Result<(ChildStdin, BufReader<ChildStdout>)> {
    io.stdin
        .write_all(&TUNNEL_MAGIC)
        .await
        .context("writing TUNNEL_MAGIC to SSH stdin")?;
    let framed = encode_tunnel_frame(client_hello_pkt)?;
    io.stdin
        .write_all(&framed)
        .await
        .context("writing ClientHello to SSH tunnel")?;
    io.stdin.flush().await.context("flushing SSH tunnel stdin")?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut window = [0u8; 4];
    let mut seen = 0usize;
    tokio::time::timeout_at(deadline, async {
        loop {
            let b = io
                .stdout
                .read_u8()
                .await
                .context("SSH TCP tunnel closed before host activated tunnel")?;
            window.rotate_left(1);
            window[3] = b;
            seen += 1;
            if seen >= 4 && window == TUNNEL_MAGIC {
                return Ok::<(), anyhow::Error>(());
            }
            if seen > 8192 {
                bail!("exceeded 8KB waiting for host SSH TCP tunnel sentinel");
            }
        }
    })
    .await
    .context("timed out waiting 5s for host SSH TCP tunnel activation")??;

    Ok((io.stdin, io.stdout))
}

fn spawn_tcp_reader_task(
    mut stdout: BufReader<ChildStdout>,
    cancel: CancellationToken,
) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
    tokio::spawn(async move {
        loop {
            let mut hdr = [0u8; 2];
            let res = tokio::select! {
                _ = cancel.cancelled() => break,
                r = stdout.read_exact(&mut hdr) => r,
            };
            if res.is_err() {
                break;
            }
            let Ok(len) = decode_tunnel_frame_len(hdr) else {
                break;
            };
            let mut buf = vec![0u8; len];
            let res = tokio::select! {
                _ = cancel.cancelled() => break,
                r = stdout.read_exact(&mut buf) => r,
            };
            if res.is_err() || tx.send(buf).await.is_err() {
                break;
            }
        }
        cancel.cancel();
    });
    rx
}

struct RxState<'a> {
    telem: &'a Telemetry,
    ctrl_tx_fb: &'a mpsc::Sender<ControlMessage>,
    enc_slot: &'a Arc<EncodedQueue>,
    late_win: &'a Arc<AtomicU32>,
    desired_dec: &'a Arc<Mutex<DecoderSpec>>,
    decoder_kind: DecoderKind,
    audio_tx: &'a Option<mpsc::Sender<AudioPacket>>,
    audio_muted: &'a Arc<AtomicBool>,
    clipboard_in_tx: &'a Option<mpsc::Sender<String>>,
    cursor_state: &'a Option<Arc<Mutex<RemoteCursorState>>>,
    proxy_rx: &'a Option<winit::event_loop::EventLoopProxy<AppEvent>>,
    reassembler: FrameReassembler,
    clip_reasm: ClipboardReassembler,
    cursor_reasm: CursorShapeReassembler,
    pkts_ok_in_win: u64,
    fec_rec_in_win: u64,
    lost_in_win: u64,
    last_pli: Instant,
    derp_warned: bool,
    video_pkts_rx: u64,
    completed_frames_rx: u64,
    start_rx: Instant,
}

impl<'a> RxState<'a> {
    fn dispatch_packet(&mut self, kind: PacketKind, plaintext: &[u8]) {
        match kind {
            PacketKind::VideoData | PacketKind::VideoFec => {
                self.pkts_ok_in_win += 1;
                self.video_pkts_rx += 1;
                match self.reassembler.push_packet(kind, plaintext) {
                    Ok(outcome) => {
                        if outcome.packets_lost_unrecovered > 0 {
                            self.lost_in_win += outcome.packets_lost_unrecovered as u64;
                        }
                        if outcome.request_keyframe
                            && self.last_pli.elapsed() >= Duration::from_millis(150)
                        {
                            self.last_pli = Instant::now();
                            let _ = self.ctrl_tx_fb.try_send(ControlMessage::RequestKeyframe);
                        }
                        if let Some(frame) = outcome.completed {
                            self.completed_frames_rx += 1;
                            if self.completed_frames_rx == 1 {
                                tracing::info!(
                                    frame_id = frame.frame_id,
                                    bytes = frame.data.len(),
                                    keyframe = (frame.flags & FRAME_FLAG_KEYFRAME) != 0,
                                    "first video frame received"
                                );
                            }
                            if frame.fec_recovered > 0 {
                                self.fec_rec_in_win += frame.fec_recovered as u64;
                                self.telem
                                    .fec_recovered
                                    .fetch_add(frame.fec_recovered as u64, Ordering::Relaxed);
                            }
                            if self.enc_slot.push(frame) {
                                self.telem.frames_dropped.fetch_add(1, Ordering::Relaxed);
                                self.late_win.fetch_add(1, Ordering::AcqRel);
                                if self.last_pli.elapsed() >= Duration::from_millis(250) {
                                    self.last_pli = Instant::now();
                                    let _ = self
                                        .ctrl_tx_fb
                                        .try_send(ControlMessage::RequestKeyframe);
                                }
                            }
                        }
                    }
                    Err(err) => {
                        tracing::debug!("reassembler warning: {err}");
                    }
                }
            }
            PacketKind::Audio => {
                if let Some(atx) = self.audio_tx
                    && let Ok(pkt) = AudioPacket::decode(plaintext)
                {
                    let _ = atx.try_send(pkt);
                }
            }
            PacketKind::Clipboard => {
                if let Some(ctx) = self.clipboard_in_tx
                    && let Ok(Some(text)) = self.clip_reasm.push_chunk(plaintext)
                {
                    let _ = ctx.try_send(text);
                }
            }
            PacketKind::Cursor => {
                if let Ok(pkt) = CursorPacket::decode(plaintext)
                    && let Some(cs) = self.cursor_state
                {
                    let changed = match pkt {
                        CursorPacket::Position {
                            seq,
                            visible,
                            x_norm,
                            y_norm,
                        } => cs
                            .lock()
                            .expect("cursor state lock poisoned")
                            .update_position(seq, visible, x_norm, y_norm),
                        chunk @ CursorPacket::ShapeChunk { .. } => {
                            if let Ok(Some(shape)) = self.cursor_reasm.push_packet(chunk) {
                                cs.lock()
                                    .expect("cursor state lock poisoned")
                                    .update_shape(shape);
                                true
                            } else {
                                false
                            }
                        }
                    };
                    if changed {
                        emit_app_event(self.proxy_rx, AppEvent::CursorUpdated);
                    }
                }
            }
            PacketKind::Pong => {
                if let Ok(pong) = PongPayload::decode(plaintext) {
                    let now_us = self.start_rx.elapsed().as_micros() as u64;
                    let rtt = now_us.saturating_sub(pong.sent_us) as u32;
                    let prev = self.telem.rtt_us.load(Ordering::Acquire);
                    let smoothed = if prev == 0 { rtt } else { (prev * 3 + rtt) / 4 };
                    self.telem.rtt_us.store(smoothed, Ordering::Release);
                    if smoothed > 60_000
                        && !self.derp_warned
                        && self.telem.transport_mode.load(Ordering::Acquire) == TRANSPORT_UDP
                    {
                        self.derp_warned = true;
                        tracing::warn!(
                            rtt_ms = smoothed as f32 / 1000.0,
                            "high RTT detected (>60ms); overlay traffic may be relayed via DERP instead of direct UDP"
                        );
                    }
                }
            }
            PacketKind::Control => {
                if let Ok(ControlMessage::StreamStatus {
                    width,
                    height,
                    fps,
                    codec,
                    target_bitrate_kbps,
                    max_bitrate_kbps,
                    fec_percent,
                    display_index,
                    prefer,
                    audio_enabled,
                    flags,
                }) = ControlMessage::decode(plaintext)
                {
                    self.telem
                        .target_bitrate_kbps
                        .store(target_bitrate_kbps, Ordering::Release);
                    self.telem
                        .max_bitrate_kbps
                        .store(max_bitrate_kbps, Ordering::Release);
                    self.telem.fec_percent.store(fec_percent, Ordering::Release);
                    self.telem
                        .active_codec
                        .store(codec.to_u8(), Ordering::Release);
                    self.telem.active_width.store(width, Ordering::Release);
                    self.telem.active_height.store(height, Ordering::Release);
                    self.telem.active_fps.store(fps, Ordering::Release);
                    self.telem
                        .active_display
                        .store(display_index, Ordering::Release);
                    self.telem
                        .active_prefer
                        .store(prefer.to_u8(), Ordering::Release);
                    self.telem
                        .audio_enabled
                        .store(audio_enabled, Ordering::Release);
                    self.telem.resolution_stepped_down.store(
                        (flags & STREAM_STATUS_FLAG_STEPPED_DOWN) != 0,
                        Ordering::Release,
                    );
                    self.audio_muted.store(!audio_enabled, Ordering::Release);

                    let mut need_keyframe = false;
                    {
                        let mut guard =
                            self.desired_dec.lock().expect("decoder spec lock poisoned");
                        let uses_ffmpeg_pipe = self.decoder_kind == DecoderKind::Ffmpeg
                            || (self.decoder_kind == DecoderKind::Auto
                                && codec != VideoCodec::H264
                                && !crate::decoder::has_native_decoder(codec));
                        if guard.codec != codec
                            || (uses_ffmpeg_pipe
                                && (guard.width != width || guard.height != height))
                        {
                            tracing::info!(
                                old_codec = %guard.codec,
                                new_codec = %codec,
                                old_w = guard.width,
                                old_h = guard.height,
                                new_w = width,
                                new_h = height,
                                "host stream status changed; switching decoder"
                            );
                            need_keyframe = true;
                            *guard = DecoderSpec {
                                codec,
                                width,
                                height,
                                chroma: guard.chroma,
                            };
                        }
                    }
                    if need_keyframe {
                        let _ = self.ctrl_tx_fb.try_send(ControlMessage::RequestKeyframe);
                    }
                    emit_app_event(self.proxy_rx, AppEvent::StreamStatusChanged);
                }
            }
            other => {
                tracing::debug!(?other, "ignored packet kind from host");
            }
        }
    }
}

pub struct TransportConfig {
    pub transport_pref: TransportPreference,
    pub decoder_kind: DecoderKind,
    pub chroma: ChromaSubsampling,
    pub audio_tx: Option<mpsc::Sender<AudioPacket>>,
    pub audio_muted: Arc<AtomicBool>,
    pub mic_rx: Option<mpsc::Receiver<AudioPacket>>,
    pub clipboard_in_tx: Option<mpsc::Sender<String>>,
    pub clipboard_out_rx: mpsc::Receiver<String>,
    pub cursor_state: Option<Arc<Mutex<RemoteCursorState>>>,
    pub proxy: Option<winit::event_loop::EventLoopProxy<AppEvent>>,
    pub cancel: CancellationToken,
}

pub async fn start_transport(
    conn: &mut HostConnection,
    cfg: TransportConfig,
) -> Result<ClientTransportHandles> {
    let TransportConfig {
        transport_pref,
        decoder_kind,
        chroma,
        audio_tx,
        audio_muted,
        mut mic_rx,
        clipboard_in_tx,
        mut clipboard_out_rx,
        cursor_state,
        proxy,
        cancel,
    } = cfg;
    let hello = conn.hello.clone();
    let session_key = hello.decode_session_key()?;
    let crypto = Arc::new(Mutex::new(SessionCrypto::new(&session_key, Role::Client)));
    let mtu = hello.mtu as usize;
    let start = Instant::now();
    let telemetry = Arc::new(Telemetry::default());
    telemetry
        .target_bitrate_kbps
        .store(hello.bitrate_kbps, Ordering::Release);
    telemetry
        .max_bitrate_kbps
        .store(hello.max_bitrate_kbps, Ordering::Release);
    telemetry
        .fec_percent
        .store(hello.fec_percent, Ordering::Release);
    telemetry
        .active_codec
        .store(hello.codec.to_u8(), Ordering::Release);
    telemetry.active_width.store(hello.width, Ordering::Release);
    telemetry
        .active_height
        .store(hello.height, Ordering::Release);
    telemetry.active_fps.store(hello.fps, Ordering::Release);
    telemetry
        .active_display
        .store(hello.display_index, Ordering::Release);
    telemetry
        .active_prefer
        .store(hello.prefer.to_u8(), Ordering::Release);
    telemetry
        .audio_enabled
        .store(hello.audio, Ordering::Release);
    telemetry
        .chroma_444
        .store(hello.chroma == ChromaSubsampling::Yuv444, Ordering::Release);

    let (input_tx, mut input_rx) = mpsc::channel::<InputEvent>(256);
    let (control_tx, mut control_rx) = mpsc::channel::<ControlMessage>(32);

    let encoded_slot = EncodedQueue::new(8);
    let decoded_frame_slot = LatestSlot::<Yuv420Frame>::new();
    let decoded_frames = Arc::new(AtomicU64::new(0));
    let connected = Arc::new(AtomicBool::new(false));
    let late_frames_win = Arc::new(AtomicU32::new(0));
    let initial_spec = DecoderSpec {
        codec: hello.codec,
        width: hello.width,
        height: hello.height,
        chroma,
    };
    let desired_decoder = Arc::new(Mutex::new(initial_spec));

    let client_hello_msg = ControlMessage::ClientHello {
        client_version: 2,
        capabilities: 0x03,
    };
    let seal_client_hello = || {
        let mut guard = crypto.lock().expect("crypto lock poisoned");
        seal_packet(&mut guard, PacketKind::Control, &client_hello_msg.encode(), mtu)
    };

    let (link_rx, mut link_tx) = match transport_pref {
        TransportPreference::Tcp => {
            if !hello.tcp_tunnel {
                bail!("remote usher-host does not advertise SSH TCP tunnel support");
            }
            let io = conn
                .take_stdio_tunnel()
                .context("SSH stdio tunnel not available for --transport tcp")?;
            let pkt = seal_client_hello()?;
            let (stdin, stdout) = activate_stdio_tunnel(io, &pkt).await?;
            connected.store(true, Ordering::Release);
            telemetry
                .transport_mode
                .store(TRANSPORT_SSH_TCP, Ordering::Release);
            tracing::info!("transport active: SSH TCP tunnel (--transport tcp)");
            let rx = spawn_tcp_reader_task(stdout, cancel.clone());
            (ClientLinkRx::Tcp { rx }, ClientLinkTx::Tcp(Some(stdin)))
        }
        TransportPreference::Auto | TransportPreference::Udp => {
            let remote_addr = SocketAddr::new(hello.bind_ip, hello.port);
            let local_bind = match hello.bind_ip {
                IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
                IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
            };
            let udp = UdpSocket::bind(local_bind)
                .await
                .context("binding client UDP socket")?;
            {
                let sock_ref = socket2::SockRef::from(&udp);
                let _ = sock_ref.set_recv_buffer_size(4 * 1024 * 1024);
                let _ = sock_ref.set_send_buffer_size(1024 * 1024);
            }
            let socket = Arc::new(udp);
            socket
                .connect(remote_addr)
                .await
                .with_context(|| format!("connecting UDP socket to {remote_addr}"))?;

            let probe_pkt = seal_client_hello()?;
            let _ = socket.send(&probe_pkt).await;

            let mut probe_buf = vec![0u8; 4096];
            let mut initial_opened: Option<(PacketKind, Vec<u8>)> = None;
            let probe_deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
            let mut probe_tick = tokio::time::interval(Duration::from_millis(200));
            probe_tick.tick().await;

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => bail!("cancelled while negotiating transport"),
                    _ = tokio::time::sleep_until(probe_deadline) => break,
                    _ = probe_tick.tick() => {
                        let _ = socket.send(&probe_pkt).await;
                    }
                    recv = socket.recv(&mut probe_buf) => {
                        if let Ok(n) = recv {
                            let opened = {
                                let mut guard = crypto.lock().expect("crypto lock poisoned");
                                open_packet(&mut guard, &probe_buf[..n])
                            };
                            if let Ok((kind, _seq, pt)) = opened {
                                initial_opened = Some((kind, pt));
                                break;
                            }
                        }
                    }
                }
            }

            if let Some((kind, pt)) = initial_opened {
                connected.store(true, Ordering::Release);
                telemetry
                    .transport_mode
                    .store(TRANSPORT_UDP, Ordering::Release);
                tracing::info!("transport active: direct UDP");
                (
                    ClientLinkRx::Udp {
                        socket: socket.clone(),
                        initial_opened: Some((kind, pt)),
                    },
                    ClientLinkTx::Udp(socket),
                )
            } else if transport_pref == TransportPreference::Auto
                && hello.tcp_tunnel
                && let Some(io) = conn.take_stdio_tunnel()
            {
                tracing::warn!(
                    "direct UDP unreachable after 2.5s; falling back to SSH TCP tunnel (higher latency, works through strict firewalls / ProxyJump)"
                );
                let tunnel_pkt = seal_client_hello()?;
                let (stdin, stdout) = activate_stdio_tunnel(io, &tunnel_pkt).await?;
                connected.store(true, Ordering::Release);
                telemetry
                    .transport_mode
                    .store(TRANSPORT_SSH_TCP, Ordering::Release);
                let rx = spawn_tcp_reader_task(stdout, cancel.clone());
                (ClientLinkRx::Tcp { rx }, ClientLinkTx::Tcp(Some(stdin)))
            } else {
                tracing::warn!(
                    transport = %transport_pref,
                    tcp_tunnel = hello.tcp_tunnel,
                    "direct UDP probe received no reply within 2.5s; continuing UDP retries"
                );
                (
                    ClientLinkRx::Udp {
                        socket: socket.clone(),
                        initial_opened: None,
                    },
                    ClientLinkTx::Udp(socket),
                )
            }
        }
    };

    // Dedicated video decoder worker thread (never stalls RX or UI)
    {
        let enc_slot = encoded_slot.clone();
        let dec_slot = decoded_frame_slot.clone();
        let dec_count = decoded_frames.clone();
        let telem = telemetry.clone();
        let ctrl_tx_dec = control_tx.clone();
        let proxy_dec = proxy.clone();
        let cancel_dec = cancel.clone();
        let desired_dec = desired_decoder.clone();

        std::thread::spawn(move || {
            let fps_win = Arc::new(Mutex::new((0u32, Instant::now())));
            let on_frame: FrameSink = {
                let dec_slot = dec_slot.clone();
                let dec_count = dec_count.clone();
                let telem = telem.clone();
                let proxy_dec = proxy_dec.clone();
                let fps_win = fps_win.clone();
                Arc::new(move |yuv| {
                    let prev = dec_count.fetch_add(1, Ordering::AcqRel);
                    telem.decoded_frames.store(prev + 1, Ordering::Release);
                    if prev == 0 {
                        tracing::info!(
                            width = yuv.width,
                            height = yuv.height,
                            "first video frame decoded"
                        );
                    }
                    let _ = dec_slot.push(yuv);
                    emit_app_event(&proxy_dec, AppEvent::NewFrame);
                    let mut st = fps_win.lock().expect("fps window lock poisoned");
                    st.0 += 1;
                    let dt = st.1.elapsed();
                    if dt >= Duration::from_millis(500) {
                        let fps_x10 = ((st.0 as f64 / dt.as_secs_f64()) * 10.0).round() as u32;
                        telem.fps_x10.store(fps_x10, Ordering::Release);
                        st.0 = 0;
                        st.1 = Instant::now();
                    }
                })
            };

            let mut active_spec = initial_spec;
            let mut hwaccel_enabled = true;
            let mut decoder = match create_decoder(
                active_spec.codec,
                decoder_kind,
                active_spec.chroma,
                active_spec.width,
                active_spec.height,
                hwaccel_enabled,
                on_frame.clone(),
            ) {
                Ok(d) => d,
                Err(err) => {
                    tracing::warn!(
                        "hwaccel decoder init failed ({err:#}); falling back to software decode"
                    );
                    hwaccel_enabled = false;
                    match create_decoder(
                        active_spec.codec,
                        decoder_kind,
                        active_spec.chroma,
                        active_spec.width,
                        active_spec.height,
                        false,
                        on_frame.clone(),
                    ) {
                        Ok(d) => d,
                        Err(err2) => {
                            tracing::error!("failed to initialize video decoder: {err2:#}");
                            cancel_dec.cancel();
                            emit_app_event(&proxy_dec, AppEvent::Shutdown);
                            return;
                        }
                    }
                }
            };

            let mut last_frame_id: Option<u32> = None;
            let mut awaiting_keyframe = true;
            let mut last_dec_pli = Instant::now() - Duration::from_secs(1);

            while !cancel_dec.is_cancelled() {
                let Some(au) = enc_slot.wait_take() else {
                    break;
                };

                let target_spec = *desired_dec.lock().expect("decoder spec lock poisoned");
                if target_spec != active_spec {
                    drop(decoder);
                    let next_dec = create_decoder(
                        target_spec.codec,
                        decoder_kind,
                        target_spec.chroma,
                        target_spec.width,
                        target_spec.height,
                        hwaccel_enabled,
                        on_frame.clone(),
                    )
                    .or_else(|err| {
                        if hwaccel_enabled {
                            tracing::warn!(
                                "hwaccel decoder switch failed ({err:#}); falling back to software decode"
                            );
                            hwaccel_enabled = false;
                            create_decoder(
                                target_spec.codec,
                                decoder_kind,
                                target_spec.chroma,
                                target_spec.width,
                                target_spec.height,
                                false,
                                on_frame.clone(),
                            )
                        } else {
                            Err(err)
                        }
                    });
                    match next_dec {
                        Ok(new_dec) => {
                            tracing::info!(
                                codec = %target_spec.codec,
                                w = target_spec.width,
                                h = target_spec.height,
                                hwaccel = hwaccel_enabled,
                                "switched client video decoder"
                            );
                            decoder = new_dec;
                            active_spec = target_spec;
                            awaiting_keyframe = true;
                            last_frame_id = None;
                        }
                        Err(err) => {
                            tracing::error!("failed to switch video decoder: {err:#}");
                            cancel_dec.cancel();
                            emit_app_event(&proxy_dec, AppEvent::Shutdown);
                            return;
                        }
                    }
                }

                let is_key = (au.flags & FRAME_FLAG_KEYFRAME) != 0;
                if let Some(prev) = last_frame_id
                    && au.frame_id != prev.wrapping_add(1)
                    && !is_key
                {
                    awaiting_keyframe = true;
                    if last_dec_pli.elapsed() >= Duration::from_millis(250) {
                        last_dec_pli = Instant::now();
                        let _ = ctrl_tx_dec.try_send(ControlMessage::RequestKeyframe);
                    }
                }
                last_frame_id = Some(au.frame_id);
                if awaiting_keyframe {
                    if !is_key {
                        if last_dec_pli.elapsed() >= Duration::from_millis(250) {
                            last_dec_pli = Instant::now();
                            let _ = ctrl_tx_dec.try_send(ControlMessage::RequestKeyframe);
                        }
                        continue;
                    }
                    awaiting_keyframe = false;
                }

                if let Err(err) = decoder.decode(&au.data) {
                    tracing::warn!(
                        hwaccel = hwaccel_enabled,
                        "video decoder error ({err:#}); recreating decoder"
                    );
                    if hwaccel_enabled {
                        hwaccel_enabled = false;
                    }
                    drop(decoder);
                    match create_decoder(
                        active_spec.codec,
                        decoder_kind,
                        active_spec.chroma,
                        active_spec.width,
                        active_spec.height,
                        hwaccel_enabled,
                        on_frame.clone(),
                    ) {
                        Ok(new_dec) => {
                            decoder = new_dec;
                            awaiting_keyframe = true;
                            last_frame_id = None;
                            let _ = ctrl_tx_dec.try_send(ControlMessage::RequestKeyframe);
                        }
                        Err(err2) => {
                            tracing::error!("failed to recreate video decoder: {err2:#}");
                            cancel_dec.cancel();
                            emit_app_event(&proxy_dec, AppEvent::Shutdown);
                            return;
                        }
                    }
                }
            }
        });
    }

    // Receive + 500ms feedback loop (UDP or SSH TCP tunnel)
    {
        let cry = crypto.clone();
        let enc_slot = encoded_slot.clone();
        let decoded_rx = decoded_frames;
        let telem = telemetry.clone();
        let ctrl_tx_fb = control_tx.clone();
        let connected_rx = connected.clone();
        let late_win = late_frames_win.clone();
        let desired_dec = desired_decoder.clone();
        let proxy_rx = proxy.clone();
        let cancel_rx = cancel.clone();
        let start_rx = start;

        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut bytes_in_window = 0u64;
            let mut bw_window_start = Instant::now();
            let mut connected_at: Option<Instant> = if connected_rx.load(Ordering::Acquire) {
                Some(Instant::now())
            } else {
                None
            };
            let mut stalled_warned = false;
            let mut fb_tick = tokio::time::interval(Duration::from_millis(500));

            let mut state = RxState {
                telem: &telem,
                ctrl_tx_fb: &ctrl_tx_fb,
                enc_slot: &enc_slot,
                late_win: &late_win,
                desired_dec: &desired_dec,
                decoder_kind,
                audio_tx: &audio_tx,
                audio_muted: &audio_muted,
                clipboard_in_tx: &clipboard_in_tx,
                cursor_state: &cursor_state,
                proxy_rx: &proxy_rx,
                reassembler: FrameReassembler::new(8),
                clip_reasm: ClipboardReassembler::new(),
                cursor_reasm: CursorShapeReassembler::new(),
                pkts_ok_in_win: 0,
                fec_rec_in_win: 0,
                lost_in_win: 0,
                last_pli: Instant::now() - Duration::from_secs(1),
                derp_warned: false,
                video_pkts_rx: 0,
                completed_frames_rx: 0,
                start_rx,
            };

            let mut rx_source = link_rx;
            if let ClientLinkRx::Udp {
                ref mut initial_opened,
                ..
            } = rx_source
                && let Some((kind, pt)) = initial_opened.take()
            {
                state.dispatch_packet(kind, &pt);
            }

            loop {
                tokio::select! {
                    _ = cancel_rx.cancelled() => break,
                    _ = fb_tick.tick() => {
                        if !stalled_warned
                            && let Some(t) = connected_at
                            && t.elapsed() >= Duration::from_secs(4)
                            && decoded_rx.load(Ordering::Acquire) == 0
                        {
                            stalled_warned = true;
                            tracing::warn!(
                                video_pkts_rx = state.video_pkts_rx,
                                completed_frames_rx = state.completed_frames_rx,
                                active_codec = telem.active_codec.load(Ordering::Acquire),
                                "no video frames decoded after 4s; check [host] logs above (on Windows hosts, ensure the desktop is unlocked)"
                            );
                        }
                        let bw_elapsed = bw_window_start.elapsed();
                        let secs = bw_elapsed.as_secs_f64().max(0.05);
                        let kbps = ((bytes_in_window as f64 * 8.0 / secs) / 1000.0) as u32;
                        telem.kbps.store(kbps, Ordering::Release);

                        let total_pkts = state.pkts_ok_in_win + state.lost_in_win;
                        let loss_bps = (state.lost_in_win * 10_000)
                            .checked_div(total_pkts)
                            .map_or(0, |v| v.min(10_000) as u16);
                        let rec_bps = (state.fec_rec_in_win * 10_000)
                            .checked_div(state.pkts_ok_in_win)
                            .map_or(0, |v| v.min(10_000) as u16);
                        let late = late_win.swap(0, Ordering::AcqRel).min(u16::MAX as u32) as u16;
                        telem
                            .loss_after_fec_bps
                            .store(loss_bps as u32, Ordering::Release);
                        telem
                            .fec_recovery_bps
                            .store(rec_bps as u32, Ordering::Release);

                        if connected_rx.load(Ordering::Acquire) {
                            let rtt_us = telem.rtt_us.load(Ordering::Acquire);
                            let _ = ctrl_tx_fb.try_send(ControlMessage::NetworkFeedback {
                                loss_after_fec_bps: loss_bps,
                                fec_recovery_bps: rec_bps,
                                rtt_us,
                                late_frames: late,
                                recv_kbps: kbps,
                            });
                        }

                        bytes_in_window = 0;
                        state.pkts_ok_in_win = 0;
                        state.fec_rec_in_win = 0;
                        state.lost_in_win = 0;
                        bw_window_start = Instant::now();
                    }
                    pkt_res = async {
                        match &mut rx_source {
                            ClientLinkRx::Udp { socket, .. } => {
                                match socket.recv(&mut buf).await {
                                    Ok(n) => Some(buf[..n].to_vec()),
                                    Err(err) => {
                                        tracing::debug!("UDP recv error: {err}");
                                        Some(Vec::new())
                                    }
                                }
                            }
                            ClientLinkRx::Tcp { rx } => rx.recv().await,
                        }
                    } => {
                        let Some(raw) = pkt_res else {
                            tracing::info!("SSH TCP tunnel closed by remote host");
                            cancel_rx.cancel();
                            break;
                        };
                        if raw.is_empty() {
                            continue;
                        }
                        bytes_in_window += raw.len() as u64;

                        let opened = {
                            let mut guard = cry.lock().expect("crypto lock poisoned");
                            open_packet(&mut guard, &raw)
                        };
                        let (kind, _seq, plaintext) = match opened {
                            Ok(v) => {
                                connected_rx.store(true, Ordering::Release);
                                if connected_at.is_none() {
                                    connected_at = Some(Instant::now());
                                }
                                v
                            }
                            Err(err) => {
                                tracing::debug!("dropped invalid/replayed packet: {err}");
                                continue;
                            }
                        };

                        state.dispatch_packet(kind, &plaintext);
                    }
                }
            }

            enc_slot.close();
        });
    }

    // Transmit + keepalive + ClientHello retry loop (UDP or SSH TCP tunnel)
    {
        let cry = crypto;
        let cancel_tx = cancel.clone();
        let connected_tx = connected;
        let start_tx = start;

        tokio::spawn(async move {
            let hello_msg = ControlMessage::ClientHello {
                client_version: 2,
                capabilities: 0x03,
            };
            if !link_tx.is_tcp() {
                send_sealed(&mut link_tx, &cry, PacketKind::Control, &hello_msg.encode(), mtu)
                    .await;
            }

            let mut ping_tick = tokio::time::interval(Duration::from_secs(1));
            let mut retry_tick = tokio::time::interval(Duration::from_millis(200));
            let mut clip_transfer_id = 1u32;
            let clip_max_pt = max_plaintext_len(mtu);

            loop {
                tokio::select! {
                    _ = cancel_tx.cancelled() => {
                        flush_inputs_and_disconnect(&mut link_tx, &cry, &mut input_rx, mtu).await;
                        link_tx.close();
                        break;
                    }
                    _ = retry_tick.tick() => {
                        if !connected_tx.load(Ordering::Acquire) && !link_tx.is_tcp() {
                            send_sealed(&mut link_tx, &cry, PacketKind::Control, &hello_msg.encode(), mtu).await;
                        }
                    }
                    _ = ping_tick.tick() => {
                        let ping = PingPayload {
                            sent_us: start_tx.elapsed().as_micros() as u64,
                        };
                        send_sealed(&mut link_tx, &cry, PacketKind::Ping, &ping.encode(), mtu).await;
                    }
                    Some(ev) = input_rx.recv() => {
                        send_sealed(&mut link_tx, &cry, PacketKind::Input, &ev.encode(), mtu).await;
                    }
                    Some(pkt) = async {
                        match &mut mic_rx {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        send_sealed(&mut link_tx, &cry, PacketKind::MicAudio, &pkt.encode(), mtu).await;
                    }
                    Some(text) = clipboard_out_rx.recv() => {
                        let tid = clip_transfer_id;
                        clip_transfer_id = clip_transfer_id.wrapping_add(1).max(1);
                        let passes = if link_tx.is_tcp() { 1 } else { 2 };
                        for chunk in packetize_clipboard_text(tid, &text, clip_max_pt) {
                            for _ in 0..passes {
                                send_sealed(&mut link_tx, &cry, PacketKind::Clipboard, &chunk, mtu).await;
                            }
                        }
                    }
                    Some(ctrl) = control_rx.recv() => {
                        if matches!(ctrl, ControlMessage::Disconnect) {
                            flush_inputs_and_disconnect(&mut link_tx, &cry, &mut input_rx, mtu).await;
                            link_tx.close();
                            cancel_tx.cancel();
                            break;
                        }
                        send_sealed(&mut link_tx, &cry, PacketKind::Control, &ctrl.encode(), mtu).await;
                    }
                }
            }
        });
    }

    Ok(ClientTransportHandles {
        input_tx,
        control_tx,
        decoded_frame_slot,
        telemetry,
    })
}

async fn flush_inputs_and_disconnect(
    link_tx: &mut ClientLinkTx,
    crypto: &Mutex<SessionCrypto>,
    input_rx: &mut mpsc::Receiver<InputEvent>,
    mtu: usize,
) {
    let mut saw_release_all = false;
    for _ in 0..64 {
        match input_rx.try_recv() {
            Ok(ev) => {
                if matches!(ev, InputEvent::ReleaseAll) {
                    saw_release_all = true;
                }
                send_sealed(link_tx, crypto, PacketKind::Input, &ev.encode(), mtu).await;
            }
            Err(_) => break,
        }
    }
    if !saw_release_all {
        send_sealed(
            link_tx,
            crypto,
            PacketKind::Input,
            &InputEvent::ReleaseAll.encode(),
            mtu,
        )
        .await;
    }
    send_sealed(
        link_tx,
        crypto,
        PacketKind::Control,
        &ControlMessage::Disconnect.encode(),
        mtu,
    )
    .await;
}

async fn send_sealed(
    link_tx: &mut ClientLinkTx,
    crypto: &Mutex<SessionCrypto>,
    kind: PacketKind,
    plaintext: &[u8],
    mtu: usize,
) {
    let sealed = {
        let mut guard = crypto.lock().expect("crypto lock poisoned");
        seal_packet(&mut guard, kind, plaintext, mtu)
    };
    if let Ok(dgram) = sealed {
        link_tx.send_datagram(&dgram).await;
    }
}
