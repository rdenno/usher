use super::{EncodedFrame, VideoPipelineConfig};
use anyhow::{Context, Result, bail};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::annexb::{AnnexBSplitter, is_keyframe_annexb};
use usher_proto::handshake::VideoCodec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwPipelineKind {
    DrmVaapi,
    X11Nvenc,
    X11Vaapi,
    PipeWireVaapi,
    PipeWireNvenc,
}

pub fn command_exists(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn ffmpeg_supports_encoder(enc: &str) -> bool {
    let Ok(out) = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains(enc)
}

pub async fn run_hw_pipeline(
    kind: HwPipelineKind,
    cfg: VideoPipelineConfig,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let mut cmd = build_command(kind, &cfg)?;
    if let Some(rt) = &cfg.wayland_runtime_dir {
        cmd.env("XDG_RUNTIME_DIR", rt);
    }
    if let Some(wd) = &cfg.wayland_display {
        cmd.env("WAYLAND_DISPLAY", wd);
    }

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn hardware pipeline {kind:?}"))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("pipeline missing stdout"))?;

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!("[hw-video] {line}");
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
                    res = stdout.read(&mut buf) => res.context("reading pipeline stdout")?
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
                    bail!("hardware video pipeline exited ({status})");
                }
                let aus = splitter.push_bytes(&buf[..n]);
                for au in aus {
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
                    res = stdout.read(&mut buf) => res.context("reading AV1 IVF stdout")?
                };
                if n == 0 {
                    let status = child.wait().await?;
                    bail!("AV1 hardware pipeline exited ({status})");
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

fn build_command(kind: HwPipelineKind, cfg: &VideoPipelineConfig) -> Result<Command> {
    let br = format!("{}k", cfg.bitrate_kbps);
    let bufsize = format!("{}k", (cfg.bitrate_kbps / cfg.fps.max(1) as u32).max(200));
    let gop = (cfg.fps as u32 * 2).max(30).to_string();
    let fps_str = cfg.fps.to_string();
    let out_fmt = match cfg.codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "ivf",
    };

    match kind {
        HwPipelineKind::DrmVaapi => {
            let render = cfg
                .render_node
                .as_deref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/dev/dri/renderD128".to_string());
            let enc = match cfg.codec {
                VideoCodec::H264 => "h264_vaapi",
                VideoCodec::Hevc => "hevc_vaapi",
                VideoCodec::Av1 => "av1_vaapi",
            };
            let mut c = Command::new("ffmpeg");
            c.args([
                "-hide_banner",
                "-loglevel",
                "warning",
                "-fflags",
                "nobuffer",
                "-device",
                &render,
                "-f",
                "kmsgrab",
                "-framerate",
                &fps_str,
                "-i",
                "-",
                "-vf",
                &format!(
                    "hwmap=derive_device=vaapi,scale_vaapi=w={}:h={}:format=nv12",
                    cfg.width, cfg.height
                ),
                "-c:v",
                enc,
                "-bf",
                "0",
                "-rc_mode",
                "CBR",
                "-b:v",
                &br,
                "-maxrate",
                &br,
                "-bufsize",
                &bufsize,
                "-g",
                &gop,
                "-f",
                out_fmt,
                "pipe:1",
            ]);
            Ok(c)
        }
        HwPipelineKind::X11Vaapi => {
            let render = cfg
                .render_node
                .as_deref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/dev/dri/renderD128".to_string());
            let enc = match cfg.codec {
                VideoCodec::H264 => "h264_vaapi",
                VideoCodec::Hevc => "hevc_vaapi",
                VideoCodec::Av1 => "av1_vaapi",
            };
            let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
            let mut c = Command::new("ffmpeg");
            c.args([
                "-hide_banner",
                "-loglevel",
                "warning",
                "-fflags",
                "nobuffer",
                "-vaapi_device",
                &render,
                "-f",
                "x11grab",
                "-framerate",
                &fps_str,
                "-video_size",
                &format!("{}x{}", cfg.width, cfg.height),
                "-i",
                &display,
                "-vf",
                "format=nv12,hwupload",
                "-c:v",
                enc,
                "-bf",
                "0",
                "-rc_mode",
                "CBR",
                "-b:v",
                &br,
                "-maxrate",
                &br,
                "-bufsize",
                &bufsize,
                "-g",
                &gop,
                "-f",
                out_fmt,
                "pipe:1",
            ]);
            Ok(c)
        }
        HwPipelineKind::X11Nvenc => {
            let enc = match cfg.codec {
                VideoCodec::H264 => "h264_nvenc",
                VideoCodec::Hevc => "hevc_nvenc",
                VideoCodec::Av1 => "av1_nvenc",
            };
            let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
            let mut c = Command::new("ffmpeg");
            c.args([
                "-hide_banner",
                "-loglevel",
                "warning",
                "-fflags",
                "nobuffer",
                "-f",
                "x11grab",
                "-framerate",
                &fps_str,
                "-video_size",
                &format!("{}x{}", cfg.width, cfg.height),
                "-i",
                &display,
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                enc,
                "-preset",
                "p1",
                "-tune",
                "ull",
                "-zerolatency",
                "1",
                "-bf",
                "0",
                "-intra-refresh",
                "1",
                "-rc",
                "cbr",
                "-b:v",
                &br,
                "-maxrate",
                &br,
                "-bufsize",
                &bufsize,
                "-g",
                &gop,
                "-f",
                out_fmt,
                "pipe:1",
            ]);
            Ok(c)
        }
        HwPipelineKind::PipeWireVaapi | HwPipelineKind::PipeWireNvenc => {
            let enc_gst = match (kind, cfg.codec) {
                (HwPipelineKind::PipeWireVaapi, VideoCodec::H264) => {
                    format!("vaapih264enc rate-control=cbr bitrate={} keyframe-period={gop}", cfg.bitrate_kbps)
                }
                (HwPipelineKind::PipeWireVaapi, VideoCodec::Hevc) => {
                    format!("vaapih265enc rate-control=cbr bitrate={} keyframe-period={gop}", cfg.bitrate_kbps)
                }
                (HwPipelineKind::PipeWireVaapi, VideoCodec::Av1) => {
                    format!("vaapiav1enc bitrate={}", cfg.bitrate_kbps)
                }
                (HwPipelineKind::PipeWireNvenc, VideoCodec::H264) => {
                    format!("nvh264enc preset=low-latency-hq zerolatency=true rc-mode=cbr bitrate={} gop-size={gop}", cfg.bitrate_kbps)
                }
                (HwPipelineKind::PipeWireNvenc, VideoCodec::Hevc) => {
                    format!("nvh265enc preset=low-latency-hq zerolatency=true rc-mode=cbr bitrate={} gop-size={gop}", cfg.bitrate_kbps)
                }
                (HwPipelineKind::PipeWireNvenc, VideoCodec::Av1) => {
                    format!("nvav1enc preset=low-latency-hq bitrate={}", cfg.bitrate_kbps)
                }
                _ => unreachable!(),
            };
            let caps = format!(
                "video/x-raw,width={},height={},framerate={}/1",
                cfg.width, cfg.height, cfg.fps
            );
            let sink_elem = match cfg.codec {
                VideoCodec::H264 => "h264parse config-interval=1 ! video/x-h264,stream-format=byte-stream",
                VideoCodec::Hevc => "h265parse config-interval=1 ! video/x-h265,stream-format=byte-stream",
                VideoCodec::Av1 => "av1parse ! ivfenc",
            };
            let pipeline = format!(
                "pipewiresrc do-timestamp=true ! videoconvert ! {caps} ! {enc_gst} ! {sink_elem} ! fdsink fd=1"
            );
            // Launch via sh so GStreamer pipeline tokens stay intact cleanly.
            let mut sh = Command::new("sh");
            sh.args(["-c", &format!("exec gst-launch-1.0 -q {pipeline}")]);
            Ok(sh)
        }
    }
}

#[derive(Default)]
struct IvfReader {
    header_done: bool,
    buf: Vec<u8>,
}

impl IvfReader {
    fn push_bytes(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        if !self.header_done {
            if self.buf.len() < 32 {
                return Ok(out);
            }
            if &self.buf[0..4] != b"DKIF" {
                bail!("invalid IVF magic from AV1 encoder");
            }
            self.buf.drain(..32);
            self.header_done = true;
        }

        while self.buf.len() >= 12 {
            let frame_len = u32::from_le_bytes(self.buf[0..4].try_into().expect("4")) as usize;
            if self.buf.len() < 12 + frame_len {
                break;
            }
            let frame = self.buf[12..12 + frame_len].to_vec();
            self.buf.drain(..12 + frame_len);
            out.push(frame);
        }
        Ok(out)
    }
}
