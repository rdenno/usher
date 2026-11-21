#[cfg(target_os = "linux")]
use anyhow::{Context, Result};
use rusty_opus::OpusDecoder;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::{AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES_5MS, AUDIO_SAMPLE_RATE, AudioCodec, AudioPacket};

#[allow(unused_variables, unused_mut)]
pub fn spawn_mic_sink_task(
    enabled: bool,
    mut rx: mpsc::Receiver<AudioPacket>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !enabled {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            let _ = tokio::task::spawn_blocking(move || run_linux_mic_sink(rx, cancel)).await;
        }
        #[cfg(target_os = "windows")]
        {
            let _ = tokio::task::spawn_blocking(move || run_windows_mic_sink(rx, cancel)).await;
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            tracing::warn!("--mic virtual microphone sink is only implemented on Linux and Windows hosts");
            while !cancel.is_cancelled() && rx.recv().await.is_some() {}
        }
    })
}

fn decode_packet(decoder: &mut OpusDecoder, pkt: &AudioPacket, pcm_scratch: &mut Vec<f32>) -> usize {
    let ch = AUDIO_CHANNELS as usize;
    match pkt.codec {
        AudioCodec::Opus => {
            let samples = (pkt.samples_per_channel as usize).max(AUDIO_FRAME_SAMPLES_5MS);
            let needed = samples * ch;
            if pcm_scratch.len() < needed {
                pcm_scratch.resize(needed, 0.0);
            }
            decoder
                .decode(&pkt.payload, samples, pcm_scratch)
                .unwrap_or(0)
        }
        AudioCodec::Pcm16 => {
            let (chunks, _) = pkt.payload.as_chunks::<2>();
            pcm_scratch.clear();
            for c in chunks {
                pcm_scratch.push(i16::from_le_bytes(*c) as f32 / 32768.0);
            }
            pcm_scratch.len() / ch
        }
    }
}

#[cfg(target_os = "linux")]
struct PulseModuleGuard {
    module_ids: Vec<String>,
}

#[cfg(target_os = "linux")]
impl Drop for PulseModuleGuard {
    fn drop(&mut self) {
        for id in self.module_ids.iter().rev() {
            let _ = std::process::Command::new("pactl")
                .args(["unload-module", id])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

#[cfg(target_os = "linux")]
fn setup_virtual_mic_pulse() -> Result<(PulseModuleGuard, String)> {
    use anyhow::bail;
    let load = |args: &[&str]| -> Result<String> {
        let out = std::process::Command::new("pactl")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .context("running pactl to create ephemeral virtual microphone")?;
        if !out.status.success() {
            bail!(
                "pactl {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    let mut guard = PulseModuleGuard {
        module_ids: Vec::new(),
    };
    let sink_id = load(&[
        "load-module",
        "module-null-sink",
        "sink_name=usher_mic",
        "sink_properties=device.description=Usher_Microphone",
    ])?;
    guard.module_ids.push(sink_id);

    let src_id = load(&[
        "load-module",
        "module-remap-source",
        "master=usher_mic.monitor",
        "source_name=usher_mic_src",
        "source_properties=device.description=Usher_Microphone",
    ])?;
    guard.module_ids.push(src_id);

    Ok((guard, "usher_mic".to_string()))
}

#[cfg(target_os = "linux")]
fn run_linux_mic_sink(mut rx: mpsc::Receiver<AudioPacket>, cancel: CancellationToken) {
    use libloading::{Library, Symbol};
    use std::ffi::{CStr, CString, c_char, c_int, c_void};

    const PA_STREAM_PLAYBACK: u32 = 1;
    const PA_SAMPLE_FLOAT32LE: u32 = 5;

    #[repr(C)]
    struct PaSampleSpec {
        format: u32,
        rate: u32,
        channels: u8,
    }

    type FnPaSimpleNew = unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        u32,
        *const c_char,
        *const c_char,
        *const PaSampleSpec,
        *const c_void,
        *const c_void,
        *mut c_int,
    ) -> *mut c_void;
    type FnPaSimpleFree = unsafe extern "C" fn(*mut c_void);
    type FnPaSimpleWrite = unsafe extern "C" fn(*mut c_void, *const c_void, usize, *mut c_int) -> c_int;
    type FnPaStrerror = unsafe extern "C" fn(c_int) -> *const c_char;

    let Ok(lib) = (unsafe { Library::new("libpulse-simple.so.0") }) else {
        tracing::warn!("--mic requested but libpulse-simple.so.0 is unavailable");
        return;
    };
    let Ok((pa_new, pa_free, pa_write, pa_err)) = (unsafe {
        (|| -> Result<(FnPaSimpleNew, FnPaSimpleFree, FnPaSimpleWrite, FnPaStrerror)> {
            let n: Symbol<'_, FnPaSimpleNew> = lib.get(b"pa_simple_new\0")?;
            let f: Symbol<'_, FnPaSimpleFree> = lib.get(b"pa_simple_free\0")?;
            let w: Symbol<'_, FnPaSimpleWrite> = lib.get(b"pa_simple_write\0")?;
            let e: Symbol<'_, FnPaStrerror> = lib.get(b"pa_strerror\0")?;
            Ok((*n, *f, *w, *e))
        })()
    }) else {
        tracing::warn!("--mic requested but PulseAudio symbols missing");
        return;
    };

    let Ok((_guard, sink_name)) = setup_virtual_mic_pulse() else {
        tracing::warn!("--mic virtual microphone unavailable (pactl / PipeWire-Pulse required)");
        return;
    };

    let app = CString::new("usher-host").expect("valid cstr");
    let dev = CString::new(sink_name.as_str()).expect("valid cstr");
    let stream_name = CString::new("Usher Remote Mic").expect("valid cstr");
    let spec = PaSampleSpec {
        format: PA_SAMPLE_FLOAT32LE,
        rate: AUDIO_SAMPLE_RATE,
        channels: AUDIO_CHANNELS,
    };
    let mut err_code = 0 as c_int;
    let pa = unsafe {
        pa_new(
            std::ptr::null(),
            app.as_ptr(),
            PA_STREAM_PLAYBACK,
            dev.as_ptr(),
            stream_name.as_ptr(),
            &raw const spec,
            std::ptr::null(),
            std::ptr::null(),
            &raw mut err_code,
        )
    };
    if pa.is_null() {
        let msg = unsafe {
            let p = pa_err(err_code);
            if p.is_null() {
                "unknown error".into()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        tracing::warn!("failed opening PulseAudio virtual mic stream: {msg}");
        return;
    }

    tracing::info!(sink = %sink_name, "ephemeral virtual microphone 'Usher_Microphone' active");

    let Ok(mut dec) = OpusDecoder::new(AUDIO_SAMPLE_RATE as i32, AUDIO_CHANNELS as usize) else {
        unsafe { pa_free(pa) };
        return;
    };
    let mut pcm = Vec::with_capacity(AUDIO_FRAME_SAMPLES_5MS * AUDIO_CHANNELS as usize * 4);
    let ch = AUDIO_CHANNELS as usize;

    while !cancel.is_cancelled() {
        let Some(pkt) = rx.blocking_recv() else {
            break;
        };
        let frames = decode_packet(&mut dec, &pkt, &mut pcm);
        if frames > 0 {
            let byte_len = frames * ch * std::mem::size_of::<f32>();
            let mut wr_err = 0 as c_int;
            let rc = unsafe { pa_write(pa, pcm.as_ptr().cast(), byte_len, &raw mut wr_err) };
            if rc < 0 {
                tracing::debug!("pa_simple_write virtual mic error ({wr_err})");
                break;
            }
        }
    }

    unsafe { pa_free(pa) };
}

#[cfg(target_os = "windows")]
fn find_virtual_cable_output() -> Option<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let devices = host.output_devices().ok()?;
    for dev in devices {
        if let Ok(desc) = dev.description() {
            let lower = desc.name().to_ascii_lowercase();
            if lower.contains("cable input")
                || lower.contains("vb-audio")
                || lower.contains("virtual")
                || lower.contains("voicemeeter")
            {
                return Some(dev);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn run_windows_mic_sink(mut rx: mpsc::Receiver<AudioPacket>, cancel: CancellationToken) {
    use cpal::SampleFormat;
    use cpal::traits::{DeviceTrait, StreamTrait};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    let Some(device) = find_virtual_cable_output() else {
        tracing::warn!(
            "--mic requested, but no Windows virtual audio cable output device (e.g. VB-Cable) was found; mic audio ignored"
        );
        while !cancel.is_cancelled() && rx.blocking_recv().is_some() {}
        return;
    };
    let Ok(supported) = device.default_output_config() else {
        tracing::warn!("failed querying Windows virtual audio cable output config");
        return;
    };
    let out_rate = supported.sample_rate();
    let out_ch = supported.channels() as usize;
    let fmt = supported.sample_format();
    let cfg: cpal::StreamConfig = supported.into();
    let queue = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(
        AUDIO_SAMPLE_RATE as usize * 2 / 20,
    )));

    let pop = |q: &Arc<Mutex<VecDeque<f32>>>, len: usize| -> Vec<f32> {
        let mut g = q.lock().expect("mic sink queue poisoned");
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(g.pop_front().unwrap_or(0.0));
        }
        out
    };
    let err_fn = |err| tracing::debug!("virtual mic output callback error: {err}");
    let stream_res = match fmt {
        SampleFormat::F32 => {
            let q = queue.clone();
            device.build_output_stream(
                cfg,
                move |out: &mut [f32], _| {
                    let s = pop(&q, out.len());
                    out.copy_from_slice(&s);
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let q = queue.clone();
            device.build_output_stream(
                cfg,
                move |out: &mut [i16], _| {
                    let s = pop(&q, out.len());
                    for (dst, src) in out.iter_mut().zip(s) {
                        *dst = (src.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    }
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let q = queue.clone();
            device.build_output_stream(
                cfg,
                move |out: &mut [u16], _| {
                    let s = pop(&q, out.len());
                    for (dst, src) in out.iter_mut().zip(s) {
                        *dst = ((src.clamp(-1.0, 1.0) * 0.5 + 0.5) * 65535.0).round() as u16;
                    }
                },
                err_fn,
                None,
            )
        }
        other => {
            tracing::warn!("unsupported virtual mic sample format {other:?}");
            return;
        }
    };
    let Ok(stream) = stream_res else {
        tracing::warn!("failed opening Windows virtual audio cable output stream");
        return;
    };
    if stream.play().is_err() {
        tracing::warn!("failed starting Windows virtual audio cable stream");
        return;
    }

    let dev_name = device
        .description()
        .map_or_else(|_| "Virtual Cable".into(), |d| d.name().to_string());
    tracing::info!(device = %dev_name, "Windows virtual microphone sink active");

    let Ok(mut dec) = OpusDecoder::new(AUDIO_SAMPLE_RATE as i32, AUDIO_CHANNELS as usize) else {
        return;
    };
    let mut pcm = Vec::with_capacity(AUDIO_FRAME_SAMPLES_5MS * AUDIO_CHANNELS as usize * 4);
    let src_ch = AUDIO_CHANNELS as usize;
    let max_samples = out_rate as usize * out_ch / 20;

    while !cancel.is_cancelled() {
        let Some(pkt) = rx.blocking_recv() else {
            break;
        };
        let frames = decode_packet(&mut dec, &pkt, &mut pcm);
        if frames == 0 {
            continue;
        }
        let mut q = queue.lock().expect("mic sink queue poisoned");
        let out_frames = if out_rate == AUDIO_SAMPLE_RATE {
            frames
        } else {
            ((frames as u64 * out_rate as u64) / AUDIO_SAMPLE_RATE as u64).max(1) as usize
        };
        for f in 0..out_frames {
            let sf = ((f * frames) / out_frames).min(frames - 1);
            let l = pcm[sf * src_ch];
            let r = if src_ch > 1 { pcm[sf * src_ch + 1] } else { l };
            for c in 0..out_ch {
                q.push_back(match c {
                    0 => l,
                    1 => r,
                    _ => 0.5 * (l + r),
                });
            }
        }
        while q.len() > max_samples {
            q.pop_front();
        }
    }
}
