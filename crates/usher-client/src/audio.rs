use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, StreamConfig};
use rusty_opus::OpusDecoder;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::{
    AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES_5MS, AUDIO_SAMPLE_RATE, AudioCodec, AudioPacket,
};

const MAX_JITTER_STEREO_SAMPLES: usize = AUDIO_SAMPLE_RATE as usize * 2 * 40 / 1000;

pub struct AudioPlayer {
    _stream: Stream,
}

impl AudioPlayer {
    pub fn start(
        mut audio_rx: mpsc::Receiver<AudioPacket>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default audio output device"))?;
        let supported = device
            .default_output_config()
            .context("querying default output audio config")?;

        let sample_format = supported.sample_format();
        let out_rate = supported.sample_rate();
        let out_channels = supported.channels() as usize;
        let stream_cfg: StreamConfig = supported.into();

        let queue = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(
            MAX_JITTER_STEREO_SAMPLES,
        )));

        let q_decode = queue.clone();
        std::thread::spawn(move || {
            let Ok(mut decoder) = OpusDecoder::new(AUDIO_SAMPLE_RATE as i32, AUDIO_CHANNELS as usize)
            else {
                return;
            };
            let mut decoded = vec![0.0f32; AUDIO_FRAME_SAMPLES_5MS * AUDIO_CHANNELS as usize * 4];
            let mut expected_seq: Option<u32> = None;

            while !cancel.is_cancelled() {
                let Some(pkt) = audio_rx.blocking_recv() else {
                    break;
                };

                if let Some(exp) = expected_seq {
                    let gap = pkt.seq.wrapping_sub(exp);
                    if gap > 0 && gap <= 6 {
                        for _ in 0..gap {
                            if let Ok(n) = decoder.decode(&[], AUDIO_FRAME_SAMPLES_5MS, &mut decoded)
                            {
                                push_resampled(
                                    &q_decode,
                                    &decoded[..n * AUDIO_CHANNELS as usize],
                                    AUDIO_SAMPLE_RATE,
                                    out_rate,
                                    out_channels,
                                );
                            }
                        }
                    }
                }
                expected_seq = Some(pkt.seq.wrapping_add(1));

                match pkt.codec {
                    AudioCodec::Opus => {
                        let frame_samples = (pkt.samples_per_channel as usize).max(120);
                        let needed = frame_samples * AUDIO_CHANNELS as usize;
                        if decoded.len() < needed {
                            decoded.resize(needed, 0.0);
                        }
                        if let Ok(n) = decoder.decode(&pkt.payload, frame_samples, &mut decoded) {
                            push_resampled(
                                &q_decode,
                                &decoded[..n * AUDIO_CHANNELS as usize],
                                AUDIO_SAMPLE_RATE,
                                out_rate,
                                out_channels,
                            );
                        }
                    }
                    AudioCodec::Pcm16 => {
                        let (chunks, _) = pkt.payload.as_chunks::<2>();
                        let pcm: Vec<f32> = chunks
                            .iter()
                            .map(|c| i16::from_le_bytes(*c) as f32 / 32768.0)
                            .collect();
                        push_resampled(&q_decode, &pcm, AUDIO_SAMPLE_RATE, out_rate, out_channels);
                    }
                }
            }
        });

        let err_fn = |err| tracing::warn!("audio output callback error: {err}");
        let stream = match sample_format {
            SampleFormat::F32 => {
                let q = queue.clone();
                device.build_output_stream(
                    stream_cfg,
                    move |out: &mut [f32], _| pop_audio_samples(&q, out, |s| s),
                    err_fn,
                    None,
                )?
            }
            SampleFormat::I16 => {
                let q = queue.clone();
                device.build_output_stream(
                    stream_cfg,
                    move |out: &mut [i16], _| {
                        pop_audio_samples(&q, out, |s| {
                            (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
                        })
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::U16 => {
                let q = queue.clone();
                device.build_output_stream(
                    stream_cfg,
                    move |out: &mut [u16], _| {
                        pop_audio_samples(&q, out, |s| {
                            ((s.clamp(-1.0, 1.0) * 0.5 + 0.5) * 65535.0).round() as u16
                        })
                    },
                    err_fn,
                    None,
                )?
            }
            other => anyhow::bail!("unsupported audio output format {other:?}"),
        };

        stream.play().context("starting audio output stream")?;
        Ok(Self { _stream: stream })
    }
}

fn push_resampled(
    queue: &Mutex<VecDeque<f32>>,
    stereo_in: &[f32],
    in_rate: u32,
    out_rate: u32,
    out_channels: usize,
) {
    if stereo_in.len() < 2 || out_channels == 0 {
        return;
    }

    let in_frames = stereo_in.len() / 2;
    let out_frames = if in_rate == out_rate {
        in_frames
    } else {
        ((in_frames as u64 * out_rate as u64) / in_rate as u64).max(1) as usize
    };

    let mut guard = queue.lock().expect("audio queue poisoned");
    let max_samples = MAX_JITTER_STEREO_SAMPLES.max(out_channels * 2048);
    if guard.len() + out_frames * out_channels > max_samples {
        let drop_frames =
            ((guard.len() + out_frames * out_channels - max_samples) / out_channels) * out_channels;
        for _ in 0..drop_frames.min(guard.len()) {
            guard.pop_front();
        }
    }

    for f in 0..out_frames {
        let src_pos = if in_rate == out_rate {
            f as f32
        } else {
            (f as f32) * (in_rate as f32) / (out_rate as f32)
        };
        let i0 = (src_pos as usize).min(in_frames - 1);
        let i1 = (i0 + 1).min(in_frames - 1);
        let frac = (src_pos - i0 as f32).clamp(0.0, 1.0);
        let l = stereo_in[i0 * 2] * (1.0 - frac) + stereo_in[i1 * 2] * frac;
        let r = stereo_in[i0 * 2 + 1] * (1.0 - frac) + stereo_in[i1 * 2 + 1] * frac;

        match out_channels {
            1 => guard.push_back(0.5 * (l + r)),
            2 => {
                guard.push_back(l);
                guard.push_back(r);
            }
            n => {
                guard.push_back(l);
                guard.push_back(r);
                for _ in 2..n {
                    guard.push_back(0.0);
                }
            }
        }
    }
}

fn pop_audio_samples<T: Sample, F: Fn(f32) -> T>(
    queue: &Mutex<VecDeque<f32>>,
    out: &mut [T],
    convert: F,
) {
    let mut guard = queue.lock().expect("audio queue poisoned");
    for dst in out.iter_mut() {
        *dst = match guard.pop_front() {
            Some(s) => convert(s),
            None => T::EQUILIBRIUM,
        };
    }
}
