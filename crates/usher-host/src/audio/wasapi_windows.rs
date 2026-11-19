use super::{create_opus_encoder, encode_opus_frame};
use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::{AUDIO_FRAME_SAMPLES_5MS, AUDIO_SAMPLE_RATE, AudioPacket};

/// Captures system desktop playback audio via WASAPI loopback (`cpal` output device as input),
/// resamples to 48 kHz stereo `f32`, and encodes 5 ms Opus frames.
pub async fn run_wasapi_loopback(
    tx: mpsc::Sender<AudioPacket>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default Windows audio output device for WASAPI loopback"))?;
    let supported = device
        .default_output_config()
        .context("querying default output config for WASAPI loopback")?;

    let in_rate = supported.sample_rate();
    let in_channels = supported.channels() as usize;
    let sample_format = supported.sample_format();
    let stream_cfg: StreamConfig = supported.into();

    let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
    let err_fn = |err| tracing::warn!("WASAPI loopback stream error: {err}");

    let stream = match sample_format {
        SampleFormat::F32 => {
            let sender = pcm_tx.clone();
            device.build_input_stream(
                stream_cfg,
                move |data: &[f32], _| {
                    let _ = sender.try_send(to_stereo_f32(data, in_channels, |s| s));
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::I16 => {
            let sender = pcm_tx.clone();
            device.build_input_stream(
                stream_cfg,
                move |data: &[i16], _| {
                    let _ = sender
                        .try_send(to_stereo_f32(data, in_channels, |s| s as f32 / 32768.0));
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::U16 => {
            let sender = pcm_tx.clone();
            device.build_input_stream(
                stream_cfg,
                move |data: &[u16], _| {
                    let _ = sender.try_send(to_stereo_f32(data, in_channels, |s| {
                        (s as f32 / 32768.0) - 1.0
                    }));
                },
                err_fn,
                None,
            )?
        }
        other => anyhow::bail!("unsupported WASAPI loopback sample format {other:?}"),
    };

    stream.play().context("starting WASAPI loopback stream")?;
    tracing::info!(
        sample_rate = in_rate,
        channels = in_channels,
        ?sample_format,
        "WASAPI loopback audio capture active"
    );

    let handle = tokio::task::spawn_blocking(move || {
        let Ok(mut enc) = create_opus_encoder() else {
            return;
        };
        let mut resampled = Vec::<f32>::with_capacity(AUDIO_FRAME_SAMPLES_5MS * 4);
        let mut opus_buf = [0u8; 1024];
        let mut seq = 0u32;
        let frame_floats = AUDIO_FRAME_SAMPLES_5MS * 2;
        let mut prev_lr = [0.0f32, 0.0f32];

        while !cancel.is_cancelled() {
            match pcm_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(stereo_chunk) => {
                    resample_stereo_into(
                        &stereo_chunk,
                        in_rate,
                        AUDIO_SAMPLE_RATE,
                        &mut prev_lr,
                        &mut resampled,
                    );
                    while resampled.len() >= frame_floats {
                        let ts_us = start.elapsed().as_micros() as u32;
                        if let Ok(pkt) = encode_opus_frame(
                            &mut enc,
                            &resampled[..frame_floats],
                            &mut seq,
                            ts_us,
                            &mut opus_buf,
                        ) {
                            let _ = tx.try_send(pkt);
                        }
                        resampled.drain(..frame_floats);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    let _ = handle.await;
    drop(stream);
    Ok(())
}

fn to_stereo_f32<T: Copy, F: Fn(T) -> f32>(data: &[T], channels: usize, conv: F) -> Vec<f32> {
    if channels == 0 {
        return Vec::new();
    }
    let frames = data.len() / channels;
    let mut out = Vec::with_capacity(frames * 2);
    for frame in data.chunks_exact(channels) {
        let l = conv(frame[0]);
        let r = if channels >= 2 { conv(frame[1]) } else { l };
        out.push(l);
        out.push(r);
    }
    out
}

fn resample_stereo_into(
    stereo_in: &[f32],
    in_rate: u32,
    out_rate: u32,
    prev_lr: &mut [f32; 2],
    out: &mut Vec<f32>,
) {
    if stereo_in.len() < 2 {
        return;
    }
    if in_rate == out_rate {
        out.extend_from_slice(stereo_in);
        let len = stereo_in.len();
        *prev_lr = [stereo_in[len - 2], stereo_in[len - 1]];
        return;
    }

    let in_frames = stereo_in.len() / 2;
    let out_frames = ((in_frames as u64 * out_rate as u64) / in_rate as u64).max(1) as usize;
    for f in 0..out_frames {
        let pos = (f as f32) * (in_rate as f32) / (out_rate as f32);
        let i0 = (pos as usize).min(in_frames - 1);
        let i1 = (i0 + 1).min(in_frames - 1);
        let frac = (pos - i0 as f32).clamp(0.0, 1.0);
        let (l0, r0) = (stereo_in[i0 * 2], stereo_in[i0 * 2 + 1]);
        let (l1, r1) = (stereo_in[i1 * 2], stereo_in[i1 * 2 + 1]);
        out.push(l0 * (1.0 - frac) + l1 * frac);
        out.push(r0 * (1.0 - frac) + r1 * frac);
    }
    let len = stereo_in.len();
    *prev_lr = [stereo_in[len - 2], stereo_in[len - 1]];
}
