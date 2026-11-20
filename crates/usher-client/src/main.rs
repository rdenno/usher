mod app;
mod audio;
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
use clap::Parser;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use usher_proto::handshake::{CodecPreference, QualityPreference, VideoCodec};
use winit::event_loop::EventLoop;

use crate::app::{ConnectedSession, UsherApp};
use crate::audio::AudioPlayer;
use crate::config::{
    EffectiveConfig, FpsSetting, HostSettings, PointerMode, ResolutionMode, default_config_path,
    load_config,
};
use crate::decoder::{DecoderKind, probe_client_codecs};
use crate::gamepad::spawn_gamepad_thread;
use crate::overlay::OverlayState;
use crate::ssh::{HostConnection, SshConnectConfig};
use crate::transport::{AppEvent, start_transport};

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

    /// Host monitor index for multi-monitor setups
    #[arg(long)]
    display: Option<u8>,

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

    /// Encoder backend passed to usher-host (auto, vaapi, nvenc, amf, qsv, openh264)
    #[arg(long)]
    encoder: Option<String>,

    /// Headless Wayland compositor passed to usher-host (none, sway, gamescope, cage)
    #[arg(long)]
    headless: Option<String>,

    /// Outer UDP MTU budget in bytes (recommended <= 1200 for Tailscale/ZeroTier)
    #[arg(long)]
    mtu: Option<u16>,

    /// Initial XOR FEC parity redundancy percentage (0..=50)
    #[arg(long)]
    fec: Option<u8>,

    /// Disable audio playback
    #[arg(long)]
    no_audio: bool,

    /// Disable input forwarding (view-only mode)
    #[arg(long)]
    no_input: bool,

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

    /// Client video decoder backend
    #[arg(long, value_enum)]
    decoder: Option<DecoderKind>,
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
        audio: if args.no_audio { Some(false) } else { None },
        clipboard: if args.no_clipboard { Some(false) } else { None },
        display: args.display,
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
    }
}

fn build_host_args(
    eff: &EffectiveConfig,
    req_w: u16,
    req_h: u16,
    req_fps: u16,
    client_codecs: &[VideoCodec],
    is_local: bool,
) -> Vec<String> {
    let codec_list = client_codecs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
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
        eff.headless.clone(),
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
    out
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !args.doctor && !args.local && args.host.is_none() {
        bail!("usage: usher <ssh-host> (or pass --local / --doctor)");
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
    let eff = EffectiveConfig::resolve(
        &cfg_file,
        if args.local {
            Some("local")
        } else {
            args.host.as_deref()
        },
        &cli_overrides,
        args.bitrate_kbps,
    );

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
            &eff,
            &config_path,
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
    let host_arg = args.host.clone();
    let is_local = args.local;

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

        let mut host_args = build_host_args(&eff, req_w, req_h, req_fps, &client_codecs, is_local);
        if no_input {
            host_args.push("--no-input".into());
        }

        let ssh_cfg = SshConnectConfig {
            host: host_arg,
            local: is_local,
            remote_bin: eff.remote_bin.clone(),
            ssh_opts: eff.ssh_opts.clone(),
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
        let conn = rt_conn.block_on(HostConnection::connect(&ssh_cfg))?;
        let hello = conn.hello.clone();
        tracing::info!(
            bind_ip = %hello.bind_ip,
            port = hello.port,
            width = hello.width,
            height = hello.height,
            fps = hello.fps,
            codec = %hello.codec,
            bitrate_kbps = hello.bitrate_kbps,
            max_bitrate_kbps = hello.max_bitrate_kbps,
            fec_percent = hello.fec_percent,
            audio = hello.audio,
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

        let (clip_in_tx, clip_in_rx) = mpsc::channel::<String>(16);
        let (clip_out_tx, clip_out_rx) = mpsc::channel::<String>(16);
        if eff.clipboard {
            clipboard::spawn_clipboard_thread(clip_out_tx, clip_in_rx, cancel_conn.clone());
        } else {
            drop(clip_out_tx);
            drop(clip_in_rx);
        }

        let transport = rt_conn.block_on(start_transport(
            hello.clone(),
            eff.decoder,
            audio_tx,
            audio_muted.clone(),
            if eff.clipboard {
                Some(clip_in_tx)
            } else {
                None
            },
            clip_out_rx,
            proxy.clone(),
            cancel_conn.clone(),
        ))?;

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
            input_tx: transport.input_tx,
            control_tx: transport.control_tx,
            frame_slot: transport.decoded_frame_slot,
            telemetry: transport.telemetry,
            audio_muted,
            overlay,
            _audio_player: audio_player,
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
