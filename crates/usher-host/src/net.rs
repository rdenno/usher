use crate::input::InputSink;
use crate::video::EncodedFrame;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;
use usher_proto::audio::AudioPacket;
use usher_proto::crypto::SessionCrypto;
use usher_proto::fragment::{FRAME_FLAG_KEYFRAME, FramePacketizer};
use usher_proto::input::InputEvent;
use usher_proto::packet::{
    ControlMessage, PacketKind, PingPayload, PongPayload, max_plaintext_len, open_packet,
    seal_packet,
};

pub struct NetConfig {
    pub socket: Arc<UdpSocket>,
    pub crypto: Arc<Mutex<SessionCrypto>>,
    pub expected_client_ip: Option<IpAddr>,
    pub mtu: usize,
    pub fec_percent: u8,
    pub idle_timeout: Duration,
    pub start: Instant,
    pub keyframe_req: Arc<AtomicBool>,
    pub cancel: CancellationToken,
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
        cfg.cancel.clone(),
        input_sink,
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
        cfg.cancel,
        audio_rx,
    ));

    vec![rx_handle, video_handle, audio_handle]
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
    cancel: CancellationToken,
    mut input_sink: Box<dyn InputSink>,
) {
    let mut buf = vec![0u8; 4096];
    let mut tick = tokio::time::interval(Duration::from_secs(1));

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
                            tracing::debug!(width, height, "received client resize hint");
                        }
                        Ok(ControlMessage::SetPointerMode { relative }) => {
                            tracing::debug!(relative, "client toggled pointer mode");
                        }
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
    fec_percent: u8,
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
            fec_percent,
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
