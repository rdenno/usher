use crate::input::InputSink;
use crate::video::{
    EncodedFrame, HostCursorEvent, VideoPipelineConfig, negotiate_codec, resolve_effective_codec,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::{RwLock, mpsc, watch};
use tokio_util::sync::CancellationToken;
use usher_proto::adapt::AdaptiveController;
use usher_proto::audio::AudioPacket;
use usher_proto::clipboard::{ClipboardReassembler, packetize_clipboard_text};
use usher_proto::crypto::SessionCrypto;
use usher_proto::cursor::{CursorPacket, packetize_cursor_shape};
use usher_proto::fragment::{FRAME_FLAG_KEYFRAME, FramePacketizer};
use usher_proto::handshake::{
    CodecPreference, DisplayInfo, QualityPreference, VideoCodec, fit_preserving_aspect,
};
use usher_proto::input::InputEvent;
use usher_proto::packet::{
    ControlMessage, PacketKind, PingPayload, PongPayload, STREAM_STATUS_FLAG_STEPPED_DOWN,
    TUNNEL_MAGIC, decode_tunnel_frame_len, encode_tunnel_frame, max_plaintext_len, open_packet,
    seal_packet,
};

fn fit_to_display(displays: &[DisplayInfo], display_idx: u8, req_w: u16, req_h: u16) -> (u16, u16) {
    if let Some(d) = displays
        .iter()
        .find(|d| d.index == display_idx)
        .or_else(|| displays.first())
    {
        fit_preserving_aspect(d.width, d.height, req_w, req_h)
    } else {
        (req_w.clamp(64, 3840) & !1, req_h.clamp(64, 2160) & !1)
    }
}

pub struct NetConfig {
    pub socket: Arc<UdpSocket>,
    pub crypto: Arc<Mutex<SessionCrypto>>,
    pub expected_client_ip: Option<IpAddr>,
    pub mtu: usize,
    pub fec_percent: Arc<AtomicU8>,
    pub audio_enabled: Arc<AtomicBool>,
    pub idle_timeout: Duration,
    pub start: Instant,
    pub keyframe_req: Arc<AtomicBool>,
    pub peer_ready: Arc<tokio::sync::Notify>,
    pub effective_codec: Arc<AtomicU8>,
    pub status_notify: Arc<tokio::sync::Notify>,
    pub cancel: CancellationToken,
    pub adaptive: Arc<Mutex<AdaptiveController>>,
    pub live_bitrate_kbps: Arc<AtomicU32>,
    pub in_place_bitrate: Arc<AtomicBool>,
    pub video_cfg_tx: watch::Sender<VideoPipelineConfig>,
    pub client_codecs: Vec<VideoCodec>,
    pub host_codecs: Vec<VideoCodec>,
    pub displays: Vec<DisplayInfo>,
    pub clipboard_in_tx: Option<mpsc::Sender<String>>,
    pub cursor_rx: Option<mpsc::Receiver<HostCursorEvent>>,
    pub mic_in_tx: Option<mpsc::Sender<AudioPacket>>,
    pub stdio_tunnel: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivePeer {
    Udp(SocketAddr),
    StdioTunnel,
}

enum StdioOutMsg {
    Sentinel,
    Packet(Vec<u8>),
}

async fn send_to_peer(
    socket: &UdpSocket,
    peer: ActivePeer,
    stdio_out_tx: &mpsc::Sender<StdioOutMsg>,
    dgram: Vec<u8>,
    drop_if_congested: bool,
) -> bool {
    match peer {
        ActivePeer::Udp(dst) => {
            if let Err(err) = socket.send_to(&dgram, dst).await {
                tracing::debug!(%dst, "UDP send_to error: {err}");
                return false;
            }
            true
        }
        ActivePeer::StdioTunnel => {
            if drop_if_congested {
                stdio_out_tx.try_send(StdioOutMsg::Packet(dgram)).is_ok()
            } else {
                stdio_out_tx.send(StdioOutMsg::Packet(dgram)).await.is_ok()
            }
        }
    }
}

pub fn spawn_network_tasks(
    cfg: NetConfig,
    input_sink: Box<dyn InputSink>,
    video_rx: mpsc::Receiver<EncodedFrame>,
    audio_rx: mpsc::Receiver<AudioPacket>,
    clipboard_out_rx: mpsc::Receiver<String>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let peer: Arc<RwLock<Option<ActivePeer>>> = Arc::new(RwLock::new(None));
    let last_rx_ms = Arc::new(AtomicU64::new(0));
    let (stdio_in_tx, stdio_in_rx) = mpsc::channel::<Vec<u8>>(256);
    let (stdio_out_tx, stdio_out_rx) = mpsc::channel::<StdioOutMsg>(256);

    let mut handles = Vec::with_capacity(6);
    if cfg.stdio_tunnel {
        handles.push(tokio::spawn(run_stdio_rx_task(
            stdio_in_tx,
            stdio_out_tx.clone(),
            cfg.cancel.clone(),
        )));
        handles.push(tokio::spawn(run_stdio_tx_task(
            stdio_out_rx,
            cfg.cancel.clone(),
        )));
    } else {
        drop(stdio_in_tx);
        drop(stdio_out_rx);
    }

    let video_tx_cfg_rx = cfg.video_cfg_tx.subscribe();
    let video_tx_live_br = cfg.live_bitrate_kbps.clone();

    handles.push(tokio::spawn(run_rx_loop(
        cfg.socket.clone(),
        stdio_in_rx,
        stdio_out_tx.clone(),
        cfg.crypto.clone(),
        peer.clone(),
        last_rx_ms,
        cfg.expected_client_ip,
        cfg.mtu,
        cfg.idle_timeout,
        cfg.start,
        cfg.keyframe_req,
        cfg.peer_ready,
        cfg.cancel.clone(),
        input_sink,
        cfg.clipboard_in_tx,
        cfg.mic_in_tx,
        RxAdaptCtx {
            fec_percent: cfg.fec_percent.clone(),
            audio_enabled: cfg.audio_enabled.clone(),
            adaptive: cfg.adaptive,
            live_bitrate_kbps: cfg.live_bitrate_kbps,
            in_place_bitrate: cfg.in_place_bitrate,
            effective_codec: cfg.effective_codec,
            status_notify: cfg.status_notify,
            video_cfg_tx: cfg.video_cfg_tx,
            client_codecs: cfg.client_codecs,
            host_codecs: cfg.host_codecs,
            displays: cfg.displays,
        },
    )));

    handles.push(tokio::spawn(run_video_tx_loop(
        cfg.socket.clone(),
        stdio_out_tx.clone(),
        cfg.crypto.clone(),
        peer.clone(),
        cfg.mtu,
        cfg.fec_percent,
        video_tx_live_br,
        video_tx_cfg_rx,
        cfg.cancel.clone(),
        video_rx,
    )));

    handles.push(tokio::spawn(run_audio_tx_loop(
        cfg.socket.clone(),
        stdio_out_tx.clone(),
        cfg.crypto.clone(),
        peer.clone(),
        cfg.mtu,
        cfg.audio_enabled,
        cfg.cancel.clone(),
        audio_rx,
    )));

    handles.push(tokio::spawn(run_clipboard_tx_loop(
        cfg.socket.clone(),
        stdio_out_tx.clone(),
        cfg.crypto.clone(),
        peer.clone(),
        cfg.mtu,
        cfg.cancel.clone(),
        clipboard_out_rx,
    )));

    if let Some(cursor_rx) = cfg.cursor_rx {
        handles.push(tokio::spawn(run_cursor_tx_loop(
            cfg.socket,
            stdio_out_tx,
            cfg.crypto,
            peer,
            cfg.mtu,
            cfg.cancel,
            cursor_rx,
        )));
    }

    handles
}

struct RxAdaptCtx {
    fec_percent: Arc<AtomicU8>,
    audio_enabled: Arc<AtomicBool>,
    adaptive: Arc<Mutex<AdaptiveController>>,
    live_bitrate_kbps: Arc<AtomicU32>,
    in_place_bitrate: Arc<AtomicBool>,
    effective_codec: Arc<AtomicU8>,
    status_notify: Arc<tokio::sync::Notify>,
    video_cfg_tx: watch::Sender<VideoPipelineConfig>,
    client_codecs: Vec<VideoCodec>,
    host_codecs: Vec<VideoCodec>,
    displays: Vec<DisplayInfo>,
}

fn current_effective_codec(ctx: &RxAdaptCtx, fallback: VideoCodec) -> VideoCodec {
    VideoCodec::from_u8(ctx.effective_codec.load(Ordering::Acquire)).unwrap_or(fallback)
}

fn build_stream_status(
    cfg: &VideoPipelineConfig,
    codec: VideoCodec,
    adapt: &AdaptiveController,
    audio_enabled: bool,
) -> ControlMessage {
    let flags = if adapt.resolution_stepped_down {
        STREAM_STATUS_FLAG_STEPPED_DOWN
    } else {
        0
    };
    ControlMessage::StreamStatus {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        codec,
        target_bitrate_kbps: adapt.applied_bitrate_kbps,
        max_bitrate_kbps: adapt.max_bitrate_kbps,
        fec_percent: adapt.fec_percent,
        display_index: cfg.display_index,
        prefer: adapt.prefer,
        audio_enabled,
        flags,
    }
}

async fn send_control_to_peer(
    socket: &UdpSocket,
    stdio_out_tx: &mpsc::Sender<StdioOutMsg>,
    crypto: &Mutex<SessionCrypto>,
    dst: ActivePeer,
    mtu: usize,
    msg: &ControlMessage,
) {
    let sealed = {
        let mut guard = crypto.lock().expect("crypto lock poisoned");
        seal_packet(&mut guard, PacketKind::Control, &msg.encode(), mtu)
    };
    if let Ok(dgram) = sealed {
        let _ = send_to_peer(socket, dst, stdio_out_tx, dgram, false).await;
    }
}

async fn run_stdio_rx_task(
    stdio_in_tx: mpsc::Sender<Vec<u8>>,
    stdio_out_tx: mpsc::Sender<StdioOutMsg>,
    cancel: CancellationToken,
) {
    let mut stdin = tokio::io::stdin();
    let mut magic_buf = [0u8; 4];
    let mut filled = 0usize;

    while filled < 4 {
        let read_res = tokio::select! {
            _ = cancel.cancelled() => return,
            r = stdin.read(&mut magic_buf[filled..]) => r,
        };
        match read_res {
            Ok(0) | Err(_) => {
                tracing::info!("SSH stdin closed (EOF); tearing down session");
                cancel.cancel();
                return;
            }
            Ok(n) => {
                filled += n;
            }
        }
    }

    if magic_buf != TUNNEL_MAGIC {
        let mut discard = [0u8; 64];
        loop {
            let read_res = tokio::select! {
                _ = cancel.cancelled() => return,
                r = stdin.read(&mut discard) => r,
            };
            match read_res {
                Ok(0) | Err(_) => {
                    tracing::info!("SSH stdin closed (EOF); tearing down session");
                    cancel.cancel();
                    return;
                }
                Ok(_) => {}
            }
        }
    }

    if stdio_out_tx.send(StdioOutMsg::Sentinel).await.is_err() {
        return;
    }

    loop {
        let mut len_hdr = [0u8; 2];
        let res = tokio::select! {
            _ = cancel.cancelled() => return,
            r = stdin.read_exact(&mut len_hdr) => r,
        };
        if let Err(err) = res {
            tracing::info!("SSH tunnel stdin closed ({err}); tearing down session");
            cancel.cancel();
            return;
        }
        let frame_len = match decode_tunnel_frame_len(len_hdr) {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!("invalid SSH tunnel frame header ({err:#}); tearing down");
                cancel.cancel();
                return;
            }
        };
        let mut payload = vec![0u8; frame_len];
        let res = tokio::select! {
            _ = cancel.cancelled() => return,
            r = stdin.read_exact(&mut payload) => r,
        };
        if let Err(err) = res {
            tracing::info!("SSH tunnel stdin truncated ({err}); tearing down session");
            cancel.cancel();
            return;
        }
        if stdio_in_tx.send(payload).await.is_err() {
            return;
        }
    }
}

async fn write_stdio_msg(stdout: &mut tokio::io::Stdout, msg: StdioOutMsg) -> std::io::Result<()> {
    match msg {
        StdioOutMsg::Sentinel => stdout.write_all(&TUNNEL_MAGIC).await,
        StdioOutMsg::Packet(dgram) => match encode_tunnel_frame(&dgram) {
            Ok(framed) => stdout.write_all(&framed).await,
            Err(err) => {
                tracing::warn!("skipping oversized tunnel packet: {err:#}");
                Ok(())
            }
        },
    }
}

async fn run_stdio_tx_task(
    mut stdio_out_rx: mpsc::Receiver<StdioOutMsg>,
    cancel: CancellationToken,
) {
    let mut stdout = tokio::io::stdout();
    loop {
        let first = tokio::select! {
            _ = cancel.cancelled() => break,
            m = stdio_out_rx.recv() => match m {
                Some(m) => m,
                None => break,
            }
        };
        if let Err(err) = write_stdio_msg(&mut stdout, first).await {
            tracing::debug!("SSH tunnel stdout write error: {err}");
            cancel.cancel();
            break;
        }
        for _ in 0..64 {
            match stdio_out_rx.try_recv() {
                Ok(next) => {
                    if let Err(err) = write_stdio_msg(&mut stdout, next).await {
                        tracing::debug!("SSH tunnel stdout write error: {err}");
                        cancel.cancel();
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        if let Err(err) = stdout.flush().await {
            tracing::debug!("SSH tunnel stdout flush error: {err}");
            cancel.cancel();
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_rx_loop(
    socket: Arc<UdpSocket>,
    mut stdio_in_rx: mpsc::Receiver<Vec<u8>>,
    stdio_out_tx: mpsc::Sender<StdioOutMsg>,
    crypto: Arc<Mutex<SessionCrypto>>,
    peer: Arc<RwLock<Option<ActivePeer>>>,
    last_rx_ms: Arc<AtomicU64>,
    expected_client_ip: Option<IpAddr>,
    mtu: usize,
    idle_timeout: Duration,
    start: Instant,
    keyframe_req: Arc<AtomicBool>,
    peer_ready: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
    mut input_sink: Box<dyn InputSink>,
    clipboard_in_tx: Option<mpsc::Sender<String>>,
    mic_in_tx: Option<mpsc::Sender<AudioPacket>>,
    ctx: RxAdaptCtx,
) {
    let mut buf = vec![0u8; 4096];
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last_status_sent = Instant::now() - Duration::from_secs(5);
    let mut clip_reasm = ClipboardReassembler::new();
    let mut stdio_open = true;

    loop {
        enum IncomingDatagram {
            Udp { len: usize, src: SocketAddr },
            Stdio(Vec<u8>),
        }

        let incoming = tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ctx.status_notify.notified() => {
                if let Some(dst) = *peer.read().await {
                    let status = {
                        let cfg = ctx.video_cfg_tx.borrow();
                        let adapt = ctx.adaptive.lock().expect("adaptive lock poisoned");
                        let codec = current_effective_codec(&ctx, cfg.codec);
                        build_stream_status(
                            &cfg,
                            codec,
                            &adapt,
                            ctx.audio_enabled.load(Ordering::Acquire),
                        )
                    };
                    send_control_to_peer(&socket, &stdio_out_tx, &crypto, dst, mtu, &status).await;
                    last_status_sent = Instant::now();
                }
                continue;
            }
            _ = tick.tick() => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                let last_ms = last_rx_ms.load(Ordering::Acquire);
                let has_peer = peer.read().await.is_some();
                let limit_ms = idle_timeout.as_millis() as u64;
                if limit_ms > 0 && elapsed_ms.saturating_sub(last_ms) >= limit_ms {
                    if has_peer {
                        tracing::info!("client idle timeout ({}s); tearing down", idle_timeout.as_secs());
                    } else {
                        tracing::info!("no authenticated client hello within {}s; tearing down", idle_timeout.as_secs());
                    }
                    cancel.cancel();
                    break;
                }
                continue;
            }
            pkt = stdio_in_rx.recv(), if stdio_open => match pkt {
                Some(bytes) => IncomingDatagram::Stdio(bytes),
                None => {
                    stdio_open = false;
                    continue;
                }
            },
            recv = socket.recv_from(&mut buf) => {
                let (len, src) = match recv {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!("UDP recv_from error: {err}");
                        continue;
                    }
                };
                IncomingDatagram::Udp { len, src }
            }
        };

        let (raw_pkt, packet_peer) = match incoming {
            IncomingDatagram::Udp { len, src } => (&buf[..len], ActivePeer::Udp(src)),
            IncomingDatagram::Stdio(ref bytes) => (bytes.as_slice(), ActivePeer::StdioTunnel),
        };

        let decoded = {
            let mut guard = crypto.lock().expect("crypto lock poisoned");
            open_packet(&mut guard, raw_pkt)
        };

        let (kind, _seq, plaintext) = match decoded {
            Ok(v) => v,
            Err(err) => {
                tracing::debug!(?packet_peer, "rejected unauthenticated/stale packet: {err}");
                continue;
            }
        };

        last_rx_ms.store(start.elapsed().as_millis() as u64, Ordering::Release);

        {
            let mut peer_guard = peer.write().await;
            if peer_guard.is_none() {
                if let ActivePeer::Udp(src) = packet_peer {
                    if let Some(exp) = expected_client_ip
                        && src.ip() != exp
                    {
                        tracing::warn!(
                            %src,
                            expected = %exp,
                            "authenticated UDP peer IP differs from SSH_CONNECTION client IP"
                        );
                    }
                    tracing::info!(%src, "authenticated client UDP peer locked");
                } else {
                    tracing::info!("authenticated client connected via SSH TCP tunnel");
                }
                *peer_guard = Some(packet_peer);
                keyframe_req.store(true, Ordering::Release);
                peer_ready.notify_one();
            } else if *peer_guard != Some(packet_peer) {
                tracing::info!(old = ?*peer_guard, new = ?packet_peer, "client endpoint updated");
                *peer_guard = Some(packet_peer);
            }
        }

        let src = packet_peer;
        {
            {
                match kind {
                    PacketKind::Control => match ControlMessage::decode(&plaintext) {
                        Ok(ControlMessage::ClientHello { client_version, capabilities }) => {
                            tracing::debug!(client_version, capabilities, "received ClientHello");
                            keyframe_req.store(true, Ordering::Release);
                            let status = {
                                let cfg = ctx.video_cfg_tx.borrow();
                                let adapt = ctx.adaptive.lock().expect("adaptive lock poisoned");
                                let codec = current_effective_codec(&ctx, cfg.codec);
                                build_stream_status(
                                    &cfg,
                                    codec,
                                    &adapt,
                                    ctx.audio_enabled.load(Ordering::Acquire),
                                )
                            };
                            send_control_to_peer(&socket, &stdio_out_tx, &crypto, src, mtu, &status).await;
                            last_status_sent = Instant::now();
                        }
                        Ok(ControlMessage::RequestKeyframe) => {
                            keyframe_req.store(true, Ordering::Release);
                        }
                        Ok(ControlMessage::Disconnect) => {
                            tracing::info!("client sent Control::Disconnect; shutting down");
                            cancel.cancel();
                            break;
                        }
                        Ok(ControlMessage::Resize { width, height }) => {
                            if width >= 64 && height >= 64 {
                                let (next_cfg, changed, status) = {
                                    let cur = ctx.video_cfg_tx.borrow().clone();
                                    let (w, h) = fit_to_display(
                                        &ctx.displays,
                                        cur.display_index,
                                        width,
                                        height,
                                    );
                                    let mut next = cur.clone();
                                    next.width = w;
                                    next.height = h;
                                    let changed = next.width != cur.width || next.height != cur.height;
                                    let mut adapt = ctx.adaptive.lock().expect("adaptive lock poisoned");
                                    if changed {
                                        adapt.reset_resolution_step_down();
                                    }
                                    let codec = current_effective_codec(&ctx, next.codec);
                                    let status = build_stream_status(
                                        &next,
                                        codec,
                                        &adapt,
                                        ctx.audio_enabled.load(Ordering::Acquire),
                                    );
                                    (next, changed, status)
                                };
                                if changed {
                                    tracing::info!(w = next_cfg.width, h = next_cfg.height, "applying client window resize");
                                    let _ = ctx.video_cfg_tx.send(next_cfg);
                                }
                                send_control_to_peer(&socket, &stdio_out_tx, &crypto, src, mtu, &status).await;
                                last_status_sent = Instant::now();
                            }
                        }
                        Ok(ControlMessage::SetPointerMode { relative }) => {
                            tracing::debug!(relative, "client toggled pointer mode");
                        }
                        Ok(fb @ ControlMessage::NetworkFeedback { .. }) => {
                            let cur_cfg = ctx.video_cfg_tx.borrow().clone();
                            let in_place = ctx.in_place_bitrate.load(Ordering::Acquire);
                            let now = Instant::now();
                            let (action, applied_br, status) = {
                                let mut adapt = ctx.adaptive.lock().expect("adaptive lock poisoned");
                                let action = adapt.on_feedback(
                                    &fb,
                                    now,
                                    in_place,
                                    cur_cfg.width,
                                    cur_cfg.height,
                                );
                                let mut preview_cfg = cur_cfg.clone();
                                if let Some(br) = action.new_bitrate_kbps {
                                    preview_cfg.bitrate_kbps = br;
                                }
                                if let Some((nw, nh)) = action.step_down_resolution {
                                    preview_cfg.width = nw;
                                    preview_cfg.height = nh;
                                } else if let Some((nw, nh)) = action.step_up_resolution {
                                    preview_cfg.width = nw;
                                    preview_cfg.height = nh;
                                }
                                let codec = current_effective_codec(&ctx, preview_cfg.codec);
                                let status = build_stream_status(
                                    &preview_cfg,
                                    codec,
                                    &adapt,
                                    ctx.audio_enabled.load(Ordering::Acquire),
                                );
                                (action, adapt.applied_bitrate_kbps, status)
                            };

                            if let Some(new_fec) = action.new_fec_percent {
                                ctx.fec_percent.store(new_fec, Ordering::Release);
                            }

                            let mut next_cfg = cur_cfg.clone();
                            let mut video_changed = false;
                            if let Some(new_br) = action.new_bitrate_kbps {
                                ctx.live_bitrate_kbps.store(new_br, Ordering::Release);
                                if !in_place {
                                    next_cfg.bitrate_kbps = new_br;
                                    video_changed = true;
                                }
                            }
                            if let Some((nw, nh)) = action.step_down_resolution {
                                next_cfg.width = nw;
                                next_cfg.height = nh;
                                video_changed = true;
                                tracing::warn!(
                                    old_w = cur_cfg.width,
                                    old_h = cur_cfg.height,
                                    new_w = nw,
                                    new_h = nh,
                                    "sustained congestion at bitrate floor; stepping down resolution one notch"
                                );
                            } else if let Some((nw, nh)) = action.step_up_resolution {
                                next_cfg.width = nw;
                                next_cfg.height = nh;
                                video_changed = true;
                                tracing::info!(
                                    old_w = cur_cfg.width,
                                    old_h = cur_cfg.height,
                                    new_w = nw,
                                    new_h = nh,
                                    "bandwidth recovered; restoring original stream resolution"
                                );
                            }
                            if video_changed {
                                next_cfg.bitrate_kbps = applied_br;
                                let _ = ctx.video_cfg_tx.send(next_cfg);
                            }

                            if action.status_changed
                                || last_status_sent.elapsed() >= Duration::from_secs(2)
                            {
                                send_control_to_peer(&socket, &stdio_out_tx, &crypto, src, mtu, &status).await;
                                last_status_sent = Instant::now();
                            }
                        }
                        Ok(ControlMessage::Reconfigure {
                            max_bitrate_kbps,
                            width,
                            height,
                            fps,
                            codec,
                            prefer,
                            audio,
                            display,
                        }) => {
                            let cur_cfg = ctx.video_cfg_tx.borrow().clone();
                            let in_place = ctx.in_place_bitrate.load(Ordering::Acquire);
                            let (next_cfg, video_changed, status) = {
                                let mut next = cur_cfg.clone();
                                let mut video_changed = false;
                                let mut adapt = ctx.adaptive.lock().expect("adaptive lock poisoned");

                                if max_bitrate_kbps > 0
                                    && let Some(clamped_br) =
                                        adapt.set_max_bitrate(max_bitrate_kbps)
                                {
                                    ctx.live_bitrate_kbps.store(clamped_br, Ordering::Release);
                                    if !in_place {
                                        next.bitrate_kbps = clamped_br;
                                        video_changed = true;
                                    }
                                }

                                if let Ok(Some(new_prefer)) = QualityPreference::from_reconfigure_u8(prefer) {
                                    if new_prefer != next.prefer {
                                        next.prefer = new_prefer;
                                        video_changed = true;
                                    }
                                    if let Some(clamped_fec) = adapt.set_prefer(new_prefer) {
                                        ctx.fec_percent.store(clamped_fec, Ordering::Release);
                                    }
                                }

                                if fps > 0 {
                                    let f = fps.clamp(1, 240);
                                    if f != next.fps {
                                        next.fps = f;
                                        video_changed = true;
                                    }
                                }

                                if let Ok(Some(pref_codec)) =
                                    CodecPreference::from_reconfigure_u8(codec)
                                {
                                    let mut target_codec = negotiate_codec(
                                        pref_codec,
                                        &ctx.client_codecs,
                                        &ctx.host_codecs,
                                    );
                                    let mut probe_cfg = next.clone();
                                    probe_cfg.codec = target_codec;
                                    target_codec = resolve_effective_codec(&probe_cfg);
                                    if target_codec != next.codec {
                                        next.codec = target_codec;
                                        ctx.effective_codec
                                            .store(target_codec.to_u8(), Ordering::Release);
                                        video_changed = true;
                                    }
                                }

                                if display != u8::MAX {
                                    let d = if ctx.displays.is_empty() {
                                        0
                                    } else {
                                        display.min((ctx.displays.len() - 1) as u8)
                                    };
                                    if d != next.display_index {
                                        next.display_index = d;
                                        if width == 0
                                            && height == 0
                                            && let Some(disp) = ctx.displays.iter().find(|x| x.index == d)
                                        {
                                            next.width = disp.width;
                                            next.height = disp.height;
                                            adapt.reset_resolution_step_down();
                                        }
                                        video_changed = true;
                                    }
                                }

                                if width >= 64 && height >= 64 {
                                    let (w, h) = fit_to_display(
                                        &ctx.displays,
                                        next.display_index,
                                        width,
                                        height,
                                    );
                                    if w != next.width || h != next.height {
                                        next.width = w;
                                        next.height = h;
                                        adapt.reset_resolution_step_down();
                                        video_changed = true;
                                    }
                                } else if display != u8::MAX && width == 0 && height == 0 {
                                    let (w, h) = fit_to_display(
                                        &ctx.displays,
                                        next.display_index,
                                        next.width,
                                        next.height,
                                    );
                                    if w != next.width || h != next.height {
                                        next.width = w;
                                        next.height = h;
                                        adapt.reset_resolution_step_down();
                                        video_changed = true;
                                    }
                                }

                                if audio == 1 {
                                    ctx.audio_enabled.store(false, Ordering::Release);
                                } else if audio == 2 {
                                    ctx.audio_enabled.store(true, Ordering::Release);
                                }

                                if video_changed {
                                    next.bitrate_kbps = adapt.applied_bitrate_kbps;
                                }

                                let status = build_stream_status(
                                    &next,
                                    next.codec,
                                    &adapt,
                                    ctx.audio_enabled.load(Ordering::Acquire),
                                );
                                (next, video_changed, status)
                            };

                            if video_changed {
                                tracing::info!(
                                    w = next_cfg.width,
                                    h = next_cfg.height,
                                    fps = next_cfg.fps,
                                    codec = %next_cfg.codec,
                                    display = next_cfg.display_index,
                                    bitrate_kbps = next_cfg.bitrate_kbps,
                                    "applying mid-stream reconfigure"
                                );
                                let _ = ctx.video_cfg_tx.send(next_cfg);
                            }
                            send_control_to_peer(&socket, &stdio_out_tx, &crypto, src, mtu, &status).await;
                            last_status_sent = Instant::now();
                        }
                        Ok(ControlMessage::StreamStatus { .. }) => {}
                        Err(err) => tracing::debug!("malformed control message: {err}"),
                    },
                    PacketKind::Input => match InputEvent::decode(&plaintext) {
                        Ok(ev) => input_sink.handle(ev),
                        Err(err) => tracing::debug!("malformed input event: {err}"),
                    },
                    PacketKind::Ping => {
                        if let Ok(ping) = PingPayload::decode(&plaintext) {
                            let pong = PongPayload {
                                sent_us: ping.sent_us,
                                host_recv_us: start.elapsed().as_micros() as u64,
                            };
                            let sealed = {
                                let mut guard = crypto.lock().expect("crypto lock poisoned");
                                seal_packet(&mut guard, PacketKind::Pong, &pong.encode(), mtu)
                            };
                            if let Ok(dgram) = sealed {
                                let _ = send_to_peer(&socket, src, &stdio_out_tx, dgram, false).await;
                            }
                        }
                    }
                    PacketKind::Clipboard => match clip_reasm.push_chunk(&plaintext) {
                        Ok(Some(text)) => {
                            if let Some(ref tx) = clipboard_in_tx {
                                let _ = tx.try_send(text);
                            }
                        }
                        Ok(None) => {}
                        Err(err) => tracing::debug!("clipboard reassembly error: {err}"),
                    },
                    PacketKind::MicAudio => {
                        if let Some(ref tx) = mic_in_tx
                            && let Ok(pkt) = AudioPacket::decode(&plaintext)
                        {
                            let _ = tx.try_send(pkt);
                        }
                    }
                    other => {
                        tracing::debug!(?other, "unexpected packet kind from client");
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_video_tx_loop(
    socket: Arc<UdpSocket>,
    stdio_out_tx: mpsc::Sender<StdioOutMsg>,
    crypto: Arc<Mutex<SessionCrypto>>,
    peer: Arc<RwLock<Option<ActivePeer>>>,
    mtu: usize,
    fec_percent: Arc<AtomicU8>,
    live_bitrate_kbps: Arc<AtomicU32>,
    video_cfg_rx: watch::Receiver<VideoPipelineConfig>,
    cancel: CancellationToken,
    mut video_rx: mpsc::Receiver<EncodedFrame>,
) {
    const PACE_BURST: usize = 4;
    let mut frame_id = 0u32;

    loop {
        let frame = tokio::select! {
            _ = cancel.cancelled() => break,
            f = video_rx.recv() => match f {
                Some(frame) => frame,
                None => break,
            }
        };

        let Some(dst) = *peer.read().await else {
            continue;
        };

        let is_tunnel = matches!(dst, ActivePeer::StdioTunnel);
        let fec = fec_percent.load(Ordering::Acquire);
        let frame_fec = if is_tunnel {
            0
        } else if frame.keyframe && fec > 0 {
            fec.max(25)
        } else {
            fec
        };
        let flags = if frame.keyframe {
            FRAME_FLAG_KEYFRAME
        } else {
            0
        };
        let shards = match FramePacketizer::packetize(
            frame_id,
            frame.timestamp_us,
            flags,
            &frame.data,
            mtu,
            frame_fec,
        ) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(frame_id, "dropping unfragmentable frame: {err}");
                frame_id = frame_id.wrapping_add(1);
                continue;
            }
        };
        frame_id = frame_id.wrapping_add(1);

        let mut datagrams = Vec::with_capacity(shards.len());
        {
            let mut guard = crypto.lock().expect("crypto lock poisoned");
            for (kind, pt) in shards {
                match seal_packet(&mut guard, kind, &pt, mtu) {
                    Ok(dgram) => datagrams.push(dgram),
                    Err(err) => {
                        tracing::warn!("seal_packet failed: {err}");
                        break;
                    }
                }
            }
        }

        let total = datagrams.len();
        let drop_if_congested = is_tunnel && !frame.keyframe;
        let (max_spread_us, pacing_spread_us) = if !is_tunnel && total > 8 {
            let fps = video_cfg_rx.borrow().fps.max(1) as u64;
            let frame_period_us = 1_000_000 / fps;
            let max_spread_us = if frame.keyframe {
                12_000
            } else {
                (frame_period_us / 2).clamp(2_000, 10_000)
            };
            let total_bytes: u64 = datagrams.iter().map(|d| d.len() as u64).sum();
            let pacing_bps = (live_bitrate_kbps.load(Ordering::Acquire).max(2_000) as u64)
                .saturating_mul(2_000);
            let ideal_us = total_bytes
                .saturating_mul(8_000_000)
                .checked_div(pacing_bps)
                .unwrap_or(0);
            let spread = if ideal_us >= 1_000 {
                ideal_us.min(max_spread_us)
            } else {
                0
            };
            (max_spread_us, spread)
        } else {
            (0, 0)
        };
        let frame_tx_start = Instant::now();
        for (idx, dgram) in datagrams.into_iter().enumerate() {
            if !send_to_peer(&socket, dst, &stdio_out_tx, dgram, drop_if_congested).await {
                break;
            }
            if pacing_spread_us > 0 {
                if idx + 1 < total && (idx + 1).is_multiple_of(PACE_BURST) {
                    let target_us = (pacing_spread_us * (idx as u64 + 1)) / (total as u64);
                    let elapsed_us = frame_tx_start.elapsed().as_micros() as u64;
                    if elapsed_us < max_spread_us && target_us > elapsed_us + 750 {
                        let sleep_us = (target_us - elapsed_us).min(max_spread_us - elapsed_us);
                        tokio::time::sleep(Duration::from_micros(sleep_us)).await;
                    }
                }
            } else if total > 24 && (idx & 15) == 15 {
                tokio::task::yield_now().await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_audio_tx_loop(
    socket: Arc<UdpSocket>,
    stdio_out_tx: mpsc::Sender<StdioOutMsg>,
    crypto: Arc<Mutex<SessionCrypto>>,
    peer: Arc<RwLock<Option<ActivePeer>>>,
    mtu: usize,
    audio_enabled: Arc<AtomicBool>,
    cancel: CancellationToken,
    mut audio_rx: mpsc::Receiver<AudioPacket>,
) {
    let max_pt = max_plaintext_len(mtu);

    loop {
        let pkt = tokio::select! {
            _ = cancel.cancelled() => break,
            p = audio_rx.recv() => match p {
                Some(pkt) => pkt,
                None => break,
            }
        };

        if !audio_enabled.load(Ordering::Acquire) {
            continue;
        }

        let Some(dst) = *peer.read().await else {
            continue;
        };

        let encoded = pkt.encode();
        if encoded.len() > max_pt {
            tracing::warn!("audio packet {}B exceeds MTU plaintext budget {max_pt}B", encoded.len());
            continue;
        }

        let sealed = {
            let mut guard = crypto.lock().expect("crypto lock poisoned");
            seal_packet(&mut guard, PacketKind::Audio, &encoded, mtu)
        };
        if let Ok(dgram) = sealed {
            let drop_if_congested = matches!(dst, ActivePeer::StdioTunnel);
            let _ = send_to_peer(&socket, dst, &stdio_out_tx, dgram, drop_if_congested).await;
        }
    }
}

async fn run_clipboard_tx_loop(
    socket: Arc<UdpSocket>,
    stdio_out_tx: mpsc::Sender<StdioOutMsg>,
    crypto: Arc<Mutex<SessionCrypto>>,
    peer: Arc<RwLock<Option<ActivePeer>>>,
    mtu: usize,
    cancel: CancellationToken,
    mut clipboard_out_rx: mpsc::Receiver<String>,
) {
    let max_pt = max_plaintext_len(mtu);
    let mut transfer_id = 1u32;

    loop {
        let text = tokio::select! {
            _ = cancel.cancelled() => break,
            c = clipboard_out_rx.recv() => match c {
                Some(t) => t,
                None => break,
            }
        };

        let Some(dst) = *peer.read().await else {
            continue;
        };

        let chunks = packetize_clipboard_text(transfer_id, &text, max_pt);
        transfer_id = transfer_id.wrapping_add(1).max(1);
        let passes = if matches!(dst, ActivePeer::StdioTunnel) { 1 } else { 2 };
        for chunk in chunks {
            for pass in 0..passes {
                let sealed = {
                    let mut guard = crypto.lock().expect("crypto lock poisoned");
                    seal_packet(&mut guard, PacketKind::Clipboard, &chunk, mtu)
                };
                if let Ok(dgram) = sealed {
                    let _ = send_to_peer(&socket, dst, &stdio_out_tx, dgram, false).await;
                }
                if passes > 1 && pass == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }
}

async fn run_cursor_tx_loop(
    socket: Arc<UdpSocket>,
    stdio_out_tx: mpsc::Sender<StdioOutMsg>,
    crypto: Arc<Mutex<SessionCrypto>>,
    peer: Arc<RwLock<Option<ActivePeer>>>,
    mtu: usize,
    cancel: CancellationToken,
    mut cursor_rx: mpsc::Receiver<HostCursorEvent>,
) {
    let max_pt = max_plaintext_len(mtu);
    let mut pos_seq = 1u32;

    loop {
        let ev = tokio::select! {
            _ = cancel.cancelled() => break,
            e = cursor_rx.recv() => match e {
                Some(ev) => ev,
                None => break,
            }
        };

        let Some(dst) = *peer.read().await else {
            continue;
        };

        match ev {
            HostCursorEvent::Position {
                visible,
                x_norm,
                y_norm,
            } => {
                let pkt = CursorPacket::Position {
                    seq: pos_seq,
                    visible,
                    x_norm,
                    y_norm,
                }
                .encode();
                pos_seq = pos_seq.wrapping_add(1).max(1);
                let sealed = {
                    let mut guard = crypto.lock().expect("crypto lock poisoned");
                    seal_packet(&mut guard, PacketKind::Cursor, &pkt, mtu)
                };
                if let Ok(dgram) = sealed {
                    let drop_if_congested = matches!(dst, ActivePeer::StdioTunnel);
                    let _ = send_to_peer(&socket, dst, &stdio_out_tx, dgram, drop_if_congested)
                        .await;
                }
            }
            HostCursorEvent::Shape {
                shape_id,
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            } => {
                let chunks = packetize_cursor_shape(
                    shape_id, width, height, hotspot_x, hotspot_y, &rgba, max_pt,
                );
                for chunk in chunks {
                    let sealed = {
                        let mut guard = crypto.lock().expect("crypto lock poisoned");
                        seal_packet(&mut guard, PacketKind::Cursor, &chunk, mtu)
                    };
                    if let Ok(dgram) = sealed {
                        let _ = send_to_peer(&socket, dst, &stdio_out_tx, dgram, false).await;
                    }
                }
            }
        }
    }
}
