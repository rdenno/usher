use super::{create_opus_encoder, encode_opus_frame};
use anyhow::{Context, Result, bail};
use std::process::Stdio;
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::{AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES_5MS, AUDIO_SAMPLE_RATE, AudioPacket};

pub fn is_available() -> bool {
    std::process::Command::new("which")
        .arg("pw-record")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub async fn run_pipewire_capture(
    tx: mpsc::Sender<AudioPacket>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let mut child = Command::new("pw-record")
        .args([
            "--target",
            "@DEFAULT_SINK@",
            "-P",
            "stream.capture.sink=true",
            "--format",
            "f32",
            "--rate",
            &AUDIO_SAMPLE_RATE.to_string(),
            "--channels",
            &AUDIO_CHANNELS.to_string(),
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn pw-record")?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("pw-record missing stdout"))?;

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!("[pw-record] {line}");
            }
        });
    }

    let mut enc = create_opus_encoder()?;
    let frame_floats = AUDIO_FRAME_SAMPLES_5MS * AUDIO_CHANNELS as usize;
    let frame_bytes = frame_floats * std::mem::size_of::<f32>();
    let mut buf = vec![0u8; frame_bytes];
    let mut pcm = vec![0.0f32; frame_floats];
    let mut opus_buf = [0u8; 1024];
    let mut seq = 0u32;

    loop {
        let mut filled = 0usize;
        while filled < frame_bytes {
            let n = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    return Ok(());
                }
                res = stdout.read(&mut buf[filled..]) => res.context("reading pw-record stdout")?
            };
            if n == 0 {
                let status = child.wait().await?;
                bail!("pw-record exited ({status})");
            }
            filled += n;
        }

        let (chunks, _) = buf.as_chunks::<4>();
        for (dst, chunk) in pcm.iter_mut().zip(chunks) {
            *dst = f32::from_le_bytes(*chunk);
        }

        let ts_us = start.elapsed().as_micros() as u32;
        let pkt = encode_opus_frame(&mut enc, &pcm, &mut seq, ts_us, &mut opus_buf)?;
        let _ = tx.try_send(pkt);
    }
}
