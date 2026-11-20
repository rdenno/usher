mod audio;
mod input;
mod net;
mod video;
#[cfg(target_os = "windows")]
mod windows_session;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
#[cfg(target_os = "windows")]
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use usher_proto::adapt::{AdaptiveController, DEFAULT_MAX_BITRATE_KBPS};
use usher_proto::audio::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use usher_proto::crypto::{Role, SessionCrypto, generate_session_key};
use usher_proto::handshake::{
    CodecPreference, DisplayInfo, HelloMessage, PROTOCOL_VERSION, QualityPreference, VideoCodec,
    encode_hello_line, is_overlay_or_private_ip, parse_ssh_connection,
};
use usher_proto::packet::DEFAULT_MTU;

use crate::audio::{AudioStreamConfig, spawn_audio_task};
use crate::input::create_input_sink;
use crate::net::{NetConfig, spawn_network_tasks};
use crate::video::drm_probe::probe_drm;
use crate::video::headless::{HeadlessBackend, HeadlessSession};
use crate::video::{
    CaptureSource, EncoderKind, VideoPipelineConfig, is_in_place_bitrate_backend, negotiate_codec,
    probe_host_codecs, resolve_effective_codec, resolve_video_backend, spawn_video_task,
};

const UDEV_RULE: &str = r#"KERNEL=="uinput", GROUP="input", MODE="0660""#;

#[derive(Debug, Parser)]
#[command(name = "usher-host", version, about = "Zero-daemon SSH-bootstrapped Usher host")]
struct Args {
    /// Explicit IP address to bind the ephemeral UDP socket to (defaults to SSH_CONNECTION server IP or 127.0.0.1)
    #[arg(long)]
    bind_ip: Option<IpAddr>,

    /// Allow binding to non-private / public WAN IPs (disabled by default for overlay safety)
    #[arg(long)]
    allow_public: bool,

    /// Stream width (0 = auto-detect DRM mode or default to 1280)
    #[arg(long, default_value_t = 0)]
    width: u16,

    /// Stream height (0 = auto-detect DRM mode or default to 720)
    #[arg(long, default_value_t = 0)]
    height: u16,

    /// Target frames per second
    #[arg(long, default_value_t = 60)]
    fps: u16,

    /// Maximum adaptive video bitrate ceiling in kbps
    #[arg(long, default_value_t = DEFAULT_MAX_BITRATE_KBPS)]
    max_bitrate_kbps: u32,

    /// Optional explicit initial video bitrate in kbps
    #[arg(long)]
    bitrate_kbps: Option<u32>,

    /// Video codec preference (auto, h264, hevc, av1)
    #[arg(long, default_value = "auto")]
    codec: CodecPreference,

    /// Comma-separated client-supported codecs for auto negotiation
    #[arg(long, default_value = "h264")]
    client_codecs: String,

    /// Stream tuning preference (latency, quality)
    #[arg(long, default_value = "latency")]
    prefer: QualityPreference,

    /// Host monitor index (for multi-monitor systems)
    #[arg(long, default_value_t = 0)]
    display: u8,

    /// Disable adaptive bitrate and FEC adjustments
    #[arg(long)]
    no_adaptive: bool,

    /// Video capture source
    #[arg(long, value_enum, default_value_t = CaptureSource::Auto)]
    capture: CaptureSource,

    /// Video encoder backend
    #[arg(long, value_enum, default_value_t = EncoderKind::Auto)]
    encoder: EncoderKind,

    /// Ephemeral headless Wayland compositor backend
    #[arg(long, value_enum, default_value_t = HeadlessBackend::None)]
    headless: HeadlessBackend,

    /// Outer UDP MTU budget in bytes (recommended <= 1200 for Tailscale/ZeroTier)
    #[arg(long, default_value_t = DEFAULT_MTU as u16)]
    mtu: u16,

    /// XOR FEC parity redundancy percentage (0..=50)
    #[arg(long, default_value_t = 10)]
    fec: u8,

    /// Disable audio capture and streaming
    #[arg(long)]
    no_audio: bool,

    /// Disable /dev/uinput virtual input device creation
    #[arg(long)]
    no_input: bool,

    /// Seconds of inactivity before tearing down automatically
    #[arg(long, default_value_t = 15)]
    idle_timeout_secs: u64,

    /// Print the recommended static udev rule for /dev/uinput access and exit
    #[arg(long)]
    print_udev_rule: bool,

    /// Internal flag: running inside Windows interactive Session 1 spawned by Session 0 bridge
    #[arg(long, hide = true)]
    windows_worker: bool,

    /// Internal flag: named pipe path used by Windows Session 0 <-> Session 1 bridge
    #[arg(long, hide = true)]
    bridge_pipe: Option<String>,

    /// Internal flag: preserve SSH_CONNECTION peer IP across Windows Session 0 -> Session 1 bridge
    #[arg(long, hide = true)]
    expected_client_ip: Option<IpAddr>,
}

fn resolve_bind_ip(explicit: Option<IpAddr>, allow_public: bool) -> Result<(IpAddr, Option<IpAddr>)> {
    let mut expected_client_ip = None;
    let bind_ip = if let Some(ip) = explicit {
        if let Ok(raw) = std::env::var("SSH_CONNECTION")
            && let Ok(conn) = parse_ssh_connection(&raw)
        {
            expected_client_ip = Some(conn.client_ip);
        }
        ip
    } else if let Ok(raw) = std::env::var("SSH_CONNECTION") {
        let conn = parse_ssh_connection(&raw).context("parsing SSH_CONNECTION")?;
        expected_client_ip = Some(conn.client_ip);
        conn.server_ip
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    };

    if !allow_public && !is_overlay_or_private_ip(bind_ip) {
        bail!(
            "refusing to bind UDP socket on non-overlay/public IP {bind_ip}. \
             Connect over Tailscale/ZeroTier/WireGuard or pass --allow-public"
        );
    }

    Ok((bind_ip, expected_client_ip))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.print_udev_rule {
        println!("{UDEV_RULE}");
        return Ok(());
    }

    // All logs MUST go to stderr so stdout remains clean for the single-line JSON handshake.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let (bind_ip, detected_client_ip) = resolve_bind_ip(args.bind_ip, args.allow_public)?;
    let expected_client_ip = args.expected_client_ip.or(detected_client_ip);
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut capture = args.capture;

    #[cfg(target_os = "windows")]
    if windows_session::needs_session_bridge(args.windows_worker, capture) {
        tracing::info!(
            "running inside Windows Session 0; launching Session 1 interactive worker for DXGI/WASAPI/SendInput"
        );
        let worker_args = build_worker_args(&args, bind_ip, expected_client_ip);
        match windows_session::run_session_bridge(&worker_args).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if capture == CaptureSource::Dxgi {
                    return Err(err);
                }
                tracing::warn!(
                    "Session 0 -> Session 1 bridge failed ({err:#}); falling back to in-process OpenH264 testsrc"
                );
                capture = CaptureSource::Testsrc;
            }
        }
    }

    #[cfg(target_os = "windows")]
    let mut worker_pipe = if let Some(ref pipe_name) = args.bridge_pipe {
        Some(windows_session::connect_worker_pipe(pipe_name).await?)
    } else {
        None
    };

    let probe = probe_drm();

    #[cfg(target_os = "windows")]
    let displays: Vec<DisplayInfo> = if capture == CaptureSource::Testsrc {
        Vec::new()
    } else {
        video::windows_capture::enumerate_displays()
    };
    #[cfg(target_os = "linux")]
    let displays: Vec<DisplayInfo> = probe.displays.clone();
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let displays: Vec<DisplayInfo> = Vec::new();

    let active_display = if displays.is_empty() {
        0
    } else {
        args.display.min((displays.len() - 1) as u8)
    };

    let (raw_w, raw_h) = if args.width > 0 && args.height > 0 {
        (args.width, args.height)
    } else if args.headless == HeadlessBackend::None && capture != CaptureSource::Testsrc {
        displays
            .iter()
            .find(|d| d.index == active_display)
            .or_else(|| displays.first())
            .map(|d| (d.width, d.height))
            .unwrap_or((1280, 720))
    } else {
        (1280, 720)
    };
    let width = (raw_w.max(64)) & !1;
    let height = (raw_h.max(64)) & !1;
    let fps = args.fps.clamp(1, 240);
    let mtu = args.mtu.max(512);
    let fec = args.fec.min(50);

    let max_bitrate_kbps = args
        .max_bitrate_kbps
        .max(args.bitrate_kbps.unwrap_or(0))
        .max(1_500);
    let adaptive_controller = AdaptiveController::new(
        max_bitrate_kbps,
        args.bitrate_kbps,
        fec,
        args.prefer,
        !args.no_adaptive,
    );
    let initial_bitrate_kbps = adaptive_controller.applied_bitrate_kbps;
    let initial_fec = adaptive_controller.fec_percent;

    let headless_session = HeadlessSession::start(args.headless, width, height, fps)?;

    let client_codecs: Vec<VideoCodec> = args
        .client_codecs
        .split(',')
        .filter_map(|s| s.trim().parse::<VideoCodec>().ok())
        .collect();
    let host_codecs = probe_host_codecs(capture, args.encoder, &probe);
    let negotiated_codec = negotiate_codec(args.codec, &client_codecs, &host_codecs);

    let mut video_cfg = VideoPipelineConfig {
        width,
        height,
        fps,
        bitrate_kbps: initial_bitrate_kbps,
        codec: negotiated_codec,
        capture,
        encoder: args.encoder,
        prefer: args.prefer,
        display_index: active_display,
        render_node: probe.render_node,
        wayland_runtime_dir: headless_session.runtime_dir().map(std::path::Path::to_path_buf),
        wayland_display: headless_session.wayland_display().map(str::to_string),
    };
    let resolved_backend = resolve_video_backend(&video_cfg);
    let effective_codec = resolve_effective_codec(&video_cfg);
    if effective_codec != negotiated_codec {
        tracing::warn!(
            requested = %negotiated_codec,
            effective = %effective_codec,
            "selected backend forces codec adjustment"
        );
    }
    video_cfg.codec = effective_codec;

    let session_key = generate_session_key()?;
    let socket = Arc::new(
        UdpSocket::bind(SocketAddr::new(bind_ip, 0))
            .await
            .with_context(|| format!("failed to bind UDP socket on {bind_ip}:0"))?,
    );
    let local_port = socket.local_addr()?.port();

    let hello = HelloMessage {
        version: PROTOCOL_VERSION,
        bind_ip,
        port: local_port,
        session_key: HelloMessage::encode_session_key(&session_key),
        width,
        height,
        fps,
        codec: effective_codec,
        audio: !args.no_audio,
        audio_sample_rate: AUDIO_SAMPLE_RATE,
        audio_channels: AUDIO_CHANNELS,
        mtu,
        max_bitrate_kbps: adaptive_controller.max_bitrate_kbps,
        bitrate_kbps: initial_bitrate_kbps,
        fec_percent: initial_fec,
        display_index: active_display,
        displays: displays.clone(),
        prefer: args.prefer,
        supported_codecs: host_codecs.clone(),
    };

    let hello_line = encode_hello_line(&hello)?;
    #[cfg(target_os = "windows")]
    if let Some(ref mut pipe) = worker_pipe {
        pipe.write_all(hello_line.as_bytes()).await?;
        pipe.write_all(b"\n").await?;
        let info = format!(
            "backend={resolved_backend:?} codec={effective_codec} audio={}\n",
            !args.no_audio
        );
        let _ = pipe.write_all(info.as_bytes()).await;
        pipe.flush().await?;
    } else {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{hello_line}")?;
        stdout.flush()?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{hello_line}")?;
        stdout.flush()?;
    }

    tracing::info!(
        %bind_ip,
        port = local_port,
        width,
        height,
        fps,
        codec = %effective_codec,
        mtu,
        fec,
        "usher-host listening"
    );

    let cancel = CancellationToken::new();
    let start = Instant::now();
    let keyframe_req = Arc::new(AtomicBool::new(true));

    // Watchdog 1: SSH stdin EOF (or bridge pipe EOF when running as Windows Session 1 worker)
    #[cfg(target_os = "windows")]
    if let Some(pipe) = worker_pipe.take() {
        let cancel_pipe = cancel.clone();
        tokio::spawn(async move {
            let (mut pipe_rx, _pipe_tx) = tokio::io::split(pipe);
            let mut buf = [0u8; 64];
            loop {
                match pipe_rx.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        tracing::info!("Session 0 bridge pipe closed; tearing down Session 1 worker");
                        cancel_pipe.cancel();
                        break;
                    }
                    Ok(_) => {}
                }
            }
        });
    } else {
        let cancel_stdin = cancel.clone();
        tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut buf = [0u8; 64];
            loop {
                match stdin.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        tracing::info!("SSH stdin closed (EOF); tearing down session");
                        cancel_stdin.cancel();
                        break;
                    }
                    Ok(_) => {}
                }
            }
        });
    }
    #[cfg(not(target_os = "windows"))]
    {
        let cancel_stdin = cancel.clone();
        tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut buf = [0u8; 64];
            loop {
                match stdin.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        tracing::info!("SSH stdin closed (EOF); tearing down session");
                        cancel_stdin.cancel();
                        break;
                    }
                    Ok(_) => {}
                }
            }
        });
    }

    // Watchdog 2: OS signals (Ctrl+C, SIGTERM, SIGHUP)
    {
        let cancel_sig = cancel.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut sigterm = signal(SignalKind::terminate()).ok();
                let mut sighup = signal(SignalKind::hangup()).ok();
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("received SIGINT; tearing down");
                    }
                    _ = async {
                        if let Some(ref mut s) = sigterm {
                            let _ = s.recv().await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {
                        tracing::info!("received SIGTERM; tearing down");
                    }
                    _ = async {
                        if let Some(ref mut s) = sighup {
                            let _ = s.recv().await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {
                        tracing::info!("received SIGHUP; tearing down");
                    }
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("received Ctrl+C; tearing down");
            }
            cancel_sig.cancel();
        });
    }

    let crypto = Arc::new(Mutex::new(SessionCrypto::new(&session_key, Role::Host)));
    let input_sink = create_input_sink(!args.no_input);
    let (video_tx, video_rx) = mpsc::channel(4);
    let (audio_tx, audio_rx) = mpsc::channel(16);

    let peer_ready = Arc::new(tokio::sync::Notify::new());
    let (video_cfg_tx, video_cfg_rx) = watch::channel(video_cfg);
    let live_bitrate_kbps = Arc::new(AtomicU32::new(initial_bitrate_kbps));
    let in_place_bitrate = Arc::new(AtomicBool::new(is_in_place_bitrate_backend(
        &resolved_backend,
    )));
    let fec_percent = Arc::new(AtomicU8::new(initial_fec));
    let audio_enabled = Arc::new(AtomicBool::new(!args.no_audio));
    let adaptive = Arc::new(Mutex::new(adaptive_controller));

    let net_handles = spawn_network_tasks(
        NetConfig {
            socket,
            crypto,
            expected_client_ip,
            mtu: mtu as usize,
            fec_percent,
            audio_enabled,
            idle_timeout: Duration::from_secs(args.idle_timeout_secs),
            start,
            keyframe_req: keyframe_req.clone(),
            peer_ready: peer_ready.clone(),
            cancel: cancel.clone(),
            adaptive,
            live_bitrate_kbps: live_bitrate_kbps.clone(),
            in_place_bitrate: in_place_bitrate.clone(),
            video_cfg_tx,
            client_codecs,
            host_codecs,
            displays,
        },
        input_sink,
        video_rx,
        audio_rx,
    );

    let video_handle = spawn_video_task(
        video_cfg_rx,
        live_bitrate_kbps,
        in_place_bitrate,
        video_tx,
        keyframe_req,
        peer_ready,
        start,
        cancel.clone(),
    );

    let use_test_audio = capture == CaptureSource::Testsrc
        || (!cfg!(target_os = "linux") && !cfg!(target_os = "windows"));
    let audio_handle = spawn_audio_task(
        AudioStreamConfig {
            enabled: !args.no_audio,
            test_tone: use_test_audio,
        },
        audio_tx,
        start,
        cancel.clone(),
    );

    cancel.cancelled().await;

    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = video_handle.await;
        let _ = audio_handle.await;
        for h in net_handles {
            let _ = h.await;
        }
    })
    .await;

    drop(headless_session);
    tracing::info!("usher-host exited cleanly");
    Ok(())
}

#[cfg(target_os = "windows")]
fn build_worker_args(
    args: &Args,
    bind_ip: IpAddr,
    expected_client_ip: Option<IpAddr>,
) -> Vec<String> {
    let capture_str = match args.capture {
        CaptureSource::Auto => "auto",
        CaptureSource::Drm => "drm",
        CaptureSource::Pipewire => "pipewire",
        CaptureSource::X11 => "x11",
        CaptureSource::Dxgi => "dxgi",
        CaptureSource::Testsrc => "testsrc",
    };
    let encoder_str = match args.encoder {
        EncoderKind::Auto => "auto",
        EncoderKind::Vaapi => "vaapi",
        EncoderKind::Nvenc => "nvenc",
        EncoderKind::Amf => "amf",
        EncoderKind::Qsv => "qsv",
        EncoderKind::Openh264 => "openh264",
    };
    let mut out = vec![
        "--bind-ip".into(),
        bind_ip.to_string(),
        "--width".into(),
        args.width.to_string(),
        "--height".into(),
        args.height.to_string(),
        "--fps".into(),
        args.fps.to_string(),
        "--max-bitrate-kbps".into(),
        args.max_bitrate_kbps.to_string(),
        "--codec".into(),
        args.codec.to_string(),
        "--client-codecs".into(),
        args.client_codecs.clone(),
        "--prefer".into(),
        args.prefer.to_string(),
        "--display".into(),
        args.display.to_string(),
        "--capture".into(),
        capture_str.into(),
        "--encoder".into(),
        encoder_str.into(),
        "--mtu".into(),
        args.mtu.to_string(),
        "--fec".into(),
        args.fec.to_string(),
        "--idle-timeout-secs".into(),
        args.idle_timeout_secs.to_string(),
    ];
    if let Some(br) = args.bitrate_kbps {
        out.push("--bitrate-kbps".into());
        out.push(br.to_string());
    }
    if args.no_adaptive {
        out.push("--no-adaptive".into());
    }
    if let Some(ip) = expected_client_ip {
        out.push("--expected-client-ip".into());
        out.push(ip.to_string());
    }
    if args.allow_public {
        out.push("--allow-public".into());
    }
    if args.no_audio {
        out.push("--no-audio".into());
    }
    if args.no_input {
        out.push("--no-input".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_public_bind_ip_by_default() {
        assert!(resolve_bind_ip(Some("8.8.8.8".parse().unwrap()), false).is_err());
        assert!(resolve_bind_ip(Some("8.8.8.8".parse().unwrap()), true).is_ok());
        assert!(resolve_bind_ip(Some("100.100.10.20".parse().unwrap()), false).is_ok());
        assert!(resolve_bind_ip(Some("127.0.0.1".parse().unwrap()), false).is_ok());
    }
}
