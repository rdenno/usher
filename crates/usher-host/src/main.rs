mod audio;
mod clipboard;
mod input;
mod net;
mod video;
#[cfg(target_os = "windows")]
mod windows_ipc;
#[cfg(target_os = "windows")]
mod windows_session;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32};
#[cfg(target_os = "windows")]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
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

    /// Disable bidirectional clipboard synchronization
    #[arg(long)]
    no_clipboard: bool,

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

    /// Internal flag: SYSTEM launcher inside Session 0 spawning console worker into target session
    #[arg(long, hide = true)]
    windows_system_launcher: bool,

    /// Internal flag: target Windows console session ID for SYSTEM launcher
    #[arg(long, hide = true)]
    target_session: Option<u32>,

    /// Internal flag: console worker is running as NT AUTHORITY\SYSTEM
    #[arg(long, hide = true)]
    is_system: bool,

    /// Internal flag: explicit absolute ffmpeg path forwarded from SSH Session 0
    #[arg(long, hide = true)]
    ffmpeg_path: Option<std::path::PathBuf>,
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

#[cfg(target_os = "windows")]
#[derive(Clone)]
struct PipeLogMakeWriter {
    tx: mpsc::UnboundedSender<String>,
}

#[cfg(target_os = "windows")]
struct PipeLogWriter {
    tx: mpsc::UnboundedSender<String>,
    buf: Vec<u8>,
}

#[cfg(target_os = "windows")]
impl std::io::Write for PipeLogWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buf))
                .trim_end_matches(&['\r', '\n'][..])
                .to_string();
            if !line.is_empty() {
                let _ = self.tx.send(line);
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
impl Drop for PipeLogWriter {
    fn drop(&mut self) {
        let _ = std::io::Write::flush(self);
    }
}

#[cfg(target_os = "windows")]
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for PipeLogMakeWriter {
    type Writer = PipeLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        PipeLogWriter {
            tx: self.tx.clone(),
            buf: Vec::with_capacity(256),
        }
    }
}

#[cfg(target_os = "windows")]
struct PipeInputSink {
    cmd_tx: mpsc::Sender<windows_ipc::HostCmd>,
}

#[cfg(target_os = "windows")]
impl crate::input::InputSink for PipeInputSink {
    fn handle(&mut self, event: usher_proto::input::InputEvent) {
        let _ = self
            .cmd_tx
            .try_send(windows_ipc::HostCmd::Input(event.encode()));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.print_udev_rule {
        println!("{UDEV_RULE}");
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    if args.windows_system_launcher {
        let target_session = args
            .target_session
            .unwrap_or_else(windows_session::active_console_session_id);
        let pipe_name = args
            .bridge_pipe
            .as_deref()
            .context("--windows-system-launcher requires --bridge-pipe")?;
        return windows_session::run_system_launcher(
            target_session,
            pipe_name,
            args.ffmpeg_path.as_deref(),
            args.no_audio,
            args.no_input,
            args.no_clipboard,
        );
    }

    #[cfg(target_os = "windows")]
    if args.windows_worker {
        return run_windows_console_worker(args).await;
    }

    // All logs MUST go to stderr with ANSI disabled so stdout remains clean for the single-line JSON handshake.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let (bind_ip, detected_client_ip) = resolve_bind_ip(args.bind_ip, args.allow_public)?;
    let expected_client_ip = args.expected_client_ip.or(detected_client_ip);
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut capture = args.capture;

    #[cfg(target_os = "windows")]
    let mut console_worker = None;
    #[cfg(target_os = "windows")]
    let mut host_ffmpeg_path: Option<std::path::PathBuf> = None;

    #[cfg(target_os = "windows")]
    if windows_session::needs_session_bridge(
        args.windows_worker,
        args.windows_system_launcher,
        capture,
    ) {
        let resolved_ff = video::windows_capture::find_working_ffmpeg_absolute();
        if let Some(ref p) = resolved_ff {
            video::windows_capture::set_explicit_ffmpeg_path(p.clone());
        }
        host_ffmpeg_path = resolved_ff;
        let target_sess = windows_session::active_console_session_id();
        tracing::info!(
            target_session = target_sess,
            "running inside Windows Session 0; launching console worker for active desktop capture/input"
        );
        match windows_session::spawn_console_worker(
            target_sess,
            host_ffmpeg_path.as_deref(),
            args.no_audio,
            args.no_input,
            args.no_clipboard,
        )
        .await
        {
            Ok((pipe, hello_info, worker_session)) => {
                tracing::info!(
                    worker_session,
                    is_system = hello_info.is_system,
                    desktop = ?hello_info.desktop_name,
                    displays = hello_info.displays.len(),
                    codecs = ?hello_info.host_codecs,
                    "Windows console worker ready"
                );
                console_worker = Some((pipe, hello_info, worker_session));
            }
            Err(err) => {
                if capture == CaptureSource::Dxgi {
                    return Err(err);
                }
                tracing::warn!(
                    "Windows console worker spawn failed ({err:#}); falling back to in-process OpenH264 testsrc"
                );
                capture = CaptureSource::Testsrc;
            }
        }
    }

    let probe = probe_drm();

    #[cfg(target_os = "windows")]
    let (displays, bridge_host_codecs): (Vec<DisplayInfo>, Option<Vec<VideoCodec>>) =
        if capture == CaptureSource::Testsrc {
            (Vec::new(), None)
        } else if let Some((_, ref info, _)) = console_worker {
            (info.displays.clone(), Some(info.host_codecs.clone()))
        } else {
            let cur_session = windows_session::current_session_id();
            let active_session = windows_session::active_console_session_id();
            if cur_session != active_session {
                tracing::warn!(
                    worker_session = cur_session,
                    active_console_session = active_session,
                    "Windows session is not the active console; unlock the host desktop to capture the real screen"
                );
            }
            if !video::windows_capture::has_attached_dxgi_output() {
                tracing::warn!(
                    "DXGI reported no attached desktop outputs in this session"
                );
            }
            (video::windows_capture::enumerate_displays(), None)
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

    let active_disp_res = if args.headless == HeadlessBackend::None
        && capture != CaptureSource::Testsrc
    {
        displays
            .iter()
            .find(|d| d.index == active_display)
            .or_else(|| displays.first())
            .map(|d| (d.width, d.height))
    } else {
        None
    };
    let (raw_w, raw_h) = match (args.width > 0 && args.height > 0, active_disp_res) {
        (true, Some((dw, dh))) => {
            usher_proto::handshake::fit_preserving_aspect(dw, dh, args.width, args.height)
        }
        (true, None) => (args.width, args.height),
        (false, Some((dw, dh))) => (dw, dh),
        (false, None) => (1280, 720),
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
    #[cfg(target_os = "windows")]
    let host_codecs = bridge_host_codecs
        .unwrap_or_else(|| probe_host_codecs(capture, args.encoder, &probe));
    #[cfg(not(target_os = "windows"))]
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
    let udp = UdpSocket::bind(SocketAddr::new(bind_ip, 0))
        .await
        .with_context(|| format!("failed to bind UDP socket on {bind_ip}:0"))?;
    {
        let sock_ref = socket2::SockRef::from(&udp);
        let _ = sock_ref.set_send_buffer_size(4 * 1024 * 1024);
        let _ = sock_ref.set_recv_buffer_size(1024 * 1024);
    }
    let socket = Arc::new(udp);
    let local_port = socket.local_addr()?.port();

    let gamepad_backend = if args.no_input {
        "disabled (--no-input)".to_string()
    } else {
        #[cfg(target_os = "windows")]
        {
            if input::win32_input::vigem_available() {
                "ViGEmBus (Xbox 360)".to_string()
            } else {
                "none (install ViGEmBus for Xbox 360 gamepad)".to_string()
            }
        }
        #[cfg(target_os = "linux")]
        {
            if std::path::Path::new("/dev/uinput").exists() {
                "uinput (Xbox 360)".to_string()
            } else {
                "none (/dev/uinput unavailable)".to_string()
            }
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            "none".to_string()
        }
    };

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
        host_version: env!("CARGO_PKG_VERSION").to_string(),
        video_backend: format!("{resolved_backend:?}"),
        gamepad_backend,
        hi_res_scroll: true,
    };

    let hello_line = encode_hello_line(&hello)?;
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

    // Watchdog 1: SSH stdin EOF
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
    #[cfg(target_os = "windows")]
    let (input_sink, bridge_cmd_rx): (
        Box<dyn crate::input::InputSink>,
        Option<mpsc::Receiver<windows_ipc::HostCmd>>,
    ) = if console_worker.is_some() && !args.no_input {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        (Box::new(PipeInputSink { cmd_tx }), Some(cmd_rx))
    } else {
        (create_input_sink(!args.no_input), None)
    };
    #[cfg(not(target_os = "windows"))]
    let input_sink = create_input_sink(!args.no_input);
    let (video_tx, video_rx) = mpsc::channel(4);
    let (audio_tx, audio_rx) = mpsc::channel(16);
    let (clip_in_tx, clip_in_rx) = mpsc::channel::<String>(16);
    let (clip_out_tx, clip_out_rx) = mpsc::channel::<String>(16);

    let peer_ready = Arc::new(tokio::sync::Notify::new());
    let effective_codec_atomic = Arc::new(AtomicU8::new(effective_codec.to_u8()));
    let status_notify = Arc::new(tokio::sync::Notify::new());
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
            audio_enabled: audio_enabled.clone(),
            idle_timeout: Duration::from_secs(args.idle_timeout_secs),
            start,
            keyframe_req: keyframe_req.clone(),
            peer_ready: peer_ready.clone(),
            effective_codec: effective_codec_atomic.clone(),
            status_notify: status_notify.clone(),
            cancel: cancel.clone(),
            adaptive,
            live_bitrate_kbps: live_bitrate_kbps.clone(),
            in_place_bitrate: in_place_bitrate.clone(),
            video_cfg_tx,
            client_codecs,
            host_codecs,
            displays,
            clipboard_in_tx: if args.no_clipboard {
                None
            } else {
                Some(clip_in_tx)
            },
        },
        input_sink,
        video_rx,
        audio_rx,
        clip_out_rx,
    );

    #[cfg(target_os = "windows")]
    let (video_handle, audio_handle, clip_handle) = if let Some((
        initial_pipe,
        _info,
        worker_session,
    )) = console_worker
    {
        let (_dummy_tx, dummy_rx) = mpsc::channel(1);
        let cmd_rx = bridge_cmd_rx.unwrap_or(dummy_rx);
        let sup = tokio::spawn(run_windows_session_supervisor(
            initial_pipe,
            worker_session,
            host_ffmpeg_path,
            !args.no_audio,
            args.no_input,
            args.no_clipboard,
            video_cfg_rx,
            live_bitrate_kbps,
            in_place_bitrate,
            effective_codec_atomic,
            status_notify,
            video_tx,
            audio_tx,
            keyframe_req,
            peer_ready,
            audio_enabled,
            cmd_rx,
            clip_in_rx,
            clip_out_tx,
            cancel.clone(),
        ));
        (sup, tokio::spawn(async {}), tokio::spawn(async {}))
    } else {
        let vh = spawn_video_task(
            video_cfg_rx,
            live_bitrate_kbps,
            in_place_bitrate,
            effective_codec_atomic,
            status_notify,
            video_tx,
            keyframe_req,
            peer_ready,
            start,
            cancel.clone(),
        );
        let ah = spawn_audio_task(
            AudioStreamConfig {
                enabled: !args.no_audio,
                test_tone: capture == CaptureSource::Testsrc,
            },
            audio_tx,
            start,
            cancel.clone(),
        );
        let ch = clipboard::spawn_clipboard_task(
            !args.no_clipboard,
            clip_out_tx,
            clip_in_rx,
            cancel.clone(),
        );
        (vh, ah, ch)
    };

    #[cfg(not(target_os = "windows"))]
    let (video_handle, audio_handle, clip_handle) = {
        let vh = spawn_video_task(
            video_cfg_rx,
            live_bitrate_kbps,
            in_place_bitrate,
            effective_codec_atomic,
            status_notify,
            video_tx,
            keyframe_req,
            peer_ready,
            start,
            cancel.clone(),
        );
        let use_test_audio = capture == CaptureSource::Testsrc
            || (!cfg!(target_os = "linux") && !cfg!(target_os = "windows"));
        let ah = spawn_audio_task(
            AudioStreamConfig {
                enabled: !args.no_audio,
                test_tone: use_test_audio,
            },
            audio_tx,
            start,
            cancel.clone(),
        );
        let ch = clipboard::spawn_clipboard_task(
            !args.no_clipboard,
            clip_out_tx,
            clip_in_rx,
            cancel.clone(),
        );
        (vh, ah, ch)
    };

    cancel.cancelled().await;

    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = video_handle.await;
        let _ = audio_handle.await;
        let _ = clip_handle.await;
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
fn to_worker_video_cfg(cfg: &VideoPipelineConfig) -> windows_ipc::WorkerVideoConfig {
    windows_ipc::WorkerVideoConfig {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        bitrate_kbps: cfg.bitrate_kbps,
        codec: cfg.codec,
        capture: cfg.capture.as_str().to_string(),
        encoder: cfg.encoder.as_str().to_string(),
        prefer: cfg.prefer,
        display_index: cfg.display_index,
    }
}

#[cfg(target_os = "windows")]
async fn run_windows_console_worker(args: Args) -> Result<()> {
    use windows_ipc::{HostCmd, WorkerHelloInfo, WorkerMsg, read_host_cmd, write_worker_msg};

    if let Some(ref ff) = args.ffmpeg_path {
        video::windows_capture::set_explicit_ffmpeg_path(ff.clone());
    }

    let pipe_name = args
        .bridge_pipe
        .as_deref()
        .context("--windows-worker requires --bridge-pipe")?;
    let pipe = windows_session::connect_worker_pipe(pipe_name).await?;
    let (mut pipe_rx, mut pipe_tx) = tokio::io::split(pipe);

    let (msg_tx, mut msg_rx) = mpsc::channel::<WorkerMsg>(64);
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<String>();

    let writer_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                maybe_msg = msg_rx.recv() => match maybe_msg {
                    Some(msg) => {
                        if write_worker_msg(&mut pipe_tx, &msg).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                maybe_log = log_rx.recv() => if let Some(line) = maybe_log
                    && write_worker_msg(&mut pipe_tx, &WorkerMsg::LogLine(line)).await.is_err()
                {
                    break;
                }
            }
        }
        while let Ok(line) = log_rx.try_recv() {
            let _ = write_worker_msg(&mut pipe_tx, &WorkerMsg::LogLine(line)).await;
        }
    });

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(PipeLogMakeWriter { tx: log_tx.clone() })
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let (desktop_name, displays, host_codecs) = windows_session::with_input_desktop(|name| {
        let displays = video::windows_capture::enumerate_displays();
        let probe = probe_drm();
        let codecs = probe_host_codecs(args.capture, args.encoder, &probe);
        (name, displays, codecs)
    });
    let worker_session = windows_session::current_session_id();
    if !args.is_system && !video::windows_capture::has_attached_dxgi_output() {
        tracing::warn!(
            "DXGI reported no attached desktop outputs in interactive worker (host desktop may be locked and SYSTEM elevation was unavailable)"
        );
    }

    msg_tx
        .send(WorkerMsg::HelloInfo(WorkerHelloInfo {
            displays,
            host_codecs,
            worker_session,
            is_system: args.is_system,
            desktop_name,
        }))
        .await
        .context("sending console worker HelloInfo")?;

    let worker_cancel = CancellationToken::new();
    let start = Instant::now();
    let mut input_sink = create_input_sink(!args.no_input);

    let (clip_out_tx, mut clip_out_rx) = mpsc::channel::<String>(16);
    let (clip_in_tx, clip_in_rx) = mpsc::channel::<String>(16);
    let clip_fwd = {
        let msg_tx = msg_tx.clone();
        let cancel = worker_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    txt = clip_out_rx.recv() => match txt {
                        Some(text) => {
                            let _ = msg_tx.try_send(WorkerMsg::ClipboardText(text));
                        }
                        None => break,
                    }
                }
            }
        })
    };
    let clip_handle = clipboard::spawn_clipboard_task(
        !args.no_clipboard,
        clip_out_tx,
        clip_in_rx,
        worker_cancel.clone(),
    );

    let audio_enabled = Arc::new(AtomicBool::new(!args.no_audio));
    let (a_tx, mut a_rx) = mpsc::channel::<usher_proto::audio::AudioPacket>(16);
    let audio_fwd = {
        let msg_tx = msg_tx.clone();
        let audio_enabled = audio_enabled.clone();
        let cancel = worker_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    pkt = a_rx.recv() => match pkt {
                        Some(p) => {
                            if audio_enabled.load(Ordering::Acquire) {
                                let _ = msg_tx.try_send(WorkerMsg::AudioPacket(p.encode()));
                            }
                        }
                        None => break,
                    }
                }
            }
        })
    };
    let audio_handle = spawn_audio_task(
        AudioStreamConfig {
            enabled: !args.no_audio,
            test_tone: false,
        },
        a_tx,
        start,
        worker_cancel.clone(),
    );

    let (vcfg_tx, mut vcfg_rx) = watch::channel::<Option<VideoPipelineConfig>>(None);
    let live_bitrate_kbps = Arc::new(AtomicU32::new(25_000));
    let in_place_bitrate = Arc::new(AtomicBool::new(false));
    let effective_codec = Arc::new(AtomicU8::new(VideoCodec::H264.to_u8()));
    let status_notify = Arc::new(tokio::sync::Notify::new());
    let keyframe_req = Arc::new(AtomicBool::new(true));

    let (v_tx, mut v_rx) = mpsc::channel::<crate::video::EncodedFrame>(8);
    let video_fwd = {
        let msg_tx = msg_tx.clone();
        let cancel = worker_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    f = v_rx.recv() => match f {
                        Some(frame) => {
                            let _ = msg_tx.try_send(WorkerMsg::VideoFrame {
                                keyframe: frame.keyframe,
                                timestamp_us: frame.timestamp_us,
                                data: frame.data,
                            });
                        }
                        None => break,
                    }
                }
            }
        })
    };

    let status_fwd = {
        let msg_tx = msg_tx.clone();
        let status_notify = status_notify.clone();
        let effective_codec = effective_codec.clone();
        let in_place_bitrate = in_place_bitrate.clone();
        let cancel = worker_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = status_notify.notified() => {
                        if let Ok(codec) = VideoCodec::from_u8(effective_codec.load(Ordering::Acquire)) {
                            let _ = msg_tx.try_send(WorkerMsg::CodecChanged {
                                codec,
                                in_place_bitrate: in_place_bitrate.load(Ordering::Acquire),
                            });
                        }
                    }
                }
            }
        })
    };

    let video_sup = {
        let cancel = worker_cancel.clone();
        let live_br = live_bitrate_kbps.clone();
        let in_place = in_place_bitrate.clone();
        let eff_codec = effective_codec.clone();
        let st_notify = status_notify.clone();
        let msg_tx = msg_tx.clone();
        let kf_req = keyframe_req.clone();
        tokio::spawn(async move {
            while vcfg_rx.borrow_and_update().is_none() {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    res = vcfg_rx.changed() => {
                        if res.is_err() {
                            return;
                        }
                    }
                }
            }
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let Some(cfg) = vcfg_rx.borrow_and_update().clone() else {
                    break;
                };
                live_br.store(cfg.bitrate_kbps, Ordering::Release);
                let backend = resolve_video_backend(&cfg);
                let ip = is_in_place_bitrate_backend(&backend);
                in_place.store(ip, Ordering::Release);
                eff_codec.store(cfg.codec.to_u8(), Ordering::Release);
                let _ = msg_tx.try_send(WorkerMsg::CodecChanged {
                    codec: cfg.codec,
                    in_place_bitrate: ip,
                });
                let child_cancel = cancel.child_token();
                let runner = video::run_one_backend(
                    cfg,
                    backend,
                    live_br.clone(),
                    in_place.clone(),
                    eff_codec.clone(),
                    st_notify.clone(),
                    v_tx.clone(),
                    kf_req.clone(),
                    start,
                    child_cancel.clone(),
                );
                tokio::pin!(runner);

                let mut restart_for_change = false;
                tokio::select! {
                    _ = &mut runner => {}
                    res = vcfg_rx.changed() => {
                        if res.is_ok() {
                            restart_for_change = true;
                            kf_req.store(true, Ordering::Release);
                            child_cancel.cancel();
                            let _ = tokio::time::timeout(Duration::from_millis(800), &mut runner).await;
                        }
                    }
                    _ = cancel.cancelled() => {
                        child_cancel.cancel();
                        let _ = tokio::time::timeout(Duration::from_millis(500), &mut runner).await;
                        break;
                    }
                }
                if !restart_for_change {
                    break;
                }
            }
        })
    };

    loop {
        match read_host_cmd(&mut pipe_rx).await {
            Ok(HostCmd::StartVideo(wcfg)) => {
                let vcfg = VideoPipelineConfig {
                    width: wcfg.width,
                    height: wcfg.height,
                    fps: wcfg.fps,
                    bitrate_kbps: wcfg.bitrate_kbps,
                    codec: wcfg.codec,
                    capture: CaptureSource::from_str_name(&wcfg.capture),
                    encoder: EncoderKind::from_str_name(&wcfg.encoder),
                    prefer: wcfg.prefer,
                    display_index: wcfg.display_index,
                    render_node: None,
                    wayland_runtime_dir: None,
                    wayland_display: None,
                };
                live_bitrate_kbps.store(vcfg.bitrate_kbps, Ordering::Release);
                let _ = vcfg_tx.send(Some(vcfg));
            }
            Ok(HostCmd::UpdateBitrate(kbps)) => {
                live_bitrate_kbps.store(kbps, Ordering::Release);
            }
            Ok(HostCmd::RequestKeyframe) => {
                keyframe_req.store(true, Ordering::Release);
            }
            Ok(HostCmd::Input(bytes)) => {
                if let Ok(ev) = usher_proto::input::InputEvent::decode(&bytes) {
                    input_sink.handle(ev);
                }
            }
            Ok(HostCmd::AudioEnable(on)) => {
                audio_enabled.store(on, Ordering::Release);
            }
            Ok(HostCmd::SetClipboardText(text)) => {
                let _ = clip_in_tx.try_send(text);
            }
            Ok(HostCmd::Shutdown) | Err(_) => {
                break;
            }
        }
    }

    worker_cancel.cancel();
    drop(input_sink);
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = video_sup.await;
        let _ = audio_handle.await;
        let _ = audio_fwd.await;
        let _ = video_fwd.await;
        let _ = status_fwd.await;
        let _ = clip_handle.await;
        let _ = clip_fwd.await;
    })
    .await;

    drop(msg_tx);
    drop(log_tx);
    let _ = tokio::time::timeout(Duration::from_millis(200), writer_task).await;
    Ok(())
}

#[cfg(target_os = "windows")]
fn spawn_pipe_reader(
    mut pipe_rx: tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeServer>,
) -> (
    mpsc::Receiver<Result<windows_ipc::WorkerMsg, String>>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::channel(64);
    let handle = tokio::spawn(async move {
        loop {
            match windows_ipc::read_worker_msg(&mut pipe_rx).await {
                Ok(msg) => {
                    if tx.send(Ok(msg)).await.is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(format!("{err:#}"))).await;
                    break;
                }
            }
        }
    });
    (rx, handle)
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
async fn run_windows_session_supervisor(
    initial_pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    mut current_session: u32,
    host_ffmpeg_path: Option<std::path::PathBuf>,
    audio_allowed: bool,
    no_input: bool,
    no_clipboard: bool,
    mut video_cfg_rx: watch::Receiver<VideoPipelineConfig>,
    live_bitrate_kbps: Arc<AtomicU32>,
    in_place_bitrate: Arc<AtomicBool>,
    effective_codec_atomic: Arc<AtomicU8>,
    status_notify: Arc<tokio::sync::Notify>,
    video_tx: mpsc::Sender<crate::video::EncodedFrame>,
    audio_tx: mpsc::Sender<usher_proto::audio::AudioPacket>,
    keyframe_req: Arc<AtomicBool>,
    peer_ready: Arc<tokio::sync::Notify>,
    audio_enabled: Arc<AtomicBool>,
    mut cmd_rx: mpsc::Receiver<windows_ipc::HostCmd>,
    mut clip_in_rx: mpsc::Receiver<String>,
    clip_out_tx: mpsc::Sender<String>,
    cancel: CancellationToken,
) {
    use windows_ipc::{HostCmd, WorkerMsg, write_host_cmd};

    let (pipe_rx, mut pipe_tx) = tokio::io::split(initial_pipe);
    let (mut msg_rx, mut reader_task) = spawn_pipe_reader(pipe_rx);

    // Wait for first authenticated client packet while still logging pre-stream worker output.
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                reader_task.abort();
                let _ = tokio::time::timeout(
                    Duration::from_millis(100),
                    write_host_cmd(&mut pipe_tx, &HostCmd::Shutdown),
                )
                .await;
                return;
            }
            _ = peer_ready.notified() => break,
            maybe_msg = msg_rx.recv() => match maybe_msg {
                Some(Ok(WorkerMsg::LogLine(line))) => tracing::info!("[console-worker] {line}"),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            }
        }
    }

    let mut cur_cfg = video_cfg_rx.borrow_and_update().clone();
    let mut last_bitrate = live_bitrate_kbps.load(Ordering::Acquire);
    let mut last_audio = audio_enabled.load(Ordering::Acquire);
    cur_cfg.bitrate_kbps = last_bitrate;

    let mut needs_respawn = write_host_cmd(&mut pipe_tx, &HostCmd::StartVideo(to_worker_video_cfg(&cur_cfg)))
        .await
        .is_err();
    if !needs_respawn {
        let _ = write_host_cmd(&mut pipe_tx, &HostCmd::AudioEnable(last_audio)).await;
    }

    let mut fast_tick = tokio::time::interval(Duration::from_millis(50));
    fast_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sess_tick = tokio::time::interval(Duration::from_millis(150));
    sess_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if cancel.is_cancelled() {
            break;
        }

        if needs_respawn {
            let _ = tokio::time::timeout(
                Duration::from_millis(100),
                write_host_cmd(&mut pipe_tx, &HostCmd::Shutdown),
            )
            .await;
            reader_task.abort();
            drop(msg_rx);
            drop(pipe_tx);

            let mut spawned = None;
            for attempt in 0..8 {
                if cancel.is_cancelled() {
                    return;
                }
                let target = windows_session::active_console_session_id();
                if target != 0 && target != u32::MAX {
                    match windows_session::spawn_console_worker(
                        target,
                        host_ffmpeg_path.as_deref(),
                        !audio_allowed,
                        no_input,
                        no_clipboard,
                    )
                    .await
                    {
                        Ok(res) => {
                            spawned = Some(res);
                            break;
                        }
                        Err(err) => {
                            tracing::warn!(
                                attempt = attempt + 1,
                                target_session = target,
                                "failed to attach console worker ({err:#}); retrying"
                            );
                        }
                    }
                }
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(400)) => {}
                }
            }

            let Some((new_pipe, hello_info, new_sess)) = spawned else {
                tracing::error!("exhausted console worker respawns; stopping video supervisor");
                return;
            };

            current_session = new_sess;
            tracing::info!(
                worker_session = new_sess,
                is_system = hello_info.is_system,
                desktop = ?hello_info.desktop_name,
                "Windows console worker migrated"
            );
            let (rx, tx) = tokio::io::split(new_pipe);
            let (new_msg_rx, new_reader) = spawn_pipe_reader(rx);
            msg_rx = new_msg_rx;
            reader_task = new_reader;
            pipe_tx = tx;

            cur_cfg = video_cfg_rx.borrow_and_update().clone();
            last_bitrate = live_bitrate_kbps.load(Ordering::Acquire);
            last_audio = audio_enabled.load(Ordering::Acquire);
            cur_cfg.bitrate_kbps = last_bitrate;
            keyframe_req.store(true, Ordering::Release);

            if write_host_cmd(&mut pipe_tx, &HostCmd::StartVideo(to_worker_video_cfg(&cur_cfg)))
                .await
                .is_err()
            {
                needs_respawn = true;
                continue;
            }
            let _ = write_host_cmd(&mut pipe_tx, &HostCmd::AudioEnable(last_audio)).await;
            needs_respawn = false;
        }

        tokio::select! {
            _ = cancel.cancelled() => break,
            maybe_msg = msg_rx.recv() => match maybe_msg {
                Some(Ok(WorkerMsg::VideoFrame { keyframe, timestamp_us, data })) => {
                    let _ = video_tx.try_send(crate::video::EncodedFrame {
                        data,
                        keyframe,
                        timestamp_us,
                    });
                }
                Some(Ok(WorkerMsg::AudioPacket(bytes))) => {
                    if let Ok(pkt) = usher_proto::audio::AudioPacket::decode(&bytes) {
                        let _ = audio_tx.try_send(pkt);
                    }
                }
                Some(Ok(WorkerMsg::LogLine(line))) => {
                    tracing::info!("[console-worker] {line}");
                }
                Some(Ok(WorkerMsg::CodecChanged { codec, in_place_bitrate: ip })) => {
                    in_place_bitrate.store(ip, Ordering::Release);
                    effective_codec_atomic.store(codec.to_u8(), Ordering::Release);
                    status_notify.notify_one();
                }
                Some(Ok(WorkerMsg::ClipboardText(text))) => {
                    if !no_clipboard {
                        let _ = clip_out_tx.try_send(text);
                    }
                }
                Some(Ok(WorkerMsg::HelloInfo(_))) => {}
                Some(Err(err)) => {
                    if !cancel.is_cancelled() {
                        tracing::warn!("console worker pipe closed ({err}); respawning worker");
                        needs_respawn = true;
                    }
                }
                None => {
                    if !cancel.is_cancelled() {
                        tracing::warn!("console worker pipe reader ended; respawning worker");
                        needs_respawn = true;
                    }
                }
            },
            res = video_cfg_rx.changed() => {
                if res.is_ok() {
                    cur_cfg = video_cfg_rx.borrow_and_update().clone();
                    last_bitrate = cur_cfg.bitrate_kbps;
                    live_bitrate_kbps.store(last_bitrate, Ordering::Release);
                    if write_host_cmd(&mut pipe_tx, &HostCmd::StartVideo(to_worker_video_cfg(&cur_cfg))).await.is_err() {
                        needs_respawn = true;
                    }
                }
            },
            Some(cmd) = cmd_rx.recv() => {
                if write_host_cmd(&mut pipe_tx, &cmd).await.is_err() {
                    needs_respawn = true;
                }
            },
            Some(text) = clip_in_rx.recv() => {
                if !no_clipboard
                    && write_host_cmd(&mut pipe_tx, &HostCmd::SetClipboardText(text))
                        .await
                        .is_err()
                {
                    needs_respawn = true;
                }
            },
            _ = fast_tick.tick() => {
                let br = live_bitrate_kbps.load(Ordering::Acquire);
                if br != last_bitrate {
                    last_bitrate = br;
                    if write_host_cmd(&mut pipe_tx, &HostCmd::UpdateBitrate(br)).await.is_err() {
                        needs_respawn = true;
                    }
                }
                if keyframe_req.swap(false, Ordering::AcqRel)
                    && write_host_cmd(&mut pipe_tx, &HostCmd::RequestKeyframe).await.is_err()
                {
                    needs_respawn = true;
                }
                let aud = audio_enabled.load(Ordering::Acquire);
                if aud != last_audio {
                    last_audio = aud;
                    if write_host_cmd(&mut pipe_tx, &HostCmd::AudioEnable(aud)).await.is_err() {
                        needs_respawn = true;
                    }
                }
            },
            _ = sess_tick.tick() => {
                let live_sess = windows_session::active_console_session_id();
                if live_sess != 0 && live_sess != u32::MAX && live_sess != current_session {
                    tracing::info!(
                        old_session = current_session,
                        new_session = live_sess,
                        "active Windows console session changed; migrating worker"
                    );
                    needs_respawn = true;
                }
            }
        }
    }

    reader_task.abort();
    let _ = tokio::time::timeout(
        Duration::from_millis(120),
        write_host_cmd(&mut pipe_tx, &HostCmd::Shutdown),
    )
    .await;
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
