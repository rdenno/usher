use super::{create_opus_encoder, encode_opus_frame};
use anyhow::Result;
use std::f32::consts::TAU;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::{AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES_5MS, AUDIO_SAMPLE_RATE, AudioPacket};

/// Emits a gentle low-amplitude 440 Hz ping for 80ms every 2 seconds (and silence otherwise)
/// encoded as 5ms 48kHz stereo Opus frames so E2E audio is immediately testable without noise fatigue.
pub async fn run_test_tone(
    tx: mpsc::Sender<AudioPacket>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let mut enc = create_opus_encoder()?;
    let samples_total = AUDIO_FRAME_SAMPLES_5MS * AUDIO_CHANNELS as usize;
    let mut pcm = vec![0.0f32; samples_total];
    let mut opus_buf = [0u8; 1024];
    let mut seq = 0u32;
    let mut sample_idx = 0u64;

    let frame_dur = Duration::from_millis(5);
    let mut next_tick = Instant::now();

    while !cancel.is_cancelled() {
        for i in 0..AUDIO_FRAME_SAMPLES_5MS {
            let phase_pos = (sample_idx + i as u64) % (AUDIO_SAMPLE_RATE as u64 * 2);
            let s = if phase_pos < (AUDIO_SAMPLE_RATE as u64 * 80 / 1000) {
                let t = (sample_idx + i as u64) as f32 / AUDIO_SAMPLE_RATE as f32;
                (TAU * 440.0 * t).sin() * 0.04
            } else {
                0.0
            };
            pcm[i * 2] = s;
            pcm[i * 2 + 1] = s;
        }
        sample_idx += AUDIO_FRAME_SAMPLES_5MS as u64;

        let ts_us = start.elapsed().as_micros() as u32;
        let pkt = encode_opus_frame(&mut enc, &pcm, &mut seq, ts_us, &mut opus_buf)?;
        let _ = tx.try_send(pkt);

        next_tick += frame_dur;
        let now = Instant::now();
        if next_tick > now {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(next_tick - now) => {}
            }
        } else {
            next_tick = now;
            tokio::task::yield_now().await;
        }
    }

    Ok(())
}
