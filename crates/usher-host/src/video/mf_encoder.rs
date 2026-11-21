use super::EncoderKind;
use anyhow::{Context, Result, anyhow};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use usher_proto::annexb::is_keyframe_annexb;
use usher_proto::handshake::VideoCodec;
use windows::Win32::Foundation::{HMODULE, RPC_E_CHANGED_MODE, VARIANT_BOOL};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
    IMFActivate, IMFMediaBuffer, IMFMediaEventGenerator, IMFSample, IMFTransform,
    METransformHaveOutput, METransformNeedInput, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_EVENT_FLAG_NO_WAIT, MF_EVENT_FLAG_NONE, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MF_VERSION, MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFSTARTUP_NOSOCKET, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFVideoFormat_AV1,
    MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFTEnumEx, MFSampleExtension_CleanPoint, eAVEncCommonRateControlMode_CBR,
    eAVEncCommonRateControlMode_LowDelayVBR,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

const IID_ICODEC_API: GUID = GUID::from_u128(0x901db4c7_31ce_41a2_85dc_8fa0bf41b8da);

#[repr(C)]
#[allow(non_snake_case)]
struct ICodecApiVtbl {
    base: IUnknown_Vtbl,
    IsSupported: unsafe extern "system" fn(*mut c_void, *const GUID) -> HRESULT,
    IsModifiable: unsafe extern "system" fn(*mut c_void, *const GUID) -> HRESULT,
    GetParameterRange: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        *mut VARIANT,
        *mut VARIANT,
        *mut VARIANT,
    ) -> HRESULT,
    GetParameterValues: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        *mut *mut VARIANT,
        *mut u32,
    ) -> HRESULT,
    GetDefaultValue: unsafe extern "system" fn(*mut c_void, *const GUID, *mut VARIANT) -> HRESULT,
    GetValue: unsafe extern "system" fn(*mut c_void, *const GUID, *mut VARIANT) -> HRESULT,
    SetValue: unsafe extern "system" fn(*mut c_void, *const GUID, *mut VARIANT) -> HRESULT,
}

struct CodecApi(IUnknown);

impl CodecApi {
    fn from_transform(t: &IMFTransform) -> Option<Self> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        unsafe {
            let hr = t.query(&IID_ICODEC_API, &mut raw);
            if hr.is_ok() && !raw.is_null() {
                Some(Self(IUnknown::from_raw(raw)))
            } else {
                None
            }
        }
    }

    unsafe fn set_value(&self, api: &GUID, value: &mut VARIANT) -> HRESULT {
        unsafe {
            let raw = self.0.as_raw() as *mut *const ICodecApiVtbl;
            let vtbl = &**raw;
            (vtbl.SetValue)(raw.cast(), api as *const GUID, value as *mut VARIANT)
        }
    }
}

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

fn codec_subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::Hevc => MFVideoFormat_HEVC,
        VideoCodec::Av1 => MFVideoFormat_AV1,
    }
}

fn variant_u32(val: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: val },
            }),
        },
    }
}

fn variant_bool(val: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: VARIANT_BOOL(if val { -1 } else { 0 }),
                },
            }),
        },
    }
}

#[inline]
fn pack_u32_pair(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MftVendor {
    Nvenc,
    Amf,
    Qsv,
    Other,
}

impl MftVendor {
    fn from_name(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        if lower.contains("nvidia") {
            Self::Nvenc
        } else if lower.contains("amd") {
            Self::Amf
        } else if lower.contains("intel") {
            Self::Qsv
        } else {
            Self::Other
        }
    }

    fn dxgi_vendor_id(self) -> Option<u32> {
        match self {
            Self::Nvenc => Some(0x10DE),
            Self::Amf => Some(0x1002),
            Self::Qsv => Some(0x8086),
            Self::Other => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Nvenc => "nvidia",
            Self::Amf => "amd",
            Self::Qsv => "intel",
            Self::Other => "mf-hw",
        }
    }
}

fn vendor_matches(kind: EncoderKind, vendor: MftVendor, name: &str) -> bool {
    match kind {
        EncoderKind::Nvenc => vendor == MftVendor::Nvenc,
        EncoderKind::Amf => vendor == MftVendor::Amf,
        EncoderKind::Qsv => vendor == MftVendor::Qsv,
        EncoderKind::Openh264 | EncoderKind::Ffmpeg => false,
        EncoderKind::Auto | EncoderKind::Vaapi => {
            // Treat dedicated GPU MFTs and generic DX12 wrappers as valid, but sort
            // vendor-specific encoders ahead of generic Microsoft DX12 wrappers.
            let _ = name;
            true
        }
    }
}

struct EnumMft {
    activate: IMFActivate,
    name: String,
    vendor: MftVendor,
}

fn enumerate_hw_encoders(codec: VideoCodec, kind: EncoderKind) -> Result<Vec<EnumMft>> {
    ensure_mf_startup()?;
    if kind == EncoderKind::Openh264 {
        return Ok(Vec::new());
    }

    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: codec_subtype(codec),
    };
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);

    let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&out_info),
            &mut raw,
            &mut count,
        )
        .with_context(|| format!("MFTEnumEx hardware encoder probe failed for {codec}"))?;
    }

    let mut found = Vec::new();
    if !raw.is_null() {
        unsafe {
            let slice = std::slice::from_raw_parts_mut(raw, count as usize);
            for slot in slice.iter_mut() {
                if let Some(activate) = slot.take() {
                    let name = friendly_name(&activate);
                    let vendor = MftVendor::from_name(&name);
                    if vendor_matches(kind, vendor, &name) {
                        found.push(EnumMft {
                            activate,
                            name,
                            vendor,
                        });
                    }
                }
            }
            CoTaskMemFree(Some(raw.cast()));
        }
    }

    // Prefer dedicated vendor MFTs (NVENC > AMF > QSV > generic) for lowest latency.
    found.sort_by_key(|m| match m.vendor {
        MftVendor::Nvenc => 0u8,
        MftVendor::Amf => 1u8,
        MftVendor::Qsv => 2u8,
        MftVendor::Other => 3u8,
    });
    Ok(found)
}

pub fn probe_mf_hw_codecs(kind: EncoderKind) -> Vec<VideoCodec> {
    if matches!(kind, EncoderKind::Openh264 | EncoderKind::Ffmpeg) {
        return Vec::new();
    }
    let mut supported = Vec::new();
    for codec in [VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::H264] {
        if let Ok(list) = enumerate_hw_encoders(codec, kind)
            && !list.is_empty()
        {
            supported.push(codec);
        }
    }
    supported
}

fn find_dxgi_adapter_by_vendor(vendor_id: u32) -> Option<IDXGIAdapter1> {
    unsafe {
        let factory = CreateDXGIFactory1::<IDXGIFactory1>().ok()?;
        let mut idx = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(idx) {
            idx += 1;
            if let Ok(desc) = adapter.GetDesc1()
                && desc.VendorId == vendor_id
            {
                return Some(adapter);
            }
        }
        None
    }
}

fn create_matching_d3d11_device(vendor: MftVendor) -> Result<ID3D11Device> {
    let adapter = vendor.dxgi_vendor_id().and_then(find_dxgi_adapter_by_vendor);
    let flags = D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    let mut device = None;
    unsafe {
        if let Some(adp) = adapter.as_ref() {
            D3D11CreateDevice(
                adp,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .context("D3D11CreateDevice on vendor adapter failed")?;
        } else {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .context("D3D11CreateDevice hardware default failed")?;
        }
    }
    let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned null device"))?;
    unsafe {
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
    Ok(device)
}

pub struct MfHwEncoder {
    transform: IMFTransform,
    codec_api: Option<CodecApi>,
    event_gen: Option<IMFMediaEventGenerator>,
    _dxgi_mgr: Option<windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager>,
    _d3d_device: Option<ID3D11Device>,
    codec: VideoCodec,
    width: u32,
    height: u32,
    fps: u32,
    provides_samples: bool,
    output_buf_size: u32,
    need_input_credit: usize,
    pending_keyframe: bool,
    emitted_first_frame: bool,
    encoder_name: String,
    vendor_label: &'static str,
}

unsafe impl Send for MfHwEncoder {}

impl MfHwEncoder {
    pub fn new(
        codec: VideoCodec,
        encoder_kind: EncoderKind,
        width: u16,
        height: u16,
        fps: u16,
        bitrate_kbps: u32,
        prefer_low_latency: bool,
    ) -> Result<Self> {
        let candidates = enumerate_hw_encoders(codec, encoder_kind)?;
        if candidates.is_empty() {
            return Err(anyhow!(
                "no Media Foundation hardware encoder found for {codec} ({encoder_kind:?})"
            ));
        }

        let mut last_err = anyhow!("no compatible MF hardware encoder initialized");
        for cand in candidates {
            match Self::try_init_candidate(
                cand,
                codec,
                width,
                height,
                fps,
                bitrate_kbps,
                prefer_low_latency,
            ) {
                Ok(enc) => return Ok(enc),
                Err(err) => {
                    tracing::debug!("MF hardware encoder candidate failed: {err:#}");
                    last_err = err;
                }
            }
        }
        Err(last_err)
    }

    fn try_init_candidate(
        cand: EnumMft,
        codec: VideoCodec,
        width: u16,
        height: u16,
        fps: u16,
        bitrate_kbps: u32,
        prefer_low_latency: bool,
    ) -> Result<Self> {
        ensure_mf_startup()?;
        let w = (width as u32) & !1;
        let h = (height as u32) & !1;
        let fps = (fps as u32).max(1);
        let br_bps = bitrate_kbps.max(500).saturating_mul(1000);

        let transform: IMFTransform = unsafe {
            cand.activate
                .ActivateObject()
                .with_context(|| format!("ActivateObject({}) failed", cand.name))?
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

        let d3d_device = create_matching_d3d11_device(cand.vendor).ok();
        let mut dxgi_mgr = None;
        if let Some(ref dev) = d3d_device {
            let mut reset_token = 0u32;
            let mut mgr = None;
            if unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut mgr) }.is_ok()
                && let Some(mgr) = mgr
                && unsafe { mgr.ResetDevice(dev, reset_token) }.is_ok()
            {
                let _ = unsafe {
                    transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)
                };
                dxgi_mgr = Some(mgr);
            }
        }

        let codec_api = CodecApi::from_transform(&transform);
        if let Some(ref api) = codec_api {
            unsafe {
                let mut rc_cbr = variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32);
                if api
                    .set_value(&CODECAPI_AVEncCommonRateControlMode, &mut rc_cbr)
                    .is_err()
                {
                    let mut rc_vbr = variant_u32(eAVEncCommonRateControlMode_LowDelayVBR.0 as u32);
                    let _ = api.set_value(&CODECAPI_AVEncCommonRateControlMode, &mut rc_vbr);
                }
                let mut ll = variant_bool(prefer_low_latency);
                let _ = api.set_value(&CODECAPI_AVLowLatencyMode, &mut ll);

                let mut mean_br = variant_u32(br_bps);
                let _ = api.set_value(&CODECAPI_AVEncCommonMeanBitRate, &mut mean_br);

                let gop = if prefer_low_latency {
                    fps.max(60)
                } else {
                    fps.saturating_mul(2).max(120)
                };
                let mut gop_var = variant_u32(gop);
                let _ = api.set_value(&CODECAPI_AVEncMPVGOPSize, &mut gop_var);
            }
        }

        // Configure output media type first (standard MF encoder convention).
        let out_mt = unsafe { MFCreateMediaType()? };
        unsafe {
            out_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_mt.SetGUID(&MF_MT_SUBTYPE, &codec_subtype(codec))?;
            out_mt.SetUINT32(&MF_MT_AVG_BITRATE, br_bps)?;
            out_mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_mt.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps, 1))?;
            out_mt.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(w, h))?;
            out_mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
            transform
                .SetOutputType(0, &out_mt, 0)
                .with_context(|| format!("SetOutputType({codec} {w}x{h}@{fps})"))?;
        }

        // Configure NV12 input media type, checking GetInputAvailableType first.
        let mut input_set = false;
        let mut idx = 0u32;
        while let Ok(avail) = unsafe { transform.GetInputAvailableType(0, idx) } {
            idx += 1;
            if let Ok(sub) = unsafe { avail.GetGUID(&MF_MT_SUBTYPE) }
                && sub == MFVideoFormat_NV12
            {
                unsafe {
                    let _ = avail.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(w, h));
                    let _ = avail.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps, 1));
                    let _ = avail.SetUINT32(
                        &MF_MT_INTERLACE_MODE,
                        MFVideoInterlace_Progressive.0 as u32,
                    );
                    let _ = avail.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1));
                    if transform.SetInputType(0, &avail, 0).is_ok() {
                        input_set = true;
                        break;
                    }
                }
            }
        }
        if !input_set {
            let in_mt = unsafe { MFCreateMediaType()? };
            unsafe {
                in_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                in_mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                in_mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                in_mt.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps, 1))?;
                in_mt.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(w, h))?;
                in_mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
                transform
                    .SetInputType(0, &in_mt, 0)
                    .context("SetInputType(NV12)")?;
            }
        }

        let stream_info = unsafe { transform.GetOutputStreamInfo(0)? };
        let provides_samples = (stream_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
            != 0;
        let output_buf_size = stream_info.cbSize.max(512 * 1024);

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM")?;
        }

        Ok(Self {
            transform,
            codec_api,
            event_gen,
            _dxgi_mgr: dxgi_mgr,
            _d3d_device: d3d_device,
            codec,
            width: w,
            height: h,
            fps,
            provides_samples,
            output_buf_size,
            need_input_credit: 0,
            pending_keyframe: false,
            emitted_first_frame: false,
            encoder_name: cand.name,
            vendor_label: cand.vendor.label(),
        })
    }

    pub fn encoder_name(&self) -> &str {
        &self.encoder_name
    }

    pub fn vendor_label(&self) -> &'static str {
        self.vendor_label
    }

    /// Updates the target encoder bitrate in-place with zero pipeline stall.
    pub fn set_bitrate_kbps(&mut self, kbps: u32) -> bool {
        let Some(ref api) = self.codec_api else {
            return false;
        };
        let br_bps = kbps.max(500).saturating_mul(1000);
        let mut br = variant_u32(br_bps);
        let hr = unsafe { api.set_value(&CODECAPI_AVEncCommonMeanBitRate, &mut br) };
        if hr.is_err() {
            tracing::warn!(
                ?hr,
                encoder = %self.encoder_name,
                kbps,
                "CODECAPI_AVEncCommonMeanBitRate failed"
            );
            return false;
        }
        let mut max_br = variant_u32(br_bps.saturating_mul(3) / 2);
        let _ = unsafe { api.set_value(&CODECAPI_AVEncCommonMaxBitRate, &mut max_br) };
        true
    }

    fn apply_pending_keyframe(&mut self) {
        if !self.pending_keyframe {
            return;
        }
        self.pending_keyframe = false;
        if let Some(ref api) = self.codec_api {
            let mut one = variant_u32(1);
            let hr = unsafe { api.set_value(&CODECAPI_AVEncVideoForceKeyFrame, &mut one) };
            if hr.is_err() {
                tracing::debug!(
                    ?hr,
                    encoder = %self.encoder_name,
                    "CODECAPI_AVEncVideoForceKeyFrame failed"
                );
            }
        }
    }

    fn build_nv12_sample(&self, nv12_tight: &[u8], pts_us: u64) -> Result<IMFSample> {
        let expected = (self.width as usize) * (self.height as usize) * 3 / 2;
        if nv12_tight.len() < expected {
            return Err(anyhow!(
                "NV12 input buffer too short: {} < {expected}",
                nv12_tight.len()
            ));
        }
        unsafe {
            let buf: IMFMediaBuffer = MFCreateMemoryBuffer(expected as u32)?;
            let mut ptr = std::ptr::null_mut();
            buf.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(nv12_tight.as_ptr(), ptr, expected);
            buf.Unlock()?;
            buf.SetCurrentLength(expected as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            // 100-nanosecond ticks.
            let _ = sample.SetSampleTime((pts_us as i64).saturating_mul(10));
            let _ = sample.SetSampleDuration((10_000_000i64) / (self.fps as i64));
            Ok(sample)
        }
    }

    fn pull_one_output(&mut self) -> Result<Option<(Vec<u8>, bool)>> {
        unsafe {
            let owned_sample = if self.provides_samples {
                None
            } else {
                let sample = MFCreateSample()?;
                let buf = MFCreateMemoryBuffer(self.output_buf_size)?;
                sample.AddBuffer(&buf)?;
                Some(sample)
            };

            let mut out_buf = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(owned_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let res = self.transform.ProcessOutput(0, &mut out_buf, &mut status);
            let [out_buf] = out_buf;
            let sample_opt = ManuallyDrop::into_inner(out_buf.pSample);
            let _events = ManuallyDrop::into_inner(out_buf.pEvents);

            match res {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) => return Err(anyhow!("MF encoder ProcessOutput failed: {e}")),
            }

            let Some(sample) = sample_opt else {
                return Ok(None);
            };
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut cur_len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut cur_len))?;
            let data = if !ptr.is_null() && cur_len > 0 {
                std::slice::from_raw_parts(ptr, cur_len as usize).to_vec()
            } else {
                Vec::new()
            };
            let _ = buf.Unlock();

            if data.is_empty() {
                return Ok(None);
            }

            let clean_point = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let keyframe = clean_point
                || !self.emitted_first_frame
                || matches!(self.codec, VideoCodec::H264 | VideoCodec::Hevc)
                    && is_keyframe_annexb(self.codec, &data);
            self.emitted_first_frame = true;
            Ok(Some((data, keyframe)))
        }
    }

    pub fn encode_nv12(
        &mut self,
        nv12_tight: &[u8],
        pts_us: u64,
        force_idr: bool,
    ) -> Result<Vec<(Vec<u8>, bool)>> {
        if force_idr {
            self.pending_keyframe = true;
        }
        let sample = self.build_nv12_sample(nv12_tight, pts_us)?;
        let mut outputs = Vec::new();

        if let Some(ev_gen) = self.event_gen.clone() {
            // Wait until the async MFT signals it can accept input, draining any output along the way.
            while self.need_input_credit == 0 {
                let ev = unsafe { ev_gen.GetEvent(MF_EVENT_FLAG_NONE)? };
                let ty = unsafe { ev.GetType()? };
                if ty == METransformNeedInput.0 as u32 {
                    self.need_input_credit += 1;
                } else if ty == METransformHaveOutput.0 as u32
                    && let Some(pkt) = self.pull_one_output()?
                {
                    outputs.push(pkt);
                }
            }

            self.need_input_credit -= 1;
            self.apply_pending_keyframe();
            unsafe {
                self.transform
                    .ProcessInput(0, &sample, 0)
                    .context("MF async ProcessInput failed")?;
            }

            // Wait for at least one output packet (or a new NeedInput credit during initial priming).
            loop {
                let ev = unsafe { ev_gen.GetEvent(MF_EVENT_FLAG_NONE)? };
                let ty = unsafe { ev.GetType()? };
                if ty == METransformHaveOutput.0 as u32 {
                    if let Some(pkt) = self.pull_one_output()? {
                        outputs.push(pkt);
                    }
                    // Drain any immediately queued output/need-input events without blocking.
                    while let Ok(ev2) = unsafe { ev_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                        let ty2 = unsafe { ev2.GetType()? };
                        if ty2 == METransformHaveOutput.0 as u32
                            && let Some(pkt) = self.pull_one_output()?
                        {
                            outputs.push(pkt);
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
        } else {
            self.apply_pending_keyframe();
            loop {
                let hr = unsafe { self.transform.ProcessInput(0, &sample, 0) };
                match hr {
                    Ok(()) => break,
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        if let Some(pkt) = self.pull_one_output()? {
                            outputs.push(pkt);
                        } else {
                            return Err(anyhow!("MF sync encoder returned MF_E_NOTACCEPTING with no output"));
                        }
                    }
                    Err(e) => return Err(anyhow!("MF sync ProcessInput failed: {e}")),
                }
            }
            while let Some(pkt) = self.pull_one_output()? {
                outputs.push(pkt);
            }
        }

        Ok(outputs)
    }
}

impl Drop for MfHwEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_nv12(w: u16, h: u16, step: u8) -> Vec<u8> {
        let w = w as usize;
        let h = h as usize;
        let mut buf = vec![0u8; w * h * 3 / 2];
        for y in 0..h {
            for x in 0..w {
                buf[y * w + x] = ((x + y + step as usize * 7) % 210 + 16) as u8;
            }
        }
        buf[w * h..].fill(128);
        buf
    }

    #[test]
    fn mf_probe_and_encode_h264_and_hevc() {
        let probed = probe_mf_hw_codecs(EncoderKind::Auto);
        if probed.is_empty() {
            return;
        }
        for codec in [VideoCodec::H264, VideoCodec::Hevc] {
            if !probed.contains(&codec) {
                continue;
            }
            let Ok(mut enc) =
                MfHwEncoder::new(codec, EncoderKind::Auto, 640, 360, 60, 4000, true)
            else {
                continue;
            };
            let mut total_bytes = 0usize;
            let mut saw_keyframe = false;
            let mut forced_keyframe = false;
            for idx in 0..8u64 {
                let frame = synthetic_nv12(640, 360, idx as u8);
                if idx == 3 {
                    let _ = enc.set_bitrate_kbps(6500);
                }
                let pkts = enc
                    .encode_nv12(&frame, idx * 16_666, idx == 0 || idx == 3)
                    .expect("encode_nv12");
                for (bytes, kf) in pkts {
                    total_bytes += bytes.len();
                    saw_keyframe |= kf;
                    if idx >= 3 {
                        forced_keyframe |= kf;
                    }
                }
            }
            assert!(total_bytes > 200, "expected compressed {codec} output");
            assert!(saw_keyframe, "expected at least one {codec} keyframe");
            assert!(
                forced_keyframe,
                "expected forced keyframe at/after idx 3 for {codec}"
            );
        }
    }
}
