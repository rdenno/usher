#![cfg(target_os = "macos")]

use crate::decoder::{FrameSink, VideoDecoder, Yuv420Frame};
use anyhow::{Result, anyhow, bail};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use usher_proto::handshake::VideoCodec;

type OSStatus = i32;
type CFAllocatorRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFNumberRef = *const c_void;
type CFTypeRef = *const c_void;
type CMFormatDescriptionRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type VTDecompressionSessionRef = *mut c_void;
type CVImageBufferRef = *mut c_void;
type CVPixelBufferRef = *mut c_void;

const K_CF_NUMBER_SINT32_TYPE: isize = 3;
const K_CV_PIXEL_FORMAT_NV12_VIDEO_RANGE: i32 = 0x3432_3076; // '420v'

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    decompression_output_callback: Option<
        unsafe extern "C" fn(
            decompression_output_refcon: *mut c_void,
            source_frame_refcon: *mut c_void,
            status: OSStatus,
            info_flags: u32,
            image_buffer: CVImageBufferRef,
            presentation_ts: CMTime,
            presentation_duration: CMTime,
        ),
    >,
    decompression_output_refcon: *mut c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFAllocatorNull: CFAllocatorRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;

    fn CFRelease(cf: CFTypeRef);
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: isize,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        num_values: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFTypeRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFTypeRef;

    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: CVPixelBufferRef, unlock_flags: u64) -> i32;
    fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> *const u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> usize;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        extensions: CFDictionaryRef,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        num_samples: isize,
        num_sample_timing_entries: isize,
        sample_timing_array: *const c_void,
        num_sample_size_entries: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
}

pub fn probe_vt_decoders() -> Vec<VideoCodec> {
    vec![VideoCodec::Hevc, VideoCodec::H264]
}

unsafe extern "C" fn vt_output_callback(
    refcon: *mut c_void,
    _source_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: CVImageBufferRef,
    _pts: CMTime,
    _dur: CMTime,
) {
    if status != 0 || image_buffer.is_null() || refcon.is_null() {
        return;
    }
    let sink = unsafe { &*(refcon as *const FrameSink) };
    let lock_res = unsafe { CVPixelBufferLockBaseAddress(image_buffer, 1) };
    if lock_res != 0 {
        return;
    }

    let w = unsafe { CVPixelBufferGetWidth(image_buffer) };
    let h = unsafe { CVPixelBufferGetHeight(image_buffer) };
    let y_ptr = unsafe { CVPixelBufferGetBaseAddressOfPlane(image_buffer, 0) };
    let uv_ptr = unsafe { CVPixelBufferGetBaseAddressOfPlane(image_buffer, 1) };
    let y_pitch = unsafe { CVPixelBufferGetBytesPerRowOfPlane(image_buffer, 0) };
    let uv_pitch = unsafe { CVPixelBufferGetBytesPerRowOfPlane(image_buffer, 1) };

    if !y_ptr.is_null() && !uv_ptr.is_null() && w >= 2 && h >= 2 && y_pitch >= w && uv_pitch >= w
    {
        let cw = w / 2;
        let ch = h / 2;
        let mut y = Vec::with_capacity(w * h);
        for row in 0..h {
            let line = unsafe { std::slice::from_raw_parts(y_ptr.add(row * y_pitch), w) };
            y.extend_from_slice(line);
        }

        let mut u = Vec::with_capacity(cw * ch);
        let mut v = Vec::with_capacity(cw * ch);
        for row in 0..ch {
            let uv_line = unsafe { std::slice::from_raw_parts(uv_ptr.add(row * uv_pitch), cw * 2) };
            for pair in uv_line.as_chunks::<2>().0 {
                u.push(pair[0]);
                v.push(pair[1]);
            }
        }

        (sink)(Yuv420Frame {
            width: w as u32,
            height: h as u32,
            y_stride: w as u32,
            u_stride: cw as u32,
            v_stride: cw as u32,
            y,
            u,
            v,
        });
    }

    unsafe {
        let _ = CVPixelBufferUnlockBaseAddress(image_buffer, 1);
    }
}

pub struct VideoToolboxDecoder {
    codec: VideoCodec,
    sink_ptr: *mut FrameSink,
    fmt_desc: CMFormatDescriptionRef,
    session: VTDecompressionSessionRef,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    vps: Option<Vec<u8>>,
}

unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    pub fn new(codec: VideoCodec, on_frame: FrameSink) -> Result<Self> {
        if !matches!(codec, VideoCodec::H264 | VideoCodec::Hevc) {
            bail!("VideoToolbox decoder supports H.264 and HEVC (got {codec})");
        }
        Ok(Self {
            codec,
            sink_ptr: Box::into_raw(Box::new(on_frame)),
            fmt_desc: null_mut(),
            session: null_mut(),
            sps: None,
            pps: None,
            vps: None,
        })
    }

    fn destroy_session(&mut self) {
        unsafe {
            if !self.session.is_null() {
                let _ = VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session as CFTypeRef);
                self.session = null_mut();
            }
            if !self.fmt_desc.is_null() {
                CFRelease(self.fmt_desc as CFTypeRef);
                self.fmt_desc = null_mut();
            }
        }
    }

    fn rebuild_session(&mut self) -> Result<()> {
        self.destroy_session();

        let mut param_ptrs: Vec<*const u8> = Vec::with_capacity(3);
        let mut param_sizes: Vec<usize> = Vec::with_capacity(3);
        match self.codec {
            VideoCodec::H264 => {
                let Some(ref sps) = self.sps else {
                    return Ok(());
                };
                let Some(ref pps) = self.pps else {
                    return Ok(());
                };
                param_ptrs.push(sps.as_ptr());
                param_sizes.push(sps.len());
                param_ptrs.push(pps.as_ptr());
                param_sizes.push(pps.len());
            }
            VideoCodec::Hevc => {
                let (Some(vps), Some(sps), Some(pps)) = (&self.vps, &self.sps, &self.pps) else {
                    return Ok(());
                };
                param_ptrs.push(vps.as_ptr());
                param_sizes.push(vps.len());
                param_ptrs.push(sps.as_ptr());
                param_sizes.push(sps.len());
                param_ptrs.push(pps.as_ptr());
                param_sizes.push(pps.len());
            }
            VideoCodec::Av1 => bail!("AV1 not supported in VideoToolboxDecoder"),
        }

        let mut fmt_desc: CMFormatDescriptionRef = null_mut();
        let fmt_status = unsafe {
            match self.codec {
                VideoCodec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    null(),
                    param_ptrs.len(),
                    param_ptrs.as_ptr(),
                    param_sizes.as_ptr(),
                    4,
                    &mut fmt_desc,
                ),
                VideoCodec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    null(),
                    param_ptrs.len(),
                    param_ptrs.as_ptr(),
                    param_sizes.as_ptr(),
                    4,
                    null(),
                    &mut fmt_desc,
                ),
                VideoCodec::Av1 => unreachable!(),
            }
        };
        if fmt_status != 0 || fmt_desc.is_null() {
            return Err(anyhow!(
                "CMVideoFormatDescriptionCreateFromParameterSets failed ({fmt_status})"
            ));
        }

        let pix_fmt_val = K_CV_PIXEL_FORMAT_NV12_VIDEO_RANGE;
        let pix_num = unsafe {
            CFNumberCreate(
                null(),
                K_CF_NUMBER_SINT32_TYPE,
                (&pix_fmt_val as *const i32).cast(),
            )
        };
        let empty_dict = unsafe {
            CFDictionaryCreate(
                null(),
                null(),
                null(),
                0,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            )
        };
        let keys = unsafe { [kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferIOSurfacePropertiesKey] };
        let vals = [pix_num, empty_dict];
        let attrs = unsafe {
            CFDictionaryCreate(
                null(),
                keys.as_ptr(),
                vals.as_ptr(),
                2,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            )
        };

        let cb = VTDecompressionOutputCallbackRecord {
            decompression_output_callback: Some(vt_output_callback),
            decompression_output_refcon: self.sink_ptr.cast(),
        };

        let mut session: VTDecompressionSessionRef = null_mut();
        let sess_status = unsafe {
            VTDecompressionSessionCreate(null(), fmt_desc, null(), attrs, &cb, &mut session)
        };

        unsafe {
            if !attrs.is_null() {
                CFRelease(attrs);
            }
            if !empty_dict.is_null() {
                CFRelease(empty_dict);
            }
            if !pix_num.is_null() {
                CFRelease(pix_num);
            }
        }

        if sess_status != 0 || session.is_null() {
            unsafe { CFRelease(fmt_desc as CFTypeRef) };
            return Err(anyhow!("VTDecompressionSessionCreate failed ({sess_status})"));
        }

        self.fmt_desc = fmt_desc;
        self.session = session;
        Ok(())
    }
}

impl VideoDecoder for VideoToolboxDecoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<()> {
        let mut params_changed = false;
        let mut avcc = Vec::with_capacity(access_unit.len() + 16);

        for nal in split_annexb_nals(access_unit) {
            if nal.is_empty() {
                continue;
            }
            match self.codec {
                VideoCodec::H264 => match nal[0] & 0x1f {
                    7 => {
                        if self.sps.as_deref() != Some(nal) {
                            self.sps = Some(nal.to_vec());
                            params_changed = true;
                        }
                        continue;
                    }
                    8 => {
                        if self.pps.as_deref() != Some(nal) {
                            self.pps = Some(nal.to_vec());
                            params_changed = true;
                        }
                        continue;
                    }
                    9 => continue,
                    _ => {}
                },
                VideoCodec::Hevc => match (nal[0] >> 1) & 0x3f {
                    32 => {
                        if self.vps.as_deref() != Some(nal) {
                            self.vps = Some(nal.to_vec());
                            params_changed = true;
                        }
                        continue;
                    }
                    33 => {
                        if self.sps.as_deref() != Some(nal) {
                            self.sps = Some(nal.to_vec());
                            params_changed = true;
                        }
                        continue;
                    }
                    34 => {
                        if self.pps.as_deref() != Some(nal) {
                            self.pps = Some(nal.to_vec());
                            params_changed = true;
                        }
                        continue;
                    }
                    35 => continue,
                    _ => {}
                },
                VideoCodec::Av1 => return Ok(()),
            }

            avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            avcc.extend_from_slice(nal);
        }

        if params_changed || self.session.is_null() {
            self.rebuild_session()?;
        }
        if self.session.is_null() || avcc.is_empty() {
            return Ok(());
        }

        let mut block_buf: CMBlockBufferRef = null_mut();
        let bb_status = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                null(),
                avcc.as_mut_ptr().cast(),
                avcc.len(),
                kCFAllocatorNull,
                null(),
                0,
                avcc.len(),
                0,
                &mut block_buf,
            )
        };
        if bb_status != 0 || block_buf.is_null() {
            bail!("CMBlockBufferCreateWithMemoryBlock failed ({bb_status})");
        }

        let mut sample_buf: CMSampleBufferRef = null_mut();
        let sample_size = avcc.len();
        let sb_status = unsafe {
            CMSampleBufferCreateReady(
                null(),
                block_buf,
                self.fmt_desc,
                1,
                0,
                null(),
                1,
                &sample_size,
                &mut sample_buf,
            )
        };
        unsafe { CFRelease(block_buf as CFTypeRef) };
        if sb_status != 0 || sample_buf.is_null() {
            bail!("CMSampleBufferCreateReady failed ({sb_status})");
        }

        let mut info_flags = 0u32;
        let dec_status = unsafe {
            VTDecompressionSessionDecodeFrame(self.session, sample_buf, 0, null_mut(), &mut info_flags)
        };
        unsafe {
            let _ = VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            CFRelease(sample_buf as CFTypeRef);
        }
        if dec_status != 0 {
            bail!("VTDecompressionSessionDecodeFrame failed ({dec_status})");
        }
        Ok(())
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        self.destroy_session();
        if !self.sink_ptr.is_null() {
            unsafe { drop(Box::from_raw(self.sink_ptr)) };
            self.sink_ptr = null_mut();
        }
    }
}

fn split_annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut nal_start: Option<usize> = None;
    while i + 2 < data.len() {
        let sc_len = if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                3
            } else if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                4
            } else {
                0
            }
        } else {
            0
        };
        if sc_len > 0 {
            if let Some(s) = nal_start {
                out.push(&data[s..i]);
            }
            i += sc_len;
            nal_start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = nal_start {
        out.push(&data[s..]);
    }
    out
}
