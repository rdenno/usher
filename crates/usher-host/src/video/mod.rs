pub mod drm_probe;
pub mod headless;
#[cfg(target_os = "linux")]
pub mod hw_pipeline;
pub mod software;

use clap::ValueEnum;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::handshake::VideoCodec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CaptureSource {
    Auto,
    Drm,
    Pipewire,
    X11,
    Testsrc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EncoderKind {
    Auto,
    Vaapi,
    Nvenc,
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
    pub render_node: Option<PathBuf>,
    pub wayland_runtime_dir: Option<PathBuf>,
    pub wayland_display: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedBackend {
    #[cfg(target_os = "linux")]
    Hw(hw_pipeline::HwPipelineKind),
    Software,
}

pub fn resolve_video_backend(cfg: &VideoPipelineConfig) -> ResolvedBackend {
    if cfg.capture == CaptureSource::Testsrc || cfg.encoder == EncoderKind::Openh264 {
        return ResolvedBackend::Software;
    }

    #[cfg(target_os = "linux")]
    if let Some(hw_kind) = select_linux_hw_pipeline(cfg) {
        return ResolvedBackend::Hw(hw_kind);
    }

    ResolvedBackend::Software
}

pub fn spawn_video_task(
    cfg: VideoPipelineConfig,
    backend: ResolvedBackend,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match backend {
            #[cfg(target_os = "linux")]
            ResolvedBackend::Hw(hw_kind) => {
                tracing::info!(?hw_kind, codec = %cfg.codec, "starting hardware zero-copy video pipeline");
                match hw_pipeline::run_hw_pipeline(
                    hw_kind,
                    cfg.clone(),
                    tx.clone(),
                    keyframe_req.clone(),
                    start,
                    cancel.clone(),
                )
                .await
                {
                    Ok(()) => return,
                    Err(err) => {
                        tracing::warn!(
                            "hardware video pipeline failed ({err:#}); falling back to OpenH264 testsrc"
                        );
                    }
                }
            }
            ResolvedBackend::Software => {
                tracing::info!("starting built-in OpenH264 video stream");
            }
        }

        if !cancel.is_cancelled()
            && let Err(err) =
                software::run_software_testsrc(cfg, tx, keyframe_req, start, cancel).await
        {
            tracing::error!("software encoder stopped: {err:#}");
        }
    })
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
        EncoderKind::Openh264 => return None,
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
        CaptureSource::Testsrc => None,
    }
}
