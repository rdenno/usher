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
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
#[cfg(target_os = "windows")]
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use usher_proto::audio::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use usher_proto::crypto::{Role, SessionCrypto, generate_session_key};
use usher_proto::handshake::{
    HelloMessage, PROTOCOL_VERSION, VideoCodec, encode_hello_line, is_overlay_or_private_ip,
    parse_ssh_connection,
};
use usher_proto::packet::DEFAULT_MTU;

use crate::audio::{AudioStreamConfig, spawn_audio_task};
use crate::input::create_input_sink;
use crate::net::{NetConfig, spawn_network_tasks};
use crate::video::drm_probe::probe_drm;
use crate::video::headless::{HeadlessBackend, HeadlessSession};
use crate::video::{
    CaptureSource, EncoderKind, ResolvedBackend, VideoPipelineConfig, resolve_video_backend,
    spawn_video_task,
};

const UDEV_RULE: &str = r#"KERNEL=="uinput", GROUP="input", MODE="0660""#;

#[derive(Debug, Parser)]
#[command(name = "usher-host", about = "Zero-daemon SSH-bootstrapped Usher host")]
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

    /// Target video bitrate in kbps
    #[arg(long, default_value_t = 20_000)]
    bitrate_kbps: u32,

    /// Video codec (h264, hevc, av1)
    #[arg(long, default_value = "h264")]
    codec: VideoCodec,

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

    let (raw_w, raw_h) = if args.width > 0 && args.height > 0 {
        (args.width, args.height)
    } else if args.headless == HeadlessBackend::None && capture != CaptureSource::Testsrc {
        #[cfg(target_os = "linux")]
        let detected = probe.active_mode;
        #[cfg(target_os = "windows")]
        let detected = video::windows_capture::primary_monitor_resolution();
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        let res = detected.unwrap_or((1280, 720));
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        let res = (1280, 720);
        res
    } else {
        (1280, 720)
    };
    let width = (raw_w.max(64)) & !1;
    let height = (raw_h.max(64)) & !1;
    let fps = args.fps.clamp(1, 240);
    let mtu = args.mtu.max(512);
    let fec = args.fec.min(50);

    let headless_session = HeadlessSession::start(args.headless, width, height, fps)?;

    let mut video_cfg = VideoPipelineConfig {
        width,
        height,
        fps,
        bitrate_kbps: args.bitrate_kbps,
        codec: args.codec,
        capture,
        encoder: args.encoder,
        render_node: probe.render_node,
        wayland_runtime_dir: headless_session.runtime_dir().map(std::path::Path::to_path_buf),
        wayland_display: headless_session.wayland_display().map(str::to_string),
    };
    let resolved_backend = resolve_video_backend(&video_cfg);
    let effective_codec = match &resolved_backend {
        ResolvedBackend::Software => {
            if args.codec != VideoCodec::H264 {
                tracing::warn!(
                    "resolved OpenH264 software backend emits H.264; adjusting handshake codec to h264"
                );
            }
            VideoCodec::H264
        }
        #[cfg(target_os = "linux")]
        ResolvedBackend::Hw(_) => args.codec,
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(video::windows_capture::WinVideoBackend::NativeDxgiOpenH264) => {
            if args.codec != VideoCodec::H264 {
                tracing::warn!(
                    "built-in DXGI + OpenH264 backend emits H.264; adjusting handshake codec to h264"
                );
            }
            VideoCodec::H264
        }
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(video::windows_capture::WinVideoBackend::DdagrabHw { .. }) => {
            args.codec
        }
    };
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

    let net_handles = spawn_network_tasks(
        NetConfig {
            socket,
            crypto,
            expected_client_ip,
            mtu: mtu as usize,
            fec_percent: fec,
            idle_timeout: Duration::from_secs(args.idle_timeout_secs),
            start,
            keyframe_req: keyframe_req.clone(),
            cancel: cancel.clone(),
        },
        input_sink,
        video_rx,
        audio_rx,
    );

    let video_handle = spawn_video_task(
        video_cfg,
        resolved_backend,
        video_tx,
        keyframe_req,
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
        "--bitrate-kbps".into(),
        args.bitrate_kbps.to_string(),
        "--codec".into(),
        args.codec.to_string(),
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
