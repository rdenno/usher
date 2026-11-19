#[cfg(target_os = "linux")]
pub mod pipewire_linux;
pub mod test_tone;
#[cfg(target_os = "windows")]
pub mod wasapi_windows;

use anyhow::{Result, anyhow};
use rusty_opus::{Application, OpusEncoder};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::{
    AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES_5MS, AUDIO_SAMPLE_RATE, AudioCodec, AudioPacket,
};

#[derive(Debug, Clone, Copy)]
pub struct AudioStreamConfig {
    pub enabled: bool,
    pub test_tone: bool,
}

pub fn create_opus_encoder() -> Result<OpusEncoder> {
    let mut enc = OpusEncoder::new(
        AUDIO_SAMPLE_RATE as i32,
        AUDIO_CHANNELS as usize,
        Application::RestrictedLowDelay,
    )
    .map_err(|e| anyhow!("failed to initialize Opus encoder: {e:?}"))?;
    enc.bitrate_bps = 128_000;
    enc.complexity = 5;
    Ok(enc)
}

pub fn spawn_audio_task(
    cfg: AudioStreamConfig,
    tx: mpsc::Sender<AudioPacket>,
    start: Instant,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            return;
        }

        if cfg.test_tone {
            if let Err(err) = test_tone::run_test_tone(tx, start, cancel).await {
                tracing::warn!("test audio stream stopped: {err:#}");
            }
            return;
        }

        #[cfg(target_os = "linux")]
        {
            if pipewire_linux::is_available() {
                if let Err(err) =
                    pipewire_linux::run_pipewire_capture(tx.clone(), start, cancel.clone()).await
                {
                    tracing::warn!(
                        "PipeWire capture unavailable ({err:#}); audio stream disabled for this session"
                    );
                }
            } else {
                tracing::info!("pw-record not found; audio capture disabled");
            }
        }

        #[cfg(target_os = "windows")]
        {
            if let Err(err) =
                wasapi_windows::run_wasapi_loopback(tx.clone(), start, cancel.clone()).await
            {
                tracing::warn!(
                    "WASAPI loopback capture unavailable ({err:#}); falling back to silent test audio"
                );
                if !cancel.is_cancelled() {
                    let _ = test_tone::run_test_tone(tx, start, cancel).await;
                }
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            tracing::info!("desktop audio capture not supported on this OS unless testsrc is active");
            let _ = tx;
            let _ = start;
            cancel.cancelled().await;
        }
    })
}

pub fn encode_opus_frame(
    enc: &mut OpusEncoder,
    pcm: &[f32],
    seq: &mut u32,
    timestamp_us: u32,
    packet_buf: &mut [u8],
) -> Result<AudioPacket> {
    let len = enc
        .encode(pcm, AUDIO_FRAME_SAMPLES_5MS, packet_buf)
        .map_err(|e| anyhow!("Opus encode failed: {e:?}"))?;
    let current_seq = *seq;
    *seq = seq.wrapping_add(1);
    Ok(AudioPacket {
        seq: current_seq,
        timestamp_us,
        codec: AudioCodec::Opus,
        samples_per_channel: AUDIO_FRAME_SAMPLES_5MS as u16,
        payload: packet_buf[..len].to_vec(),
    })
}
