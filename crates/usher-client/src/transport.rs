use crate::decoder::{DecoderKind, FrameSink, Yuv420Frame, create_decoder};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::AudioPacket;
use usher_proto::clipboard::{ClipboardReassembler, packetize_clipboard_text};
use usher_proto::crypto::{Role, SessionCrypto};
use usher_proto::fragment::{CompletedFrame, FRAME_FLAG_KEYFRAME, FrameReassembler};
use usher_proto::handshake::{HelloMessage, VideoCodec};
use usher_proto::input::InputEvent;
use usher_proto::packet::{
    ControlMessage, PacketKind, PingPayload, PongPayload, STREAM_STATUS_FLAG_STEPPED_DOWN,
    max_plaintext_len, open_packet, seal_packet,
};

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
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    NewFrame,
    StreamStatusChanged,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DecoderSpec {
    codec: VideoCodec,
    width: u16,
    height: u16,
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

#[allow(clippy::too_many_arguments)]
pub async fn start_transport(
    hello: HelloMessage,
    decoder_kind: DecoderKind,
    audio_tx: Option<mpsc::Sender<AudioPacket>>,
    audio_muted: Arc<AtomicBool>,
    clipboard_in_tx: Option<mpsc::Sender<String>>,
    mut clipboard_out_rx: mpsc::Receiver<String>,
    proxy: winit::event_loop::EventLoopProxy<AppEvent>,
    cancel: CancellationToken,
) -> Result<ClientTransportHandles> {
    let session_key = hello.decode_session_key()?;
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
    };
    let desired_decoder = Arc::new(Mutex::new(initial_spec));

    // Dedicated video decoder worker thread (never stalls UDP rx or UI)
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
                    if prev == 0 {
                        tracing::info!(
                            width = yuv.width,
                            height = yuv.height,
                            "first video frame decoded"
                        );
                    }
                    let _ = dec_slot.push(yuv);
                    let _ = proxy_dec.send_event(AppEvent::NewFrame);
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
                        active_spec.width,
                        active_spec.height,
                        false,
                        on_frame.clone(),
                    ) {
                        Ok(d) => d,
                        Err(err2) => {
                            tracing::error!("failed to initialize video decoder: {err2:#}");
                            cancel_dec.cancel();
                            let _ = proxy_dec.send_event(AppEvent::Shutdown);
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
                            let _ = proxy_dec.send_event(AppEvent::Shutdown);
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
                            let _ = proxy_dec.send_event(AppEvent::Shutdown);
                            return;
                        }
                    }
                }
            }
        });
    }

    // UDP receive + 500ms feedback loop
    {
        let sock = socket.clone();
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
            let mut reassembler = FrameReassembler::new(8);
            let mut clip_reasm = ClipboardReassembler::new();
            let mut bytes_in_window = 0u64;
            let mut pkts_ok_in_win = 0u64;
            let mut fec_rec_in_win = 0u64;
            let mut lost_in_win = 0u64;
            let mut bw_window_start = Instant::now();
            let mut last_pli = Instant::now() - Duration::from_secs(1);
            let mut derp_warned = false;
            let mut video_pkts_rx = 0u64;
            let mut completed_frames_rx = 0u64;
            let mut connected_at: Option<Instant> = None;
            let mut stalled_warned = false;
            let mut fb_tick = tokio::time::interval(Duration::from_millis(500));

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
                                video_pkts_rx,
                                completed_frames_rx,
                                active_codec = telem.active_codec.load(Ordering::Acquire),
                                "no video frames decoded after 4s; check [host] logs above (on Windows hosts, ensure the desktop is unlocked)"
                            );
                        }
                        let bw_elapsed = bw_window_start.elapsed();
                        let secs = bw_elapsed.as_secs_f64().max(0.05);
                        let kbps = ((bytes_in_window as f64 * 8.0 / secs) / 1000.0) as u32;
                        telem.kbps.store(kbps, Ordering::Release);

                        let total_pkts = pkts_ok_in_win + lost_in_win;
                        let loss_bps = (lost_in_win * 10_000)
                            .checked_div(total_pkts)
                            .map_or(0, |v| v.min(10_000) as u16);
                        let rec_bps = (fec_rec_in_win * 10_000)
                            .checked_div(pkts_ok_in_win)
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
                        pkts_ok_in_win = 0;
                        fec_rec_in_win = 0;
                        lost_in_win = 0;
                        bw_window_start = Instant::now();
                    }
                    recv = sock.recv(&mut buf) => {
                        let n = match recv {
                            Ok(n) => n,
                            Err(err) => {
                                tracing::debug!("UDP recv error: {err}");
                                continue;
                            }
                        };
                        bytes_in_window += n as u64;

                        let opened = {
                            let mut guard = cry.lock().expect("crypto lock poisoned");
                            open_packet(&mut guard, &buf[..n])
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

                        match kind {
                            PacketKind::VideoData | PacketKind::VideoFec => {
                                pkts_ok_in_win += 1;
                                video_pkts_rx += 1;
                                match reassembler.push_packet(kind, &plaintext) {
                                    Ok(outcome) => {
                                        if outcome.packets_lost_unrecovered > 0 {
                                            lost_in_win += outcome.packets_lost_unrecovered as u64;
                                        }
                                        if outcome.request_keyframe
                                            && last_pli.elapsed() >= Duration::from_millis(150)
                                        {
                                            last_pli = Instant::now();
                                            let _ = ctrl_tx_fb
                                                .try_send(ControlMessage::RequestKeyframe);
                                        }
                                        if let Some(frame) = outcome.completed {
                                            completed_frames_rx += 1;
                                            if completed_frames_rx == 1 {
                                                tracing::info!(
                                                    frame_id = frame.frame_id,
                                                    bytes = frame.data.len(),
                                                    keyframe = (frame.flags & FRAME_FLAG_KEYFRAME) != 0,
                                                    "first video frame received"
                                                );
                                            }
                                            if frame.fec_recovered > 0 {
                                                fec_rec_in_win += frame.fec_recovered as u64;
                                                telem.fec_recovered.fetch_add(
                                                    frame.fec_recovered as u64,
                                                    Ordering::Relaxed,
                                                );
                                            }
                                            if enc_slot.push(frame) {
                                                telem.frames_dropped.fetch_add(1, Ordering::Relaxed);
                                                late_win.fetch_add(1, Ordering::AcqRel);
                                                if last_pli.elapsed() >= Duration::from_millis(250) {
                                                    last_pli = Instant::now();
                                                    let _ = ctrl_tx_fb
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
                                if let Some(ref atx) = audio_tx
                                    && let Ok(pkt) = AudioPacket::decode(&plaintext)
                                {
                                    let _ = atx.try_send(pkt);
                                }
                            }
                            PacketKind::Clipboard => {
                                if let Some(ref ctx) = clipboard_in_tx
                                    && let Ok(Some(text)) = clip_reasm.push_chunk(&plaintext)
                                {
                                    let _ = ctx.try_send(text);
                                }
                            }
                            PacketKind::Pong => {
                                if let Ok(pong) = PongPayload::decode(&plaintext) {
                                    let now_us = start_rx.elapsed().as_micros() as u64;
                                    let rtt = now_us.saturating_sub(pong.sent_us) as u32;
                                    let prev = telem.rtt_us.load(Ordering::Acquire);
                                    let smoothed = if prev == 0 {
                                        rtt
                                    } else {
                                        (prev * 3 + rtt) / 4
                                    };
                                    telem.rtt_us.store(smoothed, Ordering::Release);
                                    if smoothed > 60_000 && !derp_warned {
                                        derp_warned = true;
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
                                }) = ControlMessage::decode(&plaintext)
                                {
                                    telem
                                        .target_bitrate_kbps
                                        .store(target_bitrate_kbps, Ordering::Release);
                                    telem
                                        .max_bitrate_kbps
                                        .store(max_bitrate_kbps, Ordering::Release);
                                    telem.fec_percent.store(fec_percent, Ordering::Release);
                                    telem.active_codec.store(codec.to_u8(), Ordering::Release);
                                    telem.active_width.store(width, Ordering::Release);
                                    telem.active_height.store(height, Ordering::Release);
                                    telem.active_fps.store(fps, Ordering::Release);
                                    telem.active_display.store(display_index, Ordering::Release);
                                    telem
                                        .active_prefer
                                        .store(prefer.to_u8(), Ordering::Release);
                                    telem.audio_enabled.store(audio_enabled, Ordering::Release);
                                    telem.resolution_stepped_down.store(
                                        (flags & STREAM_STATUS_FLAG_STEPPED_DOWN) != 0,
                                        Ordering::Release,
                                    );
                                    audio_muted.store(!audio_enabled, Ordering::Release);

                                    let mut need_keyframe = false;
                                    {
                                        let mut guard =
                                            desired_dec.lock().expect("decoder spec lock poisoned");
                                        let uses_ffmpeg_pipe = decoder_kind == DecoderKind::Ffmpeg
                                            || (decoder_kind == DecoderKind::Auto
                                                && codec != VideoCodec::H264
                                                && !crate::decoder::has_native_decoder(codec));
                                        if guard.codec != codec
                                            || (uses_ffmpeg_pipe
                                                && (guard.width != width
                                                    || guard.height != height))
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
                                            };
                                        }
                                    }
                                    if need_keyframe {
                                        let _ = ctrl_tx_fb.try_send(ControlMessage::RequestKeyframe);
                                    }
                                    let _ = proxy_rx.send_event(AppEvent::StreamStatusChanged);
                                }
                            }
                            other => {
                                tracing::debug!(?other, "ignored packet kind from host");
                            }
                        }
                    }
                }
            }

            enc_slot.close();
        });
    }

    // UDP transmit + keepalive + ClientHello retry loop
    {
        let sock = socket;
        let cry = crypto;
        let cancel_tx = cancel.clone();
        let connected_tx = connected;
        let start_tx = start;

        tokio::spawn(async move {
            let hello_msg = ControlMessage::ClientHello {
                client_version: 2,
                capabilities: 0x03,
            };
            send_sealed(&sock, &cry, PacketKind::Control, &hello_msg.encode(), mtu).await;

            let mut ping_tick = tokio::time::interval(Duration::from_secs(1));
            let mut retry_tick = tokio::time::interval(Duration::from_millis(200));
            let mut clip_transfer_id = 1u32;
            let clip_max_pt = max_plaintext_len(mtu);

            loop {
                tokio::select! {
                    _ = cancel_tx.cancelled() => {
                        let _ = send_sealed(
                            &sock,
                            &cry,
                            PacketKind::Control,
                            &ControlMessage::Disconnect.encode(),
                            mtu,
                        )
                        .await;
                        break;
                    }
                    _ = retry_tick.tick() => {
                        if !connected_tx.load(Ordering::Acquire) {
                            send_sealed(&sock, &cry, PacketKind::Control, &hello_msg.encode(), mtu).await;
                        }
                    }
                    _ = ping_tick.tick() => {
                        let ping = PingPayload {
                            sent_us: start_tx.elapsed().as_micros() as u64,
                        };
                        send_sealed(&sock, &cry, PacketKind::Ping, &ping.encode(), mtu).await;
                    }
                    Some(ev) = input_rx.recv() => {
                        send_sealed(&sock, &cry, PacketKind::Input, &ev.encode(), mtu).await;
                    }
                    Some(text) = clipboard_out_rx.recv() => {
                        let tid = clip_transfer_id;
                        clip_transfer_id = clip_transfer_id.wrapping_add(1).max(1);
                        for chunk in packetize_clipboard_text(tid, &text, clip_max_pt) {
                            send_sealed(&sock, &cry, PacketKind::Clipboard, &chunk, mtu).await;
                            send_sealed(&sock, &cry, PacketKind::Clipboard, &chunk, mtu).await;
                        }
                    }
                    Some(ctrl) = control_rx.recv() => {
                        let is_disc = matches!(ctrl, ControlMessage::Disconnect);
                        send_sealed(&sock, &cry, PacketKind::Control, &ctrl.encode(), mtu).await;
                        if is_disc {
                            cancel_tx.cancel();
                            break;
                        }
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

async fn send_sealed(
    socket: &UdpSocket,
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
        let _ = socket.send(&dgram).await;
    }
}
