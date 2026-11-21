use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, StreamConfig};
use rusty_opus::{Application, OpusDecoder, OpusEncoder};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::{
    AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES_5MS, AUDIO_SAMPLE_RATE, AudioCodec, AudioPacket,
};

const MAX_JITTER_STEREO_SAMPLES: usize = AUDIO_SAMPLE_RATE as usize * 2 * 40 / 1000;

pub struct AudioPlayer {
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        drop(self.stop_tx.take());
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl AudioPlayer {
    pub fn start(
        mut audio_rx: mpsc::Receiver<AudioPacket>,
        muted: Arc<AtomicBool>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

        // Keep all cpal/WASAPI/COM setup off the caller thread so Windows STA/OleInitialize
        // remains intact for winit drag-and-drop.
        let handle = std::thread::Builder::new()
            .name("usher-audio-out".into())
            .spawn(move || {
                let init_res = (|| -> Result<cpal::Stream> {
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
                    let muted_dec = muted.clone();
                    let dec_cancel = cancel.clone();
                    std::thread::spawn(move || {
                        let Ok(mut decoder) =
                            OpusDecoder::new(AUDIO_SAMPLE_RATE as i32, AUDIO_CHANNELS as usize)
                        else {
                            return;
                        };
                        let mut decoded =
                            vec![0.0f32; AUDIO_FRAME_SAMPLES_5MS * AUDIO_CHANNELS as usize * 4];
                        let mut expected_seq: Option<u32> = None;

                        while !dec_cancel.is_cancelled() {
                            let Some(pkt) = audio_rx.blocking_recv() else {
                                break;
                            };

                            if muted_dec.load(Ordering::Acquire) {
                                expected_seq = Some(pkt.seq.wrapping_add(1));
                                let mut guard = q_decode.lock().expect("audio queue poisoned");
                                guard.clear();
                                continue;
                            }

                            if let Some(exp) = expected_seq {
                                let gap = pkt.seq.wrapping_sub(exp);
                                if gap > 0 && gap <= 6 {
                                    for _ in 0..gap {
                                        if let Ok(n) = decoder.decode(
                                            &[],
                                            AUDIO_FRAME_SAMPLES_5MS,
                                            &mut decoded,
                                        ) {
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
                                    let frame_samples =
                                        (pkt.samples_per_channel as usize).max(120);
                                    let needed = frame_samples * AUDIO_CHANNELS as usize;
                                    if decoded.len() < needed {
                                        decoded.resize(needed, 0.0);
                                    }
                                    if let Ok(n) =
                                        decoder.decode(&pkt.payload, frame_samples, &mut decoded)
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
                                AudioCodec::Pcm16 => {
                                    let (chunks, _) = pkt.payload.as_chunks::<2>();
                                    let pcm: Vec<f32> = chunks
                                        .iter()
                                        .map(|c| i16::from_le_bytes(*c) as f32 / 32768.0)
                                        .collect();
                                    push_resampled(
                                        &q_decode,
                                        &pcm,
                                        AUDIO_SAMPLE_RATE,
                                        out_rate,
                                        out_channels,
                                    );
                                }
                            }
                        }
                    });

                    let err_fn = |err| tracing::warn!("audio output callback error: {err}");
                    let stream = match sample_format {
                        SampleFormat::F32 => {
                            let q = queue.clone();
                            let m = muted.clone();
                            device.build_output_stream(
                                stream_cfg,
                                move |out: &mut [f32], _| pop_audio_samples(&q, &m, out, |s| s),
                                err_fn,
                                None,
                            )?
                        }
                        SampleFormat::I16 => {
                            let q = queue.clone();
                            let m = muted.clone();
                            device.build_output_stream(
                                stream_cfg,
                                move |out: &mut [i16], _| {
                                    pop_audio_samples(&q, &m, out, |s| {
                                        (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
                                    })
                                },
                                err_fn,
                                None,
                            )?
                        }
                        SampleFormat::U16 => {
                            let q = queue.clone();
                            let m = muted.clone();
                            device.build_output_stream(
                                stream_cfg,
                                move |out: &mut [u16], _| {
                                    pop_audio_samples(&q, &m, out, |s| {
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
                    Ok(stream)
                })();

                match init_res {
                    Ok(stream) => {
                        let _ = init_tx.send(Ok(()));
                        let _ = stop_rx.recv();
                        drop(stream);
                    }
                    Err(err) => {
                        let _ = init_tx.send(Err(err));
                    }
                }
            })
            .context("spawning usher-audio-out thread")?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop_tx: Some(stop_tx),
                thread: Some(handle),
            }),
            Ok(Err(err)) => {
                let _ = handle.join();
                Err(err)
            }
            Err(_) => {
                let _ = handle.join();
                Err(anyhow!("audio output thread exited during startup"))
            }
        }
    }
}

pub struct MicCapturer {
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MicCapturer {
    fn drop(&mut self) {
        drop(self.stop_tx.take());
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl MicCapturer {
    pub fn start(
        mic_tx: mpsc::Sender<AudioPacket>,
        muted: Arc<AtomicBool>,
        start: Instant,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

        let handle = std::thread::Builder::new()
            .name("usher-mic-in".into())
            .spawn(move || {
                let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
                let setup = (|| -> Result<(cpal::Stream, u32, usize)> {
                    let host = cpal::default_host();
                    let device = host
                        .default_input_device()
                        .ok_or_else(|| anyhow!("no default microphone input device"))?;
                    let supported = device
                        .default_input_config()
                        .context("querying default microphone input config")?;
                    let in_rate = supported.sample_rate();
                    let in_ch = supported.channels() as usize;
                    let fmt = supported.sample_format();
                    let cfg: StreamConfig = supported.into();
                    let err_fn = |err| tracing::debug!("mic input callback error: {err}");

                    let stream = match fmt {
                        SampleFormat::F32 => {
                            let tx = pcm_tx.clone();
                            device.build_input_stream(
                                cfg,
                                move |data: &[f32], _| {
                                    let _ = tx.try_send(data.to_vec());
                                },
                                err_fn,
                                None,
                            )?
                        }
                        SampleFormat::I16 => {
                            let tx = pcm_tx.clone();
                            device.build_input_stream(
                                cfg,
                                move |data: &[i16], _| {
                                    let conv: Vec<f32> =
                                        data.iter().map(|&s| s as f32 / 32768.0).collect();
                                    let _ = tx.try_send(conv);
                                },
                                err_fn,
                                None,
                            )?
                        }
                        SampleFormat::U16 => {
                            let tx = pcm_tx.clone();
                            device.build_input_stream(
                                cfg,
                                move |data: &[u16], _| {
                                    let conv: Vec<f32> = data
                                        .iter()
                                        .map(|&s| (s as f32 / 65535.0) * 2.0 - 1.0)
                                        .collect();
                                    let _ = tx.try_send(conv);
                                },
                                err_fn,
                                None,
                            )?
                        }
                        other => anyhow::bail!("unsupported microphone input format {other:?}"),
                    };
                    stream.play().context("starting microphone input stream")?;
                    Ok((stream, in_rate, in_ch))
                })();

                let (stream, in_rate, in_ch) = match setup {
                    Ok(v) => v,
                    Err(err) => {
                        let _ = init_tx.send(Err(err));
                        return;
                    }
                };

                let Ok(mut enc) = OpusEncoder::new(
                    AUDIO_SAMPLE_RATE as i32,
                    AUDIO_CHANNELS as usize,
                    Application::Voip,
                ) else {
                    let _ = init_tx.send(Err(anyhow!("failed initializing mic Opus encoder")));
                    drop(stream);
                    return;
                };
                enc.bitrate_bps = 48_000;
                enc.complexity = 5;

                let _ = init_tx.send(Ok(()));

                let frame_floats = AUDIO_FRAME_SAMPLES_5MS * AUDIO_CHANNELS as usize;
                let mut accum = VecDeque::<f32>::with_capacity(frame_floats * 4);
                let mut frame_buf = vec![0.0f32; frame_floats];
                let mut opus_out = vec![0u8; 512];
                let mut seq = 0u32;

                while !cancel.is_cancelled() {
                    if !matches!(stop_rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                        break;
                    }
                    match pcm_rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(raw) if in_ch > 0 && raw.len() >= in_ch => {
                            if muted.load(Ordering::Acquire) {
                                accum.clear();
                                continue;
                            }
                            let in_frames = raw.len() / in_ch;
                            let out_frames = if in_rate == AUDIO_SAMPLE_RATE {
                                in_frames
                            } else {
                                ((in_frames as u64 * AUDIO_SAMPLE_RATE as u64) / in_rate as u64)
                                    .max(1) as usize
                            };
                            for f in 0..out_frames {
                                let src_pos = if in_rate == AUDIO_SAMPLE_RATE {
                                    f as f32
                                } else {
                                    (f as f32) * (in_rate as f32) / (AUDIO_SAMPLE_RATE as f32)
                                };
                                let i0 = (src_pos as usize).min(in_frames - 1);
                                let i1 = (i0 + 1).min(in_frames - 1);
                                let frac = (src_pos - i0 as f32).clamp(0.0, 1.0);
                                let l = raw[i0 * in_ch] * (1.0 - frac) + raw[i1 * in_ch] * frac;
                                let r = if in_ch >= 2 {
                                    raw[i0 * in_ch + 1] * (1.0 - frac)
                                        + raw[i1 * in_ch + 1] * frac
                                } else {
                                    l
                                };
                                accum.push_back(l);
                                accum.push_back(r);
                            }

                            while accum.len() >= frame_floats {
                                for dst in &mut frame_buf {
                                    *dst = accum.pop_front().unwrap_or(0.0);
                                }
                                if let Ok(n) =
                                    enc.encode(&frame_buf, AUDIO_FRAME_SAMPLES_5MS, &mut opus_out)
                                {
                                    let pkt = AudioPacket {
                                        seq,
                                        timestamp_us: start.elapsed().as_micros() as u32,
                                        codec: AudioCodec::Opus,
                                        samples_per_channel: AUDIO_FRAME_SAMPLES_5MS as u16,
                                        payload: opus_out[..n].to_vec(),
                                    };
                                    seq = seq.wrapping_add(1);
                                    let _ = mic_tx.try_send(pkt);
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                drop(stream);
            })
            .context("spawning usher-mic-in thread")?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop_tx: Some(stop_tx),
                thread: Some(handle),
            }),
            Ok(Err(err)) => {
                let _ = handle.join();
                Err(err)
            }
            Err(_) => {
                let _ = handle.join();
                Err(anyhow!("microphone input thread exited during startup"))
            }
        }
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
    muted: &AtomicBool,
    out: &mut [T],
    convert: F,
) {
    if muted.load(Ordering::Acquire) {
        out.fill(T::EQUILIBRIUM);
        return;
    }
    let mut guard = queue.lock().expect("audio queue poisoned");
    for dst in out.iter_mut() {
        *dst = match guard.pop_front() {
            Some(s) => convert(s),
            None => T::EQUILIBRIUM,
        };
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn audio_and_mf_probe_preserve_caller_thread_sta_for_drag_and_drop() {
        std::thread::spawn(|| {
            let _ = crate::mf_decoder::probe_mf_decoders();
            let (_tx, rx) = mpsc::channel(4);
            let muted = Arc::new(AtomicBool::new(false));
            let cancel = CancellationToken::new();
            let player = AudioPlayer::start(rx, muted, cancel.clone());
            unsafe {
                let hr = windows::Win32::System::Ole::OleInitialize(None);
                assert!(
                    hr.is_ok(),
                    "OleInitialize failed after probe_mf_decoders + AudioPlayer::start: {hr:?}"
                );
                windows::Win32::System::Ole::OleUninitialize();
            }
            cancel.cancel();
            drop(player);
        })
        .join()
        .expect("STA test thread panicked");
    }
}
