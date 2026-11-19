mod app;
mod audio;
mod decoder;
mod gamepad;
mod keymap;
mod renderer;
mod ssh;
mod transport;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::net::IpAddr;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use usher_proto::handshake::VideoCodec;
use usher_proto::packet::DEFAULT_MTU;
use winit::event_loop::EventLoop;

use crate::app::UsherApp;
use crate::audio::AudioPlayer;
use crate::decoder::DecoderKind;
use crate::gamepad::spawn_gamepad_thread;
use crate::ssh::{HostConnection, SshConnectConfig};
use crate::transport::{AppEvent, start_transport};

#[derive(Debug, Parser)]
#[command(
    name = "usher",
    about = "Zero-daemon, SSH-bootstrapped low-latency remote desktop & game streamer"
)]
struct Args {
    /// Host alias from ~/.ssh/config or user@host (not needed with --local)
    host: Option<String>,

    /// Spawn a local usher-host over 127.0.0.1 for testing without SSH
    #[arg(long)]
    local: bool,

    /// Remote usher-host binary path/name on the target machine
    #[arg(long, default_value = "usher-host")]
    remote_bin: String,

    /// Extra OpenSSH -o Key=Value option (repeatable)
    #[arg(long = "ssh-opt")]
    ssh_opts: Vec<String>,

    /// Explicit overlay IP to ask usher-host to bind to (optional)
    #[arg(long)]
    bind_ip: Option<IpAddr>,

    /// Requested stream width (0 = host auto-detects DRM display or 1280)
    #[arg(long, default_value_t = 0)]
    width: u16,

    /// Requested stream height (0 = host auto-detects DRM display or 720)
    #[arg(long, default_value_t = 0)]
    height: u16,

    /// Target frame rate
    #[arg(long, default_value_t = 60)]
    fps: u16,

    /// Target video bitrate in kbps
    #[arg(long, default_value_t = 20_000)]
    bitrate_kbps: u32,

    /// Video codec (h264, hevc, av1)
    #[arg(long, default_value = "h264")]
    codec: VideoCodec,

    /// Capture source passed to usher-host (auto, drm, pipewire, x11, dxgi, testsrc)
    #[arg(long, default_value = "auto")]
    capture: String,

    /// Encoder backend passed to usher-host (auto, vaapi, nvenc, amf, qsv, openh264)
    #[arg(long, default_value = "auto")]
    encoder: String,

    /// Headless Wayland compositor passed to usher-host (none, sway, gamescope, cage)
    #[arg(long, default_value = "none")]
    headless: String,

    /// Outer UDP MTU budget in bytes (recommended <= 1200 for Tailscale/ZeroTier)
    #[arg(long, default_value_t = DEFAULT_MTU as u16)]
    mtu: u16,

    /// XOR FEC parity redundancy percentage (0..=50)
    #[arg(long, default_value_t = 10)]
    fec: u8,

    /// Disable audio playback
    #[arg(long)]
    no_audio: bool,

    /// Disable input forwarding (view-only mode)
    #[arg(long)]
    no_input: bool,

    /// Start in borderless fullscreen
    #[arg(long)]
    fullscreen: bool,

    /// Start with locked relative mouse mode enabled (toggle anytime with Ctrl+Alt+G)
    #[arg(long)]
    relative_mouse: bool,

    /// Client video decoder backend
    #[arg(long, value_enum, default_value_t = DecoderKind::Auto)]
    decoder: DecoderKind,
}

fn build_host_args(args: &Args) -> Vec<String> {
    let mut out = vec![
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
        args.capture.clone(),
        "--encoder".into(),
        args.encoder.clone(),
        "--headless".into(),
        args.headless.clone(),
        "--mtu".into(),
        args.mtu.to_string(),
        "--fec".into(),
        args.fec.to_string(),
    ];
    if let Some(ip) = args.bind_ip {
        out.push("--bind-ip".into());
        out.push(ip.to_string());
    } else if args.local {
        out.push("--bind-ip".into());
        out.push("127.0.0.1".into());
    }
    if args.no_audio {
        out.push("--no-audio".into());
    }
    if args.no_input {
        out.push("--no-input".into());
    }
    out
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !args.local && args.host.is_none() {
        bail!("usage: usher <ssh-host> (or pass --local for local testing)");
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

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating Tokio runtime")?;

    let ssh_cfg = SshConnectConfig {
        host: args.host.clone(),
        local: args.local,
        remote_bin: args.remote_bin.clone(),
        ssh_opts: args.ssh_opts.clone(),
        host_args: build_host_args(&args),
    };

    tracing::info!(target = %host_label, "bootstrapping usher-host via SSH...");
    let conn = rt.block_on(HostConnection::connect(&ssh_cfg))?;
    let hello = conn.hello.clone();
    tracing::info!(
        bind_ip = %hello.bind_ip,
        port = hello.port,
        width = hello.width,
        height = hello.height,
        fps = hello.fps,
        codec = %hello.codec,
        audio = hello.audio,
        "handshake received"
    );

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .context("creating winit EventLoop")?;
    let proxy = event_loop.create_proxy();
    let cancel = CancellationToken::new();

    let (audio_tx, _audio_player) = if !args.no_audio && hello.audio {
        let (tx, rx) = mpsc::channel(32);
        match AudioPlayer::start(rx, cancel.clone()) {
            Ok(player) => (Some(tx), Some(player)),
            Err(err) => {
                tracing::warn!("audio output disabled ({err:#})");
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    let transport = rt.block_on(start_transport(
        hello.clone(),
        args.decoder,
        audio_tx,
        proxy.clone(),
        cancel.clone(),
    ))?;

    // Watch SSH liveness and exit UI cleanly if remote exits
    {
        let cancel_ssh = cancel.clone();
        let proxy_ssh = proxy.clone();
        rt.spawn(async move {
            conn.watch_until_shutdown(cancel_ssh).await;
            let _ = proxy_ssh.send_event(AppEvent::Shutdown);
        });
    }

    if !args.no_input {
        spawn_gamepad_thread(transport.input_tx.clone(), cancel.clone());
    }

    let mut app = UsherApp::new(
        host_label,
        hello.width,
        hello.height,
        args.fullscreen,
        args.relative_mouse,
        !args.no_input,
        transport.input_tx,
        transport.control_tx,
        transport.decoded_frame_slot,
        transport.telemetry,
        cancel.clone(),
    );

    event_loop.run_app(&mut app)?;
    cancel.cancel();
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    Ok(())
}
