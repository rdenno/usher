use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use openh264::OpenH264API;
use openh264::decoder::{Decoder as OpenH264Inner, DecoderConfig};
use openh264::formats::YUVSource;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
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

pub trait VideoDecoder: Send {
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<Yuv420Frame>>;
}

pub fn create_decoder(
    codec: VideoCodec,
    kind: DecoderKind,
    width: u16,
    height: u16,
) -> Result<Box<dyn VideoDecoder>> {
    match (codec, kind) {
        (VideoCodec::H264, DecoderKind::Auto | DecoderKind::Openh264) => {
            Ok(Box::new(OpenH264Decoder::new()?))
        }
        (_, DecoderKind::Openh264) => {
            bail!("OpenH264 decoder only supports H.264 streams (got {codec})")
        }
        (codec, DecoderKind::Ffmpeg) | (codec, DecoderKind::Auto) => {
            Ok(Box::new(FfmpegPipeDecoder::new(codec, width, height)?))
        }
    }
}

pub struct OpenH264Decoder {
    inner: OpenH264Inner,
}

impl OpenH264Decoder {
    pub fn new() -> Result<Self> {
        let inner = OpenH264Inner::with_api_config(
            OpenH264API::from_source(),
            DecoderConfig::new(),
        )
        .map_err(|e| anyhow!("failed to initialize OpenH264 decoder: {e}"))?;
        Ok(Self { inner })
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<Yuv420Frame>> {
        let mut latest = None;
        for nal in openh264::nal_units(access_unit) {
            match self.inner.decode(nal) {
                Ok(Some(yuv)) => {
                    let (w, h) = yuv.dimensions();
                    let (sy, su, sv) = yuv.strides();
                    latest = Some(Yuv420Frame {
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
        Ok(latest)
    }
}

pub struct FfmpegPipeDecoder {
    codec: VideoCodec,
    width: u32,
    height: u32,
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    frame_idx: u64,
    ivf_header_sent: bool,
}

impl FfmpegPipeDecoder {
    pub fn new(codec: VideoCodec, width: u16, height: u16) -> Result<Self> {
        let in_fmt = match codec {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
            VideoCodec::Av1 => "ivf",
        };
        let ffmpeg_bin = find_working_ffmpeg().unwrap_or_else(|| PathBuf::from("ffmpeg"));
        let mut child = Command::new(&ffmpeg_bin)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-hwaccel",
                "auto",
                "-f",
                in_fmt,
                "-i",
                "pipe:0",
                "-vf",
                &format!("scale={width}:{height}"),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn ffmpeg decoder (ensure ffmpeg is in PATH)")?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("ffmpeg missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ffmpeg missing stdout"))?;

        Ok(Self {
            codec,
            width: width as u32,
            height: height as u32,
            child,
            stdin,
            stdout,
            frame_idx: 0,
            ivf_header_sent: false,
        })
    }
}

impl VideoDecoder for FfmpegPipeDecoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<Yuv420Frame>> {
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
                self.stdin.write_all(&hdr)?;
                self.ivf_header_sent = true;
            }
            let mut fhdr = [0u8; 12];
            fhdr[0..4].copy_from_slice(&(access_unit.len() as u32).to_le_bytes());
            fhdr[4..12].copy_from_slice(&self.frame_idx.to_le_bytes());
            self.frame_idx += 1;
            self.stdin.write_all(&fhdr)?;
        }

        self.stdin.write_all(access_unit)?;
        self.stdin.flush()?;

        let y_len = (self.width * self.height) as usize;
        let uv_len = ((self.width / 2) * (self.height / 2)) as usize;
        let mut y = vec![0u8; y_len];
        let mut u = vec![0u8; uv_len];
        let mut v = vec![0u8; uv_len];

        self.stdout.read_exact(&mut y)?;
        self.stdout.read_exact(&mut u)?;
        self.stdout.read_exact(&mut v)?;

        Ok(Some(Yuv420Frame {
            width: self.width,
            height: self.height,
            y_stride: self.width,
            u_stride: self.width / 2,
            v_stride: self.width / 2,
            y,
            u,
            v,
        }))
    }
}

impl Drop for FfmpegPipeDecoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
