use crate::decoder::{FrameSink, VideoDecoder, Yuv420Frame};
use anyhow::{Context, Result, anyhow};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use usher_proto::handshake::VideoCodec;
use windows::Win32::Foundation::{HMODULE, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaEventGenerator,
    IMFMediaType, IMFSample, IMFTransform, METransformHaveOutput, METransformNeedInput,
    MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NO_WAIT, MF_EVENT_FLAG_NONE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_GEOMETRIC_APERTURE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateDXGIDeviceManager,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_NOSOCKET,
    MFStartup, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFVideoArea, MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_I420, MFVideoFormat_IYUV, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFTEnumEx,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::{GUID, Interface};

fn ensure_mf_startup() -> Result<()> {
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            tracing::debug!("CoInitializeEx returned {hr:?}");
        }
    }
    let res = INIT.get_or_init(|| {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(|e| format!("{e}"))
    });
    match res {
        Ok(()) => Ok(()),
        Err(e) => Err(anyhow!("MFStartup failed: {e}")),
    }
}

#[inline]
fn pack_u32_pair(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

#[inline]
fn unpack_u32_pair(val: u64) -> (u32, u32) {
    ((val >> 32) as u32, (val & 0xffff_ffff) as u32)
}

fn codec_subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::Hevc => MFVideoFormat_HEVC,
        VideoCodec::Av1 => MFVideoFormat_AV1,
    }
}

fn friendly_name(act: &IMFActivate) -> String {
    let mut buf = [0u16; 256];
    let mut len = 0u32;
    let ok = unsafe { act.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)) };
    if ok.is_ok() {
        let end = (len as usize).min(buf.len());
        String::from_utf16_lossy(&buf[..end])
            .trim_end_matches('\0')
            .to_string()
    } else {
        String::new()
    }
}

fn enumerate_mf_decoders(codec: VideoCodec) -> Result<Vec<(IMFActivate, String)>> {
    ensure_mf_startup()?;
    let in_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: codec_subtype(codec),
    };
    let flags = MFT_ENUM_FLAG(
        MFT_ENUM_FLAG_SYNCMFT.0
            | MFT_ENUM_FLAG_ASYNCMFT.0
            | MFT_ENUM_FLAG_HARDWARE.0
            | MFT_ENUM_FLAG_SORTANDFILTER.0,
    );
    let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            flags,
            Some(&in_info),
            None,
            &mut raw,
            &mut count,
        )
        .with_context(|| format!("MFTEnumEx decoder probe failed for {codec}"))?;
    }

    let mut out = Vec::new();
    if !raw.is_null() {
        unsafe {
            let slice = std::slice::from_raw_parts_mut(raw, count as usize);
            for slot in slice.iter_mut() {
                if let Some(act) = slot.take() {
                    let name = friendly_name(&act);
                    out.push((act, name));
                }
            }
            CoTaskMemFree(Some(raw.cast()));
        }
    }
    Ok(out)
}

pub fn probe_mf_decoders() -> Vec<VideoCodec> {
    static CACHED: OnceLock<Vec<VideoCodec>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            // Enumerate MF decoders on a short-lived MTA thread so calling this from the
            // winit main UI thread never switches the UI thread away from STA (OleInitialize).
            std::thread::Builder::new()
                .name("usher-mf-probe".into())
                .spawn(|| {
                    let mut out = Vec::new();
                    for codec in [VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::H264] {
                        if let Ok(list) = enumerate_mf_decoders(codec)
                            && !list.is_empty()
                        {
                            out.push(codec);
                        }
                    }
                    unsafe { CoUninitialize() };
                    out
                })
                .ok()
                .and_then(|h| h.join().ok())
                .unwrap_or_default()
        })
        .clone()
}

fn create_d3d11_video_device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut ctx = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .ok()?;
    }
    let device = device?;
    let ctx = ctx?;
    unsafe {
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
    Some((device, ctx))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Nv12,
    I420,
}

fn read_display_aperture(mt: &IMFMediaType) -> Option<(u32, u32)> {
    let mut buf = [0u8; std::mem::size_of::<MFVideoArea>()];
    for guid in [&MF_MT_MINIMUM_DISPLAY_APERTURE, &MF_MT_GEOMETRIC_APERTURE] {
        let mut sz = 0u32;
        if unsafe { mt.GetBlob(guid, &mut buf, Some(&mut sz)) }.is_ok()
            && sz as usize >= std::mem::size_of::<MFVideoArea>()
        {
            let area: MFVideoArea =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<MFVideoArea>()) };
            if area.Area.cx >= 2 && area.Area.cy >= 2 {
                return Some((area.Area.cx as u32, area.Area.cy as u32));
            }
        }
    }
    None
}

pub struct MfVideoDecoder {
    transform: IMFTransform,
    event_gen: Option<IMFMediaEventGenerator>,
    need_input_credit: usize,
    d3d: Option<(ID3D11Device, ID3D11DeviceContext)>,
    _dxgi_mgr: Option<IMFDXGIDeviceManager>,
    staging: Option<(ID3D11Texture2D, u32, u32, u32)>,
    width: u32,
    height: u32,
    coded_width: u32,
    coded_height: u32,
    output_fmt: OutputFormat,
    provides_samples: bool,
    output_buf_size: u32,
    pts_us: u64,
    on_frame: FrameSink,
}

unsafe impl Send for MfVideoDecoder {}

impl MfVideoDecoder {
    pub fn new(
        codec: VideoCodec,
        width: u16,
        height: u16,
        use_hwaccel: bool,
        on_frame: FrameSink,
    ) -> Result<Self> {
        let candidates = enumerate_mf_decoders(codec)?;
        if candidates.is_empty() {
            return Err(anyhow!("no Media Foundation decoder registered for {codec}"));
        }
        let mut last_err = anyhow!("no compatible MF decoder initialized");
        for (act, name) in candidates {
            match Self::try_init(act, &name, codec, width, height, use_hwaccel, on_frame.clone()) {
                Ok(dec) => {
                    tracing::info!(decoder = %name, codec = %codec, "initialized native Media Foundation decoder");
                    return Ok(dec);
                }
                Err(err) => {
                    tracing::debug!("MF decoder `{name}` init failed: {err:#}");
                    last_err = err;
                }
            }
        }
        Err(last_err)
    }

    fn try_init(
        act: IMFActivate,
        name: &str,
        codec: VideoCodec,
        width: u16,
        height: u16,
        use_hwaccel: bool,
        on_frame: FrameSink,
    ) -> Result<Self> {
        ensure_mf_startup()?;
        let transform: IMFTransform = unsafe {
            act.ActivateObject()
                .with_context(|| format!("ActivateObject({name})"))?
        };
        let mut is_async = false;
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            is_async = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
            if is_async {
                unsafe {
                    attrs
                        .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                        .context("MF_TRANSFORM_ASYNC_UNLOCK")?;
                }
            } else {
                let _ = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
            }
        }
        let event_gen = if is_async {
            transform.cast::<IMFMediaEventGenerator>().ok()
        } else {
            None
        };

        let d3d_aware = unsafe { transform.GetAttributes() }
            .ok()
            .and_then(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE) }.ok())
            .unwrap_or(0)
            != 0;
        let mut d3d = None;
        let mut dxgi_mgr = None;
        if use_hwaccel
            && d3d_aware
            && let Some((dev, ctx)) = create_d3d11_video_device()
        {
            let mut token = 0u32;
            let mut mgr = None;
            if unsafe { MFCreateDXGIDeviceManager(&mut token, &mut mgr) }.is_ok()
                && let Some(mgr) = mgr
                && unsafe { mgr.ResetDevice(&dev, token) }.is_ok()
                && unsafe {
                    transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)
                }
                .is_ok()
            {
                d3d = Some((dev, ctx));
                dxgi_mgr = Some(mgr);
            }
        }

        let w = (width as u32).max(2);
        let h = (height as u32).max(2);
        let in_mt = unsafe { MFCreateMediaType()? };
        unsafe {
            in_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_mt.SetGUID(&MF_MT_SUBTYPE, &codec_subtype(codec))?;
            in_mt.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(w, h))?;
            in_mt.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(60, 1))?;
            in_mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            in_mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
            transform
                .SetInputType(0, &in_mt, 0)
                .with_context(|| format!("SetInputType({codec})"))?;
        }

        let mut dec = Self {
            transform,
            event_gen,
            need_input_credit: 0,
            d3d,
            _dxgi_mgr: dxgi_mgr,
            staging: None,
            width: w,
            height: h,
            coded_width: w,
            coded_height: h,
            output_fmt: OutputFormat::Nv12,
            provides_samples: false,
            output_buf_size: w * h * 3 / 2,
            pts_us: 0,
            on_frame,
        };
        dec.select_output_type()?;
        unsafe {
            dec.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            dec.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(dec)
    }

    fn select_output_type(&mut self) -> Result<()> {
        let mut nv12_mt = None;
        let mut i420_mt = None;
        let mut idx = 0u32;
        while let Ok(mt) = unsafe { self.transform.GetOutputAvailableType(0, idx) } {
            idx += 1;
            let Ok(sub) = (unsafe { mt.GetGUID(&MF_MT_SUBTYPE) }) else {
                continue;
            };
            if sub == MFVideoFormat_NV12 && nv12_mt.is_none() {
                nv12_mt = Some((mt, OutputFormat::Nv12));
                break;
            } else if (sub == MFVideoFormat_IYUV || sub == MFVideoFormat_I420) && i420_mt.is_none()
            {
                i420_mt = Some((mt, OutputFormat::I420));
            }
        }
        let (chosen_mt, chosen_fmt) = nv12_mt
            .or(i420_mt)
            .ok_or_else(|| anyhow!("MF decoder offers neither NV12 nor IYUV/I420 output"))?;

        unsafe {
            self.transform
                .SetOutputType(0, &chosen_mt, 0)
                .context("SetOutputType")?;
        }
        self.output_fmt = chosen_fmt;
        if let Ok(cur) = unsafe { self.transform.GetOutputCurrentType(0) } {
            if let Ok(fs) = unsafe { cur.GetUINT64(&MF_MT_FRAME_SIZE) } {
                let (nw, nh) = unpack_u32_pair(fs);
                if nw >= 2 && nh >= 2 {
                    self.coded_width = nw;
                    self.coded_height = nh;
                }
            }
            if let Some((aw, ah)) = read_display_aperture(&cur) {
                self.width = aw;
                self.height = ah;
            } else {
                self.width = self.coded_width;
                self.height = self.coded_height;
            }
        }

        let stream_info = unsafe { self.transform.GetOutputStreamInfo(0)? };
        self.provides_samples = (stream_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
            != 0;
        self.output_buf_size = stream_info
            .cbSize
            .max(self.coded_width * self.coded_height * 3 / 2);
        Ok(())
    }

    fn pull_one_output(&mut self) -> Result<bool> {
        loop {
            let owned_sample = if self.provides_samples {
                None
            } else {
                unsafe {
                    let s = MFCreateSample()?;
                    let b = MFCreateMemoryBuffer(self.output_buf_size)?;
                    s.AddBuffer(&b)?;
                    Some(s)
                }
            };
            let mut out_buf = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(owned_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let res = unsafe { self.transform.ProcessOutput(0, &mut out_buf, &mut status) };
            let [out_buf] = out_buf;
            let sample_opt = ManuallyDrop::into_inner(out_buf.pSample);
            let _events = ManuallyDrop::into_inner(out_buf.pEvents);

            match res {
                Ok(()) => {
                    if let Some(sample) = sample_opt {
                        self.emit_sample(&sample)?;
                        return Ok(true);
                    }
                    return Ok(false);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.select_output_type()?;
                    continue;
                }
                Err(e) => return Err(anyhow!("MF decoder ProcessOutput failed: {e}")),
            }
        }
    }

    fn drain_outputs(&mut self) -> Result<bool> {
        let mut emitted_any = false;
        while self.pull_one_output()? {
            emitted_any = true;
        }
        Ok(emitted_any)
    }

    fn emit_sample(&mut self, sample: &IMFSample) -> Result<()> {
        let buf = unsafe { sample.GetBufferByIndex(0)? };

        if let Ok(dxgi_buf) = buf.cast::<IMFDXGIBuffer>()
            && let Some((ref dev, ref ctx)) = self.d3d
        {
            let mut tex_ptr: *mut c_void = std::ptr::null_mut();
            unsafe {
                dxgi_buf
                    .GetResource(&ID3D11Texture2D::IID, &mut tex_ptr)
                    .context("IMFDXGIBuffer::GetResource")?;
            }
            let src_tex = unsafe { ID3D11Texture2D::from_raw(tex_ptr) };
            let subidx = unsafe { dxgi_buf.GetSubresourceIndex().unwrap_or(0) };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { src_tex.GetDesc(&mut desc) };

            let need_new_staging = self.staging.as_ref().is_none_or(|(_, sw, sh, sfmt)| {
                *sw != desc.Width || *sh != desc.Height || *sfmt != desc.Format.0 as u32
            });
            if need_new_staging {
                let mut stg_desc = desc;
                stg_desc.Usage = D3D11_USAGE_STAGING;
                stg_desc.BindFlags = 0;
                stg_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                stg_desc.MiscFlags = 0;
                stg_desc.MipLevels = 1;
                stg_desc.ArraySize = 1;
                let mut staging = None;
                unsafe {
                    dev.CreateTexture2D(&stg_desc, None, Some(&mut staging))
                        .context("CreateTexture2D staging for MF decoder")?;
                }
                if let Some(st) = staging {
                    self.staging = Some((st, desc.Width, desc.Height, desc.Format.0 as u32));
                }
            }

            if let Some((ref staging, _, _, _)) = self.staging {
                unsafe {
                    ctx.CopySubresourceRegion(staging, 0, 0, 0, 0, &src_tex, subidx, None);
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                        .context("Map MF staging texture")?;
                    let pitch = mapped.RowPitch as usize;
                    let slice_len = pitch.saturating_mul(desc.Height as usize * 3 / 2);
                    let bytes = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), slice_len);
                    let frame = nv12_to_yuv420(
                        bytes,
                        pitch,
                        self.width.min(desc.Width),
                        self.height.min(desc.Height),
                        desc.Height as usize,
                    );
                    ctx.Unmap(staging, 0);
                    (self.on_frame)(frame);
                }
                return Ok(());
            }
        }

        if let Ok(buf2d) = buf.cast::<IMF2DBuffer>() {
            let mut ptr = std::ptr::null_mut();
            let mut pitch = 0i32;
            unsafe { buf2d.Lock2D(&mut ptr, &mut pitch)? };
            let stride = pitch.unsigned_abs() as usize;
            let coded_h = self.coded_height.max(self.height) as usize;
            let frame = if pitch > 0 && !ptr.is_null() {
                let slice_len = stride.saturating_mul(coded_h * 3 / 2);
                let bytes = unsafe { std::slice::from_raw_parts(ptr, slice_len) };
                match self.output_fmt {
                    OutputFormat::Nv12 => {
                        nv12_to_yuv420(bytes, stride, self.width, self.height, coded_h)
                    }
                    OutputFormat::I420 => i420_to_yuv420(
                        bytes,
                        stride,
                        self.width,
                        self.height,
                        coded_h as u32,
                    ),
                }
            } else {
                unsafe {
                    let _ = buf2d.Unlock2D();
                }
                return Ok(());
            };
            unsafe {
                let _ = buf2d.Unlock2D();
            }
            (self.on_frame)(frame);
            return Ok(());
        }

        unsafe {
            let mut ptr = std::ptr::null_mut();
            let mut cur_len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut cur_len))?;
            if !ptr.is_null() && cur_len > 0 {
                let bytes = std::slice::from_raw_parts(ptr, cur_len as usize);
                let stride = self.coded_width.max(self.width) as usize;
                let coded_h = self.coded_height.max(self.height) as usize;
                let needed = stride * coded_h * 3 / 2;
                if bytes.len() >= needed {
                    let frame = match self.output_fmt {
                        OutputFormat::Nv12 => nv12_to_yuv420(
                            bytes,
                            stride,
                            self.width,
                            self.height,
                            coded_h,
                        ),
                        OutputFormat::I420 => i420_to_yuv420(
                            bytes,
                            stride,
                            self.width,
                            self.height,
                            coded_h as u32,
                        ),
                    };
                    let _ = buf.Unlock();
                    (self.on_frame)(frame);
                    return Ok(());
                }
            }
            let _ = buf.Unlock();
        }
        Ok(())
    }
}

fn nv12_to_yuv420(
    nv12: &[u8],
    pitch: usize,
    width: u32,
    height: u32,
    uv_offset_rows: usize,
) -> Yuv420Frame {
    let w = (width as usize) & !1;
    let h = (height as usize) & !1;
    let cw = w / 2;
    let ch = h / 2;

    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];

    for row in 0..h {
        let src_off = row * pitch;
        if src_off + w <= nv12.len() {
            y[row * w..(row + 1) * w].copy_from_slice(&nv12[src_off..src_off + w]);
        }
    }

    let uv_base = uv_offset_rows * pitch;
    for row in 0..ch {
        let src_off = uv_base + row * pitch;
        if src_off + w <= nv12.len() {
            let src_row = &nv12[src_off..src_off + w];
            let u_row = &mut u[row * cw..(row + 1) * cw];
            let v_row = &mut v[row * cw..(row + 1) * cw];
            for col in 0..cw {
                u_row[col] = src_row[col * 2];
                v_row[col] = src_row[col * 2 + 1];
            }
        }
    }

    Yuv420Frame {
        width: w as u32,
        height: h as u32,
        y_stride: w as u32,
        u_stride: cw as u32,
        v_stride: cw as u32,
        y,
        u,
        v,
    }
}

fn i420_to_yuv420(
    i420: &[u8],
    y_stride: usize,
    width: u32,
    height: u32,
    coded_height: u32,
) -> Yuv420Frame {
    let w = (width as usize) & !1;
    let h = (height as usize) & !1;
    let cw = w / 2;
    let ch = h / 2;
    let c_stride = y_stride / 2;
    let coded_h = (coded_height as usize).max(h);

    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];

    for row in 0..h {
        let off = row * y_stride;
        if off + w <= i420.len() {
            y[row * w..(row + 1) * w].copy_from_slice(&i420[off..off + w]);
        }
    }
    let u_base = y_stride * coded_h;
    let v_base = u_base + c_stride * (coded_h / 2);
    for row in 0..ch {
        let u_off = u_base + row * c_stride;
        let v_off = v_base + row * c_stride;
        if u_off + cw <= i420.len() {
            u[row * cw..(row + 1) * cw].copy_from_slice(&i420[u_off..u_off + cw]);
        }
        if v_off + cw <= i420.len() {
            v[row * cw..(row + 1) * cw].copy_from_slice(&i420[v_off..v_off + cw]);
        }
    }

    Yuv420Frame {
        width: w as u32,
        height: h as u32,
        y_stride: w as u32,
        u_stride: cw as u32,
        v_stride: cw as u32,
        y,
        u,
        v,
    }
}

impl VideoDecoder for MfVideoDecoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<()> {
        if access_unit.is_empty() {
            return Ok(());
        }
        let sample = unsafe {
            let buf = MFCreateMemoryBuffer(access_unit.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buf.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(access_unit.as_ptr(), ptr, access_unit.len());
            buf.Unlock()?;
            buf.SetCurrentLength(access_unit.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            let _ = sample.SetSampleTime((self.pts_us as i64).saturating_mul(10));
            let _ = sample.SetSampleDuration(166_666);
            self.pts_us = self.pts_us.saturating_add(16_667);
            sample
        };

        if let Some(ev_gen) = self.event_gen.clone() {
            while self.need_input_credit == 0 {
                let ev = unsafe { ev_gen.GetEvent(MF_EVENT_FLAG_NONE)? };
                let ty = unsafe { ev.GetType()? };
                if ty == METransformNeedInput.0 as u32 {
                    self.need_input_credit += 1;
                } else if ty == METransformHaveOutput.0 as u32 {
                    let _ = self.pull_one_output()?;
                }
            }

            self.need_input_credit -= 1;
            unsafe {
                self.transform
                    .ProcessInput(0, &sample, 0)
                    .context("MF async decoder ProcessInput")?;
            }

            loop {
                let ev = unsafe { ev_gen.GetEvent(MF_EVENT_FLAG_NONE)? };
                let ty = unsafe { ev.GetType()? };
                if ty == METransformHaveOutput.0 as u32 {
                    let _ = self.pull_one_output()?;
                    while let Ok(ev2) = unsafe { ev_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                        let ty2 = unsafe { ev2.GetType()? };
                        if ty2 == METransformHaveOutput.0 as u32 {
                            let _ = self.pull_one_output()?;
                        } else if ty2 == METransformNeedInput.0 as u32 {
                            self.need_input_credit += 1;
                        }
                    }
                    break;
                } else if ty == METransformNeedInput.0 as u32 {
                    self.need_input_credit += 1;
                    break;
                }
            }
            return Ok(());
        }

        loop {
            let hr = unsafe { self.transform.ProcessInput(0, &sample, 0) };
            match hr {
                Ok(()) => break,
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    if !self.drain_outputs()? {
                        return Err(anyhow!(
                            "MF sync decoder returned MF_E_NOTACCEPTING with no output"
                        ));
                    }
                }
                Err(e) => return Err(anyhow!("MF decoder ProcessInput: {e}")),
            }
        }
        let _ = self.drain_outputs()?;
        Ok(())
    }
}

impl Drop for MfVideoDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::OpenH264API;
    use openh264::encoder::{
        BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, SpsPpsStrategy, UsageType,
    };
    use openh264::formats::YUVBuffer;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn mf_decoder_probes_and_decodes_h264() {
        let codecs = probe_mf_decoders();
        assert!(
            codecs.contains(&VideoCodec::H264),
            "Windows should always expose Microsoft H264 Video Decoder MFT, got {codecs:?}"
        );

        let frames = Arc::new(AtomicU64::new(0));
        let frames_cb = frames.clone();
        let sink: FrameSink = Arc::new(move |f| {
            assert_eq!((f.width, f.height), (160, 120));
            frames_cb.fetch_add(1, Ordering::Release);
        });
        let mut dec = MfVideoDecoder::new(VideoCodec::H264, 160, 120, false, sink)
            .expect("create MF H264 decoder");

        let cfg = EncoderConfig::new()
            .bitrate(BitRate::from_bps(500_000))
            .max_frame_rate(FrameRate::from_hz(30.0))
            .usage_type(UsageType::ScreenContentRealTime)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .intra_frame_period(IntraFramePeriod::from_num_frames(4));
        let mut enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).unwrap();

        let mut rgb = vec![120u8; 160 * 120 * 3];
        for i in 0..8usize {
            rgb.fill(((i * 25) % 240) as u8);
            let yuv = YUVBuffer::from_rgb_source(
                openh264::formats::RgbSliceU8::new(&rgb, (160, 120)),
            );
            let bs = enc.encode(&yuv).unwrap().to_vec();
            dec.decode(&bs).expect("decode H264 AU via MF");
        }
        unsafe {
            let _ = dec.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
        let _ = dec.drain_outputs();
        assert!(
            frames.load(Ordering::Acquire) >= 1,
            "expected >= 1 decoded frame from MF H264 decoder"
        );
    }

    #[test]
    fn probe_mf_decoders_keeps_caller_thread_sta_compatible() {
        let res = std::thread::spawn(|| {
            let codecs = probe_mf_decoders();
            assert!(codecs.contains(&VideoCodec::H264));
            let ole_hr = unsafe { windows::Win32::System::Ole::OleInitialize(None) };
            let ok = ole_hr.is_ok();
            if ok {
                unsafe { windows::Win32::System::Ole::OleUninitialize() };
            }
            (ok, ole_hr)
        })
        .join()
        .expect("probe STA thread panicked");
        assert!(
            res.0,
            "OleInitialize failed after probe_mf_decoders on caller thread: {:?}",
            res.1
        );
    }

    #[test]
    fn mf_decoder_decodes_hevc_when_available() {
        if !probe_mf_decoders().contains(&VideoCodec::Hevc) {
            return;
        }
        let Some(ffmpeg) = crate::decoder::find_working_ffmpeg() else {
            return;
        };

        let run_hevc_check = |use_hwaccel: bool| -> Option<u64> {
            let enc_variants: &[&[&str]] = &[
                &[
                    "-pix_fmt", "nv12", "-c:v", "hevc_nvenc", "-preset", "p1", "-tune", "ull",
                    "-zerolatency", "1", "-bf", "0", "-g", "4", "-forced-idr", "1", "-aud", "1",
                ],
                &[
                    "-pix_fmt", "yuv420p", "-c:v", "libx265", "-preset", "ultrafast", "-tune",
                    "zerolatency", "-x265-params", "keyint=4:bframes=0:aud=1",
                ],
            ];
            let mut raw = Vec::new();
            for enc_args in enc_variants {
                if let Ok(out) = std::process::Command::new(&ffmpeg)
                    .args([
                        "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                        "testsrc2=size=320x180:rate=30", "-frames:v", "12",
                    ])
                    .args(*enc_args)
                    .args(["-f", "hevc", "pipe:1"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                    && out.status.success()
                    && !out.stdout.is_empty()
                {
                    raw = out.stdout;
                    break;
                }
            }
            if raw.is_empty() {
                return None;
            }

            let mut splitter = usher_proto::annexb::AnnexBSplitter::new(VideoCodec::Hevc);
            let mut aus = splitter.push_bytes(&raw);
            aus.extend(splitter.flush());
            assert!(aus.len() >= 6, "expected >= 6 HEVC AUs, got {}", aus.len());

            let frames = Arc::new(AtomicU64::new(0));
            let frames_cb = frames.clone();
            let sink: FrameSink = Arc::new(move |f| {
                assert_eq!((f.width, f.height), (320, 180));
                frames_cb.fetch_add(1, Ordering::Release);
            });
            let mut dec = MfVideoDecoder::new(VideoCodec::Hevc, 320, 180, use_hwaccel, sink)
                .expect("create MF HEVC decoder");
            for au in &aus {
                dec.decode(&au.data).expect("decode HEVC AU via MF");
            }
            if dec.event_gen.is_none() {
                unsafe {
                    let _ = dec.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
                }
                let _ = dec.drain_outputs();
            }
            Some(frames.load(Ordering::Acquire))
        };

        let mut tested = false;
        for hw in [true, false] {
            if let Some(count) = run_hevc_check(hw) {
                tested = true;
                assert!(
                    count >= 1,
                    "expected >= 1 decoded HEVC frame (hwaccel={hw}), got {count}"
                );
            }
        }
        assert!(tested, "ffmpeg produced no HEVC test stream");
    }
}
