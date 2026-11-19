use crate::decoder::{DecoderKind, Yuv420Frame, create_decoder};
use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::AudioPacket;
use usher_proto::crypto::{Role, SessionCrypto};
use usher_proto::fragment::{CompletedFrame, FrameReassembler};
use usher_proto::handshake::HelloMessage;
use usher_proto::input::InputEvent;
use usher_proto::packet::{
    ControlMessage, PacketKind, PingPayload, PongPayload, open_packet, seal_packet,
};

#[derive(Debug, Default)]
pub struct Telemetry {
    pub fps_x10: AtomicU32,
    pub rtt_us: AtomicU32,
    pub kbps: AtomicU32,
    pub fec_recovered: AtomicU64,
    pub frames_dropped: AtomicU64,
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    NewFrame,
    Shutdown,
}

pub struct LatestSlot<T> {
    inner: Mutex<Option<T>>,
    cv: Condvar,
    closed: AtomicBool,
}

impl<T> LatestSlot<T> {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(None),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
        })
    }

    /// Stores `val`, returning `true` if a previous unconsumed item was overwritten.
    pub fn push(&self, val: T) -> bool {
        let mut guard = self.inner.lock().expect("slot lock poisoned");
        let replaced = guard.replace(val).is_some();
        self.cv.notify_one();
        replaced
    }

    pub fn try_take(&self) -> Option<T> {
        self.inner.lock().expect("slot lock poisoned").take()
    }

    pub fn wait_take(&self) -> Option<T> {
        let mut guard = self.inner.lock().expect("slot lock poisoned");
        loop {
            if let Some(v) = guard.take() {
                return Some(v);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            guard = self.cv.wait(guard).expect("slot condvar poisoned");
        }
    }

    pub fn close(&self) {
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

pub async fn start_transport(
    hello: HelloMessage,
    decoder_kind: DecoderKind,
    audio_tx: Option<mpsc::Sender<AudioPacket>>,
    proxy: winit::event_loop::EventLoopProxy<AppEvent>,
    cancel: CancellationToken,
) -> Result<ClientTransportHandles> {
    let session_key = hello.decode_session_key()?;
    let remote_addr = SocketAddr::new(hello.bind_ip, hello.port);
    let local_bind = match hello.bind_ip {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };

    let socket = Arc::new(
        UdpSocket::bind(local_bind)
            .await
            .context("binding client UDP socket")?,
    );
    socket
        .connect(remote_addr)
        .await
        .with_context(|| format!("connecting UDP socket to {remote_addr}"))?;

    let crypto = Arc::new(Mutex::new(SessionCrypto::new(&session_key, Role::Client)));
    let mtu = hello.mtu as usize;
    let start = Instant::now();
    let telemetry = Arc::new(Telemetry::default());

    let (input_tx, mut input_rx) = mpsc::channel::<InputEvent>(256);
    let (control_tx, mut control_rx) = mpsc::channel::<ControlMessage>(32);

    let encoded_slot = LatestSlot::<CompletedFrame>::new();
    let decoded_frame_slot = LatestSlot::<Yuv420Frame>::new();
    let connected = Arc::new(AtomicBool::new(false));

    // Dedicated video decoder worker thread (never stalls UDP rx or UI)
    {
        let enc_slot = encoded_slot.clone();
        let dec_slot = decoded_frame_slot.clone();
        let telem = telemetry.clone();
        let proxy_dec = proxy.clone();
        let cancel_dec = cancel.clone();
        let codec = hello.codec;
        let width = hello.width;
        let height = hello.height;

        std::thread::spawn(move || {
            let mut decoder = match create_decoder(codec, decoder_kind, width, height) {
                Ok(d) => d,
                Err(err) => {
                    tracing::error!("failed to initialize video decoder: {err:#}");
                    cancel_dec.cancel();
                    let _ = proxy_dec.send_event(AppEvent::Shutdown);
                    return;
                }
            };

            let mut frames_in_window = 0u32;
            let mut fps_window_start = Instant::now();

            while !cancel_dec.is_cancelled() {
                let Some(au) = enc_slot.wait_take() else {
                    break;
                };
                match decoder.decode(&au.data) {
                    Ok(Some(yuv)) => {
                        let _ = dec_slot.push(yuv);
                        let _ = proxy_dec.send_event(AppEvent::NewFrame);
                        frames_in_window += 1;
                        let dt = fps_window_start.elapsed();
                        if dt >= Duration::from_millis(500) {
                            let fps_x10 =
                                ((frames_in_window as f64 / dt.as_secs_f64()) * 10.0).round() as u32;
                            telem.fps_x10.store(fps_x10, Ordering::Release);
                            frames_in_window = 0;
                            fps_window_start = Instant::now();
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!("video decode error: {err:#}");
                    }
                }
            }
        });
    }

    // UDP receive loop
    {
        let sock = socket.clone();
        let cry = crypto.clone();
        let enc_slot = encoded_slot.clone();
        let telem = telemetry.clone();
        let ctrl_tx_pli = control_tx.clone();
        let connected_rx = connected.clone();
        let cancel_rx = cancel.clone();
        let start_rx = start;

        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut reassembler = FrameReassembler::new(8);
            let mut bytes_in_window = 0u64;
            let mut bw_window_start = Instant::now();
            let mut last_pli = Instant::now() - Duration::from_secs(1);
            let mut derp_warned = false;

            loop {
                let n = tokio::select! {
                    _ = cancel_rx.cancelled() => break,
                    res = sock.recv(&mut buf) => match res {
                        Ok(n) => n,
                        Err(err) => {
                            tracing::debug!("UDP recv error: {err}");
                            continue;
                        }
                    }
                };

                bytes_in_window += n as u64;
                let bw_elapsed = bw_window_start.elapsed();
                if bw_elapsed >= Duration::from_millis(500) {
                    let kbps =
                        ((bytes_in_window as f64 * 8.0 / bw_elapsed.as_secs_f64()) / 1000.0) as u32;
                    telem.kbps.store(kbps, Ordering::Release);
                    bytes_in_window = 0;
                    bw_window_start = Instant::now();
                }

                let opened = {
                    let mut guard = cry.lock().expect("crypto lock poisoned");
                    open_packet(&mut guard, &buf[..n])
                };
                let (kind, _seq, plaintext) = match opened {
                    Ok(v) => {
                        connected_rx.store(true, Ordering::Release);
                        v
                    }
                    Err(err) => {
                        tracing::debug!("dropped invalid/replayed packet: {err}");
                        continue;
                    }
                };

                match kind {
                    PacketKind::VideoData | PacketKind::VideoFec => {
                        match reassembler.push_packet(kind, &plaintext) {
                            Ok(outcome) => {
                                if outcome.request_keyframe
                                    && last_pli.elapsed() >= Duration::from_millis(150)
                                {
                                    last_pli = Instant::now();
                                    let _ = ctrl_tx_pli.try_send(ControlMessage::RequestKeyframe);
                                }
                                if let Some(frame) = outcome.completed {
                                    if frame.fec_recovered > 0 {
                                        telem
                                            .fec_recovered
                                            .fetch_add(frame.fec_recovered as u64, Ordering::Relaxed);
                                    }
                                    if enc_slot.push(frame) {
                                        telem.frames_dropped.fetch_add(1, Ordering::Relaxed);
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
                    other => {
                        tracing::debug!(?other, "ignored packet kind from host");
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
                client_version: 1,
                capabilities: 0x01,
            };
            send_sealed(&sock, &cry, PacketKind::Control, &hello_msg.encode(), mtu).await;

            let mut ping_tick = tokio::time::interval(Duration::from_secs(1));
            let mut retry_tick = tokio::time::interval(Duration::from_millis(200));

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
