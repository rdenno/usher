use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use usher_proto::handshake::{DisplayInfo, QualityPreference, VideoCodec};

pub const MAX_IPC_FRAME_LEN: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerHelloInfo {
    pub displays: Vec<DisplayInfo>,
    pub host_codecs: Vec<VideoCodec>,
    pub worker_session: u32,
    pub is_system: bool,
    pub desktop_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerVideoConfig {
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_kbps: u32,
    pub codec: VideoCodec,
    pub capture: String,
    pub encoder: String,
    pub prefer: QualityPreference,
    pub display_index: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerMsg {
    HelloInfo(WorkerHelloInfo),
    VideoFrame {
        keyframe: bool,
        timestamp_us: u32,
        data: Vec<u8>,
    },
    AudioPacket(Vec<u8>),
    LogLine(String),
    CodecChanged {
        codec: VideoCodec,
        in_place_bitrate: bool,
    },
    ClipboardText(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCmd {
    StartVideo(WorkerVideoConfig),
    UpdateBitrate(u32),
    RequestKeyframe,
    Input(Vec<u8>),
    AudioEnable(bool),
    Shutdown,
    SetClipboardText(String),
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, tag: u8, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_IPC_FRAME_LEN {
        bail!("IPC payload {}B exceeds {MAX_IPC_FRAME_LEN}B", payload.len());
    }
    let mut hdr = [0u8; 5];
    hdr[0] = tag;
    hdr[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&hdr).await?;
    if !payload.is_empty() {
        w.write_all(payload).await?;
    }
    w.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr).await?;
    let tag = hdr[0];
    let len = u32::from_le_bytes(hdr[1..5].try_into().expect("4 bytes")) as usize;
    if len > MAX_IPC_FRAME_LEN {
        bail!("IPC frame {len}B exceeds {MAX_IPC_FRAME_LEN}B");
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload).await?;
    }
    Ok((tag, payload))
}

pub async fn write_worker_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &WorkerMsg) -> Result<()> {
    match msg {
        WorkerMsg::HelloInfo(info) => {
            let bytes = serde_json::to_vec(info).context("encoding WorkerHelloInfo")?;
            write_frame(w, 0x01, &bytes).await
        }
        WorkerMsg::VideoFrame {
            keyframe,
            timestamp_us,
            data,
        } => {
            if 5 + data.len() > MAX_IPC_FRAME_LEN {
                bail!("video frame {}B exceeds IPC cap", data.len());
            }
            let mut hdr = [0u8; 10];
            hdr[0] = 0x02;
            hdr[1..5].copy_from_slice(&((5 + data.len()) as u32).to_le_bytes());
            hdr[5] = u8::from(*keyframe);
            hdr[6..10].copy_from_slice(&timestamp_us.to_le_bytes());
            w.write_all(&hdr).await?;
            if !data.is_empty() {
                w.write_all(data).await?;
            }
            w.flush().await?;
            Ok(())
        }
        WorkerMsg::AudioPacket(bytes) => write_frame(w, 0x03, bytes).await,
        WorkerMsg::LogLine(line) => write_frame(w, 0x04, line.as_bytes()).await,
        WorkerMsg::CodecChanged {
            codec,
            in_place_bitrate,
        } => write_frame(w, 0x05, &[codec.to_u8(), u8::from(*in_place_bitrate)]).await,
        WorkerMsg::ClipboardText(text) => write_frame(w, 0x06, text.as_bytes()).await,
    }
}

pub async fn read_worker_msg<R: AsyncRead + Unpin>(r: &mut R) -> Result<WorkerMsg> {
    let (tag, payload) = read_frame(r).await?;
    match tag {
        0x01 => {
            let info: WorkerHelloInfo =
                serde_json::from_slice(&payload).context("decoding WorkerHelloInfo")?;
            Ok(WorkerMsg::HelloInfo(info))
        }
        0x02 => {
            if payload.len() < 5 {
                bail!("short VideoFrame IPC payload");
            }
            let keyframe = payload[0] != 0;
            let timestamp_us = u32::from_le_bytes(payload[1..5].try_into().expect("4 bytes"));
            let data = payload[5..].to_vec();
            Ok(WorkerMsg::VideoFrame {
                keyframe,
                timestamp_us,
                data,
            })
        }
        0x03 => Ok(WorkerMsg::AudioPacket(payload)),
        0x04 => Ok(WorkerMsg::LogLine(
            String::from_utf8_lossy(&payload).into_owned(),
        )),
        0x05 => {
            if payload.len() < 2 {
                bail!("short CodecChanged IPC payload");
            }
            let codec = VideoCodec::from_u8(payload[0])?;
            Ok(WorkerMsg::CodecChanged {
                codec,
                in_place_bitrate: payload[1] != 0,
            })
        }
        0x06 => Ok(WorkerMsg::ClipboardText(
            String::from_utf8_lossy(&payload).into_owned(),
        )),
        other => bail!("unknown WorkerMsg tag 0x{other:02x}"),
    }
}

pub async fn write_host_cmd<W: AsyncWrite + Unpin>(w: &mut W, cmd: &HostCmd) -> Result<()> {
    match cmd {
        HostCmd::StartVideo(cfg) => {
            let bytes = serde_json::to_vec(cfg).context("encoding WorkerVideoConfig")?;
            write_frame(w, 0x10, &bytes).await
        }
        HostCmd::UpdateBitrate(kbps) => write_frame(w, 0x11, &kbps.to_le_bytes()).await,
        HostCmd::RequestKeyframe => write_frame(w, 0x12, &[]).await,
        HostCmd::Input(bytes) => write_frame(w, 0x13, bytes).await,
        HostCmd::AudioEnable(on) => write_frame(w, 0x14, &[u8::from(*on)]).await,
        HostCmd::Shutdown => write_frame(w, 0x15, &[]).await,
        HostCmd::SetClipboardText(text) => write_frame(w, 0x16, text.as_bytes()).await,
    }
}

pub async fn read_host_cmd<R: AsyncRead + Unpin>(r: &mut R) -> Result<HostCmd> {
    let (tag, payload) = read_frame(r).await?;
    match tag {
        0x10 => {
            let cfg: WorkerVideoConfig =
                serde_json::from_slice(&payload).context("decoding WorkerVideoConfig")?;
            Ok(HostCmd::StartVideo(cfg))
        }
        0x11 => {
            if payload.len() != 4 {
                bail!("invalid UpdateBitrate IPC payload");
            }
            Ok(HostCmd::UpdateBitrate(u32::from_le_bytes(
                payload.try_into().expect("4 bytes"),
            )))
        }
        0x12 => Ok(HostCmd::RequestKeyframe),
        0x13 => Ok(HostCmd::Input(payload)),
        0x14 => {
            if payload.is_empty() {
                bail!("short AudioEnable IPC payload");
            }
            Ok(HostCmd::AudioEnable(payload[0] != 0))
        }
        0x15 => Ok(HostCmd::Shutdown),
        0x16 => Ok(HostCmd::SetClipboardText(
            String::from_utf8_lossy(&payload).into_owned(),
        )),
        other => bail!("unknown HostCmd tag 0x{other:02x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ipc_roundtrip_messages() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);

        let w_msgs = vec![
            WorkerMsg::HelloInfo(WorkerHelloInfo {
                displays: vec![DisplayInfo {
                    index: 0,
                    name: "DISPLAY1".into(),
                    width: 1920,
                    height: 1080,
                }],
                host_codecs: vec![VideoCodec::Hevc, VideoCodec::H264],
                worker_session: 2,
                is_system: true,
                desktop_name: Some("Winlogon".into()),
            }),
            WorkerMsg::VideoFrame {
                keyframe: true,
                timestamp_us: 123_456,
                data: vec![0, 0, 0, 1, 0x26, 0x01, 0x80],
            },
            WorkerMsg::AudioPacket(vec![1, 2, 3, 4]),
            WorkerMsg::LogLine("hello from worker".into()),
            WorkerMsg::CodecChanged {
                codec: VideoCodec::H264,
                in_place_bitrate: true,
            },
            WorkerMsg::ClipboardText("copied from host".into()),
        ];

        let h_cmds = vec![
            HostCmd::StartVideo(WorkerVideoConfig {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_kbps: 25_000,
                codec: VideoCodec::Hevc,
                capture: "auto".into(),
                encoder: "auto".into(),
                prefer: QualityPreference::Latency,
                display_index: 0,
            }),
            HostCmd::UpdateBitrate(18_000),
            HostCmd::RequestKeyframe,
            HostCmd::Input(vec![9, 8, 7]),
            HostCmd::AudioEnable(false),
            HostCmd::Shutdown,
            HostCmd::SetClipboardText("copied from client".into()),
        ];

        for m in &w_msgs {
            write_worker_msg(&mut a, m).await.unwrap();
        }
        for m in &w_msgs {
            assert_eq!(&read_worker_msg(&mut b).await.unwrap(), m);
        }

        for c in &h_cmds {
            write_host_cmd(&mut b, c).await.unwrap();
        }
        for c in &h_cmds {
            assert_eq!(&read_host_cmd(&mut a).await.unwrap(), c);
        }
    }
}
