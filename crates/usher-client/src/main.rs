mod app;
mod audio;
mod cli;
mod clipboard;
mod config;
mod decoder;
mod doctor;
mod gamepad;
mod icon;
mod keymap;
#[cfg(target_os = "windows")]
mod mf_decoder;
mod overlay;
mod renderer;
mod ssh;
mod transport;
#[cfg(target_os = "macos")]
mod vt_decoder;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use usher_proto::handshake::{
    CodecPreference, QualityPreference, VideoCodec, encode_session_cmd_b64,
};
use winit::event_loop::EventLoop;

use crate::app::{ConnectedSession, UsherApp};
use crate::audio::{AudioPlayer, MicCapturer};
use crate::config::{
    ChromaPreference, DisplaySelector, EffectiveConfig, FpsSetting, HostSettings, PointerMode,
    ResolutionMode, TransportPreference, default_config_path, load_config,
};
use crate::decoder::{DecoderKind, client_supports_chroma_444, probe_client_codecs};
use crate::gamepad::spawn_gamepad_thread;
use crate::overlay::OverlayState;
use crate::ssh::{HostConnection, SshConnectConfig};
use crate::transport::{AppEvent, RemoteCursorState, TransportConfig, start_transport};

#[derive(Debug, Parser)]
#[command(
    name = "usher",
    version,
    about = "Zero-daemon, SSH-bootstrapped low-latency remote desktop & game streamer"
)]
struct Args {
    /// Host alias from ~/.ssh/config or user@host (not needed with --local)
    host: Option<String>,

    /// Spawn a local usher-host over 127.0.0.1 for testing without SSH
    #[arg(long)]
    local: bool,

    /// Path to config.toml (defaults to ~/.config/usher/config.toml or %APPDATA%\usher\config.toml)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Remote usher-host binary path/name on the target machine
    #[arg(long)]
    remote_bin: Option<String>,

    /// Extra OpenSSH -o Key=Value option (repeatable)
    #[arg(long = "ssh-opt")]
    ssh_opts: Vec<String>,

    /// Explicit overlay IP to ask usher-host to bind to (optional)
    #[arg(long)]
    bind_ip: Option<IpAddr>,

    /// Maximum adaptive bitrate ceiling in kbps (default: 60000)
    #[arg(long)]
    max_bitrate_kbps: Option<u32>,

    /// Optional explicit initial video bitrate in kbps
    #[arg(long)]
    bitrate_kbps: Option<u32>,

    /// Resolution mode (auto, native, match-window, or WxH e.g. 1920x1080)
    #[arg(long)]
    resolution: Option<ResolutionMode>,

    /// Explicit stream width (overrides resolution mode when paired with --height)
    #[arg(long)]
    width: Option<u16>,

    /// Explicit stream height (overrides resolution mode when paired with --width)
    #[arg(long)]
    height: Option<u16>,

    /// Frame rate setting (auto or 1..=240)
    #[arg(long)]
    fps: Option<FpsSetting>,

    /// Video codec preference (auto, h264, hevc, av1)
    #[arg(long)]
    codec: Option<CodecPreference>,

    /// Stream tuning preference (latency, quality)
    #[arg(long)]
    prefer: Option<QualityPreference>,

    /// Chroma subsampling preference (auto, 420, 444)
    #[arg(long)]
    chroma: Option<ChromaPreference>,

    /// Host monitor index or name (from --list-displays)
    #[arg(long)]
    display: Option<DisplaySelector>,

    /// Query available host displays and exit
    #[arg(long)]
    list_displays: bool,

    /// Default mouse pointer mode (absolute, relative)
    #[arg(long)]
    pointer_mode: Option<PointerMode>,

    /// Show compact live stats HUD overlay
    #[arg(long)]
    stats_overlay: bool,

    /// Hide compact live stats HUD overlay
    #[arg(long, conflicts_with = "stats_overlay")]
    no_stats_overlay: bool,

    /// Capture source passed to usher-host (auto, drm, pipewire, x11, dxgi, testsrc)
    #[arg(long)]
    capture: Option<String>,

    /// Encoder backend passed to usher-host (auto, vaapi, nvenc, amf, qsv, openh264, ffmpeg)
    #[arg(long)]
    encoder: Option<String>,

    /// Headless Wayland compositor passed to usher-host (none, auto, sway, gamescope, cage)
    #[arg(long)]
    headless: Option<String>,

    /// Enable OpenSSH BatchMode=yes (disabled by default so hardware keys & prompts work)
    #[arg(long)]
    batch_mode: bool,

    /// Network transport mode (auto = UDP with SSH TCP tunnel fallback, udp, tcp)
    #[arg(long, value_enum)]
    transport: Option<TransportPreference>,

    /// Automatically install usher-host over SSH if missing on the remote target
    #[arg(long)]
    auto_install: bool,

    /// Never install usher-host automatically if missing on the remote target
    #[arg(long, conflicts_with = "auto_install")]
    no_auto_install: bool,

    /// Outer UDP MTU budget in bytes (recommended <= 1200 for Tailscale/ZeroTier)
    #[arg(long)]
    mtu: Option<u16>,

    /// Initial XOR FEC parity redundancy percentage (0..=50)
    #[arg(long)]
    fec: Option<u8>,

    /// Disable audio playback
    #[arg(long)]
    no_audio: bool,

    /// Forward local microphone audio to host virtual mic (off by default)
    #[arg(long)]
    mic: bool,

    /// Disable input forwarding (view-only mode)
    #[arg(long, visible_alias = "view-only")]
    no_input: bool,

    /// Disable client-side separate cursor plane (bake cursor into host video)
    #[arg(long)]
    no_cursor_plane: bool,

    /// Disable bidirectional clipboard synchronization
    #[arg(long)]
    no_clipboard: bool,

    /// Run end-to-end local and/or SSH + mesh P2P diagnostics and exit
    #[arg(long)]
    doctor: bool,

    /// Start in borderless fullscreen (default)
    #[arg(long)]
    fullscreen: bool,

    /// Start in a normal desktop window instead of fullscreen
    #[arg(long, conflicts_with = "fullscreen")]
    windowed: bool,

    /// Start with locked relative mouse mode enabled (toggle anytime with Ctrl+Alt+G)
    #[arg(long)]
    relative_mouse: bool,

    /// Disable automatic adaptive bitrate/FEC on the host
    #[arg(long)]
    no_adaptive: bool,

    /// Windows host only: run console worker as InteractiveToken only (never NT AUTHORITY\SYSTEM)
    #[arg(long)]
    no_system: bool,

    /// Client video decoder backend
    #[arg(long, value_enum)]
    decoder: Option<DecoderKind>,

    /// Headless smoke test: decode N video frames without opening a window, then exit 0
    #[arg(long, value_name = "N")]
    smoke_frames: Option<usize>,
}

fn build_cli_overrides(args: &Args) -> HostSettings {
    let resolution = if args.width.is_some() || args.height.is_some() {
        Some(ResolutionMode::Fixed(
            args.width.unwrap_or(1280),
            args.height.unwrap_or(720),
        ))
    } else {
        args.resolution
    };
    let pointer_mode = if args.relative_mouse {
        Some(PointerMode::Relative)
    } else {
        args.pointer_mode
    };
    let stats_overlay = if args.stats_overlay {
        Some(true)
    } else if args.no_stats_overlay {
        Some(false)
    } else {
        None
    };

    HostSettings {
        max_bitrate_kbps: args.max_bitrate_kbps,
        resolution,
        fps: args.fps,
        codec: args.codec,
        prefer: args.prefer,
        chroma: args.chroma,
        audio: if args.no_audio { Some(false) } else { None },
        mic: if args.mic { Some(true) } else { None },
        clipboard: if args.no_clipboard { Some(false) } else { None },
        display: args.display.clone(),
        pointer_mode,
        stats_overlay,
        fullscreen: if args.fullscreen {
            Some(true)
        } else if args.windowed {
            Some(false)
        } else {
            None
        },
        capture: args.capture.clone(),
        encoder: args.encoder.clone(),
        decoder: args.decoder,
        headless: args.headless.clone(),
        mtu: args.mtu,
        fec: args.fec,
        remote_bin: args.remote_bin.clone(),
        bind_ip: args.bind_ip,
        ssh_opts: if args.ssh_opts.is_empty() {
            None
        } else {
            Some(args.ssh_opts.clone())
        },
        no_adaptive: if args.no_adaptive { Some(true) } else { None },
        no_system: if args.no_system { Some(true) } else { None },
        batch_mode: if args.batch_mode { Some(true) } else { None },
        transport: args.transport,
        auto_install: if args.auto_install {
            Some(true)
        } else if args.no_auto_install {
            Some(false)
        } else {
            None
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn build_host_args(
    eff: &EffectiveConfig,
    req_w: u16,
    req_h: u16,
    req_fps: u16,
    client_codecs: &[VideoCodec],
    is_local: bool,
    no_cursor_plane: bool,
    remote_cmd: &[String],
) -> Vec<String> {
    let codec_list = client_codecs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let effective_headless = if !remote_cmd.is_empty() && eff.headless == "none" {
        "auto".to_string()
    } else {
        eff.headless.clone()
    };
    let mut out = vec![
        "--width".into(),
        req_w.to_string(),
        "--height".into(),
        req_h.to_string(),
        "--fps".into(),
        req_fps.to_string(),
        "--max-bitrate-kbps".into(),
        eff.max_bitrate_kbps.to_string(),
        "--codec".into(),
        eff.codec.to_string(),
        "--client-codecs".into(),
        codec_list,
        "--prefer".into(),
        eff.prefer.to_string(),
        "--display".into(),
        eff.display.to_string(),
        "--capture".into(),
        eff.capture.clone(),
        "--encoder".into(),
        eff.encoder.clone(),
        "--headless".into(),
        effective_headless,
        "--mtu".into(),
        eff.mtu.to_string(),
        "--fec".into(),
        eff.fec.to_string(),
    ];
    if let Some(br) = eff.explicit_bitrate_kbps {
        out.push("--bitrate-kbps".into());
        out.push(br.to_string());
    }
    if eff.no_adaptive {
        out.push("--no-adaptive".into());
    }
    if eff.no_system {
        out.push("--no-system".into());
    }
    if let Some(ip) = eff.bind_ip {
        out.push("--bind-ip".into());
        out.push(ip.to_string());
    } else if is_local {
        out.push("--bind-ip".into());
        out.push("127.0.0.1".into());
    }
    if !eff.audio {
        out.push("--no-audio".into());
    }
    if !eff.clipboard {
        out.push("--no-clipboard".into());
    }
    let client_444 = client_supports_chroma_444();
    match eff.chroma {
        ChromaPreference::Auto => {
            out.push("--chroma".into());
            out.push("444".into());
            if client_444 {
                out.push("--client-chroma-444".into());
            }
        }
        ChromaPreference::Yuv420 => {
            out.push("--chroma".into());
            out.push("420".into());
        }
        ChromaPreference::Yuv444 => {
            if !client_444 {
                tracing::warn!(
                    "4:4:4 chroma requested, but ffmpeg is unavailable on client; falling back to 4:2:0"
                );
            }
            out.push("--chroma".into());
            out.push("444".into());
            if client_444 {
                out.push("--client-chroma-444".into());
            }
        }
    }
    if eff.mic {
        out.push("--mic".into());
    }
    if no_cursor_plane {
        out.push("--no-cursor-plane".into());
    }
    if !remote_cmd.is_empty() {
        out.push("--session-cmd-b64".into());
        out.push(encode_session_cmd_b64(remote_cmd));
    }
    out
}

fn main() -> Result<()> {
    let split = cli::split_cli_args(std::env::args().collect(), &Args::command())?;
    let args = Args::parse_from(&split.usher_args);
    if !args.doctor && !args.local && args.host.is_none() {
        bail!("usage: usher [ssh args...] <ssh-host> [-- command...] (or pass --local / --doctor)");
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let host_label = if args.local {
        "local".to_string()
    } else {
        args.host.clone().unwrap_or_default()
    };

    let config_path = args.config.clone().unwrap_or_else(default_config_path);
    let cfg_file = load_config(&config_path)?;
    let cli_overrides = build_cli_overrides(&args);
    let mut eff = EffectiveConfig::resolve(
        &cfg_file,
        if args.local {
            Some("local")
        } else {
            args.host.as_deref()
        },
        &cli_overrides,
        args.bitrate_kbps,
    );
    if !split.remote_cmd.is_empty() && args.headless.is_none() && eff.headless == "none" {
        eff.headless = "auto".to_string();
    }

    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("creating Tokio runtime")?,
    );

    if args.doctor {
        return rt.block_on(doctor::run_doctor(
            args.host.as_deref(),
            args.local,
            &split.ssh_args,
            &eff,
            &config_path,
        ));
    }

    if args.list_displays {
        let mut host_args = Vec::new();
        if eff.no_system {
            host_args.push("--no-system".into());
        }
        let ssh_cfg = SshConnectConfig {
            host: args.host.clone(),
            local: args.local,
            remote_bin: eff.remote_bin.clone(),
            ssh_args: split.ssh_args.clone(),
            ssh_opts: eff.ssh_opts.clone(),
            batch_mode: eff.batch_mode,
            auto_install: eff.auto_install,
            host_args,
        };
        let displays = rt.block_on(crate::ssh::query_remote_displays(&ssh_cfg))?;
        if displays.is_empty() {
            println!("No displays reported by {host_label}.");
        } else {
            for d in &displays {
                println!("{}: {} ({}x{})", d.index, d.name, d.width, d.height);
            }
        }
        return Ok(());
    }

    if let Some(target_frames) = args.smoke_frames {
        if target_frames == 0 {
            bail!("--smoke-frames must be >= 1");
        }
        let (req_w, req_h) = match eff.resolution {
            ResolutionMode::Auto | ResolutionMode::MatchWindow => {
                if eff.capture == "testsrc" {
                    (640, 360)
                } else {
                    (1280, 720)
                }
            }
            ResolutionMode::Native => (0, 0),
            ResolutionMode::Fixed(w, h) => ((w.clamp(64, 3840)) & !1, (h.clamp(64, 2160)) & !1),
        };
        let req_fps = match eff.fps {
            FpsSetting::Auto => 30,
            FpsSetting::Fixed(f) => f.clamp(1, 240),
        };
        let client_codecs = probe_client_codecs(eff.decoder);
        let mut host_args = build_host_args(
            &eff,
            req_w,
            req_h,
            req_fps,
            &client_codecs,
            args.local,
            args.no_cursor_plane,
            &split.remote_cmd,
        );
        if args.no_input {
            host_args.push("--no-input".into());
        }
        let ssh_cfg = SshConnectConfig {
            host: args.host.clone(),
            local: args.local,
            remote_bin: eff.remote_bin.clone(),
            ssh_args: split.ssh_args.clone(),
            ssh_opts: eff.ssh_opts.clone(),
            batch_mode: eff.batch_mode,
            auto_install: eff.auto_install,
            host_args,
        };
        return rt.block_on(run_headless_smoke(
            &ssh_cfg,
            eff.transport,
            eff.decoder,
            target_frames,
        ));
    }

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .context("creating winit EventLoop")?;
    let proxy = event_loop.create_proxy();
    let cancel = CancellationToken::new();

    let rt_conn = rt.clone();
    let cancel_conn = cancel.clone();
    let no_input = args.no_input;
    let no_cursor_plane = args.no_cursor_plane;
    let host_arg = args.host.clone();
    let is_local = args.local;
    let ssh_args = split.ssh_args;
    let remote_cmd = split.remote_cmd;

    let connect_fn = Box::new(move |mon_info: Option<(u16, u16, u16)>| -> Result<ConnectedSession> {
        let (mon_w, mon_h, mon_fps) = mon_info.unwrap_or((1920, 1080, 60));

        let (req_w, req_h) = match eff.resolution {
            ResolutionMode::Auto | ResolutionMode::MatchWindow => {
                if eff.capture == "testsrc" {
                    (1280, 720)
                } else {
                    (mon_w, mon_h)
                }
            }
            ResolutionMode::Native => (0, 0),
            ResolutionMode::Fixed(w, h) => ((w.clamp(64, 3840)) & !1, (h.clamp(64, 2160)) & !1),
        };
        let req_fps = match eff.fps {
            FpsSetting::Auto => mon_fps,
            FpsSetting::Fixed(f) => f.clamp(1, 240),
        };
        let client_codecs = probe_client_codecs(eff.decoder);

        let mut host_args = build_host_args(
            &eff,
            req_w,
            req_h,
            req_fps,
            &client_codecs,
            is_local,
            no_cursor_plane,
            &remote_cmd,
        );
        if no_input {
            host_args.push("--no-input".into());
        }

        let drop_host = host_arg.clone();
        let drop_ssh_args = ssh_args.clone();
        let drop_ssh_opts = eff.ssh_opts.clone();
        let drop_batch_mode = eff.batch_mode;

        let ssh_cfg = SshConnectConfig {
            host: host_arg,
            local: is_local,
            remote_bin: eff.remote_bin.clone(),
            ssh_args,
            ssh_opts: eff.ssh_opts.clone(),
            batch_mode: eff.batch_mode,
            auto_install: eff.auto_install,
            host_args,
        };

        tracing::info!(
            target = %host_label,
            max_bitrate_kbps = eff.max_bitrate_kbps,
            codec = %eff.codec,
            prefer = %eff.prefer,
            width = req_w,
            height = req_h,
            fps = req_fps,
            "bootstrapping usher-host via SSH..."
        );
        let mut conn = rt_conn.block_on(HostConnection::connect(&ssh_cfg))?;
        let hello = conn.hello.clone();
        tracing::info!(
            bind_ip = %hello.bind_ip,
            port = hello.port,
            width = hello.width,
            height = hello.height,
            fps = hello.fps,
            codec = %hello.codec,
            chroma = %hello.chroma,
            cursor_plane = hello.cursor_plane,
            bitrate_kbps = hello.bitrate_kbps,
            max_bitrate_kbps = hello.max_bitrate_kbps,
            fec_percent = hello.fec_percent,
            audio = hello.audio,
            mic = hello.mic_enabled,
            "handshake received"
        );

        let audio_muted = Arc::new(AtomicBool::new(!eff.audio || !hello.audio));
        let (audio_tx, audio_player) = if hello.audio {
            let (tx, rx) = mpsc::channel(32);
            match AudioPlayer::start(rx, audio_muted.clone(), cancel_conn.clone()) {
                Ok(player) => (Some(tx), Some(player)),
                Err(err) => {
                    tracing::warn!("audio output disabled ({err:#})");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let (mic_rx, mic_capturer) = if !no_input && eff.mic && hello.mic_enabled {
            let (tx, rx) = mpsc::channel(32);
            let mic_muted = Arc::new(AtomicBool::new(false));
            match MicCapturer::start(tx, mic_muted, std::time::Instant::now(), cancel_conn.clone()) {
                Ok(cap) => (Some(rx), Some(cap)),
                Err(err) => {
                    tracing::warn!("microphone passthrough disabled ({err:#})");
                    (None, None)
                }
            }
        } else {
            if !no_input && eff.mic && !hello.mic_enabled {
                tracing::warn!("--mic requested, but host disabled virtual microphone sink");
            }
            (None, None)
        };

        let (clip_in_tx, clip_in_rx) = mpsc::channel::<String>(16);
        let (clip_out_tx, clip_out_rx) = mpsc::channel::<String>(16);
        if eff.clipboard {
            clipboard::spawn_clipboard_thread(clip_out_tx, clip_in_rx, cancel_conn.clone());
        } else {
            drop(clip_out_tx);
            drop(clip_in_rx);
        }

        let separate_cursor = hello.cursor_plane && !no_cursor_plane;
        let cursor_state = Arc::new(std::sync::Mutex::new(RemoteCursorState::default()));

        let transport = rt_conn.block_on(start_transport(
            &mut conn,
            TransportConfig {
                transport_pref: eff.transport,
                decoder_kind: eff.decoder,
                chroma: hello.chroma,
                audio_tx,
                audio_muted: audio_muted.clone(),
                mic_rx,
                clipboard_in_tx: if eff.clipboard {
                    Some(clip_in_tx)
                } else {
                    None
                },
                clipboard_out_rx: clip_out_rx,
                cursor_state: if separate_cursor {
                    Some(cursor_state.clone())
                } else {
                    None
                },
                proxy: Some(proxy.clone()),
                cancel: cancel_conn.clone(),
            },
        ))?;

        let drop_tx = if !no_input {
            let (tx, mut rx) = mpsc::channel::<PathBuf>(16);
            let proxy_drop = proxy.clone();
            let cancel_drop = cancel_conn.clone();
            rt_conn.spawn(async move {
                while let Some(path) = tokio::select! {
                    _ = cancel_drop.cancelled() => None,
                    p = rx.recv() => p,
                } {
                    let msg = if is_local {
                        let p = path.clone();
                        match tokio::task::spawn_blocking(move || copy_dropped_to_local_downloads(&p)).await {
                            Ok(Ok(dest)) => format!("Copied to {dest}"),
                            Ok(Err(err)) => format!("Copy failed: {err:#}"),
                            Err(err) => format!("Copy task failed: {err}"),
                        }
                    } else if let Some(ref host) = drop_host {
                        match crate::ssh::transfer_file_via_scp(
                            host,
                            &drop_ssh_args,
                            &drop_ssh_opts,
                            drop_batch_mode,
                            &path,
                        )
                        .await
                        {
                            Ok(dest) => format!("Sent to {dest}"),
                            Err(err) => format!("SCP failed: {err:#}"),
                        }
                    } else {
                        "Upload skipped: missing SSH target".to_string()
                    };
                    let _ = proxy_drop.send_event(AppEvent::Toast(msg));
                }
            });
            Some(tx)
        } else {
            None
        };

        // Watch SSH liveness and exit UI cleanly if remote exits
        {
            let cancel_ssh = cancel_conn.clone();
            let proxy_ssh = proxy.clone();
            rt_conn.spawn(async move {
                conn.watch_until_shutdown(cancel_ssh).await;
                let _ = proxy_ssh.send_event(AppEvent::Shutdown);
            });
        }

        if !no_input {
            spawn_gamepad_thread(transport.input_tx.clone(), cancel_conn.clone());
        }

        let overlay = OverlayState::new(
            host_label.clone(),
            config_path,
            hello.displays.clone(),
            (mon_w, mon_h),
            mon_fps,
            (hello.width, hello.height),
            eff.max_bitrate_kbps,
            eff.resolution,
            eff.fps,
            eff.codec,
            eff.prefer,
            eff.audio && hello.audio,
            hello.display_index,
            eff.pointer_mode,
            eff.stats_overlay,
        );

        Ok(ConnectedSession {
            host_label,
            video_w: hello.width,
            video_h: hello.height,
            initial_fullscreen: eff.fullscreen,
            relative_mouse: eff.pointer_mode == PointerMode::Relative,
            input_enabled: !no_input,
            separate_cursor,
            hi_res_scroll: hello.hi_res_scroll,
            input_tx: transport.input_tx,
            control_tx: transport.control_tx,
            frame_slot: transport.decoded_frame_slot,
            cursor_state,
            drop_tx,
            telemetry: transport.telemetry,
            audio_muted,
            overlay,
            _audio_player: audio_player,
            _mic_capturer: mic_capturer,
        })
    });

    let mut app = UsherApp::new(connect_fn, cancel.clone());
    event_loop.run_app(&mut app)?;
    let startup_err = app.startup_error.take();
    drop(app);
    cancel.cancel();
    if let Ok(rt) = Arc::try_unwrap(rt) {
        rt.shutdown_timeout(std::time::Duration::from_secs(2));
    }
    if let Some(err) = startup_err {
        return Err(err);
    }
    Ok(())
}

async fn run_headless_smoke(
    ssh_cfg: &SshConnectConfig,
    transport_pref: TransportPreference,
    decoder_kind: DecoderKind,
    target_frames: usize,
) -> Result<()> {
    let cancel = CancellationToken::new();
    let mut conn = HostConnection::connect(ssh_cfg).await?;
    let hello = conn.hello.clone();

    let (_clip_out_tx, clip_out_rx) = mpsc::channel::<String>(1);
    let transport = start_transport(
        &mut conn,
        TransportConfig {
            transport_pref,
            decoder_kind,
            chroma: hello.chroma,
            audio_tx: None,
            audio_muted: Arc::new(AtomicBool::new(true)),
            mic_rx: None,
            clipboard_in_tx: None,
            clipboard_out_rx: clip_out_rx,
            cursor_state: None,
            proxy: None,
            cancel: cancel.clone(),
        },
    )
    .await?;

    let ssh_watch = tokio::spawn({
        let cancel_ssh = cancel.clone();
        async move {
            conn.watch_until_shutdown(cancel_ssh).await;
        }
    });

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut verified_frame = false;
    let mut last_dims = (hello.width, hello.height);
    let result = loop {
        if let Some(frame) = transport.decoded_frame_slot.try_take() {
            anyhow::ensure!(
                frame.width > 0
                    && frame.height > 0
                    && !frame.y.is_empty()
                    && !frame.u.is_empty()
                    && !frame.v.is_empty(),
                "decoded YUV frame has invalid dimensions {}x{}",
                frame.width,
                frame.height
            );
            last_dims = (frame.width as u16, frame.height as u16);
            verified_frame = true;
        }
        let count = transport
            .telemetry
            .decoded_frames
            .load(std::sync::atomic::Ordering::Acquire);
        if verified_frame && count >= target_frames as u64 {
            println!(
                "[OK] Decoded {count} video frames ({}x{}, codec={})",
                last_dims.0, last_dims.1, hello.codec
            );
            break Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            break Err(anyhow::anyhow!(
                "timed out after 15s waiting for {target_frames} decoded frames (got {count}, verified_yuv={verified_frame})"
            ));
        }
        if cancel.is_cancelled() {
            break Err(anyhow::anyhow!(
                "session closed before {target_frames} frames decoded (got {count})"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };

    drop(_clip_out_tx);
    let _ = transport
        .control_tx
        .try_send(usher_proto::packet::ControlMessage::Disconnect);
    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), ssh_watch).await;
    result
}

fn copy_dropped_to_local_downloads(src: &std::path::Path) -> Result<String> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let downloads = home.join("Downloads");
    std::fs::create_dir_all(&downloads)?;
    let name = src
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("dropped path has no filename"))?;
    let dest = downloads.join(name);
    if src == dest {
        return Ok(format!("Downloads/{}", name.to_string_lossy()));
    }
    if src.is_dir() {
        copy_dir_recursive(src, &dest)?;
    } else {
        std::fs::copy(src, &dest)?;
    }
    Ok(format!("Downloads/{}", name.to_string_lossy()))
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
