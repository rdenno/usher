use anyhow::{Context, Result, anyhow, bail};
use libloading::{Library, Symbol};
use std::ffi::c_void;
use usher_proto::annexb::is_keyframe_annexb;
use usher_proto::handshake::{QualityPreference, VideoCodec};

type CUresult = libc::c_int;
type CUdevice = libc::c_int;
type CUcontext = *mut c_void;
type CUdeviceptr = libc::c_ulonglong;
#[allow(clippy::upper_case_acronyms)]
type NVENCSTATUS = libc::c_int;

const CUDA_SUCCESS: CUresult = 0;
const NV_ENC_SUCCESS: NVENCSTATUS = 0;
const CU_MEMORYTYPE_HOST: u32 = 1;
const CU_MEMORYTYPE_DEVICE: u32 = 2;

const NV_ENC_DEVICE_TYPE_CUDA: u32 = 1;
const NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR: u32 = 1;
const NV_ENC_BUFFER_FORMAT_NV12: u32 = 1;
const NV_ENC_BUFFER_FORMAT_YUV444: u32 = 0x0000_1000;
const NV_ENC_INPUT_IMAGE: u32 = 0;

const NV_ENC_PARAMS_RC_CBR: u32 = 2;
const NV_ENC_TUNING_INFO_LOW_LATENCY: u32 = 2;
const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;

const NV_ENC_PIC_STRUCT_FRAME: u32 = 1;
const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
const NV_ENC_PIC_TYPE_I: u32 = 2;
const NV_ENC_PIC_TYPE_IDR: u32 = 3;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

const GUID_CODEC_H264: Guid = Guid {
    data1: 0x6bc8_2762,
    data2: 0x4e63,
    data3: 0x4ca4,
    data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
};
const GUID_CODEC_HEVC: Guid = Guid {
    data1: 0x790c_dc88,
    data2: 0x4522,
    data3: 0x4d7b,
    data4: [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
};
const GUID_CODEC_AV1: Guid = Guid {
    data1: 0x0a35_2289,
    data2: 0x0aa7,
    data3: 0x4759,
    data4: [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
};
const GUID_PRESET_P1: Guid = Guid {
    data1: 0xfc0a_8d3e,
    data2: 0x45f8,
    data3: 0x4cf8,
    data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
};
const GUID_PRESET_P4: Guid = Guid {
    data1: 0x90a7_b826,
    data2: 0xdf06,
    data3: 0x4862,
    data4: [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
};
const GUID_H264_PROFILE_HIGH_444: Guid = Guid {
    data1: 0x7ac6_63cb,
    data2: 0xa598,
    data3: 0x4960,
    data4: [0xb8, 0x44, 0x33, 0x9b, 0x26, 0x1a, 0x7d, 0x52],
};
const GUID_HEVC_PROFILE_FREXT: Guid = Guid {
    data1: 0x51ec_32b5,
    data2: 0x1b4c,
    data3: 0x453c,
    data4: [0x9c, 0xbd, 0xb6, 0x16, 0xbd, 0x62, 0x13, 0x41],
};
const GUID_ZERO: Guid = Guid {
    data1: 0,
    data2: 0,
    data3: 0,
    data4: [0; 8],
};

#[inline]
pub const fn nvenc_api_version(major: u32, minor: u32) -> u32 {
    major | (minor << 24)
}

#[inline]
pub const fn nvenc_struct_version(api_ver: u32, struct_ver: u32, flag_msb: bool) -> u32 {
    let base = api_ver | (struct_ver << 16) | (0x7 << 28);
    if flag_msb { base | (1u32 << 31) } else { base }
}

#[allow(non_camel_case_types, dead_code)]
#[repr(C)]
struct CUDA_MEMCPY2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: *const c_void,
    src_device: CUdeviceptr,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: *mut c_void,
    dst_device: CUdeviceptr,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

type FnCuInit = unsafe extern "C" fn(u32) -> CUresult;
type FnCuDeviceGet = unsafe extern "C" fn(*mut CUdevice, libc::c_int) -> CUresult;
type FnCuDevicePrimaryCtxRetain = unsafe extern "C" fn(*mut CUcontext, CUdevice) -> CUresult;
type FnCuDevicePrimaryCtxRelease = unsafe extern "C" fn(CUdevice) -> CUresult;
type FnCuCtxPushCurrent = unsafe extern "C" fn(CUcontext) -> CUresult;
type FnCuCtxPopCurrent = unsafe extern "C" fn(*mut CUcontext) -> CUresult;
type FnCuMemAllocPitch =
    unsafe extern "C" fn(*mut CUdeviceptr, *mut usize, usize, usize, u32) -> CUresult;
type FnCuMemcpy2D = unsafe extern "C" fn(*const CUDA_MEMCPY2D) -> CUresult;
type FnCuMemFree = unsafe extern "C" fn(CUdeviceptr) -> CUresult;

#[allow(dead_code)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NvEncQp {
    qp_inter_p: u32,
    qp_inter_b: u32,
    qp_intra: u32,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncRcParams {
    version: u32,
    rate_control_mode: u32,
    const_qp: NvEncQp,
    average_bit_rate: u32,
    max_bit_rate: u32,
    vbv_buffer_size: u32,
    vbv_initial_delay: u32,
    bit_flags: u32,
    min_qp: NvEncQp,
    max_qp: NvEncQp,
    initial_rc_qp: NvEncQp,
    temporal_layer_idx_mask: u32,
    temporal_layer_qp: [u8; 8],
    target_quality: u8,
    target_quality_lsb: u8,
    lookahead_depth: u16,
    low_delay_key_frame_scale: u8,
    reserved1: [u8; 3],
    qp_map_mode: u32,
    multi_pass: u32,
    alpha_layer_bitrate_ratio: u32,
    cb_qp_index_offset: i8,
    cr_qp_index_offset: i8,
    reserved2: u16,
    reserved: [u32; 4],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncVuiParams {
    overscan_info_present_flag: u32,
    overscan_info: u32,
    video_signal_type_present_flag: u32,
    video_format: u32,
    video_full_range_flag: u32,
    colour_description_present_flag: u32,
    colour_primaries: u32,
    transfer_characteristics: u32,
    colour_matrix: u32,
    chroma_sample_location_flag: u32,
    chroma_sample_location_top: u32,
    chroma_sample_location_bot: u32,
    bitstream_restriction_flag: u32,
    reserved: [u32; 15],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncConfigH264 {
    bit_flags: u32,
    level: u32,
    idr_period: u32,
    separate_colour_plane_flag: u32,
    disable_deblocking_filter_idc: u32,
    num_temporal_layers: u32,
    sps_id: u32,
    pps_id: u32,
    adaptive_transform_mode: u32,
    fmo_mode: u32,
    bdirect_mode: u32,
    entropy_coding_mode: u32,
    stereo_mode: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames: u32,
    slice_mode: u32,
    slice_mode_data: u32,
    vui: NvEncVuiParams,
    ltr_num_frames: u32,
    ltr_trust_mode: u32,
    chroma_format_idc: u32,
    max_temporal_layers: u32,
    use_bframes_as_ref: u32,
    num_ref_l0: u32,
    num_ref_l1: u32,
    reserved1: [u32; 267],
    reserved2: [*mut c_void; 64],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncConfigHevc {
    level: u32,
    tier: u32,
    min_cu_size: u32,
    max_cu_size: u32,
    bit_flags: u32,
    idr_period: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames_in_dpb: u32,
    ltr_num_frames: u32,
    vps_id: u32,
    sps_id: u32,
    pps_id: u32,
    slice_mode: u32,
    slice_mode_data: u32,
    max_temporal_layers_minus1: u32,
    vui: NvEncVuiParams,
    ltr_trust_mode: u32,
    use_bframes_as_ref: u32,
    num_ref_l0: u32,
    num_ref_l1: u32,
    reserved1: [u32; 214],
    reserved2: [*mut c_void; 64],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncConfigAv1 {
    level: u32,
    tier: u32,
    min_part_size: u32,
    max_part_size: u32,
    bit_flags: u32,
    idr_period: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames_in_dpb: u32,
    num_tile_columns: u32,
    num_tile_rows: u32,
    tile_widths: *mut u32,
    tile_heights: *mut u32,
    max_temporal_layers_minus1: u32,
    color_primaries: u32,
    transfer_characteristics: u32,
    matrix_coefficients: u32,
    color_range: u32,
    chroma_sample_position: u32,
    use_bframes_as_ref: u32,
    film_grain_params: *mut c_void,
    num_fwd_refs: u32,
    num_bwd_refs: u32,
    reserved1: [u32; 235],
    reserved2: [*mut c_void; 62],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
union NvEncCodecConfig {
    h264: NvEncConfigH264,
    hevc: NvEncConfigHevc,
    av1: NvEncConfigAv1,
    raw_u32: [u32; 320],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncConfig {
    version: u32,
    profile_guid: Guid,
    gop_length: u32,
    frame_interval_p: i32,
    mono_chrome_encoding: u32,
    frame_field_mode: u32,
    mv_precision: u32,
    rc_params: NvEncRcParams,
    encode_codec_config: NvEncCodecConfig,
    reserved: [u32; 278],
    reserved2: [*mut c_void; 64],
}

#[allow(dead_code)]
#[repr(C)]
struct NvEncPresetConfig {
    version: u32,
    preset_cfg: NvEncConfig,
    reserved1: [u32; 255],
    reserved2: [*mut c_void; 64],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncInitializeParams {
    version: u32,
    encode_guid: Guid,
    preset_guid: Guid,
    encode_width: u32,
    encode_height: u32,
    dar_width: u32,
    dar_height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    enable_encode_async: u32,
    enable_ptd: u32,
    bit_flags: u32,
    priv_data_size: u32,
    priv_data: *mut c_void,
    encode_config: *mut NvEncConfig,
    max_encode_width: u32,
    max_encode_height: u32,
    max_me_hint_counts_per_block: [[u32; 4]; 2],
    tuning_info: u32,
    buffer_format: u32,
    num_state_buffers: u32,
    output_stats_level: u32,
    reserved: [u32; 285],
    reserved2: [*mut c_void; 64],
}

#[allow(dead_code)]
#[repr(C)]
struct NvEncReconfigureParams {
    version: u32,
    re_init_encode_params: NvEncInitializeParams,
    flags: u32,
}

#[allow(dead_code)]
#[repr(C)]
struct NvEncOpenEncodeSessionExParams {
    version: u32,
    device_type: u32,
    device: *mut c_void,
    reserved: *mut c_void,
    api_version: u32,
    reserved1: [u32; 253],
    reserved2: [*mut c_void; 64],
}

#[allow(dead_code)]
#[repr(C)]
struct NvEncCreateBitstreamBuffer {
    version: u32,
    size: u32,
    memory_heap: u32,
    reserved: u32,
    bitstream_buffer: *mut c_void,
    bitstream_buffer_ptr: *mut c_void,
    reserved1: [u32; 58],
    reserved2: [*mut c_void; 64],
}

#[allow(dead_code)]
#[repr(C)]
struct NvEncRegisterResource {
    version: u32,
    resource_type: u32,
    width: u32,
    height: u32,
    pitch: u32,
    sub_resource_index: u32,
    resource_to_register: *mut c_void,
    registered_resource: *mut c_void,
    buffer_format: u32,
    buffer_usage: u32,
    p_input_fence_point: *mut c_void,
    p_output_fence_point: *mut c_void,
    reserved1: [u32; 247],
    reserved2: [*mut c_void; 60],
}

#[allow(dead_code)]
#[repr(C)]
struct NvEncMapInputResource {
    version: u32,
    sub_resource_index: u32,
    input_resource: *mut c_void,
    registered_resource: *mut c_void,
    mapped_resource: *mut c_void,
    mapped_buffer_fmt: u32,
    reserved1: [u32; 251],
    reserved2: [*mut c_void; 63],
}

#[allow(dead_code)]
#[repr(C)]
struct NvEncPicParams {
    version: u32,
    input_width: u32,
    input_height: u32,
    input_pitch: u32,
    encode_pic_flags: u32,
    frame_idx: u32,
    input_time_stamp: u64,
    input_duration: u64,
    input_buffer: *mut c_void,
    output_bitstream: *mut c_void,
    completion_event: *mut c_void,
    buffer_fmt: u32,
    picture_struct: u32,
    picture_type: u32,
    codec_pic_params: [u32; 320],
    me_hint_counts_per_block: [[u32; 4]; 2],
    me_external_hints: *mut c_void,
    reserved1: [u32; 6],
    reserved2: [*mut c_void; 2],
    qp_delta_map: *mut i8,
    qp_delta_map_size: u32,
    reserved_bit_fields: u32,
    me_hint_ref_pic_dist: [u16; 2],
    alpha_buffer: *mut c_void,
    me_external_sb_hints: *mut c_void,
    me_sb_hints_count: u32,
    state_buffer_idx: u32,
    output_recon_buffer: *mut c_void,
    reserved3: [u32; 284],
    reserved4: [*mut c_void; 57],
}

#[allow(dead_code)]
#[repr(C)]
struct NvEncLockBitstream {
    version: u32,
    bit_flags: u32,
    output_bitstream: *mut c_void,
    slice_offsets: *mut u32,
    frame_idx: u32,
    hw_encode_status: u32,
    num_slices: u32,
    bitstream_size_in_bytes: u32,
    output_time_stamp: u64,
    output_duration: u64,
    bitstream_buffer_ptr: *mut c_void,
    picture_type: u32,
    picture_struct: u32,
    frame_avg_qp: u32,
    frame_satd: u32,
    ltr_frame_idx: u32,
    ltr_frame_bitmap: u32,
    temporal_id: u32,
    reserved: [u32; 12],
    intra_mb_count: u32,
    inter_mb_count: u32,
    average_mv_x: i32,
    average_mv_y: i32,
    alpha_layer_size_in_bytes: u32,
    reserved1: [u32; 218],
    reserved2: [*mut c_void; 64],
}

type FnNvEncGetMaxSupportedVersion = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;
type FnNvEncCreateInstance = unsafe extern "C" fn(*mut NvEncApiFunctionList) -> NVENCSTATUS;

type FnNvEncOpenEncodeSession = unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void) -> NVENCSTATUS;
type FnNvEncGetEncodeGuidCount = unsafe extern "C" fn(*mut c_void, *mut u32) -> NVENCSTATUS;
type FnNvEncGetEncodeGuids = unsafe extern "C" fn(*mut c_void, *mut Guid, u32, *mut u32) -> NVENCSTATUS;
type FnNvEncGetEncodePresetConfig =
    unsafe extern "C" fn(*mut c_void, Guid, Guid, *mut NvEncPresetConfig) -> NVENCSTATUS;
type FnNvEncInitializeEncoder =
    unsafe extern "C" fn(*mut c_void, *mut NvEncInitializeParams) -> NVENCSTATUS;
type FnNvEncCreateBitstreamBuffer =
    unsafe extern "C" fn(*mut c_void, *mut NvEncCreateBitstreamBuffer) -> NVENCSTATUS;
type FnNvEncDestroyBitstreamBuffer = unsafe extern "C" fn(*mut c_void, *mut c_void) -> NVENCSTATUS;
type FnNvEncEncodePicture = unsafe extern "C" fn(*mut c_void, *mut NvEncPicParams) -> NVENCSTATUS;
type FnNvEncLockBitstream = unsafe extern "C" fn(*mut c_void, *mut NvEncLockBitstream) -> NVENCSTATUS;
type FnNvEncUnlockBitstream = unsafe extern "C" fn(*mut c_void, *mut c_void) -> NVENCSTATUS;
type FnNvEncMapInputResource =
    unsafe extern "C" fn(*mut c_void, *mut NvEncMapInputResource) -> NVENCSTATUS;
type FnNvEncUnmapInputResource = unsafe extern "C" fn(*mut c_void, *mut c_void) -> NVENCSTATUS;
type FnNvEncDestroyEncoder = unsafe extern "C" fn(*mut c_void) -> NVENCSTATUS;
type FnNvEncOpenEncodeSessionEx =
    unsafe extern "C" fn(*mut NvEncOpenEncodeSessionExParams, *mut *mut c_void) -> NVENCSTATUS;
type FnNvEncRegisterResource =
    unsafe extern "C" fn(*mut c_void, *mut NvEncRegisterResource) -> NVENCSTATUS;
type FnNvEncUnregisterResource = unsafe extern "C" fn(*mut c_void, *mut c_void) -> NVENCSTATUS;
type FnNvEncReconfigureEncoder =
    unsafe extern "C" fn(*mut c_void, *mut NvEncReconfigureParams) -> NVENCSTATUS;
type FnNvEncGetEncodePresetConfigEx = unsafe extern "C" fn(
    *mut c_void,
    Guid,
    Guid,
    u32,
    *mut NvEncPresetConfig,
) -> NVENCSTATUS;

#[allow(dead_code)]
#[repr(C)]
struct NvEncApiFunctionList {
    version: u32,
    reserved: u32,
    nv_enc_open_encode_session: Option<FnNvEncOpenEncodeSession>,
    nv_enc_get_encode_guid_count: Option<FnNvEncGetEncodeGuidCount>,
    nv_enc_get_encode_profile_guid_count: *mut c_void,
    nv_enc_get_encode_profile_guids: *mut c_void,
    nv_enc_get_encode_guids: Option<FnNvEncGetEncodeGuids>,
    nv_enc_get_input_format_count: *mut c_void,
    nv_enc_get_input_formats: *mut c_void,
    nv_enc_get_encode_caps: *mut c_void,
    nv_enc_get_encode_preset_count: *mut c_void,
    nv_enc_get_encode_preset_guids: *mut c_void,
    nv_enc_get_encode_preset_config: Option<FnNvEncGetEncodePresetConfig>,
    nv_enc_initialize_encoder: Option<FnNvEncInitializeEncoder>,
    nv_enc_create_input_buffer: *mut c_void,
    nv_enc_destroy_input_buffer: *mut c_void,
    nv_enc_create_bitstream_buffer: Option<FnNvEncCreateBitstreamBuffer>,
    nv_enc_destroy_bitstream_buffer: Option<FnNvEncDestroyBitstreamBuffer>,
    nv_enc_encode_picture: Option<FnNvEncEncodePicture>,
    nv_enc_lock_bitstream: Option<FnNvEncLockBitstream>,
    nv_enc_unlock_bitstream: Option<FnNvEncUnlockBitstream>,
    nv_enc_lock_input_buffer: *mut c_void,
    nv_enc_unlock_input_buffer: *mut c_void,
    nv_enc_get_encode_stats: *mut c_void,
    nv_enc_get_sequence_params: *mut c_void,
    nv_enc_register_async_event: *mut c_void,
    nv_enc_unregister_async_event: *mut c_void,
    nv_enc_map_input_resource: Option<FnNvEncMapInputResource>,
    nv_enc_unmap_input_resource: Option<FnNvEncUnmapInputResource>,
    nv_enc_destroy_encoder: Option<FnNvEncDestroyEncoder>,
    nv_enc_invalidate_ref_frames: *mut c_void,
    nv_enc_open_encode_session_ex: Option<FnNvEncOpenEncodeSessionEx>,
    nv_enc_register_resource: Option<FnNvEncRegisterResource>,
    nv_enc_unregister_resource: Option<FnNvEncUnregisterResource>,
    nv_enc_reconfigure_encoder: Option<FnNvEncReconfigureEncoder>,
    reserved1: *mut c_void,
    nv_enc_create_mv_buffer: *mut c_void,
    nv_enc_destroy_mv_buffer: *mut c_void,
    nv_enc_run_motion_estimation_only: *mut c_void,
    nv_enc_get_last_error_string: *mut c_void,
    nv_enc_set_io_cuda_streams: *mut c_void,
    nv_enc_get_encode_preset_config_ex: Option<FnNvEncGetEncodePresetConfigEx>,
    nv_enc_get_sequence_param_ex: *mut c_void,
    nv_enc_restore_encoder_state: *mut c_void,
    nv_enc_lookahead_picture: *mut c_void,
    reserved2: [*mut c_void; 275],
}

impl Default for NvEncApiFunctionList {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

struct CudaLib {
    _lib: Library,
    cu_init: FnCuInit,
    cu_device_get: FnCuDeviceGet,
    cu_device_primary_ctx_retain: FnCuDevicePrimaryCtxRetain,
    cu_device_primary_ctx_release: FnCuDevicePrimaryCtxRelease,
    cu_ctx_push_current: FnCuCtxPushCurrent,
    cu_ctx_pop_current: FnCuCtxPopCurrent,
    cu_mem_alloc_pitch: FnCuMemAllocPitch,
    cu_memcpy_2d: FnCuMemcpy2D,
    cu_mem_free: FnCuMemFree,
}

unsafe impl Send for CudaLib {}
unsafe impl Sync for CudaLib {}

impl CudaLib {
    fn load() -> Result<Self> {
        let lib = unsafe { Library::new("libcuda.so.1") }.context("loading libcuda.so.1")?;
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let s: Symbol<'_, $ty> = unsafe { lib.get($name) }
                    .with_context(|| format!("missing CUDA symbol {:?}", $name))?;
                *s
            }};
        }
        Ok(Self {
            cu_init: sym!(b"cuInit\0", FnCuInit),
            cu_device_get: sym!(b"cuDeviceGet\0", FnCuDeviceGet),
            cu_device_primary_ctx_retain: sym!(b"cuDevicePrimaryCtxRetain\0", FnCuDevicePrimaryCtxRetain),
            cu_device_primary_ctx_release: sym!(b"cuDevicePrimaryCtxRelease\0", FnCuDevicePrimaryCtxRelease),
            cu_ctx_push_current: sym!(b"cuCtxPushCurrent_v2\0", FnCuCtxPushCurrent),
            cu_ctx_pop_current: sym!(b"cuCtxPopCurrent_v2\0", FnCuCtxPopCurrent),
            cu_mem_alloc_pitch: sym!(b"cuMemAllocPitch_v2\0", FnCuMemAllocPitch),
            cu_memcpy_2d: sym!(b"cuMemcpy2D_v2\0", FnCuMemcpy2D),
            cu_mem_free: sym!(b"cuMemFree_v2\0", FnCuMemFree),
            _lib: lib,
        })
    }
}

struct NvencLib {
    _lib: Library,
    max_ver: FnNvEncGetMaxSupportedVersion,
    create_instance: FnNvEncCreateInstance,
}

unsafe impl Send for NvencLib {}
unsafe impl Sync for NvencLib {}

impl NvencLib {
    fn load() -> Result<Self> {
        let lib = unsafe { Library::new("libnvidia-encode.so.1") }
            .context("loading libnvidia-encode.so.1")?;
        let s_max: Symbol<'_, FnNvEncGetMaxSupportedVersion> =
            unsafe { lib.get(b"NvEncodeAPIGetMaxSupportedVersion\0") }
                .context("missing NvEncodeAPIGetMaxSupportedVersion")?;
        let s_create: Symbol<'_, FnNvEncCreateInstance> =
            unsafe { lib.get(b"NvEncodeAPICreateInstance\0") }
                .context("missing NvEncodeAPICreateInstance")?;
        Ok(Self {
            max_ver: *s_max,
            create_instance: *s_create,
            _lib: lib,
        })
    }
}

fn pick_nvenc_api_version(nv_lib: &NvencLib, prefer_v12: bool) -> (u32, bool) {
    let _ = prefer_v12;
    let mut driver_ver = 0u32;
    let st = unsafe { (nv_lib.max_ver)(&raw mut driver_ver) };
    let max_maj = driver_ver >> 4;
    let max_min = driver_ver & 0x0f;
    if st == NV_ENC_SUCCESS && (max_maj > 12 || (max_maj == 12 && max_min >= 1)) {
        (nvenc_api_version(12, 1), true)
    } else {
        (nvenc_api_version(11, 1), false)
    }
}

struct TempNvencSession {
    cuda: CudaLib,
    nv: NvencLib,
    dev: CUdevice,
    cu_ctx: CUcontext,
    fn_list: NvEncApiFunctionList,
    enc: *mut c_void,
    api_ver: u32,
    is_v12: bool,
}

fn open_temp_cuda_nvenc(prefer_v12: bool) -> Result<TempNvencSession> {
    let cuda = CudaLib::load()?;
    let nv = NvencLib::load()?;
    if unsafe { (cuda.cu_init)(0) } != CUDA_SUCCESS {
        bail!("cuInit(0) failed");
    }
    let mut dev = 0;
    if unsafe { (cuda.cu_device_get)(&raw mut dev, 0) } != CUDA_SUCCESS {
        bail!("cuDeviceGet(0) failed");
    }
    let mut cu_ctx: CUcontext = std::ptr::null_mut();
    if unsafe { (cuda.cu_device_primary_ctx_retain)(&raw mut cu_ctx, dev) } != CUDA_SUCCESS
        || cu_ctx.is_null()
    {
        bail!("cuDevicePrimaryCtxRetain failed");
    }

    let (api_ver, is_v12) = pick_nvenc_api_version(&nv, prefer_v12);
    let mut fn_list = NvEncApiFunctionList {
        version: nvenc_struct_version(api_ver, 2, false),
        ..Default::default()
    };
    let st = unsafe { (nv.create_instance)(&raw mut fn_list) };
    if st != NV_ENC_SUCCESS {
        unsafe {
            let _ = (cuda.cu_device_primary_ctx_release)(dev);
        }
        bail!("NvEncodeAPICreateInstance failed (status={st})");
    }

    let open_ex = fn_list
        .nv_enc_open_encode_session_ex
        .ok_or_else(|| anyhow!("missing nvEncOpenEncodeSessionEx"))?;
    let mut open_params: NvEncOpenEncodeSessionExParams = unsafe { std::mem::zeroed() };
    open_params.version = nvenc_struct_version(api_ver, 1, false);
    open_params.device_type = NV_ENC_DEVICE_TYPE_CUDA;
    open_params.device = cu_ctx;
    open_params.api_version = api_ver;

    let mut enc: *mut c_void = std::ptr::null_mut();
    let st = unsafe { open_ex(&raw mut open_params, &raw mut enc) };
    if st != NV_ENC_SUCCESS || enc.is_null() {
        unsafe {
            let _ = (cuda.cu_device_primary_ctx_release)(dev);
        }
        bail!("nvEncOpenEncodeSessionEx failed (status={st})");
    }

    Ok(TempNvencSession {
        cuda,
        nv,
        dev,
        cu_ctx,
        fn_list,
        enc,
        api_ver,
        is_v12,
    })
}

/// Probes runtime NVENC encode GUIDs without keeping CUDA/NVENC state open.
pub fn probe_nvenc_codecs() -> std::result::Result<Vec<VideoCodec>, String> {
    let sess = open_temp_cuda_nvenc(true).map_err(|e| format!("{e:#}"))?;
    let cuda = sess.cuda;
    let dev = sess.dev;
    let fn_list = sess.fn_list;
    let enc = sess.enc;
    let mut out = Vec::new();
    if let (Some(get_cnt), Some(get_guids)) = (
        fn_list.nv_enc_get_encode_guid_count,
        fn_list.nv_enc_get_encode_guids,
    ) {
        let mut count = 0u32;
        if unsafe { get_cnt(enc, &raw mut count) } == NV_ENC_SUCCESS && count > 0 {
            let mut guids = vec![GUID_ZERO; count as usize];
            let mut written = 0u32;
            if unsafe { get_guids(enc, guids.as_mut_ptr(), count, &raw mut written) }
                == NV_ENC_SUCCESS
            {
                guids.truncate(written as usize);
                if guids.contains(&GUID_CODEC_HEVC) {
                    out.push(VideoCodec::Hevc);
                }
                if guids.contains(&GUID_CODEC_AV1) {
                    out.push(VideoCodec::Av1);
                }
                if guids.contains(&GUID_CODEC_H264) {
                    out.push(VideoCodec::H264);
                }
            }
        }
    }
    if let Some(destroy) = fn_list.nv_enc_destroy_encoder {
        unsafe {
            let _ = destroy(enc);
        }
    }
    unsafe {
        let _ = (cuda.cu_device_primary_ctx_release)(dev);
    }
    if out.is_empty() {
        return Err("NVENC opened session but reported no supported encode GUIDs".into());
    }
    Ok(out)
}

pub struct NvencEncoder {
    cuda: CudaLib,
    _nv: NvencLib,
    dev: CUdevice,
    cu_ctx: CUcontext,
    fn_list: NvEncApiFunctionList,
    enc: *mut c_void,
    api_ver: u32,
    is_v12: bool,
    codec: VideoCodec,
    chroma: usher_proto::handshake::ChromaSubsampling,
    width: u16,
    height: u16,
    fps: u16,
    bitrate_kbps: u32,
    enc_cfg: Box<NvEncConfig>,
    init_params: Box<NvEncInitializeParams>,
    cu_ptr: CUdeviceptr,
    cu_pitch: usize,
    registered_res: *mut c_void,
    bitstream_buf: *mut c_void,
    frame_idx: u32,
}

unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    pub fn new(
        codec: VideoCodec,
        chroma: usher_proto::handshake::ChromaSubsampling,
        width: u16,
        height: u16,
        fps: u16,
        bitrate_kbps: u32,
        prefer: QualityPreference,
    ) -> Result<Self> {
        let desired_chroma = if matches!(codec, VideoCodec::H264 | VideoCodec::Hevc) {
            chroma
        } else {
            usher_proto::handshake::ChromaSubsampling::Yuv420
        };
        let sess = open_temp_cuda_nvenc(codec == VideoCodec::Av1)?;

        match Self::finish_init(
            sess.cuda,
            sess.nv,
            sess.dev,
            sess.cu_ctx,
            sess.fn_list,
            sess.enc,
            sess.api_ver,
            sess.is_v12,
            codec,
            desired_chroma,
            width,
            height,
            fps,
            bitrate_kbps,
            prefer,
        ) {
            Ok(enc) => Ok(enc),
            Err(err) if desired_chroma == usher_proto::handshake::ChromaSubsampling::Yuv444 => {
                tracing::warn!(
                    %codec,
                    "NVENC YUV 4:4:4 initialization failed ({err:#}); falling back to YUV 4:2:0"
                );
                let fallback_sess = open_temp_cuda_nvenc(false)?;
                Self::finish_init(
                    fallback_sess.cuda,
                    fallback_sess.nv,
                    fallback_sess.dev,
                    fallback_sess.cu_ctx,
                    fallback_sess.fn_list,
                    fallback_sess.enc,
                    fallback_sess.api_ver,
                    fallback_sess.is_v12,
                    codec,
                    usher_proto::handshake::ChromaSubsampling::Yuv420,
                    width,
                    height,
                    fps,
                    bitrate_kbps,
                    prefer,
                )
            }
            Err(err) => Err(err),
        }
    }

    pub fn chroma(&self) -> usher_proto::handshake::ChromaSubsampling {
        self.chroma
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_init(
        cuda: CudaLib,
        nv: NvencLib,
        dev: CUdevice,
        cu_ctx: CUcontext,
        fn_list: NvEncApiFunctionList,
        enc: *mut c_void,
        api_ver: u32,
        is_v12: bool,
        codec: VideoCodec,
        chroma: usher_proto::handshake::ChromaSubsampling,
        width: u16,
        height: u16,
        fps: u16,
        bitrate_kbps: u32,
        prefer: QualityPreference,
    ) -> Result<Self> {
        let cleanup_session = |enc: *mut c_void| {
            if let Some(destroy) = fn_list.nv_enc_destroy_encoder {
                unsafe {
                    let _ = destroy(enc);
                }
            }
            unsafe {
                let _ = (cuda.cu_device_primary_ctx_release)(dev);
            }
        };

        let encode_guid = match codec {
            VideoCodec::H264 => GUID_CODEC_H264,
            VideoCodec::Hevc => GUID_CODEC_HEVC,
            VideoCodec::Av1 => GUID_CODEC_AV1,
        };
        if codec == VideoCodec::Av1 && !is_v12 {
            cleanup_session(enc);
            bail!("NVENC driver API < 12.1 does not support AV1");
        }

        let preset_guid = match prefer {
            QualityPreference::Latency => GUID_PRESET_P1,
            QualityPreference::Quality => GUID_PRESET_P4,
        };
        let tuning = match prefer {
            QualityPreference::Latency => NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            QualityPreference::Quality => NV_ENC_TUNING_INFO_LOW_LATENCY,
        };

        let mut preset_cfg: NvEncPresetConfig = unsafe { std::mem::zeroed() };
        let cfg_struct_ver = if is_v12 { 8 } else { 7 };
        let init_struct_ver = if is_v12 { 6 } else { 5 };
        preset_cfg.version = nvenc_struct_version(api_ver, 4, true);
        preset_cfg.preset_cfg.version = nvenc_struct_version(api_ver, cfg_struct_ver, true);

        let st_preset = if let Some(get_ex) = fn_list.nv_enc_get_encode_preset_config_ex {
            unsafe { get_ex(enc, encode_guid, preset_guid, tuning, &raw mut preset_cfg) }
        } else if let Some(get_old) = fn_list.nv_enc_get_encode_preset_config {
            unsafe { get_old(enc, encode_guid, preset_guid, &raw mut preset_cfg) }
        } else {
            -1
        };
        if st_preset != NV_ENC_SUCCESS {
            cleanup_session(enc);
            bail!("nvEncGetEncodePresetConfig failed (codec={codec}, status={st_preset})");
        }

        let mut enc_cfg = Box::new(preset_cfg.preset_cfg);
        enc_cfg.version = nvenc_struct_version(api_ver, cfg_struct_ver, true);
        let gop = match prefer {
            QualityPreference::Latency => (fps as u32).max(30),
            QualityPreference::Quality => (fps as u32 * 2).max(60),
        };
        enc_cfg.gop_length = gop;
        enc_cfg.frame_interval_p = 1; // IPPP zero B-frame reordering delay
        enc_cfg.rc_params.version = nvenc_struct_version(api_ver, 1, false);
        enc_cfg.rc_params.rate_control_mode = NV_ENC_PARAMS_RC_CBR;
        let bps = bitrate_kbps.max(500).saturating_mul(1000);
        enc_cfg.rc_params.average_bit_rate = bps;
        enc_cfg.rc_params.max_bit_rate = bps;
        enc_cfg.rc_params.vbv_buffer_size = (bps / fps.max(1) as u32).max(64_000);
        enc_cfg.rc_params.vbv_initial_delay = enc_cfg.rc_params.vbv_buffer_size;
        enc_cfg.rc_params.bit_flags |= 1 << 9; // zeroReorderDelay = 1

        let use_yuv444 = chroma == usher_proto::handshake::ChromaSubsampling::Yuv444
            && matches!(codec, VideoCodec::H264 | VideoCodec::Hevc);
        let buf_fmt = if use_yuv444 {
            NV_ENC_BUFFER_FORMAT_YUV444
        } else {
            NV_ENC_BUFFER_FORMAT_NV12
        };

        match codec {
            VideoCodec::H264 => unsafe {
                // repeatSPSPPS = bit 12
                enc_cfg.encode_codec_config.h264.bit_flags |= 1 << 12;
                enc_cfg.encode_codec_config.h264.idr_period = gop;
                if use_yuv444 {
                    enc_cfg.profile_guid = GUID_H264_PROFILE_HIGH_444;
                    enc_cfg.encode_codec_config.h264.chroma_format_idc = 3;
                }
                enc_cfg.encode_codec_config.h264.vui.video_signal_type_present_flag = 1;
                enc_cfg.encode_codec_config.h264.vui.colour_description_present_flag = 1;
                enc_cfg.encode_codec_config.h264.vui.colour_primaries = 1;
                enc_cfg.encode_codec_config.h264.vui.transfer_characteristics = 1;
                enc_cfg.encode_codec_config.h264.vui.colour_matrix = 1;
            },
            VideoCodec::Hevc => unsafe {
                // repeatSPSPPS = bit 7; chromaFormatIDC = bits 9..10 (3 = 4:4:4)
                enc_cfg.encode_codec_config.hevc.bit_flags |= 1 << 7;
                if use_yuv444 {
                    enc_cfg.profile_guid = GUID_HEVC_PROFILE_FREXT;
                    enc_cfg.encode_codec_config.hevc.bit_flags =
                        (enc_cfg.encode_codec_config.hevc.bit_flags & !(0b11 << 9)) | (3 << 9);
                }
                enc_cfg.encode_codec_config.hevc.idr_period = gop;
                enc_cfg.encode_codec_config.hevc.vui.video_signal_type_present_flag = 1;
                enc_cfg.encode_codec_config.hevc.vui.colour_description_present_flag = 1;
                enc_cfg.encode_codec_config.hevc.vui.colour_primaries = 1;
                enc_cfg.encode_codec_config.hevc.vui.transfer_characteristics = 1;
                enc_cfg.encode_codec_config.hevc.vui.colour_matrix = 1;
            },
            VideoCodec::Av1 => unsafe {
                // repeatSeqHdr = bit 5
                enc_cfg.encode_codec_config.av1.bit_flags |= 1 << 5;
                enc_cfg.encode_codec_config.av1.idr_period = gop;
            },
        }

        let mut init_params: Box<NvEncInitializeParams> = Box::new(unsafe { std::mem::zeroed() });
        init_params.version = nvenc_struct_version(api_ver, init_struct_ver, true);
        init_params.encode_guid = encode_guid;
        init_params.preset_guid = preset_guid;
        init_params.encode_width = width as u32;
        init_params.encode_height = height as u32;
        init_params.dar_width = width as u32;
        init_params.dar_height = height as u32;
        init_params.frame_rate_num = fps.max(1) as u32;
        init_params.frame_rate_den = 1;
        init_params.enable_ptd = 1;
        init_params.encode_config = &mut *enc_cfg;
        init_params.max_encode_width = width as u32;
        init_params.max_encode_height = height as u32;
        init_params.tuning_info = tuning;
        init_params.buffer_format = buf_fmt;

        let init_fn = fn_list
            .nv_enc_initialize_encoder
            .ok_or_else(|| anyhow!("missing nvEncInitializeEncoder"))?;
        let st_init = unsafe { init_fn(enc, &mut *init_params) };
        if st_init != NV_ENC_SUCCESS {
            cleanup_session(enc);
            bail!("nvEncInitializeEncoder failed (status={st_init})");
        }

        // Push CUDA context to allocate pitched NV12 buffer, register & create bitstream buffer.
        if unsafe { (cuda.cu_ctx_push_current)(cu_ctx) } != CUDA_SUCCESS {
            cleanup_session(enc);
            bail!("cuCtxPushCurrent failed");
        }
        let mut cu_ptr: CUdeviceptr = 0;
        let mut cu_pitch: usize = 0;
        let alloc_rows = if use_yuv444 {
            height as usize * 3
        } else {
            height as usize * 3 / 2
        };
        let alloc_st = unsafe {
            (cuda.cu_mem_alloc_pitch)(
                &raw mut cu_ptr,
                &raw mut cu_pitch,
                width as usize,
                alloc_rows,
                16,
            )
        };
        let mut popped: CUcontext = std::ptr::null_mut();
        unsafe {
            let _ = (cuda.cu_ctx_pop_current)(&raw mut popped);
        }
        if alloc_st != CUDA_SUCCESS || cu_ptr == 0 {
            cleanup_session(enc);
            bail!("cuMemAllocPitch failed (CUresult={alloc_st})");
        }

        let reg_fn = fn_list
            .nv_enc_register_resource
            .ok_or_else(|| anyhow!("missing nvEncRegisterResource"))?;
        let mut reg: NvEncRegisterResource = unsafe { std::mem::zeroed() };
        reg.version = nvenc_struct_version(api_ver, if is_v12 { 4 } else { 3 }, false);
        reg.resource_type = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
        reg.width = width as u32;
        reg.height = height as u32;
        reg.pitch = cu_pitch as u32;
        reg.resource_to_register = cu_ptr as usize as *mut c_void;
        reg.buffer_format = buf_fmt;
        reg.buffer_usage = NV_ENC_INPUT_IMAGE;
        let st_reg = unsafe { reg_fn(enc, &raw mut reg) };
        if st_reg != NV_ENC_SUCCESS || reg.registered_resource.is_null() {
            unsafe {
                let _ = (cuda.cu_mem_free)(cu_ptr);
            }
            cleanup_session(enc);
            bail!("nvEncRegisterResource failed (status={st_reg})");
        }

        let create_bs = fn_list
            .nv_enc_create_bitstream_buffer
            .ok_or_else(|| anyhow!("missing nvEncCreateBitstreamBuffer"))?;
        let mut bs: NvEncCreateBitstreamBuffer = unsafe { std::mem::zeroed() };
        bs.version = nvenc_struct_version(api_ver, 1, false);
        let st_bs = unsafe { create_bs(enc, &raw mut bs) };
        if st_bs != NV_ENC_SUCCESS || bs.bitstream_buffer.is_null() {
            if let Some(unreg) = fn_list.nv_enc_unregister_resource {
                unsafe {
                    let _ = unreg(enc, reg.registered_resource);
                }
            }
            unsafe {
                let _ = (cuda.cu_mem_free)(cu_ptr);
            }
            cleanup_session(enc);
            bail!("nvEncCreateBitstreamBuffer failed (status={st_bs})");
        }

        tracing::info!(
            %codec,
            %chroma,
            width,
            height,
            fps,
            bitrate_kbps,
            api_v12 = is_v12,
            "NVENC runtime CUDA+NVENC hardware encoder active (zero ffmpeg dependency)"
        );

        Ok(Self {
            cuda,
            _nv: nv,
            dev,
            cu_ctx,
            fn_list,
            enc,
            api_ver,
            is_v12,
            codec,
            chroma,
            width,
            height,
            fps: fps.max(1),
            bitrate_kbps: bitrate_kbps.max(500),
            enc_cfg,
            init_params,
            cu_ptr,
            cu_pitch,
            registered_res: reg.registered_resource,
            bitstream_buf: bs.bitstream_buffer,
            frame_idx: 0,
        })
    }

    pub fn update_bitrate_kbps(&mut self, new_kbps: u32) -> Result<()> {
        let clamped = new_kbps.max(500);
        if clamped == self.bitrate_kbps {
            return Ok(());
        }
        let Some(reconf) = self.fn_list.nv_enc_reconfigure_encoder else {
            return Ok(());
        };
        let bps = clamped.saturating_mul(1000);
        self.enc_cfg.rc_params.average_bit_rate = bps;
        self.enc_cfg.rc_params.max_bit_rate = bps;
        self.enc_cfg.rc_params.vbv_buffer_size = (bps / self.fps as u32).max(64_000);
        self.enc_cfg.rc_params.vbv_initial_delay = self.enc_cfg.rc_params.vbv_buffer_size;
        self.init_params.encode_config = &mut *self.enc_cfg;

        let mut rp = NvEncReconfigureParams {
            version: nvenc_struct_version(self.api_ver, 1, true),
            re_init_encode_params: *self.init_params,
            flags: 0,
        };
        let st = unsafe { reconf(self.enc, &raw mut rp) };
        if st == NV_ENC_SUCCESS {
            self.bitrate_kbps = clamped;
            Ok(())
        } else {
            bail!("nvEncReconfigureEncoder({clamped}kbps) failed (status={st})")
        }
    }

    pub fn encode_nv12(&mut self, y: &[u8], uv: &[u8], force_idr: bool) -> Result<(Vec<u8>, bool)> {
        if self.chroma == usher_proto::handshake::ChromaSubsampling::Yuv444 {
            bail!("encode_nv12 called on YUV 4:4:4 NVENC session");
        }
        let w = self.width as usize;
        let h = self.height as usize;
        if y.len() < w * h || uv.len() < w * (h / 2) {
            bail!("NV12 slice too small for NVENC {}x{}", w, h);
        }

        if unsafe { (self.cuda.cu_ctx_push_current)(self.cu_ctx) } != CUDA_SUCCESS {
            bail!("cuCtxPushCurrent failed");
        }
        let st_y = self.upload_cuda_plane(y, w, h, 0);
        let st_uv = if st_y == CUDA_SUCCESS {
            self.upload_cuda_plane(uv, w, h / 2, h)
        } else {
            st_y
        };
        let mut popped: CUcontext = std::ptr::null_mut();
        unsafe {
            let _ = (self.cuda.cu_ctx_pop_current)(&raw mut popped);
        }
        if st_y != CUDA_SUCCESS || st_uv != CUDA_SUCCESS {
            bail!("cuMemcpy2D NV12 upload failed ({st_y}, {st_uv})");
        }

        self.encode_registered(force_idr)
    }

    pub fn encode_yuv444(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        force_idr: bool,
    ) -> Result<(Vec<u8>, bool)> {
        if self.chroma != usher_proto::handshake::ChromaSubsampling::Yuv444 {
            bail!("encode_yuv444 called on YUV 4:2:0 NVENC session");
        }
        let w = self.width as usize;
        let h = self.height as usize;
        if y.len() < w * h || u.len() < w * h || v.len() < w * h {
            bail!("YUV444 slice too small for NVENC {}x{}", w, h);
        }

        if unsafe { (self.cuda.cu_ctx_push_current)(self.cu_ctx) } != CUDA_SUCCESS {
            bail!("cuCtxPushCurrent failed");
        }
        let st_y = self.upload_cuda_plane(y, w, h, 0);
        let st_u = if st_y == CUDA_SUCCESS {
            self.upload_cuda_plane(u, w, h, h)
        } else {
            st_y
        };
        let st_v = if st_u == CUDA_SUCCESS {
            self.upload_cuda_plane(v, w, h, h * 2)
        } else {
            st_u
        };
        let mut popped: CUcontext = std::ptr::null_mut();
        unsafe {
            let _ = (self.cuda.cu_ctx_pop_current)(&raw mut popped);
        }
        if st_y != CUDA_SUCCESS || st_u != CUDA_SUCCESS || st_v != CUDA_SUCCESS {
            bail!("cuMemcpy2D YUV444 upload failed ({st_y}, {st_u}, {st_v})");
        }

        self.encode_registered(force_idr)
    }

    fn upload_cuda_plane(&self, src: &[u8], width: usize, rows: usize, dst_y_offset: usize) -> CUresult {
        let copy = CUDA_MEMCPY2D {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: CU_MEMORYTYPE_HOST,
            src_host: src.as_ptr().cast(),
            src_device: 0,
            src_array: std::ptr::null_mut(),
            src_pitch: width,
            dst_x_in_bytes: 0,
            dst_y: dst_y_offset,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_host: std::ptr::null_mut(),
            dst_device: self.cu_ptr,
            dst_array: std::ptr::null_mut(),
            dst_pitch: self.cu_pitch,
            width_in_bytes: width,
            height: rows,
        };
        unsafe { (self.cuda.cu_memcpy_2d)(&raw const copy) }
    }

    fn encode_registered(&mut self, force_idr: bool) -> Result<(Vec<u8>, bool)> {

        let map_fn = self
            .fn_list
            .nv_enc_map_input_resource
            .ok_or_else(|| anyhow!("missing nvEncMapInputResource"))?;
        let unmap_fn = self
            .fn_list
            .nv_enc_unmap_input_resource
            .ok_or_else(|| anyhow!("missing nvEncUnmapInputResource"))?;
        let enc_fn = self
            .fn_list
            .nv_enc_encode_picture
            .ok_or_else(|| anyhow!("missing nvEncEncodePicture"))?;
        let lock_fn = self
            .fn_list
            .nv_enc_lock_bitstream
            .ok_or_else(|| anyhow!("missing nvEncLockBitstream"))?;
        let unlock_fn = self
            .fn_list
            .nv_enc_unlock_bitstream
            .ok_or_else(|| anyhow!("missing nvEncUnlockBitstream"))?;

        let mut map_p: NvEncMapInputResource = unsafe { std::mem::zeroed() };
        map_p.version = nvenc_struct_version(self.api_ver, 4, false);
        map_p.registered_resource = self.registered_res;
        let st_map = unsafe { map_fn(self.enc, &raw mut map_p) };
        if st_map != NV_ENC_SUCCESS || map_p.mapped_resource.is_null() {
            bail!("nvEncMapInputResource failed (status={st_map})");
        }

        let mut pic: NvEncPicParams = unsafe { std::mem::zeroed() };
        pic.version = nvenc_struct_version(self.api_ver, if self.is_v12 { 6 } else { 4 }, true);
        pic.input_width = self.width as u32;
        pic.input_height = self.height as u32;
        pic.input_pitch = self.cu_pitch as u32;
        pic.encode_pic_flags = if force_idr || self.frame_idx == 0 {
            NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS
        } else {
            0
        };
        pic.frame_idx = self.frame_idx;
        pic.input_buffer = map_p.mapped_resource;
        pic.output_bitstream = self.bitstream_buf;
        pic.buffer_fmt = map_p.mapped_buffer_fmt;
        pic.picture_struct = NV_ENC_PIC_STRUCT_FRAME;

        let st_enc = unsafe { enc_fn(self.enc, &raw mut pic) };
        let _ = unsafe { unmap_fn(self.enc, map_p.mapped_resource) };
        if st_enc != NV_ENC_SUCCESS {
            bail!("nvEncEncodePicture failed (status={st_enc})");
        }

        let mut lock_bs: NvEncLockBitstream = unsafe { std::mem::zeroed() };
        lock_bs.version = nvenc_struct_version(self.api_ver, 1, self.is_v12);
        lock_bs.output_bitstream = self.bitstream_buf;
        let st_lock = unsafe { lock_fn(self.enc, &raw mut lock_bs) };
        if st_lock != NV_ENC_SUCCESS || lock_bs.bitstream_buffer_ptr.is_null() {
            bail!("nvEncLockBitstream failed (status={st_lock})");
        }

        let bytes = unsafe {
            std::slice::from_raw_parts(
                lock_bs.bitstream_buffer_ptr.cast::<u8>(),
                lock_bs.bitstream_size_in_bytes as usize,
            )
            .to_vec()
        };
        let pic_type = lock_bs.picture_type;
        let _ = unsafe { unlock_fn(self.enc, self.bitstream_buf) };
        self.frame_idx = self.frame_idx.wrapping_add(1);

        let keyframe = matches!(pic_type, NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE_I)
            || is_keyframe_annexb(self.codec, &bytes);
        Ok((bytes, keyframe))
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        if let Some(unreg) = self.fn_list.nv_enc_unregister_resource {
            unsafe {
                let _ = unreg(self.enc, self.registered_res);
            }
        }
        if let Some(destroy_bs) = self.fn_list.nv_enc_destroy_bitstream_buffer {
            unsafe {
                let _ = destroy_bs(self.enc, self.bitstream_buf);
            }
        }
        if let Some(destroy) = self.fn_list.nv_enc_destroy_encoder {
            unsafe {
                let _ = destroy(self.enc);
            }
        }
        if self.cu_ptr != 0 {
            unsafe {
                let _ = (self.cuda.cu_mem_free)(self.cu_ptr);
            }
        }
        unsafe {
            let _ = (self.cuda.cu_device_primary_ctx_release)(self.dev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvenc_struct_version_matches_sdk_macros() {
        let v11 = nvenc_api_version(11, 1);
        assert_eq!(nvenc_struct_version(v11, 1, false), 0x7101_000b);
        assert_eq!(nvenc_struct_version(v11, 7, true), 0xf107_000b);
        let v12 = nvenc_api_version(12, 1);
        assert_eq!(nvenc_struct_version(v12, 6, true), 0xf106_000c);
    }

    #[test]
    fn nvenc_guids_are_distinct() {
        assert_ne!(GUID_CODEC_H264, GUID_CODEC_HEVC);
        assert_ne!(GUID_CODEC_HEVC, GUID_CODEC_AV1);
        assert_ne!(GUID_PRESET_P1, GUID_PRESET_P4);
        assert_ne!(GUID_H264_PROFILE_HIGH_444, GUID_HEVC_PROFILE_FREXT);
        assert_ne!(NV_ENC_BUFFER_FORMAT_NV12, NV_ENC_BUFFER_FORMAT_YUV444);
    }
}
