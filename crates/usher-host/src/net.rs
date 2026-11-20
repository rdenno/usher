use crate::input::InputSink;
use crate::video::{EncodedFrame, VideoPipelineConfig, negotiate_codec, resolve_effective_codec};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{RwLock, mpsc, watch};
use tokio_util::sync::CancellationToken;
use usher_proto::adapt::AdaptiveController;
use usher_proto::audio::AudioPacket;
use usher_proto::crypto::SessionCrypto;
use usher_proto::fragment::{FRAME_FLAG_KEYFRAME, FramePacketizer};
use usher_proto::handshake::{CodecPreference, DisplayInfo, QualityPreference, VideoCodec};
use usher_proto::input::InputEvent;
use usher_proto::packet::{
    ControlMessage, PacketKind, PingPayload, PongPayload, STREAM_STATUS_FLAG_STEPPED_DOWN,
    max_plaintext_len, open_packet, seal_packet,
};

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
    pub cancel: CancellationToken,
    pub adaptive: Arc<Mutex<AdaptiveController>>,
    pub live_bitrate_kbps: Arc<AtomicU32>,
    pub in_place_bitrate: Arc<AtomicBool>,
    pub video_cfg_tx: watch::Sender<VideoPipelineConfig>,
    pub client_codecs: Vec<VideoCodec>,
    pub host_codecs: Vec<VideoCodec>,
    pub displays: Vec<DisplayInfo>,
}

pub fn spawn_network_tasks(
    cfg: NetConfig,
    input_sink: Box<dyn InputSink>,
    video_rx: mpsc::Receiver<EncodedFrame>,
    audio_rx: mpsc::Receiver<AudioPacket>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let peer: Arc<RwLock<Option<SocketAddr>>> = Arc::new(RwLock::new(None));
    let last_rx_ms = Arc::new(AtomicU64::new(0));

    let rx_handle = tokio::spawn(run_rx_loop(
        cfg.socket.clone(),
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
        RxAdaptCtx {
            fec_percent: cfg.fec_percent.clone(),
            audio_enabled: cfg.audio_enabled.clone(),
            adaptive: cfg.adaptive,
            live_bitrate_kbps: cfg.live_bitrate_kbps,
            in_place_bitrate: cfg.in_place_bitrate,
            video_cfg_tx: cfg.video_cfg_tx,
            client_codecs: cfg.client_codecs,
            host_codecs: cfg.host_codecs,
            displays: cfg.displays,
        },
    ));

    let video_handle = tokio::spawn(run_video_tx_loop(
        cfg.socket.clone(),
        cfg.crypto.clone(),
        peer.clone(),
        cfg.mtu,
        cfg.fec_percent,
        cfg.cancel.clone(),
        video_rx,
    ));

    let audio_handle = tokio::spawn(run_audio_tx_loop(
        cfg.socket,
        cfg.crypto,
        peer,
        cfg.mtu,
        cfg.audio_enabled,
        cfg.cancel,
        audio_rx,
    ));

    vec![rx_handle, video_handle, audio_handle]
}

struct RxAdaptCtx {
    fec_percent: Arc<AtomicU8>,
    audio_enabled: Arc<AtomicBool>,
    adaptive: Arc<Mutex<AdaptiveController>>,
    live_bitrate_kbps: Arc<AtomicU32>,
    in_place_bitrate: Arc<AtomicBool>,
    video_cfg_tx: watch::Sender<VideoPipelineConfig>,
    client_codecs: Vec<VideoCodec>,
    host_codecs: Vec<VideoCodec>,
    displays: Vec<DisplayInfo>,
}

fn build_stream_status(
    cfg: &VideoPipelineConfig,
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
        codec: cfg.codec,
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
    crypto: &Mutex<SessionCrypto>,
    dst: SocketAddr,
    mtu: usize,
    msg: &ControlMessage,
) {
    let sealed = {
        let mut guard = crypto.lock().expect("crypto lock poisoned");
        seal_packet(&mut guard, PacketKind::Control, &msg.encode(), mtu)
    };
    if let Ok(dgram) = sealed {
        let _ = socket.send_to(&dgram, dst).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_rx_loop(
    socket: Arc<UdpSocket>,
    crypto: Arc<Mutex<SessionCrypto>>,
    peer: Arc<RwLock<Option<SocketAddr>>>,
    last_rx_ms: Arc<AtomicU64>,
    expected_client_ip: Option<IpAddr>,
    mtu: usize,
    idle_timeout: Duration,
    start: Instant,
    keyframe_req: Arc<AtomicBool>,
    peer_ready: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
    mut input_sink: Box<dyn InputSink>,
    ctx: RxAdaptCtx,
) {
    let mut buf = vec![0u8; 4096];
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last_status_sent = Instant::now() - Duration::from_secs(5);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                let last_ms = last_rx_ms.load(Ordering::Acquire);
                let has_peer = peer.read().await.is_some();
                let limit_ms = idle_timeout.as_millis() as u64;
                if limit_ms > 0 && elapsed_ms.saturating_sub(last_ms) >= limit_ms {
                    if has_peer {
                        tracing::info!("UDP client idle timeout ({}s); tearing down", idle_timeout.as_secs());
                    } else {
                        tracing::info!("no authenticated client hello within {}s; tearing down", idle_timeout.as_secs());
                    }
                    cancel.cancel();
                    break;
                }
            }
            recv = socket.recv_from(&mut buf) => {
                let (len, src) = match recv {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!("UDP recv_from error: {err}");
                        continue;
                    }
                };

                let decoded = {
                    let mut guard = crypto.lock().expect("crypto lock poisoned");
                    open_packet(&mut guard, &buf[..len])
                };

                let (kind, _seq, plaintext) = match decoded {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::debug!(%src, "rejected unauthenticated/stale UDP datagram: {err}");
                        continue;
                    }
                };

                last_rx_ms.store(start.elapsed().as_millis() as u64, Ordering::Release);

                {
                    let mut peer_guard = peer.write().await;
                    if peer_guard.is_none() {
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
                        *peer_guard = Some(src);
                        keyframe_req.store(true, Ordering::Release);
                        peer_ready.notify_one();
                    } else if *peer_guard != Some(src) {
                        tracing::info!(old = ?*peer_guard, new = %src, "client UDP endpoint roamed");
                        *peer_guard = Some(src);
                    }
                }

                match kind {
                    PacketKind::Control => match ControlMessage::decode(&plaintext) {
                        Ok(ControlMessage::ClientHello { client_version, capabilities }) => {
                            tracing::debug!(client_version, capabilities, "received ClientHello");
                            keyframe_req.store(true, Ordering::Release);
                            let status = {
                                let cfg = ctx.video_cfg_tx.borrow();
                                let adapt = ctx.adaptive.lock().expect("adaptive lock poisoned");
                                build_stream_status(
                                    &cfg,
                                    &adapt,
                                    ctx.audio_enabled.load(Ordering::Acquire),
                                )
                            };
                            send_control_to_peer(&socket, &crypto, src, mtu, &status).await;
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
                                let w = width.clamp(64, 3840) & !1;
                                let h = height.clamp(64, 2160) & !1;
                                let (next_cfg, changed, status) = {
                                    let cur = ctx.video_cfg_tx.borrow().clone();
                                    let mut next = cur.clone();
                                    next.width = w;
                                    next.height = h;
                                    let changed = next.width != cur.width || next.height != cur.height;
                                    let mut adapt = ctx.adaptive.lock().expect("adaptive lock poisoned");
                                    if changed {
                                        adapt.reset_resolution_step_down();
                                    }
                                    let status = build_stream_status(
                                        &next,
                                        &adapt,
                                        ctx.audio_enabled.load(Ordering::Acquire),
                                    );
                                    (next, changed, status)
                                };
                                if changed {
                                    tracing::info!(w = next_cfg.width, h = next_cfg.height, "applying client window resize");
                                    let _ = ctx.video_cfg_tx.send(next_cfg);
                                }
                                send_control_to_peer(&socket, &crypto, src, mtu, &status).await;
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
                            let (action, status) = {
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
                                }
                                let status = build_stream_status(
                                    &preview_cfg,
                                    &adapt,
                                    ctx.audio_enabled.load(Ordering::Acquire),
                                );
                                (action, status)
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
                            }
                            if video_changed {
                                let _ = ctx.video_cfg_tx.send(next_cfg);
                            }

                            if action.status_changed
                                || last_status_sent.elapsed() >= Duration::from_secs(2)
                            {
                                send_control_to_peer(&socket, &crypto, src, mtu, &status).await;
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

                                if width >= 64 && height >= 64 {
                                    let w = width.clamp(64, 3840) & !1;
                                    let h = height.clamp(64, 2160) & !1;
                                    if w != next.width || h != next.height {
                                        next.width = w;
                                        next.height = h;
                                        adapt.reset_resolution_step_down();
                                        video_changed = true;
                                    }
                                }

                                if fps > 0 {
                                    let f = fps.clamp(1, 240);
                                    if f != next.fps {
                                        next.fps = f;
                                        video_changed = true;
                                    }
                                }

                                if let Ok(Some(pref_codec)) = CodecPreference::from_reconfigure_u8(codec) {
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

                                if audio == 1 {
                                    ctx.audio_enabled.store(false, Ordering::Release);
                                } else if audio == 2 {
                                    ctx.audio_enabled.store(true, Ordering::Release);
                                }

                                if video_changed && !in_place {
                                    next.bitrate_kbps = adapt.applied_bitrate_kbps;
                                }

                                let status = build_stream_status(
                                    &next,
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
                            send_control_to_peer(&socket, &crypto, src, mtu, &status).await;
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
                                let _ = socket.send_to(&dgram, src).await;
                            }
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

async fn run_video_tx_loop(
    socket: Arc<UdpSocket>,
    crypto: Arc<Mutex<SessionCrypto>>,
    peer: Arc<RwLock<Option<SocketAddr>>>,
    mtu: usize,
    fec_percent: Arc<AtomicU8>,
    cancel: CancellationToken,
    mut video_rx: mpsc::Receiver<EncodedFrame>,
) {
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

        let fec = fec_percent.load(Ordering::Acquire);
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
            fec,
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

        for dgram in datagrams {
            if let Err(err) = socket.send_to(&dgram, dst).await {
                tracing::debug!(%dst, "video send_to error: {err}");
                break;
            }
        }
    }
}

async fn run_audio_tx_loop(
    socket: Arc<UdpSocket>,
    crypto: Arc<Mutex<SessionCrypto>>,
    peer: Arc<RwLock<Option<SocketAddr>>>,
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
            let _ = socket.send_to(&dgram, dst).await;
        }
    }
}
