pub mod drm_probe;
pub mod headless;
#[cfg(target_os = "linux")]
pub mod hw_pipeline;
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod ivf;
pub mod software;
#[cfg(target_os = "windows")]
pub mod windows_capture;

use clap::ValueEnum;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use usher_proto::handshake::{CodecPreference, QualityPreference, VideoCodec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CaptureSource {
    Auto,
    Drm,
    Pipewire,
    X11,
    Dxgi,
    Testsrc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EncoderKind {
    Auto,
    Vaapi,
    Nvenc,
    Amf,
    Qsv,
    Openh264,
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_us: u32,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct VideoPipelineConfig {
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_kbps: u32,
    pub codec: VideoCodec,
    pub capture: CaptureSource,
    pub encoder: EncoderKind,
    pub prefer: QualityPreference,
    pub display_index: u8,
    pub render_node: Option<PathBuf>,
    pub wayland_runtime_dir: Option<PathBuf>,
    pub wayland_display: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedBackend {
    #[cfg(target_os = "linux")]
    Hw(hw_pipeline::HwPipelineKind),
    #[cfg(target_os = "windows")]
    Win(windows_capture::WinVideoBackend),
    Software,
}

#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
pub fn probe_host_codecs(
    capture: CaptureSource,
    encoder: EncoderKind,
    probe: &drm_probe::DrmProbe,
) -> Vec<VideoCodec> {
    if capture == CaptureSource::Testsrc || encoder == EncoderKind::Openh264 {
        return vec![VideoCodec::H264];
    }
    #[cfg(target_os = "windows")]
    {
        windows_capture::probe_supported_codecs(capture, encoder)
    }
    #[cfg(target_os = "linux")]
    {
        hw_pipeline::probe_linux_supported_codecs(probe)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = probe;
        vec![VideoCodec::H264]
    }
}

pub fn negotiate_codec(
    pref: CodecPreference,
    client_codecs: &[VideoCodec],
    host_codecs: &[VideoCodec],
) -> VideoCodec {
    match pref {
        CodecPreference::H264 => VideoCodec::H264,
        CodecPreference::Hevc => VideoCodec::Hevc,
        CodecPreference::Av1 => VideoCodec::Av1,
        CodecPreference::Auto => {
            for candidate in [VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::H264] {
                if host_codecs.contains(&candidate) && client_codecs.contains(&candidate) {
                    return candidate;
                }
            }
            VideoCodec::H264
        }
    }
}

pub fn is_in_place_bitrate_backend(backend: &ResolvedBackend) -> bool {
    match backend {
        ResolvedBackend::Software => true,
        #[cfg(target_os = "linux")]
        ResolvedBackend::Hw(_) => false,
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(windows_capture::WinVideoBackend::NativeDxgiOpenH264) => true,
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(windows_capture::WinVideoBackend::DdagrabHw { .. }) => false,
    }
}

pub fn resolve_effective_codec(cfg: &VideoPipelineConfig) -> VideoCodec {
    match resolve_video_backend(cfg) {
        ResolvedBackend::Software => VideoCodec::H264,
        #[cfg(target_os = "linux")]
        ResolvedBackend::Hw(_) => cfg.codec,
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(windows_capture::WinVideoBackend::NativeDxgiOpenH264) => {
            VideoCodec::H264
        }
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(windows_capture::WinVideoBackend::DdagrabHw { .. }) => cfg.codec,
    }
}

pub fn resolve_video_backend(cfg: &VideoPipelineConfig) -> ResolvedBackend {
    if cfg.capture == CaptureSource::Testsrc {
        return ResolvedBackend::Software;
    }

    #[cfg(target_os = "linux")]
    if cfg.encoder != EncoderKind::Openh264
        && let Some(hw_kind) = select_linux_hw_pipeline(cfg)
    {
        return ResolvedBackend::Hw(hw_kind);
    }

    #[cfg(target_os = "windows")]
    if let Some(win_backend) = windows_capture::select_windows_backend(cfg) {
        return ResolvedBackend::Win(win_backend);
    }

    ResolvedBackend::Software
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_video_task(
    mut config_rx: watch::Receiver<VideoPipelineConfig>,
    live_bitrate_kbps: Arc<AtomicU32>,
    in_place_bitrate: Arc<AtomicBool>,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    peer_ready: Arc<tokio::sync::Notify>,
    start: Instant,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Wait until the first authenticated client UDP packet arrives before starting
        // capture/encode so Frame 0 (VPS/SPS/PPS + IDR) is delivered cleanly.
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = peer_ready.notified() => {}
        }

        loop {
            if cancel.is_cancelled() {
                break;
            }
            let cfg = config_rx.borrow_and_update().clone();
            live_bitrate_kbps.store(cfg.bitrate_kbps, Ordering::Release);
            let backend = resolve_video_backend(&cfg);
            in_place_bitrate.store(is_in_place_bitrate_backend(&backend), Ordering::Release);
            let child_cancel = cancel.child_token();

            let runner = run_one_backend(
                cfg,
                backend,
                live_bitrate_kbps.clone(),
                in_place_bitrate.clone(),
                tx.clone(),
                keyframe_req.clone(),
                start,
                child_cancel.clone(),
            );
            tokio::pin!(runner);

            let mut restart_for_change = false;
            tokio::select! {
                _ = &mut runner => {}
                res = config_rx.changed() => {
                    if res.is_ok() {
                        restart_for_change = true;
                        keyframe_req.store(true, Ordering::Release);
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
}

#[allow(clippy::too_many_arguments)]
async fn run_one_backend(
    cfg: VideoPipelineConfig,
    backend: ResolvedBackend,
    live_bitrate_kbps: Arc<AtomicU32>,
    in_place_bitrate: Arc<AtomicBool>,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    child_cancel: CancellationToken,
) {
    match backend {
        #[cfg(target_os = "linux")]
        ResolvedBackend::Hw(hw_kind) => {
            tracing::info!(
                ?hw_kind,
                codec = %cfg.codec,
                bitrate_kbps = cfg.bitrate_kbps,
                "starting hardware zero-copy video pipeline"
            );
            match hw_pipeline::run_hw_pipeline(
                hw_kind,
                cfg.clone(),
                tx.clone(),
                keyframe_req.clone(),
                start,
                child_cancel.clone(),
            )
            .await
            {
                Ok(()) => return,
                Err(err) => {
                    if child_cancel.is_cancelled() {
                        return;
                    }
                    tracing::warn!(
                        "hardware video pipeline failed ({err:#}); falling back to OpenH264 testsrc"
                    );
                }
            }
        }
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(win_backend) => {
            let was_hw = matches!(win_backend, windows_capture::WinVideoBackend::DdagrabHw { .. });
            match windows_capture::run_windows_backend(
                win_backend,
                cfg.clone(),
                live_bitrate_kbps.clone(),
                tx.clone(),
                keyframe_req.clone(),
                start,
                child_cancel.clone(),
            )
            .await
            {
                Ok(()) => return,
                Err(err) => {
                    if child_cancel.is_cancelled() {
                        return;
                    }
                    tracing::warn!("Windows capture pipeline failed ({err:#})");
                    if was_hw && cfg.codec == VideoCodec::H264 {
                        tracing::info!("falling back to built-in DXGI + OpenH264 capture");
                        in_place_bitrate.store(true, Ordering::Release);
                        if windows_capture::run_windows_backend(
                            windows_capture::WinVideoBackend::NativeDxgiOpenH264,
                            cfg.clone(),
                            live_bitrate_kbps.clone(),
                            tx.clone(),
                            keyframe_req.clone(),
                            start,
                            child_cancel.clone(),
                        )
                        .await
                        .is_ok()
                        {
                            return;
                        }
                    }
                }
            }
        }
        ResolvedBackend::Software => {
            tracing::info!("starting built-in OpenH264 video stream");
        }
    }

    if !child_cancel.is_cancelled() {
        in_place_bitrate.store(true, Ordering::Release);
        if let Err(err) = software::run_software_testsrc(
            cfg,
            live_bitrate_kbps,
            tx,
            keyframe_req,
            start,
            child_cancel,
        )
        .await
        {
            tracing::error!("software encoder stopped: {err:#}");
        }
    }
}

#[cfg(target_os = "linux")]
fn select_linux_hw_pipeline(cfg: &VideoPipelineConfig) -> Option<hw_pipeline::HwPipelineKind> {
    use drm_probe::GpuVendor;
    use hw_pipeline::{HwPipelineKind, command_exists, ffmpeg_supports_encoder};

    let probe = drm_probe::probe_drm();
    let has_ffmpeg = command_exists("ffmpeg");
    let has_gst = command_exists("gst-launch-1.0");
    let has_wayland = cfg.wayland_display.is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    let has_x11 = std::env::var_os("DISPLAY").is_some();

    let vaapi_enc = match cfg.codec {
        VideoCodec::H264 => "h264_vaapi",
        VideoCodec::Hevc => "hevc_vaapi",
        VideoCodec::Av1 => "av1_vaapi",
    };
    let nvenc_enc = match cfg.codec {
        VideoCodec::H264 => "h264_nvenc",
        VideoCodec::Hevc => "hevc_nvenc",
        VideoCodec::Av1 => "av1_nvenc",
    };

    let prefer_nvenc = match cfg.encoder {
        EncoderKind::Nvenc => true,
        EncoderKind::Vaapi => false,
        EncoderKind::Auto => matches!(probe.vendor, Some(GpuVendor::Nvidia)),
        EncoderKind::Amf | EncoderKind::Qsv | EncoderKind::Openh264 => return None,
    };

    match cfg.capture {
        CaptureSource::Drm => {
            if has_ffmpeg && probe.render_node.is_some() && ffmpeg_supports_encoder(vaapi_enc) {
                return Some(HwPipelineKind::DrmVaapi);
            }
            None
        }
        CaptureSource::Pipewire => {
            if has_gst && has_wayland {
                return Some(if prefer_nvenc {
                    HwPipelineKind::PipeWireNvenc
                } else {
                    HwPipelineKind::PipeWireVaapi
                });
            }
            None
        }
        CaptureSource::X11 => {
            if has_ffmpeg && has_x11 {
                if prefer_nvenc && ffmpeg_supports_encoder(nvenc_enc) {
                    return Some(HwPipelineKind::X11Nvenc);
                }
                if ffmpeg_supports_encoder(vaapi_enc) {
                    return Some(HwPipelineKind::X11Vaapi);
                }
            }
            None
        }
        CaptureSource::Auto => {
            if has_wayland && has_gst {
                return Some(if prefer_nvenc {
                    HwPipelineKind::PipeWireNvenc
                } else {
                    HwPipelineKind::PipeWireVaapi
                });
            }
            if has_ffmpeg && !prefer_nvenc && probe.render_node.is_some() && ffmpeg_supports_encoder(vaapi_enc) {
                return Some(HwPipelineKind::DrmVaapi);
            }
            if has_ffmpeg && has_x11 {
                if prefer_nvenc && ffmpeg_supports_encoder(nvenc_enc) {
                    return Some(HwPipelineKind::X11Nvenc);
                }
                if ffmpeg_supports_encoder(vaapi_enc) {
                    return Some(HwPipelineKind::X11Vaapi);
                }
            }
            None
        }
        CaptureSource::Dxgi | CaptureSource::Testsrc => None,
    }
}
