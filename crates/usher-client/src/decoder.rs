use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use openh264::OpenH264API;
use openh264::decoder::{Decoder as OpenH264Inner, DecoderConfig};
use openh264::formats::YUVSource;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use usher_proto::handshake::VideoCodec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecoderKind {
    Auto,
    Openh264,
    Ffmpeg,
}

pub fn find_working_ffmpeg() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("USHER_FFMPEG") {
        candidates.push(PathBuf::from(p));
    }
    candidates.push(PathBuf::from("ffmpeg"));

    #[cfg(target_os = "windows")]
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
        if let Ok(out) = Command::new(&cand)
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

pub fn probe_client_codecs(kind: DecoderKind) -> Vec<VideoCodec> {
    if kind == DecoderKind::Openh264 {
        return vec![VideoCodec::H264];
    }
    let Some(ffmpeg) = find_working_ffmpeg() else {
        return vec![VideoCodec::H264];
    };

    let hw_ok = Command::new(&ffmpeg)
        .args(["-hide_banner", "-hwaccels"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            #[cfg(target_os = "windows")]
            {
                text.contains("d3d11va") || text.contains("cuda") || text.contains("dxva2")
            }
            #[cfg(target_os = "macos")]
            {
                text.contains("videotoolbox")
            }
            #[cfg(target_os = "linux")]
            {
                text.contains("vaapi") || text.contains("cuda") || text.contains("vulkan")
            }
            #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
            {
                let _ = text;
                false
            }
        });

    let decs_text = Command::new(&ffmpeg)
        .args(["-hide_banner", "-decoders"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();

    let mut out = Vec::new();
    if hw_ok || kind == DecoderKind::Ffmpeg {
        if decs_text.contains(" hevc") {
            out.push(VideoCodec::Hevc);
        }
        if decs_text.contains(" av1") || decs_text.contains("libdav1d") {
            out.push(VideoCodec::Av1);
        }
    }
    if !out.contains(&VideoCodec::H264) {
        out.push(VideoCodec::H264);
    }
    out
}

#[derive(Debug, Clone)]
pub struct Yuv420Frame {
    pub width: u32,
    pub height: u32,
    pub y_stride: u32,
    pub u_stride: u32,
    pub v_stride: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

pub type FrameSink = Arc<dyn Fn(Yuv420Frame) + Send + Sync>;

pub trait VideoDecoder: Send {
    fn decode(&mut self, access_unit: &[u8]) -> Result<()>;
}

pub fn create_decoder(
    codec: VideoCodec,
    kind: DecoderKind,
    width: u16,
    height: u16,
    use_hwaccel: bool,
    on_frame: FrameSink,
) -> Result<Box<dyn VideoDecoder>> {
    match (codec, kind) {
        (VideoCodec::H264, DecoderKind::Auto | DecoderKind::Openh264) => {
            Ok(Box::new(OpenH264Decoder::new(on_frame)?))
        }
        (_, DecoderKind::Openh264) => {
            bail!("OpenH264 decoder only supports H.264 streams (got {codec})")
        }
        (codec, DecoderKind::Ffmpeg) | (codec, DecoderKind::Auto) => Ok(Box::new(
            FfmpegPipeDecoder::new_with_hwaccel(codec, width, height, use_hwaccel, on_frame)?,
        )),
    }
}

pub struct OpenH264Decoder {
    inner: OpenH264Inner,
    on_frame: FrameSink,
}

impl OpenH264Decoder {
    pub fn new(on_frame: FrameSink) -> Result<Self> {
        let inner = OpenH264Inner::with_api_config(
            OpenH264API::from_source(),
            DecoderConfig::new(),
        )
        .map_err(|e| anyhow!("failed to initialize OpenH264 decoder: {e}"))?;
        Ok(Self { inner, on_frame })
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<()> {
        for nal in openh264::nal_units(access_unit) {
            match self.inner.decode(nal) {
                Ok(Some(yuv)) => {
                    let (w, h) = yuv.dimensions();
                    let (sy, su, sv) = yuv.strides();
                    (self.on_frame)(Yuv420Frame {
                        width: w as u32,
                        height: h as u32,
                        y_stride: sy as u32,
                        u_stride: su as u32,
                        v_stride: sv as u32,
                        y: yuv.y().to_vec(),
                        u: yuv.u().to_vec(),
                        v: yuv.v().to_vec(),
                    });
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!("OpenH264 NAL decode warning: {err}");
                }
            }
        }
        Ok(())
    }
}

pub struct FfmpegPipeDecoder {
    codec: VideoCodec,
    width: u32,
    height: u32,
    child: Child,
    stdin: Option<ChildStdin>,
    frame_idx: u64,
    ivf_header_sent: bool,
    reader_dead: Arc<AtomicBool>,
    frames_decoded: Arc<AtomicU64>,
    reader_handle: Option<std::thread::JoinHandle<()>>,
}

impl FfmpegPipeDecoder {
    #[allow(dead_code)]
    pub fn new(codec: VideoCodec, width: u16, height: u16, on_frame: FrameSink) -> Result<Self> {
        Self::new_with_hwaccel(codec, width, height, true, on_frame)
    }

    pub fn new_with_hwaccel(
        codec: VideoCodec,
        width: u16,
        height: u16,
        use_hwaccel: bool,
        on_frame: FrameSink,
    ) -> Result<Self> {
        let in_fmt = match codec {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
            VideoCodec::Av1 => "ivf",
        };
        let ffmpeg_bin = find_working_ffmpeg().unwrap_or_else(|| PathBuf::from("ffmpeg"));
        // Omit `-fflags nobuffer` here: on raw Annex-B streams it causes avformat_find_stream_info
        // to discard Frame 0 (IDR + parameter sets), stalling or corrupting decoding until the next IDR.
        let mut args = vec![
            "-hide_banner".to_string(),
            "-loglevel".to_string(),
            "error".to_string(),
            "-probesize".to_string(),
            "32".to_string(),
            "-analyzeduration".to_string(),
            "0".to_string(),
            "-flags".to_string(),
            "low_delay".to_string(),
        ];

        if use_hwaccel {
            #[cfg(target_os = "windows")]
            args.extend(["-hwaccel".to_string(), "d3d11va".to_string()]);
            #[cfg(target_os = "macos")]
            args.extend(["-hwaccel".to_string(), "videotoolbox".to_string()]);
            #[cfg(target_os = "linux")]
            args.extend(["-hwaccel".to_string(), "auto".to_string()]);
            #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
            args.extend([
                "-thread_type".to_string(),
                "slice".to_string(),
                "-threads".to_string(),
                "4".to_string(),
            ]);
        } else {
            args.extend([
                "-thread_type".to_string(),
                "slice".to_string(),
                "-threads".to_string(),
                "4".to_string(),
            ]);
        }

        args.extend([
            "-f".to_string(),
            in_fmt.to_string(),
            "-i".to_string(),
            "pipe:0".to_string(),
            "-vf".to_string(),
            format!("scale={width}:{height}"),
            "-f".to_string(),
            "rawvideo".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-flush_packets".to_string(),
            "1".to_string(),
            "pipe:1".to_string(),
        ]);

        let mut child = Command::new(&ffmpeg_bin)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn ffmpeg decoder (ensure ffmpeg is in PATH)")?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("ffmpeg missing stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ffmpeg missing stdout"))?;

        let w = width as u32;
        let h = height as u32;
        let reader_dead = Arc::new(AtomicBool::new(false));
        let frames_decoded = Arc::new(AtomicU64::new(0));
        let dead_rx = reader_dead.clone();
        let count_rx = frames_decoded.clone();

        let reader_handle = std::thread::Builder::new()
            .name("usher-ffmpeg-rx".into())
            .spawn(move || {
                let y_len = (w * h) as usize;
                let uv_len = ((w / 2) * (h / 2)) as usize;
                loop {
                    let mut y = vec![0u8; y_len];
                    let mut u = vec![0u8; uv_len];
                    let mut v = vec![0u8; uv_len];
                    if stdout.read_exact(&mut y).is_err()
                        || stdout.read_exact(&mut u).is_err()
                        || stdout.read_exact(&mut v).is_err()
                    {
                        dead_rx.store(true, Ordering::Release);
                        break;
                    }
                    count_rx.fetch_add(1, Ordering::Relaxed);
                    (on_frame)(Yuv420Frame {
                        width: w,
                        height: h,
                        y_stride: w,
                        u_stride: w / 2,
                        v_stride: w / 2,
                        y,
                        u,
                        v,
                    });
                }
            })
            .context("spawning ffmpeg stdout reader thread")?;

        Ok(Self {
            codec,
            width: w,
            height: h,
            child,
            stdin: Some(stdin),
            frame_idx: 0,
            ivf_header_sent: false,
            reader_dead,
            frames_decoded,
            reader_handle: Some(reader_handle),
        })
    }

    #[allow(dead_code)]
    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded.load(Ordering::Relaxed)
    }
}

impl VideoDecoder for FfmpegPipeDecoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<()> {
        if self.reader_dead.load(Ordering::Acquire) {
            bail!("ffmpeg decoder process exited");
        }
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("ffmpeg stdin closed"))?;

        if self.codec == VideoCodec::Av1 {
            if !self.ivf_header_sent {
                let mut hdr = [0u8; 32];
                hdr[0..4].copy_from_slice(b"DKIF");
                hdr[6..8].copy_from_slice(&32u16.to_le_bytes());
                hdr[8..12].copy_from_slice(b"AV01");
                hdr[12..14].copy_from_slice(&(self.width as u16).to_le_bytes());
                hdr[14..16].copy_from_slice(&(self.height as u16).to_le_bytes());
                hdr[16..20].copy_from_slice(&60u32.to_le_bytes());
                hdr[20..24].copy_from_slice(&1u32.to_le_bytes());
                stdin.write_all(&hdr)?;
                self.ivf_header_sent = true;
            }
            let mut fhdr = [0u8; 12];
            fhdr[0..4].copy_from_slice(&(access_unit.len() as u32).to_le_bytes());
            fhdr[4..12].copy_from_slice(&self.frame_idx.to_le_bytes());
            self.frame_idx += 1;
            stdin.write_all(&fhdr)?;
        }

        stdin.write_all(access_unit)?;
        stdin.flush()?;
        Ok(())
    }
}

impl Drop for FfmpegPipeDecoder {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.reader_handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use usher_proto::annexb::AnnexBSplitter;

    fn run_pipe_stream_test(
        codec: VideoCodec,
        width: u16,
        height: u16,
        use_hwaccel: bool,
        enc_args: &[&str],
    ) -> bool {
        let Some(ffmpeg) = find_working_ffmpeg() else {
            return false;
        };
        let src = format!("testsrc2=size={width}x{height}:rate=30");
        let fmt = match codec {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
            VideoCodec::Av1 => "ivf",
        };
        let mut cmd = Command::new(&ffmpeg);
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &src,
            "-frames:v",
            "8",
        ]);
        cmd.args(enc_args);
        cmd.args(["-f", fmt, "pipe:1"]);

        let Ok(enc_out) = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        else {
            return false;
        };
        if !enc_out.status.success() || enc_out.stdout.is_empty() {
            return false;
        }

        let mut splitter = AnnexBSplitter::new(codec);
        let mut aus = splitter.push_bytes(&enc_out.stdout);
        aus.extend(splitter.flush());
        assert!(aus.len() >= 6, "expected >= 6 Annex-B AUs, got {}", aus.len());

        let count = Arc::new(AtomicU64::new(0));
        let count_cb = count.clone();
        let sink: FrameSink = Arc::new(move |frame| {
            assert_eq!((frame.width, frame.height), (width as u32, height as u32));
            count_cb.fetch_add(1, Ordering::Release);
        });

        let mut dec = FfmpegPipeDecoder::new_with_hwaccel(codec, width, height, use_hwaccel, sink)
            .expect("spawn FfmpegPipeDecoder");

        for au in &aus {
            dec.decode(&au.data).expect("write AU while stdin stays open");
        }

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && count.load(Ordering::Acquire) < 5 {
            std::thread::sleep(Duration::from_millis(20));
        }

        let got = count.load(Ordering::Acquire);
        assert!(
            got >= 5 && dec.frames_decoded() >= 5,
            "expected >= 5 decoded {codec} (hw={use_hwaccel}) frames without closing stdin, got {got} (internal={})",
            dec.frames_decoded()
        );
        true
    }

    #[test]
    fn ffmpeg_pipe_decoder_streams_h264_without_deadlock() {
        let _ = run_pipe_stream_test(
            VideoCodec::H264,
            160,
            90,
            false,
            &[
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "libx264",
                "-tune",
                "zerolatency",
                "-bf",
                "0",
                "-g",
                "4",
            ],
        );
    }

    #[test]
    fn ffmpeg_pipe_decoder_streams_hevc_without_deadlock() {
        let nvenc_args = [
            "-pix_fmt",
            "nv12",
            "-c:v",
            "hevc_nvenc",
            "-preset",
            "p1",
            "-tune",
            "ull",
            "-zerolatency",
            "1",
            "-bf",
            "0",
            "-g",
            "4",
            "-forced-idr",
            "1",
            "-aud",
            "1",
        ];
        if run_pipe_stream_test(VideoCodec::Hevc, 320, 180, false, &nvenc_args) {
            if cfg!(target_os = "windows") {
                assert!(run_pipe_stream_test(VideoCodec::Hevc, 320, 180, true, &nvenc_args));
            }
            return;
        }
        let _ = run_pipe_stream_test(
            VideoCodec::Hevc,
            160,
            90,
            false,
            &[
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "libx265",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-x265-params",
                "keyint=4:bframes=0:aud=1",
            ],
        );
    }
}
