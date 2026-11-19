use super::ivf::IvfReader;
use super::{CaptureSource, EncodedFrame, EncoderKind, VideoPipelineConfig};
use anyhow::{Context, Result, anyhow, bail};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::annexb::{AnnexBSplitter, is_keyframe_annexb};
use usher_proto::handshake::VideoCodec;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice,
    IDXGIOutput1, IDXGIOutputDuplication,
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

fn ffmpeg_has_ddagrab_and_encoder(ffmpeg: &Path, encoder: &str) -> bool {
    let Ok(filters) = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-filters"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let f_text = String::from_utf8_lossy(&filters.stdout);
    if !f_text.contains("ddagrab") || !f_text.contains("scale_d3d11") {
        return false;
    }

    let Ok(encs) = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let e_text = String::from_utf8_lossy(&encs.stdout);
    e_text.contains(encoder)
}

pub fn select_windows_backend(cfg: &VideoPipelineConfig) -> Option<WinVideoBackend> {
    if !matches!(cfg.capture, CaptureSource::Auto | CaptureSource::Dxgi) {
        return None;
    }
    if cfg.encoder == EncoderKind::Openh264 {
        return Some(WinVideoBackend::NativeDxgiOpenH264);
    }

    if let Some(ffmpeg) = find_working_ffmpeg() {
        let candidates: &[(&'static str, WinHwVendor)] = match (cfg.encoder, cfg.codec) {
            (EncoderKind::Nvenc, VideoCodec::H264) => &[("h264_nvenc", WinHwVendor::Nvenc)],
            (EncoderKind::Nvenc, VideoCodec::Hevc) => &[("hevc_nvenc", WinHwVendor::Nvenc)],
            (EncoderKind::Nvenc, VideoCodec::Av1) => &[("av1_nvenc", WinHwVendor::Nvenc)],
            (EncoderKind::Amf, VideoCodec::H264) => &[("h264_amf", WinHwVendor::Amf)],
            (EncoderKind::Amf, VideoCodec::Hevc) => &[("hevc_amf", WinHwVendor::Amf)],
            (EncoderKind::Amf, VideoCodec::Av1) => &[("av1_amf", WinHwVendor::Amf)],
            (EncoderKind::Qsv, VideoCodec::H264) => &[("h264_qsv", WinHwVendor::Qsv)],
            (EncoderKind::Qsv, VideoCodec::Hevc) => &[("hevc_qsv", WinHwVendor::Qsv)],
            (EncoderKind::Qsv, VideoCodec::Av1) => &[("av1_qsv", WinHwVendor::Qsv)],
            (EncoderKind::Auto, VideoCodec::H264) => &[
                ("h264_nvenc", WinHwVendor::Nvenc),
                ("h264_amf", WinHwVendor::Amf),
                ("h264_qsv", WinHwVendor::Qsv),
            ],
            (EncoderKind::Auto, VideoCodec::Hevc) => &[
                ("hevc_nvenc", WinHwVendor::Nvenc),
                ("hevc_amf", WinHwVendor::Amf),
                ("hevc_qsv", WinHwVendor::Qsv),
            ],
            (EncoderKind::Auto, VideoCodec::Av1) => &[
                ("av1_nvenc", WinHwVendor::Nvenc),
                ("av1_amf", WinHwVendor::Amf),
                ("av1_qsv", WinHwVendor::Qsv),
            ],
            _ => &[],
        };

        for &(enc_name, vendor) in candidates {
            if ffmpeg_has_ddagrab_and_encoder(&ffmpeg, enc_name) {
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
                "starting Windows zero-copy ddagrab hardware video pipeline"
            );
            run_ddagrab_hw(ffmpeg_path, encoder_name, vendor, cfg, tx, keyframe_req, start, cancel)
                .await
        }
        WinVideoBackend::NativeDxgiOpenH264 => {
            tracing::info!("starting built-in DXGI Desktop Duplication + OpenH264 capture");
            run_native_dxgi_openh264(cfg, tx, keyframe_req, start, cancel).await
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
    let br = format!("{}k", cfg.bitrate_kbps);
    let bufsize = format!("{}k", (cfg.bitrate_kbps / cfg.fps.max(1) as u32).max(200));
    let gop = (cfg.fps as u32 * 2).max(30).to_string();
    let out_fmt = match cfg.codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "ivf",
    };

    let filter = format!(
        "ddagrab=output_idx=0:framerate={}:draw_mouse=1:dup_frames=1,scale_d3d11=width={}:height={}:format=nv12",
        cfg.fps, cfg.width, cfg.height
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
            args.extend([
                "-preset".into(),
                "p1".into(),
                "-tune".into(),
                "ull".into(),
                "-zerolatency".into(),
                "1".into(),
                "-bf".into(),
                "0".into(),
                "-intra-refresh".into(),
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
        }
        WinHwVendor::Amf => {
            args.extend([
                "-usage".into(),
                "ultralowlatency".into(),
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
            args.extend([
                "-preset".into(),
                "veryfast".into(),
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

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!("[ddagrab] {line}");
            }
        });
    }

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
                    let status = child.wait().await?;
                    bail!("ddagrab pipeline exited ({status})");
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
                    let status = child.wait().await?;
                    bail!("ddagrab AV1 pipeline exited ({status})");
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
}

impl DxgiSession {
    fn new() -> Result<Self> {
        ensure_dpi_aware();
        unsafe {
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

            let device = device.ok_or_else(|| anyhow!("missing D3D11Device"))?;
            let ctx = ctx.ok_or_else(|| anyhow!("missing D3D11DeviceContext"))?;
            let (dupl, staging, src_w, src_h) = Self::create_duplication(&device)?;

            Ok(Self {
                device,
                ctx,
                dupl,
                staging,
                src_w,
                src_h,
            })
        }
    }

    unsafe fn create_duplication(
        device: &ID3D11Device,
    ) -> Result<(IDXGIOutputDuplication, ID3D11Texture2D, usize, usize)> {
        unsafe {
            let dxgi_dev: IDXGIDevice = device.cast().context("cast IDXGIDevice")?;
            let adapter = dxgi_dev.GetAdapter().context("IDXGIAdapter")?;
            let output = adapter.EnumOutputs(0).context("EnumOutputs(0)")?;
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
            let (dupl, staging, src_w, src_h) = Self::create_duplication(&self.device)?;
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
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    tokio::task::spawn_blocking(move || run_dxgi_blocking(cfg, tx, keyframe_req, start, cancel))
        .await
        .map_err(|e| anyhow!("DXGI worker panicked: {e}"))?
}

fn run_dxgi_blocking(
    cfg: VideoPipelineConfig,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let mut dxgi = DxgiSession::new()?;
    let enc_cfg = EncoderConfig::new()
        .bitrate(BitRate::from_bps(cfg.bitrate_kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(cfg.fps as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::ScreenContentRealTime)
        .complexity(Complexity::Low)
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        .intra_frame_period(IntraFramePeriod::from_num_frames(cfg.fps as u32 * 2))
        .skip_frames(false)
        .vui(VuiConfig::bt709());

    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), enc_cfg)
        .map_err(|e| anyhow!("failed to create OpenH264 encoder: {e}"))?;

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

        if keyframe_req.swap(false, Ordering::AcqRel) {
            encoder.force_intra_frame();
        }

        let bs = encoder
            .encode(&frame)
            .map_err(|e| anyhow!("OpenH264 encode failed: {e}"))?;
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
