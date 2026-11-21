use super::color::{CpuFrame, cpu_frame_to_i420, cpu_frame_to_nv12, cpu_frame_to_yuv444};
use super::drm_probe::{DrmProbe, GpuVendor};
use super::linux_kms::{KmsCapturer, probe_kms_status};
use super::linux_nvenc::{NvencEncoder, probe_nvenc_codecs};
use super::linux_portal::{PipeWireCapturer, load_portal_token, probe_portal_status};
use super::linux_vaapi::{VaapiEncoder, probe_vaapi_codecs};
use super::linux_wlr::{WlrScreencopyCapturer, probe_wlr_screencopy, resolve_wayland_socket_path};
use super::linux_x11::{X11Capturer, probe_x11_displays};
use super::software::apply_openh264_bitrate;
use super::{CaptureSource, EncodedFrame, EncoderKind, StaticRefreshTracker, VideoPipelineConfig};
use anyhow::{Context, Result, anyhow};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::annexb::is_keyframe_annexb;
use usher_proto::handshake::{ChromaSubsampling, DisplayInfo, QualityPreference, VideoCodec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxCaptureKind {
    Kms,
    Portal,
    WlrScreencopy,
    X11,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxEncoderKind {
    Vaapi,
    Nvenc,
    OpenH264,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxNativeBackend {
    pub capture: LinuxCaptureKind,
    pub encoder: LinuxEncoderKind,
}

#[allow(dead_code)]
pub struct LinuxCaptureReport {
    pub selected: Option<LinuxNativeBackend>,
    pub diagnostics: Vec<String>,
    pub supported_codecs: Vec<VideoCodec>,
    pub x11_displays: Vec<DisplayInfo>,
}

struct ReusableI420 {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl ReusableI420 {
    fn new(width: u16, height: u16) -> Self {
        let w = (width as usize).max(2) & !1;
        let h = (height as usize).max(2) & !1;
        Self {
            width: w,
            height: h,
            y: vec![16; w * h],
            u: vec![128; (w / 2) * (h / 2)],
            v: vec![128; (w / 2) * (h / 2)],
        }
    }
}

impl YUVSource for ReusableI420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

#[allow(clippy::too_many_arguments)]
fn select_capture_with_reason(
    capture: CaptureSource,
    headless_active: bool,
    kms_res: &std::result::Result<String, String>,
    portal_res: &std::result::Result<String, String>,
    wlr_res: &std::result::Result<String, String>,
    x11_res: &std::result::Result<(Vec<DisplayInfo>, bool), String>,
) -> (Option<LinuxCaptureKind>, Vec<String>) {
    let mut notes = Vec::new();
    let chosen = match capture {
        CaptureSource::Drm => match kms_res {
            Ok(msg) => {
                notes.push(format!("capture selected: kms ({msg})"));
                Some(LinuxCaptureKind::Kms)
            }
            Err(err) => {
                notes.push(format!("capture skipped: kms (--capture drm requested, unavailable: {err})"));
                None
            }
        },
        CaptureSource::Pipewire => {
            if headless_active {
                match wlr_res {
                    Ok(msg) => {
                        notes.push(format!("capture selected: wlr-screencopy ({msg})"));
                        Some(LinuxCaptureKind::WlrScreencopy)
                    }
                    Err(err) => {
                        notes.push(format!("capture skipped: wlr-screencopy ({err})"));
                        None
                    }
                }
            } else if std::env::var_os("WAYLAND_DISPLAY").is_some()
                && super::linux_portal::pipewire_lib_available()
            {
                let suffix = if load_portal_token().is_some() {
                    "saved restore_token present"
                } else {
                    "no saved restore_token (run `usher-host --setup-portal` locally if unattended)"
                };
                notes.push(format!("capture selected: wayland-portal ({suffix})"));
                Some(LinuxCaptureKind::Portal)
            } else {
                let err = portal_res
                    .as_ref()
                    .err()
                    .cloned()
                    .unwrap_or_else(|| "unavailable".into());
                notes.push(format!("capture skipped: wayland-portal ({err})"));
                None
            }
        }
        CaptureSource::X11 => match x11_res {
            Ok((disps, shm)) => {
                notes.push(format!(
                    "capture selected: x11 ({} monitor(s), MIT-SHM={shm})",
                    disps.len()
                ));
                Some(LinuxCaptureKind::X11)
            }
            Err(err) => {
                notes.push(format!("capture skipped: x11 ({err})"));
                None
            }
        },
        CaptureSource::Testsrc | CaptureSource::Dxgi => {
            notes.push("capture: synthetic testsrc requested".into());
            None
        }
        CaptureSource::Auto => {
            if headless_active {
                match wlr_res {
                    Ok(msg) => {
                        notes.push(format!(
                            "capture selected: wlr-screencopy for ephemeral headless session ({msg})"
                        ));
                        return (Some(LinuxCaptureKind::WlrScreencopy), notes);
                    }
                    Err(err) => {
                        notes.push(format!(
                            "capture skipped: wlr-screencopy for ephemeral headless session ({err})"
                        ));
                    }
                }
            }
            // Priority: KMS (works on Wayland/X11/login without prompts) -> Portal (only if remembered) -> wlr-screencopy -> X11.
            let mut pick = None;
            match kms_res {
                Ok(msg) => {
                    notes.push(format!("capture selected: kms ({msg})"));
                    pick = Some(LinuxCaptureKind::Kms);
                }
                Err(err) => notes.push(format!("capture skipped: kms ({err})")),
            }
            match portal_res {
                Ok(msg) if pick.is_none() => {
                    notes.push(format!("capture selected: wayland-portal ({msg})"));
                    pick = Some(LinuxCaptureKind::Portal);
                }
                Ok(msg) => notes.push(format!("capture standby: wayland-portal ({msg})")),
                Err(err) => notes.push(format!("capture skipped: wayland-portal ({err})")),
            }
            match wlr_res {
                Ok(msg) if pick.is_none() => {
                    notes.push(format!("capture selected: wlr-screencopy ({msg})"));
                    pick = Some(LinuxCaptureKind::WlrScreencopy);
                }
                Ok(msg) => notes.push(format!("capture standby: wlr-screencopy ({msg})")),
                Err(err) => notes.push(format!("capture skipped: wlr-screencopy ({err})")),
            }
            match x11_res {
                Ok((disps, shm)) if pick.is_none() => {
                    notes.push(format!(
                        "capture selected: x11 ({} monitor(s), MIT-SHM={shm})",
                        disps.len()
                    ));
                    pick = Some(LinuxCaptureKind::X11);
                }
                Ok((disps, shm)) => notes.push(format!(
                    "capture standby: x11 ({} monitor(s), MIT-SHM={shm})",
                    disps.len()
                )),
                Err(err) => notes.push(format!("capture skipped: x11 ({err})")),
            }
            if pick.is_none() {
                notes.push("capture fallback: testsrc (no KMS, remembered Wayland portal, wlr-screencopy, or X11 display available)".into());
            }
            pick
        }
    };
    (chosen, notes)
}

pub fn diagnose_linux_capture(cfg: &VideoPipelineConfig, probe: &DrmProbe) -> LinuxCaptureReport {
    let kms_res = probe_kms_status(probe);
    let portal_res = probe_portal_status();
    let wlr_res = probe_wlr_screencopy(
        cfg.wayland_runtime_dir.as_deref(),
        cfg.wayland_display.as_deref(),
    );
    let x11_res = probe_x11_displays();
    let x11_displays = x11_res
        .as_ref()
        .map(|(d, _)| d.clone())
        .unwrap_or_default();

    let (cap_opt, mut diagnostics) = select_capture_with_reason(
        cfg.capture,
        cfg.wayland_display.is_some(),
        &kms_res,
        &portal_res,
        &wlr_res,
        &x11_res,
    );

    let nvenc_probe = probe_nvenc_codecs();
    let vaapi_probe = probe_vaapi_codecs(probe);

    let mut supported_codecs = Vec::new();
    match cfg.encoder {
        EncoderKind::Openh264 => {
            supported_codecs.push(VideoCodec::H264);
        }
        EncoderKind::Ffmpeg => {
            return LinuxCaptureReport {
                selected: None,
                diagnostics: vec![
                    "encoder override: --encoder ffmpeg escape hatch".into(),
                    crate::audio::pipewire_linux::audio_backend_diagnostic(),
                ],
                supported_codecs: super::hw_pipeline::probe_linux_supported_codecs(probe),
                x11_displays,
            };
        }
        _ => {
            if let Ok(ref c) = nvenc_probe
                && matches!(cfg.encoder, EncoderKind::Auto | EncoderKind::Nvenc)
            {
                for &codec in c {
                    if !supported_codecs.contains(&codec) {
                        supported_codecs.push(codec);
                    }
                }
            }
            if let Ok(ref c) = vaapi_probe
                && matches!(cfg.encoder, EncoderKind::Auto | EncoderKind::Vaapi)
            {
                for &codec in c {
                    if !supported_codecs.contains(&codec) {
                        supported_codecs.push(codec);
                    }
                }
            }
            if !supported_codecs.contains(&VideoCodec::H264) {
                supported_codecs.push(VideoCodec::H264);
            }
        }
    }

    let prefer_nvidia = matches!(cfg.encoder, EncoderKind::Nvenc)
        || (cfg.encoder == EncoderKind::Auto && matches!(probe.vendor, Some(GpuVendor::Nvidia)));

    let enc_kind = match cfg.encoder {
        EncoderKind::Openh264 => LinuxEncoderKind::OpenH264,
        EncoderKind::Nvenc => {
            if nvenc_probe.is_ok() {
                LinuxEncoderKind::Nvenc
            } else {
                LinuxEncoderKind::OpenH264
            }
        }
        EncoderKind::Vaapi => {
            if vaapi_probe.is_ok() {
                LinuxEncoderKind::Vaapi
            } else {
                LinuxEncoderKind::OpenH264
            }
        }
        EncoderKind::Amf | EncoderKind::Qsv | EncoderKind::Ffmpeg => LinuxEncoderKind::OpenH264,
        EncoderKind::Auto => {
            if prefer_nvidia && let Ok(ref cs) = nvenc_probe && cs.contains(&cfg.codec) {
                LinuxEncoderKind::Nvenc
            } else if let Ok(ref cs) = vaapi_probe
                && cs.contains(&cfg.codec)
            {
                LinuxEncoderKind::Vaapi
            } else if let Ok(ref cs) = nvenc_probe
                && cs.contains(&cfg.codec)
            {
                LinuxEncoderKind::Nvenc
            } else if vaapi_probe.is_ok() {
                LinuxEncoderKind::Vaapi
            } else if nvenc_probe.is_ok() {
                LinuxEncoderKind::Nvenc
            } else {
                LinuxEncoderKind::OpenH264
            }
        }
    };

    match &nvenc_probe {
        Ok(cs) => {
            let list = cs.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
            diagnostics.push(format!("encoder available: nvenc ({list}, libcuda+libnvidia-encode)"));
        }
        Err(err) => diagnostics.push(format!("encoder skipped: nvenc ({err})")),
    }
    match &vaapi_probe {
        Ok(cs) => {
            let list = cs.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
            diagnostics.push(format!("encoder available: vaapi ({list}, libva+libva-drm)"));
        }
        Err(err) => diagnostics.push(format!("encoder skipped: vaapi ({err})")),
    }
    diagnostics.push(format!(
        "selected runtime encoder: {:?} (built-in OpenH264 fallback ready)",
        enc_kind
    ));
    diagnostics.push(crate::audio::pipewire_linux::audio_backend_diagnostic());

    let selected = cap_opt.map(|capture| LinuxNativeBackend {
        capture,
        encoder: enc_kind,
    });

    LinuxCaptureReport {
        selected,
        diagnostics,
        supported_codecs,
        x11_displays,
    }
}

pub fn probe_linux_native_codecs(encoder: EncoderKind, probe: &DrmProbe) -> Vec<VideoCodec> {
    let mut out = Vec::new();
    if encoder == EncoderKind::Openh264 {
        return vec![VideoCodec::H264];
    }
    if matches!(encoder, EncoderKind::Auto | EncoderKind::Nvenc)
        && let Ok(cs) = probe_nvenc_codecs()
    {
        for c in cs {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    if matches!(encoder, EncoderKind::Auto | EncoderKind::Vaapi)
        && let Ok(cs) = probe_vaapi_codecs(probe)
    {
        for c in cs {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    if !out.contains(&VideoCodec::H264) {
        out.push(VideoCodec::H264);
    }
    out
}

pub fn select_linux_native_backend(
    cfg: &VideoPipelineConfig,
    probe: &DrmProbe,
) -> Option<LinuxNativeBackend> {
    if cfg.capture == CaptureSource::Testsrc || cfg.encoder == EncoderKind::Ffmpeg {
        return None;
    }
    diagnose_linux_capture(cfg, probe).selected
}

enum ActiveCapture {
    Kms(KmsCapturer),
    Portal(PipeWireCapturer),
    Wlr(Box<WlrScreencopyCapturer>),
    X11(Box<X11Capturer>),
}

impl ActiveCapture {
    fn open(kind: LinuxCaptureKind, cfg: &VideoPipelineConfig) -> Result<Self> {
        match kind {
            LinuxCaptureKind::Kms => {
                let probe = super::drm_probe::probe_drm();
                let card = probe
                    .card_node
                    .unwrap_or_else(|| PathBuf::from("/dev/dri/card0"));
                Ok(Self::Kms(KmsCapturer::open(&card, cfg.display_index)?))
            }
            LinuxCaptureKind::Portal => {
                Ok(Self::Portal(PipeWireCapturer::open_from_portal(
                    cfg.width,
                    cfg.height,
                    cfg.fps,
                )?))
            }
            LinuxCaptureKind::WlrScreencopy => {
                let sock = resolve_wayland_socket_path(
                    cfg.wayland_runtime_dir.as_deref(),
                    cfg.wayland_display.as_deref(),
                )
                .context("headless/Wayland socket path not found")?;
                Ok(Self::Wlr(Box::new(WlrScreencopyCapturer::open(
                    &sock,
                    cfg.display_index,
                )?)))
            }
            LinuxCaptureKind::X11 => Ok(Self::X11(Box::new(X11Capturer::open(cfg.display_index)?))),
        }
    }
}

enum ActiveEncoder {
    Vaapi(Box<VaapiEncoder>),
    Nvenc(Box<NvencEncoder>),
    OpenH264 {
        enc: Box<Encoder>,
        applied_kbps: u32,
        i420: ReusableI420,
    },
}

fn create_openh264_encoder(cfg: &VideoPipelineConfig, initial_kbps: u32) -> Result<ActiveEncoder> {
    let complexity = match cfg.prefer {
        QualityPreference::Latency => Complexity::Low,
        QualityPreference::Quality => Complexity::Medium,
    };
    let enc_cfg = EncoderConfig::new()
        .bitrate(BitRate::from_bps(initial_kbps.max(500).saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(cfg.fps.max(1) as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::ScreenContentRealTime)
        .complexity(complexity)
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        .intra_frame_period(IntraFramePeriod::from_num_frames(cfg.fps.max(1) as u32 * 2))
        .skip_frames(false)
        .vui(VuiConfig::bt709());
    let enc = Encoder::with_api_config(OpenH264API::from_source(), enc_cfg)
        .map_err(|e| anyhow!("failed to initialize OpenH264 encoder: {e}"))?;
    Ok(ActiveEncoder::OpenH264 {
        enc: Box::new(enc),
        applied_kbps: initial_kbps.max(500),
        i420: ReusableI420::new(cfg.width, cfg.height),
    })
}

fn create_active_encoder(
    backend: &LinuxNativeBackend,
    cfg: &VideoPipelineConfig,
    probe: &DrmProbe,
    initial_kbps: u32,
    effective_codec: &AtomicU8,
    status_notify: &tokio::sync::Notify,
) -> Result<ActiveEncoder> {
    match backend.encoder {
        LinuxEncoderKind::Vaapi => {
            match VaapiEncoder::new(
                probe,
                cfg.codec,
                cfg.width,
                cfg.height,
                cfg.fps,
                initial_kbps,
                cfg.prefer,
            ) {
                Ok(va) => return Ok(ActiveEncoder::Vaapi(Box::new(va))),
                Err(err) => {
                    tracing::warn!("VA-API init failed ({err:#}); falling back to OpenH264 on real screen capture");
                }
            }
        }
        LinuxEncoderKind::Nvenc => {
            match NvencEncoder::new(
                cfg.codec,
                cfg.chroma,
                cfg.width,
                cfg.height,
                cfg.fps,
                initial_kbps,
                cfg.prefer,
            ) {
                Ok(nv) => return Ok(ActiveEncoder::Nvenc(Box::new(nv))),
                Err(err) => {
                    tracing::warn!("NVENC init failed ({err:#}); falling back to OpenH264 on real screen capture");
                }
            }
        }
        LinuxEncoderKind::OpenH264 => {}
    }

    effective_codec.store(VideoCodec::H264.to_u8(), Ordering::Release);
    status_notify.notify_one();
    create_openh264_encoder(cfg, initial_kbps)
}

#[allow(clippy::too_many_arguments)]
pub async fn run_linux_native_pipeline(
    backend: LinuxNativeBackend,
    cfg: VideoPipelineConfig,
    live_bitrate_kbps: Arc<AtomicU32>,
    effective_codec: Arc<AtomicU8>,
    status_notify: Arc<tokio::sync::Notify>,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        run_blocking(
            backend,
            cfg,
            live_bitrate_kbps,
            effective_codec,
            status_notify,
            tx,
            keyframe_req,
            start,
            cancel,
        )
    })
    .await
    .map_err(|e| anyhow!("Linux native video worker panicked: {e}"))?
}

#[allow(clippy::too_many_arguments)]
fn run_blocking(
    backend: LinuxNativeBackend,
    cfg: VideoPipelineConfig,
    live_bitrate_kbps: Arc<AtomicU32>,
    effective_codec: Arc<AtomicU8>,
    status_notify: Arc<tokio::sync::Notify>,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let mut capture = ActiveCapture::open(backend.capture, &cfg)
        .with_context(|| format!("starting {:?} capture", backend.capture))?;

    let probe = super::drm_probe::probe_drm();
    let initial_kbps = live_bitrate_kbps.load(Ordering::Acquire).max(500);
    let mut encoder = create_active_encoder(
        &backend,
        &cfg,
        &probe,
        initial_kbps,
        &effective_codec,
        &status_notify,
    )?;

    let w = (cfg.width as usize).max(2) & !1;
    let h = (cfg.height as usize).max(2) & !1;
    let mut nv12_y = vec![16u8; w * h];
    let mut nv12_uv = vec![128u8; w * (h / 2)];
    let mut yuv444_u = vec![128u8; w * h];
    let mut yuv444_v = vec![128u8; w * h];
    let mut static_refresh = StaticRefreshTracker::new(start);
    let mut have_encoded_first = false;
    let mut emitted_frames = 0u64;

    let frame_period = Duration::from_micros(1_000_000 / cfg.fps.max(1) as u64);
    let mut next_tick = Instant::now();

    while !cancel.is_cancelled() {
        let desired_kbps = live_bitrate_kbps.load(Ordering::Acquire).max(500);
        let force_idr = keyframe_req.swap(false, Ordering::AcqRel) || !have_encoded_first;

        let enc_res: Result<(Vec<u8>, bool)> = match &mut capture {
            ActiveCapture::Kms(kms) => {
                let dmabuf = kms.capture_frame()?;
                match &mut encoder {
                    ActiveEncoder::Vaapi(va) if va.supports_dmabuf_zero_copy() => {
                        va.update_bitrate_kbps(desired_kbps);
                        let idr = force_idr
                            || static_refresh.observe_bytes(&[], false, Instant::now());
                        va.encode_dmabuf(dmabuf, idr)
                    }
                    _ => {
                        let cpu = dmabuf.read_cpu_frame()?;
                        encode_cpu_frame(
                            &cpu,
                            w,
                            h,
                            &mut encoder,
                            &mut nv12_y,
                            &mut nv12_uv,
                            &mut yuv444_u,
                            &mut yuv444_v,
                            &mut static_refresh,
                            desired_kbps,
                            force_idr,
                            have_encoded_first,
                        )
                    }
                }
            }
            ActiveCapture::Portal(pw) => {
                let cpu = pw.capture_frame()?;
                encode_cpu_frame(
                    &cpu,
                    w,
                    h,
                    &mut encoder,
                    &mut nv12_y,
                    &mut nv12_uv,
                    &mut yuv444_u,
                    &mut yuv444_v,
                    &mut static_refresh,
                    desired_kbps,
                    force_idr,
                    have_encoded_first,
                )
            }
            ActiveCapture::Wlr(wlr) => {
                let cpu = wlr.capture_frame()?;
                encode_cpu_frame(
                    &cpu,
                    w,
                    h,
                    &mut encoder,
                    &mut nv12_y,
                    &mut nv12_uv,
                    &mut yuv444_u,
                    &mut yuv444_v,
                    &mut static_refresh,
                    desired_kbps,
                    force_idr,
                    have_encoded_first,
                )
            }
            ActiveCapture::X11(x11) => {
                let cpu = x11.capture_frame()?;
                encode_cpu_frame(
                    &cpu,
                    w,
                    h,
                    &mut encoder,
                    &mut nv12_y,
                    &mut nv12_uv,
                    &mut yuv444_u,
                    &mut yuv444_v,
                    &mut static_refresh,
                    desired_kbps,
                    force_idr,
                    have_encoded_first,
                )
            }
        };

        match enc_res {
            Ok((data, keyframe)) => {
                have_encoded_first = true;
                if !data.is_empty() {
                    emitted_frames += 1;
                    let ts_us = start.elapsed().as_micros() as u32;
                    let _ = tx.try_send(EncodedFrame {
                        data,
                        keyframe,
                        timestamp_us: ts_us,
                    });
                }
            }
            Err(err) => {
                // If hardware VA-API/NVENC failed cleanly before producing frames, fall back
                // to built-in OpenH264 on the *same real capture source*!
                if emitted_frames == 0 && !matches!(encoder, ActiveEncoder::OpenH264 { .. }) {
                    tracing::warn!(
                        "hardware encoder failed ({err:#}); falling back to OpenH264 on real screen capture"
                    );
                    effective_codec.store(VideoCodec::H264.to_u8(), Ordering::Release);
                    status_notify.notify_one();
                    encoder = create_openh264_encoder(&cfg, desired_kbps)?;
                    have_encoded_first = false;
                    keyframe_req.store(true, Ordering::Release);
                    continue;
                }
                return Err(err);
            }
        }

        next_tick += frame_period;
        let now = Instant::now();
        if next_tick > now {
            let rem = next_tick - now;
            if rem > Duration::from_millis(2) {
                std::thread::sleep(rem - Duration::from_millis(1));
            }
            while Instant::now() < next_tick {
                if cancel.is_cancelled() {
                    break;
                }
                std::hint::spin_loop();
            }
        } else {
            next_tick = now;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn encode_cpu_frame(
    cpu: &CpuFrame,
    w: usize,
    h: usize,
    encoder: &mut ActiveEncoder,
    nv12_y: &mut [u8],
    nv12_uv: &mut [u8],
    yuv444_u: &mut [u8],
    yuv444_v: &mut [u8],
    static_refresh: &mut StaticRefreshTracker,
    desired_kbps: u32,
    force_idr: bool,
    have_encoded_first: bool,
) -> Result<(Vec<u8>, bool)> {
    match encoder {
        ActiveEncoder::Vaapi(va) => {
            cpu_frame_to_nv12(cpu, w, h, nv12_y, nv12_uv)?;
            let idr = force_idr || static_refresh.observe_bytes(nv12_y, true, Instant::now());
            va.update_bitrate_kbps(desired_kbps);
            va.encode_nv12(nv12_y, nv12_uv, idr)
        }
        ActiveEncoder::Nvenc(nv) => {
            let _ = nv.update_bitrate_kbps(desired_kbps);
            if nv.chroma() == ChromaSubsampling::Yuv444 {
                cpu_frame_to_yuv444(cpu, w, h, nv12_y, yuv444_u, yuv444_v)?;
                let idr = force_idr || static_refresh.observe_bytes(nv12_y, true, Instant::now());
                nv.encode_yuv444(nv12_y, yuv444_u, yuv444_v, idr)
            } else {
                cpu_frame_to_nv12(cpu, w, h, nv12_y, nv12_uv)?;
                let idr = force_idr || static_refresh.observe_bytes(nv12_y, true, Instant::now());
                nv.encode_nv12(nv12_y, nv12_uv, idr)
            }
        }
        ActiveEncoder::OpenH264 {
            enc,
            applied_kbps,
            i420,
        } => {
            cpu_frame_to_i420(cpu, w, h, &mut i420.y, &mut i420.u, &mut i420.v)?;
            let idr = force_idr || static_refresh.observe_bytes(&i420.y, true, Instant::now());
            if have_encoded_first && desired_kbps != *applied_kbps {
                apply_openh264_bitrate(enc, desired_kbps);
                *applied_kbps = desired_kbps;
            }
            if idr {
                enc.force_intra_frame();
            }
            let bs = enc
                .encode(i420)
                .map_err(|e| anyhow!("OpenH264 encode failed: {e}"))?;
            let data = bs.to_vec();
            let key = matches!(bs.frame_type(), FrameType::IDR | FrameType::I)
                || is_keyframe_annexb(VideoCodec::H264, &data);
            Ok((data, key))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_selection_prefers_kms_then_remembered_portal_then_x11() {
        let kms_ok = Ok("card0 ready".into());
        let portal_ok = Ok("portal ready".into());
        let wlr_err = Err("no wlr-screencopy".into());
        let wlr_ok = Ok("wlr-screencopy v3 ready".into());
        let x11_ok = Ok((vec![], true));
        assert_eq!(
            select_capture_with_reason(
                CaptureSource::Auto,
                false,
                &kms_ok,
                &portal_ok,
                &wlr_err,
                &x11_ok,
            )
            .0,
            Some(LinuxCaptureKind::Kms)
        );
        assert_eq!(
            select_capture_with_reason(
                CaptureSource::Auto,
                true,
                &kms_ok,
                &portal_ok,
                &wlr_ok,
                &x11_ok,
            )
            .0,
            Some(LinuxCaptureKind::WlrScreencopy)
        );

        let kms_err = Err("missing CAP_SYS_ADMIN".into());
        assert_eq!(
            select_capture_with_reason(
                CaptureSource::Auto,
                false,
                &kms_err,
                &portal_ok,
                &wlr_err,
                &x11_ok,
            )
            .0,
            Some(LinuxCaptureKind::Portal)
        );

        let portal_err = Err("no restore_token".into());
        assert_eq!(
            select_capture_with_reason(
                CaptureSource::Auto,
                false,
                &kms_err,
                &portal_err,
                &wlr_err,
                &x11_ok,
            )
            .0,
            Some(LinuxCaptureKind::X11)
        );

        let x11_err = Err("DISPLAY not set".into());
        let (none_pick, reasons) = select_capture_with_reason(
            CaptureSource::Auto,
            false,
            &kms_err,
            &portal_err,
            &wlr_err,
            &x11_err,
        );
        assert_eq!(none_pick, None);
        assert!(reasons.iter().any(|r| r.contains("CAP_SYS_ADMIN")));
        assert!(reasons.iter().any(|r| r.contains("no restore_token")));
        assert!(reasons.iter().any(|r| r.contains("DISPLAY not set")));
    }
}
