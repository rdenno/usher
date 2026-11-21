pub mod color;
pub mod drm_probe;
pub mod headless;
#[cfg(target_os = "linux")]
pub mod hw_pipeline;
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod ivf;
#[cfg(target_os = "linux")]
pub mod linux_kms;
#[cfg(target_os = "linux")]
pub mod linux_native;
#[cfg(target_os = "linux")]
pub mod linux_nvenc;
#[cfg(target_os = "linux")]
pub mod linux_portal;
#[cfg(target_os = "linux")]
pub mod linux_vaapi;
#[cfg(target_os = "linux")]
pub mod linux_wlr;
#[cfg(target_os = "linux")]
pub mod linux_x11;
#[cfg(target_os = "windows")]
pub mod mf_encoder;
pub mod software;
#[cfg(target_os = "windows")]
pub mod windows_capture;

use clap::ValueEnum;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use usher_proto::handshake::{ChromaSubsampling, CodecPreference, QualityPreference, VideoCodec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CaptureSource {
    Auto,
    Drm,
    Pipewire,
    X11,
    Dxgi,
    Testsrc,
}

impl CaptureSource {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Drm => "drm",
            Self::Pipewire => "pipewire",
            Self::X11 => "x11",
            Self::Dxgi => "dxgi",
            Self::Testsrc => "testsrc",
        }
    }

    #[allow(dead_code)]
    pub fn from_str_name(s: &str) -> Self {
        match s {
            "drm" => Self::Drm,
            "pipewire" => Self::Pipewire,
            "x11" => Self::X11,
            "dxgi" => Self::Dxgi,
            "testsrc" => Self::Testsrc,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EncoderKind {
    Auto,
    Vaapi,
    Nvenc,
    Amf,
    Qsv,
    Openh264,
    Ffmpeg,
}

impl EncoderKind {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Vaapi => "vaapi",
            Self::Nvenc => "nvenc",
            Self::Amf => "amf",
            Self::Qsv => "qsv",
            Self::Openh264 => "openh264",
            Self::Ffmpeg => "ffmpeg",
        }
    }

    #[allow(dead_code)]
    pub fn from_str_name(s: &str) -> Self {
        match s {
            "vaapi" => Self::Vaapi,
            "nvenc" => Self::Nvenc,
            "amf" => Self::Amf,
            "qsv" => Self::Qsv,
            "openh264" => Self::Openh264,
            "ffmpeg" => Self::Ffmpeg,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_us: u32,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCursorEvent {
    Position {
        visible: bool,
        x_norm: u16,
        y_norm: u16,
    },
    Shape {
        shape_id: u32,
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        rgba: Vec<u8>,
    },
}

/// Requests a single crisp IDR frame refresh after the screen stays visually
/// unchanged for at least 350ms, then stays idle until the next pixel change.
pub struct StaticRefreshTracker {
    last_hash: Option<u64>,
    changed_at: Instant,
    refreshed: bool,
}

impl StaticRefreshTracker {
    pub fn new(now: Instant) -> Self {
        Self {
            last_hash: None,
            changed_at: now,
            refreshed: false,
        }
    }

    pub fn observe_bytes(&mut self, data: &[u8], changed_hint: bool, now: Instant) -> bool {
        if !changed_hint {
            if self.last_hash.is_some()
                && !self.refreshed
                && now.saturating_duration_since(self.changed_at) >= Duration::from_millis(350)
            {
                self.refreshed = true;
                return true;
            }
            return false;
        }
        let hash = sample_frame_hash(data);
        if self.last_hash == Some(hash) {
            if !self.refreshed
                && now.saturating_duration_since(self.changed_at) >= Duration::from_millis(350)
            {
                self.refreshed = true;
                true
            } else {
                false
            }
        } else {
            self.last_hash = Some(hash);
            self.changed_at = now;
            self.refreshed = false;
            false
        }
    }
}

fn sample_frame_hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (data.len() as u64);
    if data.is_empty() {
        return h;
    }
    let step = (data.len() / 512).max(1);
    for &b in data.iter().step_by(step) {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

#[cfg(test)]
mod static_refresh_tests {
    use super::*;

    #[test]
    fn static_refresh_fires_once_after_350ms_and_resets_on_change() {
        let t0 = Instant::now();
        let mut tracker = StaticRefreshTracker::new(t0);
        let frame_a = vec![42u8; 4096];
        let mut frame_b = vec![42u8; 4096];
        frame_b[256] = 99;

        assert!(!tracker.observe_bytes(&frame_a, true, t0));
        assert!(!tracker.observe_bytes(&frame_a, false, t0 + Duration::from_millis(200)));
        assert!(tracker.observe_bytes(&frame_a, false, t0 + Duration::from_millis(360)));
        // Must fire at most once per static interval.
        assert!(!tracker.observe_bytes(&frame_a, false, t0 + Duration::from_millis(800)));

        // Visual change resets timer and arms next static refresh.
        assert!(!tracker.observe_bytes(&frame_b, true, t0 + Duration::from_millis(900)));
        assert!(!tracker.observe_bytes(&frame_b, true, t0 + Duration::from_millis(1100)));
        assert!(tracker.observe_bytes(&frame_b, true, t0 + Duration::from_millis(1300)));
    }
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
    pub chroma: ChromaSubsampling,
    pub separate_cursor: bool,
    pub render_node: Option<PathBuf>,
    pub wayland_runtime_dir: Option<PathBuf>,
    pub wayland_display: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedBackend {
    #[cfg(target_os = "linux")]
    LinuxNative(linux_native::LinuxNativeBackend),
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
        if encoder == EncoderKind::Ffmpeg {
            hw_pipeline::probe_linux_supported_codecs(probe)
        } else {
            linux_native::probe_linux_native_codecs(encoder, probe)
        }
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
        ResolvedBackend::LinuxNative(_) => true,
        #[cfg(target_os = "linux")]
        ResolvedBackend::Hw(_) => false,
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(
            windows_capture::WinVideoBackend::NativeDxgiMf { .. }
            | windows_capture::WinVideoBackend::NativeDxgiOpenH264,
        ) => true,
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(windows_capture::WinVideoBackend::DdagrabHw { .. }) => false,
    }
}

pub fn resolve_effective_codec(cfg: &VideoPipelineConfig) -> VideoCodec {
    match resolve_video_backend(cfg) {
        ResolvedBackend::Software => VideoCodec::H264,
        #[cfg(target_os = "linux")]
        ResolvedBackend::LinuxNative(b) => {
            if b.encoder == linux_native::LinuxEncoderKind::OpenH264 {
                VideoCodec::H264
            } else {
                cfg.codec
            }
        }
        #[cfg(target_os = "linux")]
        ResolvedBackend::Hw(_) => cfg.codec,
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(windows_capture::WinVideoBackend::NativeDxgiOpenH264) => {
            VideoCodec::H264
        }
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(
            windows_capture::WinVideoBackend::NativeDxgiMf { .. }
            | windows_capture::WinVideoBackend::DdagrabHw { .. },
        ) => cfg.codec,
    }
}

pub fn resolve_effective_chroma(cfg: &VideoPipelineConfig) -> ChromaSubsampling {
    if cfg.chroma != ChromaSubsampling::Yuv444
        || !matches!(cfg.codec, VideoCodec::H264 | VideoCodec::Hevc)
    {
        return ChromaSubsampling::Yuv420;
    }
    match resolve_video_backend(cfg) {
        #[cfg(target_os = "linux")]
        ResolvedBackend::LinuxNative(b)
            if b.encoder == linux_native::LinuxEncoderKind::Nvenc =>
        {
            ChromaSubsampling::Yuv444
        }
        #[cfg(target_os = "linux")]
        ResolvedBackend::Hw(hw_pipeline::HwPipelineKind::X11Nvenc) => ChromaSubsampling::Yuv444,
        _ => ChromaSubsampling::Yuv420,
    }
}

pub fn supports_separate_cursor(cfg: &VideoPipelineConfig) -> bool {
    if !cfg.separate_cursor {
        return false;
    }
    match resolve_video_backend(cfg) {
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(
            windows_capture::WinVideoBackend::NativeDxgiMf { .. }
            | windows_capture::WinVideoBackend::NativeDxgiOpenH264,
        ) => true,
        _ => false,
    }
}

pub fn resolve_video_backend(cfg: &VideoPipelineConfig) -> ResolvedBackend {
    if cfg.capture == CaptureSource::Testsrc {
        return ResolvedBackend::Software;
    }

    #[cfg(target_os = "linux")]
    {
        if cfg.encoder == EncoderKind::Ffmpeg {
            if let Some(hw_kind) = select_linux_hw_pipeline(cfg) {
                return ResolvedBackend::Hw(hw_kind);
            }
        } else {
            let probe = drm_probe::probe_drm();
            if let Some(native) = linux_native::select_linux_native_backend(cfg, &probe) {
                return ResolvedBackend::LinuxNative(native);
            }
        }
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
    effective_codec: Arc<AtomicU8>,
    status_notify: Arc<tokio::sync::Notify>,
    tx: mpsc::Sender<EncodedFrame>,
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    cursor_tx: Option<mpsc::Sender<HostCursorEvent>>,
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
                effective_codec.clone(),
                status_notify.clone(),
                tx.clone(),
                cursor_tx.clone(),
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
pub(crate) async fn run_one_backend(
    mut cfg: VideoPipelineConfig,
    backend: ResolvedBackend,
    live_bitrate_kbps: Arc<AtomicU32>,
    in_place_bitrate: Arc<AtomicBool>,
    effective_codec: Arc<AtomicU8>,
    status_notify: Arc<tokio::sync::Notify>,
    tx: mpsc::Sender<EncodedFrame>,
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    cursor_tx: Option<mpsc::Sender<HostCursorEvent>>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    child_cancel: CancellationToken,
) {
    effective_codec.store(cfg.codec.to_u8(), Ordering::Release);

    match backend {
        #[cfg(target_os = "linux")]
        ResolvedBackend::LinuxNative(native_backend) => {
            tracing::info!(
                ?native_backend,
                codec = %cfg.codec,
                bitrate_kbps = cfg.bitrate_kbps,
                "starting native Linux capture + encode pipeline"
            );
            match linux_native::run_linux_native_pipeline(
                native_backend,
                cfg.clone(),
                live_bitrate_kbps.clone(),
                effective_codec.clone(),
                status_notify.clone(),
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
                        "native Linux pipeline failed ({err:#}); falling back to OpenH264 testsrc (switching stream codec to h264)"
                    );
                }
            }
        }
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
                        "hardware video pipeline failed ({err:#}); falling back to OpenH264 testsrc (switching stream codec to h264)"
                    );
                }
            }
        }
        #[cfg(target_os = "windows")]
        ResolvedBackend::Win(win_backend) => {
            let was_mf = matches!(
                win_backend,
                windows_capture::WinVideoBackend::NativeDxgiMf { .. }
            );
            let was_hw = was_mf
                || matches!(win_backend, windows_capture::WinVideoBackend::DdagrabHw { .. });
            let initial_codec = cfg.codec;
            let mut hw_failures = 0u32;
            let mut active_backend = win_backend;

            while !child_cancel.is_cancelled() {
                match windows_capture::run_windows_backend(
                    active_backend.clone(),
                    cfg.clone(),
                    live_bitrate_kbps.clone(),
                    tx.clone(),
                    cursor_tx.clone(),
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
                        if crate::windows_session::current_session_id()
                            != crate::windows_session::active_console_session_id()
                        {
                            tracing::info!(
                                "Windows console session changed; pausing capture while supervisor migrates"
                            );
                            child_cancel.cancelled().await;
                            return;
                        }
                        tracing::warn!("Windows capture pipeline interrupted ({err:#})");

                        if was_hw {
                            hw_failures += 1;
                            if hw_failures < 2 {
                                tokio::select! {
                                    _ = child_cancel.cancelled() => return,
                                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                                }
                                continue;
                            }

                            if matches!(
                                active_backend,
                                windows_capture::WinVideoBackend::NativeDxgiMf { .. }
                            ) && let Some(ddagrab) =
                                windows_capture::select_ddagrab_fallback(&cfg)
                            {
                                tracing::info!(
                                    codec = %cfg.codec,
                                    "Media Foundation hardware pipeline unavailable; trying optional ffmpeg ddagrab fallback"
                                );
                                in_place_bitrate.store(false, Ordering::Release);
                                status_notify.notify_one();
                                active_backend = ddagrab;
                                hw_failures = 0;
                                continue;
                            }

                            cfg.codec = VideoCodec::H264;
                            in_place_bitrate.store(true, Ordering::Release);
                            effective_codec.store(VideoCodec::H264.to_u8(), Ordering::Release);
                            status_notify.notify_one();
                            tracing::info!(
                                "falling back to built-in DXGI + OpenH264 capture (stream codec: h264)"
                            );
                            match windows_capture::run_windows_backend(
                                windows_capture::WinVideoBackend::NativeDxgiOpenH264,
                                cfg.clone(),
                                live_bitrate_kbps.clone(),
                                tx.clone(),
                                cursor_tx.clone(),
                                keyframe_req.clone(),
                                start,
                                child_cancel.clone(),
                            )
                            .await
                            {
                                Ok(()) => return,
                                Err(dxgi_err) => {
                                    if child_cancel.is_cancelled() {
                                        return;
                                    }
                                    if crate::windows_session::current_session_id()
                                        != crate::windows_session::active_console_session_id()
                                    {
                                        tracing::info!(
                                            "Windows console session changed; pausing capture while supervisor migrates"
                                        );
                                        child_cancel.cancelled().await;
                                        return;
                                    }
                                    tracing::warn!(
                                        "built-in DXGI + OpenH264 fallback interrupted ({dxgi_err:#}); retrying desktop capture"
                                    );
                                    cfg.codec = initial_codec;
                                    active_backend = windows_capture::select_windows_backend(&cfg)
                                        .unwrap_or(
                                            windows_capture::WinVideoBackend::NativeDxgiOpenH264,
                                        );
                                    in_place_bitrate.store(
                                        is_in_place_bitrate_backend(&ResolvedBackend::Win(
                                            active_backend.clone(),
                                        )),
                                        Ordering::Release,
                                    );
                                    effective_codec
                                        .store(cfg.codec.to_u8(), Ordering::Release);
                                    status_notify.notify_one();
                                    hw_failures = 0;
                                }
                            }
                        }

                        tokio::select! {
                            _ = child_cancel.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                        }
                    }
                }
            }
            return;
        }
        ResolvedBackend::Software => {
            tracing::info!("starting built-in OpenH264 video stream");
        }
    }

    if !child_cancel.is_cancelled() {
        if cfg.codec != VideoCodec::H264 {
            cfg.codec = VideoCodec::H264;
            tracing::warn!(
                "falling back to built-in OpenH264 testsrc (switching stream codec to h264)"
            );
        } else {
            tracing::warn!("falling back to built-in OpenH264 testsrc");
        }
        in_place_bitrate.store(true, Ordering::Release);
        effective_codec.store(VideoCodec::H264.to_u8(), Ordering::Release);
        status_notify.notify_one();
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
        EncoderKind::Ffmpeg => false,
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
