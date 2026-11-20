use super::ivf::IvfReader;
use super::software::apply_openh264_bitrate;
use super::{CaptureSource, EncodedFrame, EncoderKind, VideoPipelineConfig};
use anyhow::{Context, Result, anyhow};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::annexb::{AnnexBSplitter, is_keyframe_annexb};
use usher_proto::handshake::{DisplayInfo, QualityPreference, VideoCodec};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
use windows::core::Interface;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinHwVendor {
    Nvenc,
    Amf,
    Qsv,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinVideoBackend {
    DdagrabHw {
        ffmpeg_path: PathBuf,
        encoder_name: &'static str,
        vendor: WinHwVendor,
    },
    NativeDxgiOpenH264,
}

pub fn ensure_dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn primary_monitor_resolution() -> Option<(u16, u16)> {
    ensure_dpi_aware();
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if w >= 64 && h >= 64 && w <= u16::MAX as i32 && h <= u16::MAX as i32 {
        Some(((w as u16) & !1, (h as u16) & !1))
    } else {
        None
    }
}

struct EnumeratedDisplay {
    info: DisplayInfo,
    adapter: IDXGIAdapter1,
    output_idx: u32,
}

fn enumerate_windows_adapters_and_outputs() -> (Vec<WinHwVendor>, Vec<EnumeratedDisplay>) {
    let mut vendors = Vec::new();
    let mut displays = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return (vendors, displays);
        };
        let mut adapter_idx = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_idx) {
            adapter_idx += 1;
            if let Ok(desc) = adapter.GetDesc1() {
                let vendor = match desc.VendorId {
                    0x10DE => Some(WinHwVendor::Nvenc),
                    0x1002 => Some(WinHwVendor::Amf),
                    0x8086 => Some(WinHwVendor::Qsv),
                    _ => None,
                };
                if let Some(v) = vendor
                    && !vendors.contains(&v)
                {
                    vendors.push(v);
                }
            }
            let mut output_idx = 0u32;
            while let Ok(output) = adapter.EnumOutputs(output_idx) {
                if let Ok(desc) = output.GetDesc()
                    && desc.AttachedToDesktop.as_bool()
                {
                    let w = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).max(0);
                    let h = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).max(0);
                    if w >= 64 && h >= 64 && w <= u16::MAX as i32 && h <= u16::MAX as i32 {
                        let end = desc
                            .DeviceName
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(desc.DeviceName.len());
                        let raw_name = String::from_utf16_lossy(&desc.DeviceName[..end]);
                        let short_name = raw_name
                            .strip_prefix(r"\\.\")
                            .unwrap_or(&raw_name)
                            .to_string();
                        if let Ok(idx) = u8::try_from(displays.len()) {
                            let label = if short_name.is_empty() {
                                format!("Display {}", idx + 1)
                            } else {
                                short_name
                            };
                            displays.push(EnumeratedDisplay {
                                info: DisplayInfo {
                                    index: idx,
                                    name: label,
                                    width: (w as u16) & !1,
                                    height: (h as u16) & !1,
                                },
                                adapter: adapter.clone(),
                                output_idx,
                            });
                        }
                    }
                }
                output_idx += 1;
            }
        }
    }
    (vendors, displays)
}

pub fn enumerate_displays() -> Vec<DisplayInfo> {
    let (_, displays) = enumerate_windows_adapters_and_outputs();
    let mut out: Vec<DisplayInfo> = displays.into_iter().map(|d| d.info).collect();
    if out.is_empty()
        && let Some((w, h)) = primary_monitor_resolution()
    {
        out.push(DisplayInfo {
            index: 0,
            name: "Primary".into(),
            width: w,
            height: h,
        });
    }
    out
}

pub fn has_attached_dxgi_output() -> bool {
    !enumerate_windows_adapters_and_outputs().1.is_empty()
}

#[allow(dead_code)]
pub fn display_resolution(display_index: u8) -> Option<(u16, u16)> {
    let displays = enumerate_displays();
    displays
        .iter()
        .find(|d| d.index == display_index)
        .or_else(|| displays.first())
        .map(|d| (d.width, d.height))
}

fn find_working_ffmpeg() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("USHER_FFMPEG") {
        candidates.push(PathBuf::from(p));
    }
    candidates.push(PathBuf::from("ffmpeg"));

    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let scoop_ffmpeg = PathBuf::from(profile).join("scoop").join("apps").join("ffmpeg");
        if let Ok(entries) = std::fs::read_dir(scoop_ffmpeg) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy() != "current" {
                    let bin = entry.path().join("bin").join("ffmpeg.exe");
                    if bin.exists() {
                        candidates.push(bin);
                    }
                }
            }
        }
    }

    for cand in candidates {
        if let Ok(out) = std::process::Command::new(&cand)
            .args(["-hide_banner", "-version"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            && out.status.success()
        {
            return Some(cand);
        }
    }
    None
}

fn query_ffmpeg_caps(ffmpeg: &Path) -> Option<(bool, String)> {
    let filters = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-filters"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let f_text = String::from_utf8_lossy(&filters.stdout);
    let has_ddagrab = f_text.contains("ddagrab") && f_text.contains("scale_d3d11");
    if !has_ddagrab {
        return Some((false, String::new()));
    }

    let encs = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some((true, String::from_utf8_lossy(&encs.stdout).into_owned()))
}

fn encoder_candidates(
    encoder_kind: EncoderKind,
    codec: VideoCodec,
    detected_vendors: &[WinHwVendor],
) -> Vec<(&'static str, WinHwVendor)> {
    let mut order: Vec<WinHwVendor> = match encoder_kind {
        EncoderKind::Nvenc => vec![WinHwVendor::Nvenc],
        EncoderKind::Amf => vec![WinHwVendor::Amf],
        EncoderKind::Qsv => vec![WinHwVendor::Qsv],
        EncoderKind::Openh264 => return Vec::new(),
        EncoderKind::Vaapi | EncoderKind::Auto => {
            let mut list = Vec::new();
            for v in [WinHwVendor::Nvenc, WinHwVendor::Amf, WinHwVendor::Qsv] {
                if detected_vendors.is_empty() || detected_vendors.contains(&v) {
                    list.push(v);
                }
            }
            list
        }
    };
    if order.is_empty() {
        order = vec![WinHwVendor::Nvenc, WinHwVendor::Amf, WinHwVendor::Qsv];
    }
    order
        .into_iter()
        .map(|v| {
            let name = match (v, codec) {
                (WinHwVendor::Nvenc, VideoCodec::H264) => "h264_nvenc",
                (WinHwVendor::Nvenc, VideoCodec::Hevc) => "hevc_nvenc",
                (WinHwVendor::Nvenc, VideoCodec::Av1) => "av1_nvenc",
                (WinHwVendor::Amf, VideoCodec::H264) => "h264_amf",
                (WinHwVendor::Amf, VideoCodec::Hevc) => "hevc_amf",
                (WinHwVendor::Amf, VideoCodec::Av1) => "av1_amf",
                (WinHwVendor::Qsv, VideoCodec::H264) => "h264_qsv",
                (WinHwVendor::Qsv, VideoCodec::Hevc) => "hevc_qsv",
                (WinHwVendor::Qsv, VideoCodec::Av1) => "av1_qsv",
            };
            (name, v)
        })
        .collect()
}

pub fn probe_supported_codecs(capture: CaptureSource, encoder: EncoderKind) -> Vec<VideoCodec> {
    let mut out = Vec::new();
    if capture != CaptureSource::Testsrc
        && encoder != EncoderKind::Openh264
        && let Some(ffmpeg) = find_working_ffmpeg()
        && let Some((true, encs_text)) = query_ffmpeg_caps(&ffmpeg)
    {
        let (vendors, _) = enumerate_windows_adapters_and_outputs();
        for codec in [VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::H264] {
            for (enc_name, _) in encoder_candidates(encoder, codec, &vendors) {
                if encs_text.contains(enc_name) {
                    out.push(codec);
                    break;
                }
            }
        }
    }
    if !out.contains(&VideoCodec::H264) {
        out.push(VideoCodec::H264);
    }
    out
}

pub fn select_windows_backend(cfg: &VideoPipelineConfig) -> Option<WinVideoBackend> {
    if !matches!(cfg.capture, CaptureSource::Auto | CaptureSource::Dxgi) {
        return None;
    }
    if cfg.encoder == EncoderKind::Openh264 {
        return Some(WinVideoBackend::NativeDxgiOpenH264);
    }

    if let Some(ffmpeg) = find_working_ffmpeg()
        && let Some((true, encs_text)) = query_ffmpeg_caps(&ffmpeg)
    {
        let (vendors, _) = enumerate_windows_adapters_and_outputs();
        for (enc_name, vendor) in encoder_candidates(cfg.encoder, cfg.codec, &vendors) {
            if encs_text.contains(enc_name) {
                return Some(WinVideoBackend::DdagrabHw {
                    ffmpeg_path: ffmpeg,
                    encoder_name: enc_name,
                    vendor,
                });
            }
        }
    }

    Some(WinVideoBackend::NativeDxgiOpenH264)
}

pub async fn run_windows_backend(
    backend: WinVideoBackend,
    cfg: VideoPipelineConfig,
    live_bitrate_kbps: Arc<AtomicU32>,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    match backend {
        WinVideoBackend::DdagrabHw {
            ffmpeg_path,
            encoder_name,
            vendor,
        } => {
            tracing::info!(
                ffmpeg = %ffmpeg_path.display(),
                encoder = encoder_name,
                codec = %cfg.codec,
                display = cfg.display_index,
                bitrate_kbps = cfg.bitrate_kbps,
                "starting Windows zero-copy ddagrab hardware video pipeline"
            );
            run_ddagrab_hw(ffmpeg_path, encoder_name, vendor, cfg, tx, keyframe_req, start, cancel)
                .await
        }
        WinVideoBackend::NativeDxgiOpenH264 => {
            tracing::info!(
                display = cfg.display_index,
                "starting built-in DXGI Desktop Duplication + OpenH264 capture"
            );
            run_native_dxgi_openh264(cfg, live_bitrate_kbps, tx, keyframe_req, start, cancel).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_ddagrab_hw(
    ffmpeg_path: PathBuf,
    encoder_name: &'static str,
    vendor: WinHwVendor,
    cfg: VideoPipelineConfig,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let buf_frames = match cfg.prefer {
        QualityPreference::Latency => 1u32,
        QualityPreference::Quality => 2u32,
    };
    let br = format!("{}k", cfg.bitrate_kbps);
    let bufsize = format!(
        "{}k",
        (cfg.bitrate_kbps * buf_frames / cfg.fps.max(1) as u32).max(200)
    );
    let gop = match cfg.prefer {
        QualityPreference::Latency => (cfg.fps as u32).max(30),
        QualityPreference::Quality => (cfg.fps as u32 * 2).max(30),
    }
    .to_string();
    let out_fmt = match cfg.codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "ivf",
    };

    let filter = format!(
        "ddagrab=output_idx={}:framerate={}:draw_mouse=1:dup_frames=1,scale_d3d11=width={}:height={}:format=nv12",
        cfg.display_index, cfg.fps, cfg.width, cfg.height
    );

    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-fflags".to_string(),
        "nobuffer".to_string(),
        "-init_hw_device".to_string(),
        "d3d11va".to_string(),
        "-filter_hw_device".to_string(),
        "d3d11va".to_string(),
        "-filter_complex".to_string(),
        filter,
        "-c:v".to_string(),
        encoder_name.to_string(),
    ];

    match vendor {
        WinHwVendor::Nvenc => {
            let preset = match cfg.prefer {
                QualityPreference::Latency => "p1",
                QualityPreference::Quality => "p4",
            };
            let tune = match cfg.prefer {
                QualityPreference::Latency => "ull",
                QualityPreference::Quality => "ll",
            };
            args.extend([
                "-preset".into(),
                preset.into(),
                "-tune".into(),
                tune.into(),
                "-zerolatency".into(),
                "1".into(),
                "-bf".into(),
                "0".into(),
                "-forced-idr".into(),
                "1".into(),
                "-rc".into(),
                "cbr".into(),
                "-b:v".into(),
                br.clone(),
                "-maxrate".into(),
                br,
                "-bufsize".into(),
                bufsize,
                "-g".into(),
                gop,
            ]);
            if matches!(cfg.codec, VideoCodec::H264 | VideoCodec::Hevc) {
                args.extend(["-aud".into(), "1".into()]);
            }
        }
        WinHwVendor::Amf => {
            let (usage, quality) = match cfg.prefer {
                QualityPreference::Latency => ("ultralowlatency", "speed"),
                QualityPreference::Quality => ("lowlatency", "balanced"),
            };
            args.extend([
                "-usage".into(),
                usage.into(),
                "-quality".into(),
                quality.into(),
                "-rc".into(),
                "cbr".into(),
                "-b:v".into(),
                br.clone(),
                "-maxrate".into(),
                br,
                "-bufsize".into(),
                bufsize,
                "-bf".into(),
                "0".into(),
                "-g".into(),
                gop,
            ]);
        }
        WinHwVendor::Qsv => {
            let preset = match cfg.prefer {
                QualityPreference::Latency => "veryfast",
                QualityPreference::Quality => "balanced",
            };
            args.extend([
                "-preset".into(),
                preset.into(),
                "-low_power".into(),
                "1".into(),
                "-bf".into(),
                "0".into(),
                "-b:v".into(),
                br.clone(),
                "-maxrate".into(),
                br,
                "-bufsize".into(),
                bufsize,
                "-g".into(),
                gop,
            ]);
        }
    }

    args.extend(["-f".into(), out_fmt.into(), "pipe:1".into()]);

    let mut child = Command::new(&ffmpeg_path)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn Windows ddagrab ffmpeg pipeline")?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ffmpeg missing stdout"))?;

    let stderr_tail = Arc::new(std::sync::Mutex::new(
        std::collections::VecDeque::<String>::with_capacity(8),
    ));
    let stderr_task = child.stderr.take().map(|stderr| {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!("[ddagrab] {line}");
                if let Ok(mut g) = tail.lock() {
                    if g.len() >= 8 {
                        g.pop_front();
                    }
                    g.push_back(line);
                }
            }
        })
    });

    let mut buf = vec![0u8; 64 * 1024];
    match cfg.codec {
        VideoCodec::H264 | VideoCodec::Hevc => {
            let mut splitter = AnnexBSplitter::new(cfg.codec);
            loop {
                let n = tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = child.kill().await;
                        return Ok(());
                    }
                    res = stdout.read(&mut buf) => res.context("reading ddagrab stdout")?
                };
                if n == 0 {
                    for au in splitter.flush() {
                        let ts_us = start.elapsed().as_micros() as u32;
                        let key = au.keyframe || is_keyframe_annexb(cfg.codec, &au.data);
                        let _ = tx.try_send(EncodedFrame {
                            data: au.data,
                            keyframe: key,
                            timestamp_us: ts_us,
                        });
                    }
                    return Err(
                        ddagrab_exit_error(child, stderr_task, &stderr_tail, "ddagrab pipeline exited")
                            .await,
                    );
                }
                for au in splitter.push_bytes(&buf[..n]) {
                    let _ = keyframe_req.swap(false, Ordering::AcqRel);
                    let ts_us = start.elapsed().as_micros() as u32;
                    let key = au.keyframe || is_keyframe_annexb(cfg.codec, &au.data);
                    let _ = tx.try_send(EncodedFrame {
                        data: au.data,
                        keyframe: key,
                        timestamp_us: ts_us,
                    });
                }
            }
        }
        VideoCodec::Av1 => {
            let mut ivf = IvfReader::default();
            loop {
                let n = tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = child.kill().await;
                        return Ok(());
                    }
                    res = stdout.read(&mut buf) => res.context("reading ddagrab AV1 stdout")?
                };
                if n == 0 {
                    return Err(ddagrab_exit_error(
                        child,
                        stderr_task,
                        &stderr_tail,
                        "ddagrab AV1 pipeline exited",
                    )
                    .await);
                }
                for frame in ivf.push_bytes(&buf[..n])? {
                    let _ = keyframe_req.swap(false, Ordering::AcqRel);
                    let ts_us = start.elapsed().as_micros() as u32;
                    let _ = tx.try_send(EncodedFrame {
                        data: frame,
                        keyframe: false,
                        timestamp_us: ts_us,
                    });
                }
            }
        }
    }
}

async fn ddagrab_exit_error(
    mut child: tokio::process::Child,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    stderr_tail: &std::sync::Mutex<std::collections::VecDeque<String>>,
    label: &str,
) -> anyhow::Error {
    let status = child.wait().await;
    if let Some(task) = stderr_task {
        let _ = tokio::time::timeout(Duration::from_millis(200), task).await;
    }
    let detail = stderr_tail
        .lock()
        .map(|g| g.iter().cloned().collect::<Vec<_>>().join(" | "))
        .unwrap_or_default();
    match status {
        Ok(st) if !detail.is_empty() => anyhow!("{label} ({st}): {detail}"),
        Ok(st) => anyhow!("{label} ({st})"),
        Err(err) if !detail.is_empty() => anyhow!("{label} ({err}): {detail}"),
        Err(err) => anyhow!("{label} ({err})"),
    }
}

struct DxgiI420Frame {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl DxgiI420Frame {
    fn new(width: usize, height: usize) -> Self {
        let w = width & !1;
        let h = height & !1;
        Self {
            width: w,
            height: h,
            y: vec![16u8; w * h],
            u: vec![128u8; (w / 2) * (h / 2)],
            v: vec![128u8; (w / 2) * (h / 2)],
        }
    }

    fn convert_bgra(&mut self, src: &[u8], src_pitch: usize, src_w: usize, src_h: usize) {
        let dst_w = self.width;
        let dst_h = self.height;
        if src_w == 0 || src_h == 0 {
            return;
        }

        // Luma plane (BT.709 limited range)
        for dy in 0..dst_h {
            let sy = (dy * src_h) / dst_h;
            let row = &src[sy * src_pitch..];
            let y_row = &mut self.y[dy * dst_w..(dy + 1) * dst_w];
            for (dx, out_y) in y_row.iter_mut().enumerate() {
                let sx = (dx * src_w) / dst_w;
                let px = sx * 4;
                let b = row[px] as i32;
                let g = row[px + 1] as i32;
                let r = row[px + 2] as i32;
                *out_y = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16).clamp(16, 235) as u8;
            }
        }

        // 4:2:0 chroma planes (2x2 box average on source pixels mapped to each chroma cell)
        let cw = dst_w / 2;
        let ch = dst_h / 2;
        for cy in 0..ch {
            let sy = (((cy * 2) * src_h) / dst_h).min(src_h - 1);
            let row = &src[sy * src_pitch..];
            let u_row = &mut self.u[cy * cw..(cy + 1) * cw];
            let v_row = &mut self.v[cy * cw..(cy + 1) * cw];
            for cx in 0..cw {
                let sx = (((cx * 2) * src_w) / dst_w).min(src_w - 1);
                let px = sx * 4;
                let b = row[px] as i32;
                let g = row[px + 1] as i32;
                let r = row[px + 2] as i32;
                let u = (((-26 * r - 86 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
                let v = (((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
                u_row[cx] = u;
                v_row[cx] = v;
            }
        }
    }
}

impl YUVSource for DxgiI420Frame {
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

struct DxgiSession {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    src_w: usize,
    src_h: usize,
    display_index: u8,
}

impl DxgiSession {
    fn new(display_index: u8) -> Result<Self> {
        ensure_dpi_aware();
        unsafe {
            let (_, displays) = enumerate_windows_adapters_and_outputs();
            let chosen = displays
                .iter()
                .find(|d| d.info.index == display_index)
                .or_else(|| displays.first());

            let (device, ctx, out_idx) = if let Some(entry) = chosen {
                let mut device = None;
                let mut ctx = None;
                D3D11CreateDevice(
                    &entry.adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                )
                .context("D3D11CreateDevice on display adapter failed")?;
                (
                    device.ok_or_else(|| anyhow!("missing D3D11Device"))?,
                    ctx.ok_or_else(|| anyhow!("missing D3D11DeviceContext"))?,
                    entry.output_idx,
                )
            } else {
                let mut device = None;
                let mut ctx = None;
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                )
                .context("D3D11CreateDevice failed")?;
                (
                    device.ok_or_else(|| anyhow!("missing D3D11Device"))?,
                    ctx.ok_or_else(|| anyhow!("missing D3D11DeviceContext"))?,
                    0,
                )
            };

            let (dupl, staging, src_w, src_h) = Self::create_duplication(&device, out_idx)?;

            Ok(Self {
                device,
                ctx,
                dupl,
                staging,
                src_w,
                src_h,
                display_index,
            })
        }
    }

    unsafe fn create_duplication(
        device: &ID3D11Device,
        output_idx: u32,
    ) -> Result<(IDXGIOutputDuplication, ID3D11Texture2D, usize, usize)> {
        unsafe {
            let dxgi_dev: IDXGIDevice = device.cast().context("cast IDXGIDevice")?;
            let adapter = dxgi_dev.GetAdapter().context("IDXGIAdapter")?;
            let output = adapter
                .EnumOutputs(output_idx)
                .with_context(|| format!("EnumOutputs({output_idx})"))?;
            let output1: IDXGIOutput1 = output.cast().context("cast IDXGIOutput1")?;
            let dupl = output1
                .DuplicateOutput(device)
                .context("IDXGIOutput1::DuplicateOutput failed (Session 0 or secure desktop?)")?;
            let desc = dupl.GetDesc();
            let src_w = desc.ModeDesc.Width as usize;
            let src_h = desc.ModeDesc.Height as usize;

            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: src_w as u32,
                Height: src_h as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            device
                .CreateTexture2D(&tex_desc, None, Some(&mut staging))
                .context("CreateTexture2D staging")?;
            let staging = staging.ok_or_else(|| anyhow!("missing staging texture"))?;
            Ok((dupl, staging, src_w, src_h))
        }
    }

    fn recreate(&mut self) -> Result<()> {
        unsafe {
            let (_, displays) = enumerate_windows_adapters_and_outputs();
            let out_idx = displays
                .iter()
                .find(|d| d.info.index == self.display_index)
                .map_or(0, |d| d.output_idx);
            let (dupl, staging, src_w, src_h) = Self::create_duplication(&self.device, out_idx)?;
            self.dupl = dupl;
            self.staging = staging;
            self.src_w = src_w;
            self.src_h = src_h;
            Ok(())
        }
    }

    /// Captures the next changed desktop frame into `frame`, returning `Ok(true)` if a new
    /// frame was copied, `Ok(false)` on timeout (desktop unchanged).
    fn capture_into(&mut self, frame: &mut DxgiI420Frame, timeout_ms: u32) -> Result<bool> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res = None;
            match self.dupl.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    self.recreate()?;
                    return Ok(false);
                }
                Err(e) => return Err(anyhow!("AcquireNextFrame failed: {e}")),
            }

            let copy_res = (|| -> Result<()> {
                let desktop_res = res.ok_or_else(|| anyhow!("missing desktop resource"))?;
                let tex: ID3D11Texture2D = desktop_res.cast()?;
                self.ctx.CopyResource(&self.staging, &tex);

                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.ctx
                    .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .context("D3D11 Map staging texture")?;

                let pitch = mapped.RowPitch as usize;
                let total_bytes = pitch * self.src_h;
                let slice = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), total_bytes);
                frame.convert_bgra(slice, pitch, self.src_w, self.src_h);
                self.ctx.Unmap(&self.staging, 0);
                Ok(())
            })();

            let _ = self.dupl.ReleaseFrame();
            copy_res?;
            Ok(true)
        }
    }
}

async fn run_native_dxgi_openh264(
    cfg: VideoPipelineConfig,
    live_bitrate_kbps: Arc<AtomicU32>,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        run_dxgi_blocking(cfg, live_bitrate_kbps, tx, keyframe_req, start, cancel)
    })
    .await
    .map_err(|e| anyhow!("DXGI worker panicked: {e}"))?
}

fn run_dxgi_blocking(
    cfg: VideoPipelineConfig,
    live_bitrate_kbps: Arc<AtomicU32>,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let mut dxgi = DxgiSession::new(cfg.display_index)?;
    let initial_kbps = live_bitrate_kbps.load(Ordering::Acquire).max(500);
    let complexity = match cfg.prefer {
        QualityPreference::Latency => Complexity::Low,
        QualityPreference::Quality => Complexity::Medium,
    };
    let enc_cfg = EncoderConfig::new()
        .bitrate(BitRate::from_bps(initial_kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(cfg.fps as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::ScreenContentRealTime)
        .complexity(complexity)
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        .intra_frame_period(IntraFramePeriod::from_num_frames(cfg.fps as u32 * 2))
        .skip_frames(false)
        .vui(VuiConfig::bt709());

    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), enc_cfg)
        .map_err(|e| anyhow!("failed to create OpenH264 encoder: {e}"))?;

    let mut applied_bitrate = initial_kbps;
    let mut have_encoded_first = false;
    let mut frame = DxgiI420Frame::new(cfg.width as usize, cfg.height as usize);
    let frame_period = Duration::from_micros(1_000_000 / cfg.fps.max(1) as u64);
    let timeout_ms = frame_period.as_millis().clamp(4, 33) as u32;
    let mut next_tick = Instant::now();
    let mut have_first_frame = false;
    let mut initial_timeouts = 0u32;

    while !cancel.is_cancelled() {
        match dxgi.capture_into(&mut frame, timeout_ms) {
            Ok(true) => {
                have_first_frame = true;
            }
            Ok(false) => {
                if !have_first_frame {
                    initial_timeouts += 1;
                    if initial_timeouts < 5 {
                        continue;
                    }
                    have_first_frame = true;
                }
            }
            Err(err) => {
                tracing::warn!("DXGI capture error ({err:#}); retrying");
                std::thread::sleep(Duration::from_millis(50));
                let _ = dxgi.recreate();
                continue;
            }
        }

        if have_encoded_first {
            let desired = live_bitrate_kbps.load(Ordering::Acquire).max(500);
            if desired != applied_bitrate {
                apply_openh264_bitrate(&mut encoder, desired);
                applied_bitrate = desired;
            }
        }

        if keyframe_req.swap(false, Ordering::AcqRel) {
            encoder.force_intra_frame();
        }

        let bs = encoder
            .encode(&frame)
            .map_err(|e| anyhow!("OpenH264 encode failed: {e}"))?;
        have_encoded_first = true;
        let data = bs.to_vec();
        if !data.is_empty() {
            let keyframe = matches!(bs.frame_type(), FrameType::IDR | FrameType::I)
                || is_keyframe_annexb(VideoCodec::H264, &data);
            let ts_us = start.elapsed().as_micros() as u32;
            let _ = tx.try_send(EncodedFrame {
                data,
                keyframe,
                timestamp_us: ts_us,
            });
        }

        next_tick += frame_period;
        let now = Instant::now();
        if next_tick > now {
            let remaining = next_tick - now;
            if remaining > Duration::from_millis(2) {
                std::thread::sleep(remaining - Duration::from_millis(1));
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
