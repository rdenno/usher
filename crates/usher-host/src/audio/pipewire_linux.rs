use super::{create_opus_encoder, encode_opus_frame};
use anyhow::{Context, Result, anyhow, bail};
use libloading::{Library, Symbol};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::process::Stdio;
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::audio::{AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES_5MS, AUDIO_SAMPLE_RATE, AudioPacket};

const PA_STREAM_RECORD: u32 = 2;
const PA_SAMPLE_FLOAT32LE: u32 = 5;

#[repr(C)]
struct PaSampleSpec {
    format: u32,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct PaBufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type FnPaSimpleNew = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    u32,
    *const c_char,
    *const c_char,
    *const PaSampleSpec,
    *const c_void,
    *const PaBufferAttr,
    *mut c_int,
) -> *mut c_void;
type FnPaSimpleFree = unsafe extern "C" fn(*mut c_void);
type FnPaSimpleRead = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;
type FnPaStrerror = unsafe extern "C" fn(c_int) -> *const c_char;

struct PulseSimpleApi {
    _lib: Library,
    pa_simple_new: FnPaSimpleNew,
    pa_simple_free: FnPaSimpleFree,
    pa_simple_read: FnPaSimpleRead,
    pa_strerror: FnPaStrerror,
}

unsafe impl Send for PulseSimpleApi {}
unsafe impl Sync for PulseSimpleApi {}

impl PulseSimpleApi {
    fn load() -> Result<Self> {
        let lib = unsafe { Library::new("libpulse-simple.so.0") }
            .context("loading libpulse-simple.so.0 at runtime")?;
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let s: Symbol<'_, $ty> = unsafe { lib.get($name) }
                    .with_context(|| format!("missing PulseAudio symbol {:?}", $name))?;
                *s
            }};
        }
        Ok(Self {
            pa_simple_new: sym!(b"pa_simple_new\0", FnPaSimpleNew),
            pa_simple_free: sym!(b"pa_simple_free\0", FnPaSimpleFree),
            pa_simple_read: sym!(b"pa_simple_read\0", FnPaSimpleRead),
            pa_strerror: sym!(b"pa_strerror\0", FnPaStrerror),
            _lib: lib,
        })
    }

    fn err_str(&self, code: c_int) -> String {
        let ptr = unsafe { (self.pa_strerror)(code) };
        if ptr.is_null() {
            format!("err={code}")
        } else {
            unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
        }
    }
}

pub fn has_pw_record() -> bool {
    std::process::Command::new("which")
        .arg("pw-record")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn is_available() -> bool {
    PulseSimpleApi::load().is_ok() || has_pw_record()
}

pub fn audio_backend_diagnostic() -> String {
    if PulseSimpleApi::load().is_ok() {
        "audio capture: libpulse-simple.so.0 runtime (@DEFAULT_MONITOR@)".into()
    } else if has_pw_record() {
        "audio capture fallback: pw-record subprocess (libpulse-simple.so.0 missing)".into()
    } else {
        "audio capture unavailable (neither libpulse-simple.so.0 nor pw-record found)".into()
    }
}

pub async fn run_pipewire_capture(
    tx: mpsc::Sender<AudioPacket>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    match PulseSimpleApi::load() {
        Ok(api) => {
            let cancel_blocking = cancel.clone();
            let tx_blocking = tx.clone();
            let res = tokio::task::spawn_blocking(move || {
                run_pulse_simple_blocking(api, tx_blocking, start, cancel_blocking)
            })
            .await
            .map_err(|e| anyhow!("PulseSimple audio thread panicked: {e}"))?;
            match res {
                Ok(()) => return Ok(()),
                Err(err) => {
                    if cancel.is_cancelled() || !has_pw_record() {
                        return Err(err);
                    }
                    tracing::warn!("libpulse-simple monitor capture failed ({err:#}); falling back to pw-record");
                }
            }
        }
        Err(err) => {
            if !has_pw_record() {
                return Err(err);
            }
            tracing::debug!("libpulse-simple.so.0 unavailable ({err:#}); using pw-record fallback");
        }
    }

    run_pw_record_subprocess(tx, start, cancel).await
}

fn run_pulse_simple_blocking(
    api: PulseSimpleApi,
    tx: mpsc::Sender<AudioPacket>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let app_name = CString::new("usher-host")?;
    let stream_name = CString::new("desktop-audio")?;
    let dev_monitor = CString::new("@DEFAULT_MONITOR@")?;
    let spec = PaSampleSpec {
        format: PA_SAMPLE_FLOAT32LE,
        rate: AUDIO_SAMPLE_RATE,
        channels: AUDIO_CHANNELS,
    };
    let frame_floats = AUDIO_FRAME_SAMPLES_5MS * AUDIO_CHANNELS as usize;
    let frame_bytes = frame_floats * std::mem::size_of::<f32>();
    let attr = PaBufferAttr {
        maxlength: (frame_bytes * 4) as u32,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: frame_bytes as u32,
    };

    let mut err_code: c_int = 0;
    let pa = unsafe {
        (api.pa_simple_new)(
            std::ptr::null(),
            app_name.as_ptr(),
            PA_STREAM_RECORD,
            dev_monitor.as_ptr(),
            stream_name.as_ptr(),
            &raw const spec,
            std::ptr::null(),
            &raw const attr,
            &raw mut err_code,
        )
    };
    if pa.is_null() {
        bail!("pa_simple_new(@DEFAULT_MONITOR@) failed: {}", api.err_str(err_code));
    }

    struct PaGuard {
        free: FnPaSimpleFree,
        ptr: *mut c_void,
    }
    impl Drop for PaGuard {
        fn drop(&mut self) {
            if !self.ptr.is_null() {
                unsafe { (self.free)(self.ptr) };
            }
        }
    }
    let _guard = PaGuard {
        free: api.pa_simple_free,
        ptr: pa,
    };

    tracing::info!(
        sample_rate = AUDIO_SAMPLE_RATE,
        channels = AUDIO_CHANNELS,
        "PulseAudio/PipeWire runtime monitor capture active (zero subprocess dependency)"
    );

    let mut enc = create_opus_encoder()?;
    let mut pcm = vec![0.0f32; frame_floats];
    let mut opus_buf = [0u8; 1024];
    let mut seq = 0u32;

    while !cancel.is_cancelled() {
        let mut read_err: c_int = 0;
        let rc = unsafe {
            (api.pa_simple_read)(
                pa,
                pcm.as_mut_ptr().cast::<c_void>(),
                frame_bytes,
                &raw mut read_err,
            )
        };
        if rc < 0 {
            bail!("pa_simple_read failed: {}", api.err_str(read_err));
        }
        let ts_us = start.elapsed().as_micros() as u32;
        let pkt = encode_opus_frame(&mut enc, &pcm, &mut seq, ts_us, &mut opus_buf)?;
        let _ = tx.try_send(pkt);
    }

    Ok(())
}

async fn run_pw_record_subprocess(
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
