use super::drm_probe::DrmProbe;
use super::linux_kms::DmaBufFrame;
use anyhow::{Context, Result, anyhow, bail};
use libloading::{Library, Symbol};
use std::ffi::c_void;
use std::fs::File;
use std::os::fd::AsRawFd;
use usher_proto::handshake::{QualityPreference, VideoCodec};

pub type VADisplay = *mut c_void;
pub type VAStatus = libc::c_int;
pub type VAConfigID = u32;
pub type VAContextID = u32;
pub type VASurfaceID = u32;
pub type VABufferID = u32;
pub type VAImageID = u32;

const VA_STATUS_SUCCESS: VAStatus = 0;
const VA_INVALID_SURFACE: VASurfaceID = 0xffff_ffff;
const VA_INVALID_ID: u32 = 0xffff_ffff;

// VAProfile
const VA_PROFILE_NONE: i32 = -1;
const VA_PROFILE_H264_MAIN: i32 = 6;
const VA_PROFILE_H264_HIGH: i32 = 7;
const VA_PROFILE_H264_CONSTRAINED_BASELINE: i32 = 13;
const VA_PROFILE_HEVC_MAIN: i32 = 17;

// VAEntrypoint
const VA_ENTRYPOINT_ENC_SLICE: i32 = 6;
const VA_ENTRYPOINT_ENC_SLICE_LP: i32 = 8;
const VA_ENTRYPOINT_VIDEO_PROC: i32 = 10;

// VAConfigAttribType
const VA_CONFIG_ATTRIB_RT_FORMAT: u32 = 0;
const VA_CONFIG_ATTRIB_RATE_CONTROL: u32 = 5;
const VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS: u32 = 10;
const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

// Formats & flags
const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
const VA_RT_FORMAT_RGB32: u32 = 0x0002_0000;
const VA_RC_CBR: u32 = 0x0000_0002;
const VA_ENC_PACKED_HEADER_NONE: u32 = 0;
const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 0x0000_0001;
const VA_ENC_PACKED_HEADER_PICTURE: u32 = 0x0000_0002;

const VA_PACKED_HEADER_SEQUENCE: u32 = 1;
const VA_PACKED_HEADER_PICTURE: u32 = 2;

// VASurfaceAttribType
const VA_SURFACE_ATTRIB_PIXEL_FORMAT: u32 = 1;
const VA_SURFACE_ATTRIB_MEMORY_TYPE: u32 = 6;
const VA_SURFACE_ATTRIB_EXT_BUF_DESCRIPTOR: u32 = 7;
const VA_SURFACE_ATTRIB_SETTABLE: u32 = 0x0000_0002;
const VA_GENERIC_VALUE_TYPE_INTEGER: u32 = 1;
const VA_GENERIC_VALUE_TYPE_POINTER: u32 = 3;
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: i32 = 0x4000_0000u32 as i32;

const VA_FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

// VABufferType
const VA_ENC_CODED_BUFFER_TYPE: u32 = 21;
const VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE: u32 = 22;
const VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE: u32 = 23;
const VA_ENC_SLICE_PARAMETER_BUFFER_TYPE: u32 = 24;
const VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE: u32 = 25;
const VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE: u32 = 26;
const VA_ENC_MISC_PARAMETER_BUFFER_TYPE: u32 = 27;
const VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE: u32 = 41;

// VAEncMiscParameterType
const VA_ENC_MISC_PARAM_FRAME_RATE: u32 = 0;
const VA_ENC_MISC_PARAM_RATE_CONTROL: u32 = 1;
const VA_ENC_MISC_PARAM_HRD: u32 = 5;

const VA_PICTURE_H264_INVALID: u32 = 0x0000_0001;
const VA_PICTURE_HEVC_INVALID: u32 = 0x0000_0001;

const VA_PROC_COLOR_STANDARD_BT709: u32 = 2;
const VA_PROC_COLOR_STANDARD_SRGB: u32 = 9;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VAConfigAttrib {
    r#type: u32,
    value: u32,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
union VAGenericValueUnion {
    i: i32,
    f: f32,
    p: *mut c_void,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct VAGenericValue {
    r#type: u32,
    value: VAGenericValueUnion,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct VASurfaceAttrib {
    r#type: u32,
    flags: u32,
    value: VAGenericValue,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VADrmPrimeObject {
    fd: libc::c_int,
    size: u32,
    drm_format_modifier: u64,
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VADrmPrimeLayer {
    drm_format: u32,
    num_planes: u32,
    object_index: [u32; 4],
    offset: [u32; 4],
    pitch: [u32; 4],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VADRMPRIMESurfaceDescriptor {
    fourcc: u32,
    width: u32,
    height: u32,
    num_objects: u32,
    objects: [VADrmPrimeObject; 4],
    num_layers: u32,
    layers: [VADrmPrimeLayer; 4],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VAImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    va_reserved: [u32; 4],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VAImage {
    image_id: VAImageID,
    format: VAImageFormat,
    buf: VABufferID,
    width: u16,
    height: u16,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    num_palette_entries: i32,
    entry_bytes: i32,
    component_order: [i8; 4],
    va_reserved: [u32; 4],
}

#[allow(dead_code)]
#[repr(C)]
struct VACodedBufferSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    reserved: u32,
    buf: *mut c_void,
    next: *mut VACodedBufferSegment,
    va_reserved: [libc::c_ulong; 4],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy)]
struct VAProcColorProperties {
    chroma_sample_location: u8,
    color_range: u8,
    colour_primaries: u8,
    transfer_characteristics: u8,
    matrix_coefficients: u8,
    reserved: [u8; 3],
}

#[allow(dead_code)]
#[repr(C)]
struct VAProcPipelineParameterBuffer {
    surface: VASurfaceID,
    surface_region: *const c_void,
    surface_color_standard: u32,
    output_region: *const c_void,
    output_background_color: u32,
    output_color_standard: u32,
    pipeline_flags: u32,
    filter_flags: u32,
    filters: *mut VABufferID,
    num_filters: u32,
    forward_references: *mut VASurfaceID,
    num_forward_references: u32,
    backward_references: *mut VASurfaceID,
    num_backward_references: u32,
    rotation_state: u32,
    blend_state: *const c_void,
    mirror_state: u32,
    additional_outputs: *mut VASurfaceID,
    num_additional_outputs: u32,
    input_surface_flag: u32,
    output_surface_flag: u32,
    input_color_properties: VAProcColorProperties,
    output_color_properties: VAProcColorProperties,
    processing_mode: u32,
    output_hdr_metadata: *mut c_void,
    va_reserved: [u32; 16],
}

#[allow(dead_code)]
#[repr(C)]
struct VAEncPackedHeaderParameterBuffer {
    r#type: u32,
    bit_length: u32,
    has_emulation_bytes: u8,
    va_reserved: [u32; 4],
}

#[allow(dead_code)]
#[repr(C)]
struct VAEncMiscRateControlBuf {
    misc_type: u32,
    bits_per_second: u32,
    target_percentage: u32,
    window_size: u32,
    initial_qp: u32,
    min_qp: u32,
    basic_unit_size: u32,
    rc_flags: u32,
    icq_quality_factor: u32,
    max_qp: u32,
    quality_factor: u32,
    target_frame_size: u32,
    va_reserved: [u32; 4],
}

#[allow(dead_code)]
#[repr(C)]
struct VAEncMiscFrameRateBuf {
    misc_type: u32,
    framerate: u32,
    framerate_flags: u32,
    va_reserved: [u32; 4],
}

#[allow(dead_code)]
#[repr(C)]
struct VAEncMiscHrdBuf {
    misc_type: u32,
    initial_buffer_fullness: u32,
    buffer_size: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VAPictureH264 {
    picture_id: VASurfaceID,
    frame_idx: u32,
    flags: u32,
    top_field_order_cnt: i32,
    bottom_field_order_cnt: i32,
    va_reserved: [u32; 4],
}

impl Default for VAPictureH264 {
    fn default() -> Self {
        Self {
            picture_id: VA_INVALID_SURFACE,
            frame_idx: 0,
            flags: VA_PICTURE_H264_INVALID,
            top_field_order_cnt: 0,
            bottom_field_order_cnt: 0,
            va_reserved: [0; 4],
        }
    }
}

#[allow(dead_code)]
#[repr(C)]
struct VAEncSequenceParameterBufferH264 {
    seq_parameter_set_id: u8,
    level_idc: u8,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    max_num_ref_frames: u32,
    picture_width_in_mbs: u16,
    picture_height_in_mbs: u16,
    seq_fields: u32,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_ref_frames_in_pic_order_cnt_cycle: u8,
    offset_for_non_ref_pic: i32,
    offset_for_top_to_bottom_field: i32,
    offset_for_ref_frame: [i32; 256],
    frame_cropping_flag: u8,
    frame_crop_left_offset: u32,
    frame_crop_right_offset: u32,
    frame_crop_top_offset: u32,
    frame_crop_bottom_offset: u32,
    vui_parameters_present_flag: u8,
    vui_fields: u32,
    aspect_ratio_idc: u8,
    sar_width: u32,
    sar_height: u32,
    num_units_in_tick: u32,
    time_scale: u32,
    va_reserved: [u32; 4],
}

#[allow(dead_code)]
#[repr(C)]
struct VAEncPictureParameterBufferH264 {
    curr_pic: VAPictureH264,
    reference_frames: [VAPictureH264; 16],
    coded_buf: VABufferID,
    pic_parameter_set_id: u8,
    seq_parameter_set_id: u8,
    last_picture: u8,
    frame_num: u16,
    pic_init_qp: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    pic_fields: u32,
    va_reserved: [u32; 4],
}

#[allow(dead_code)]
#[repr(C)]
struct VAEncSliceParameterBufferH264 {
    macroblock_address: u32,
    num_macroblocks: u32,
    macroblock_info: VABufferID,
    slice_type: u8,
    pic_parameter_set_id: u8,
    idr_pic_id: u16,
    pic_order_cnt_lsb: u16,
    delta_pic_order_cnt_bottom: i32,
    delta_pic_order_cnt: [i32; 2],
    direct_spatial_mv_pred_flag: u8,
    num_ref_idx_active_override_flag: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    ref_pic_list0: [VAPictureH264; 32],
    ref_pic_list1: [VAPictureH264; 32],
    luma_log2_weight_denom: u8,
    chroma_log2_weight_denom: u8,
    luma_weight_l0_flag: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_weight_l0_flag: u8,
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_weight_l1_flag: u8,
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_weight_l1_flag: u8,
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    disable_deblocking_filter_idc: u8,
    slice_alpha_c0_offset_div2: i8,
    slice_beta_offset_div2: i8,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VAPictureHEVC {
    picture_id: VASurfaceID,
    pic_order_cnt: i32,
    flags: u32,
    va_reserved: [u32; 4],
}

impl Default for VAPictureHEVC {
    fn default() -> Self {
        Self {
            picture_id: VA_INVALID_SURFACE,
            pic_order_cnt: 0,
            flags: VA_PICTURE_HEVC_INVALID,
            va_reserved: [0; 4],
        }
    }
}

#[allow(dead_code)]
#[repr(C)]
struct VAEncSequenceParameterBufferHEVC {
    general_profile_idc: u8,
    general_level_idc: u8,
    general_tier_flag: u8,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    pic_width_in_luma_samples: u16,
    pic_height_in_luma_samples: u16,
    seq_fields: u32,
    log2_min_luma_coding_block_size_minus3: u8,
    log2_diff_max_min_luma_coding_block_size: u8,
    log2_min_transform_block_size_minus2: u8,
    log2_diff_max_min_transform_block_size: u8,
    max_transform_hierarchy_depth_inter: u8,
    max_transform_hierarchy_depth_intra: u8,
    pcm_sample_bit_depth_luma_minus1: u32,
    pcm_sample_bit_depth_chroma_minus1: u32,
    log2_min_pcm_luma_coding_block_size_minus3: u32,
    log2_max_pcm_luma_coding_block_size_minus3: u32,
    vui_parameters_present_flag: u8,
    vui_fields: u32,
    aspect_ratio_idc: u8,
    sar_width: u32,
    sar_height: u32,
    vui_num_units_in_tick: u32,
    vui_time_scale: u32,
    min_spatial_segmentation_idc: u16,
    max_bytes_per_pic_denom: u8,
    max_bits_per_min_cu_denom: u8,
    scc_fields: u32,
    va_reserved: [u32; 7],
}

#[allow(dead_code)]
#[repr(C)]
struct VAEncPictureParameterBufferHEVC {
    decoded_curr_pic: VAPictureHEVC,
    reference_frames: [VAPictureHEVC; 15],
    coded_buf: VABufferID,
    collocated_ref_pic_index: u8,
    last_picture: u8,
    pic_init_qp: u8,
    diff_cu_qp_delta_depth: u8,
    pps_cb_qp_offset: i8,
    pps_cr_qp_offset: i8,
    num_tile_columns_minus1: u8,
    num_tile_rows_minus1: u8,
    column_width_minus1: [u8; 19],
    row_height_minus1: [u8; 21],
    log2_parallel_merge_level_minus2: u8,
    ctu_max_bitsize_allowed: u8,
    num_ref_idx_l0_default_active_minus1: u8,
    num_ref_idx_l1_default_active_minus1: u8,
    slice_pic_parameter_set_id: u8,
    nal_unit_type: u8,
    pic_fields: u32,
    hierarchical_level_plus1: u8,
    va_byte_reserved: u8,
    scc_fields: u16,
    va_reserved: [u32; 15],
}

#[allow(dead_code)]
#[repr(C)]
struct VAEncSliceParameterBufferHEVC {
    slice_segment_address: u32,
    num_ctu_in_slice: u32,
    slice_type: u8,
    slice_pic_parameter_set_id: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    ref_pic_list0: [VAPictureHEVC; 15],
    ref_pic_list1: [VAPictureHEVC; 15],
    luma_log2_weight_denom: u8,
    delta_chroma_log2_weight_denom: i8,
    delta_luma_weight_l0: [i8; 15],
    luma_offset_l0: [i8; 15],
    delta_chroma_weight_l0: [[i8; 2]; 15],
    chroma_offset_l0: [[i8; 2]; 15],
    delta_luma_weight_l1: [i8; 15],
    luma_offset_l1: [i8; 15],
    delta_chroma_weight_l1: [[i8; 2]; 15],
    chroma_offset_l1: [[i8; 2]; 15],
    max_num_merge_cand: u8,
    slice_qp_delta: i8,
    slice_cb_qp_offset: i8,
    slice_cr_qp_offset: i8,
    slice_beta_offset_div2: i8,
    slice_tc_offset_div2: i8,
    slice_fields: u32,
    pred_weight_table_bit_offset: u32,
    pred_weight_table_bit_length: u32,
    va_reserved: [u32; 6],
}

type FnVaGetDisplayDRM = unsafe extern "C" fn(libc::c_int) -> VADisplay;
type FnVaInitialize = unsafe extern "C" fn(VADisplay, *mut libc::c_int, *mut libc::c_int) -> VAStatus;
type FnVaTerminate = unsafe extern "C" fn(VADisplay) -> VAStatus;
type FnVaMaxNumProfiles = unsafe extern "C" fn(VADisplay) -> libc::c_int;
type FnVaMaxNumEntrypoints = unsafe extern "C" fn(VADisplay) -> libc::c_int;
type FnVaQueryConfigProfiles = unsafe extern "C" fn(VADisplay, *mut i32, *mut libc::c_int) -> VAStatus;
type FnVaQueryConfigEntrypoints =
    unsafe extern "C" fn(VADisplay, i32, *mut i32, *mut libc::c_int) -> VAStatus;
type FnVaGetConfigAttributes =
    unsafe extern "C" fn(VADisplay, i32, i32, *mut VAConfigAttrib, libc::c_int) -> VAStatus;
type FnVaCreateConfig = unsafe extern "C" fn(
    VADisplay,
    i32,
    i32,
    *mut VAConfigAttrib,
    libc::c_int,
    *mut VAConfigID,
) -> VAStatus;
type FnVaDestroyConfig = unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus;
type FnVaCreateSurfaces = unsafe extern "C" fn(
    VADisplay,
    u32,
    u32,
    u32,
    *mut VASurfaceID,
    u32,
    *mut VASurfaceAttrib,
    u32,
) -> VAStatus;
type FnVaDestroySurfaces = unsafe extern "C" fn(VADisplay, *mut VASurfaceID, libc::c_int) -> VAStatus;
type FnVaCreateContext = unsafe extern "C" fn(
    VADisplay,
    VAConfigID,
    libc::c_int,
    libc::c_int,
    libc::c_int,
    *mut VASurfaceID,
    libc::c_int,
    *mut VAContextID,
) -> VAStatus;
type FnVaDestroyContext = unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus;
type FnVaCreateBuffer = unsafe extern "C" fn(
    VADisplay,
    VAContextID,
    u32,
    u32,
    u32,
    *mut c_void,
    *mut VABufferID,
) -> VAStatus;
type FnVaDestroyBuffer = unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus;
type FnVaMapBuffer = unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus;
type FnVaUnmapBuffer = unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus;
type FnVaBeginPicture = unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus;
type FnVaRenderPicture = unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, libc::c_int) -> VAStatus;
type FnVaEndPicture = unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus;
type FnVaSyncSurface = unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus;
type FnVaCreateImage = unsafe extern "C" fn(VADisplay, *mut VAImageFormat, i32, i32, *mut VAImage) -> VAStatus;
type FnVaDestroyImage = unsafe extern "C" fn(VADisplay, VAImageID) -> VAStatus;
type FnVaPutImage = unsafe extern "C" fn(
    VADisplay,
    VASurfaceID,
    VAImageID,
    i32,
    i32,
    u32,
    u32,
    i32,
    i32,
    u32,
    u32,
) -> VAStatus;
type FnVaMaxNumImageFormats = unsafe extern "C" fn(VADisplay) -> libc::c_int;
type FnVaQueryImageFormats = unsafe extern "C" fn(VADisplay, *mut VAImageFormat, *mut libc::c_int) -> VAStatus;

struct VaApiLib {
    _va: Library,
    _va_drm: Library,
    va_get_display_drm: FnVaGetDisplayDRM,
    va_initialize: FnVaInitialize,
    va_terminate: FnVaTerminate,
    va_max_num_profiles: FnVaMaxNumProfiles,
    va_max_num_entrypoints: FnVaMaxNumEntrypoints,
    va_query_config_profiles: FnVaQueryConfigProfiles,
    va_query_config_entrypoints: FnVaQueryConfigEntrypoints,
    va_get_config_attributes: FnVaGetConfigAttributes,
    va_create_config: FnVaCreateConfig,
    va_destroy_config: FnVaDestroyConfig,
    va_create_surfaces: FnVaCreateSurfaces,
    va_destroy_surfaces: FnVaDestroySurfaces,
    va_create_context: FnVaCreateContext,
    va_destroy_context: FnVaDestroyContext,
    va_create_buffer: FnVaCreateBuffer,
    va_destroy_buffer: FnVaDestroyBuffer,
    va_map_buffer: FnVaMapBuffer,
    va_unmap_buffer: FnVaUnmapBuffer,
    va_begin_picture: FnVaBeginPicture,
    va_render_picture: FnVaRenderPicture,
    va_end_picture: FnVaEndPicture,
    va_sync_surface: FnVaSyncSurface,
    va_create_image: FnVaCreateImage,
    va_destroy_image: FnVaDestroyImage,
    va_put_image: FnVaPutImage,
    va_max_num_image_formats: FnVaMaxNumImageFormats,
    va_query_image_formats: FnVaQueryImageFormats,
}

unsafe impl Send for VaApiLib {}
unsafe impl Sync for VaApiLib {}

impl VaApiLib {
    fn load() -> Result<Self> {
        let va = unsafe { Library::new("libva.so.2") }
            .context("loading libva.so.2 at runtime")?;
        let va_drm = unsafe { Library::new("libva-drm.so.2") }
            .context("loading libva-drm.so.2 at runtime")?;
        macro_rules! sym_va {
            ($name:literal, $ty:ty) => {{
                let s: Symbol<'_, $ty> = unsafe { va.get($name) }
                    .with_context(|| format!("missing libva symbol {:?}", $name))?;
                *s
            }};
        }
        let s_drm: Symbol<'_, FnVaGetDisplayDRM> =
            unsafe { va_drm.get(b"vaGetDisplayDRM\0") }.context("missing vaGetDisplayDRM")?;
        let va_get_display_drm = *s_drm;

        Ok(Self {
            va_get_display_drm,
            va_initialize: sym_va!(b"vaInitialize\0", FnVaInitialize),
            va_terminate: sym_va!(b"vaTerminate\0", FnVaTerminate),
            va_max_num_profiles: sym_va!(b"vaMaxNumProfiles\0", FnVaMaxNumProfiles),
            va_max_num_entrypoints: sym_va!(b"vaMaxNumEntrypoints\0", FnVaMaxNumEntrypoints),
            va_query_config_profiles: sym_va!(b"vaQueryConfigProfiles\0", FnVaQueryConfigProfiles),
            va_query_config_entrypoints: sym_va!(b"vaQueryConfigEntrypoints\0", FnVaQueryConfigEntrypoints),
            va_get_config_attributes: sym_va!(b"vaGetConfigAttributes\0", FnVaGetConfigAttributes),
            va_create_config: sym_va!(b"vaCreateConfig\0", FnVaCreateConfig),
            va_destroy_config: sym_va!(b"vaDestroyConfig\0", FnVaDestroyConfig),
            va_create_surfaces: sym_va!(b"vaCreateSurfaces\0", FnVaCreateSurfaces),
            va_destroy_surfaces: sym_va!(b"vaDestroySurfaces\0", FnVaDestroySurfaces),
            va_create_context: sym_va!(b"vaCreateContext\0", FnVaCreateContext),
            va_destroy_context: sym_va!(b"vaDestroyContext\0", FnVaDestroyContext),
            va_create_buffer: sym_va!(b"vaCreateBuffer\0", FnVaCreateBuffer),
            va_destroy_buffer: sym_va!(b"vaDestroyBuffer\0", FnVaDestroyBuffer),
            va_map_buffer: sym_va!(b"vaMapBuffer\0", FnVaMapBuffer),
            va_unmap_buffer: sym_va!(b"vaUnmapBuffer\0", FnVaUnmapBuffer),
            va_begin_picture: sym_va!(b"vaBeginPicture\0", FnVaBeginPicture),
            va_render_picture: sym_va!(b"vaRenderPicture\0", FnVaRenderPicture),
            va_end_picture: sym_va!(b"vaEndPicture\0", FnVaEndPicture),
            va_sync_surface: sym_va!(b"vaSyncSurface\0", FnVaSyncSurface),
            va_create_image: sym_va!(b"vaCreateImage\0", FnVaCreateImage),
            va_destroy_image: sym_va!(b"vaDestroyImage\0", FnVaDestroyImage),
            va_put_image: sym_va!(b"vaPutImage\0", FnVaPutImage),
            va_max_num_image_formats: sym_va!(b"vaMaxNumImageFormats\0", FnVaMaxNumImageFormats),
            va_query_image_formats: sym_va!(b"vaQueryImageFormats\0", FnVaQueryImageFormats),
            _va: va,
            _va_drm: va_drm,
        })
    }
}

fn pick_encode_profile(
    lib: &VaApiLib,
    dpy: VADisplay,
    codec: VideoCodec,
) -> Option<(i32, i32)> {
    let max_prof = unsafe { (lib.va_max_num_profiles)(dpy) }.max(0) as usize;
    let max_ep = unsafe { (lib.va_max_num_entrypoints)(dpy) }.max(0) as usize;
    if max_prof == 0 || max_ep == 0 {
        return None;
    }
    let mut profiles = vec![0i32; max_prof];
    let mut num_prof = 0;
    if unsafe { (lib.va_query_config_profiles)(dpy, profiles.as_mut_ptr(), &raw mut num_prof) }
        != VA_STATUS_SUCCESS
    {
        return None;
    }
    profiles.truncate(num_prof.max(0) as usize);

    let candidates: &[i32] = match codec {
        VideoCodec::Hevc => &[VA_PROFILE_HEVC_MAIN],
        VideoCodec::H264 => &[
            VA_PROFILE_H264_CONSTRAINED_BASELINE,
            VA_PROFILE_H264_MAIN,
            VA_PROFILE_H264_HIGH,
        ],
        VideoCodec::Av1 => return None,
    };

    for &prof in candidates {
        if !profiles.contains(&prof) {
            continue;
        }
        let mut eps = vec![0i32; max_ep];
        let mut num_ep = 0;
        if unsafe {
            (lib.va_query_config_entrypoints)(dpy, prof, eps.as_mut_ptr(), &raw mut num_ep)
        } != VA_STATUS_SUCCESS
        {
            continue;
        }
        eps.truncate(num_ep.max(0) as usize);
        if eps.contains(&VA_ENTRYPOINT_ENC_SLICE_LP) {
            return Some((prof, VA_ENTRYPOINT_ENC_SLICE_LP));
        }
        if eps.contains(&VA_ENTRYPOINT_ENC_SLICE) {
            return Some((prof, VA_ENTRYPOINT_ENC_SLICE));
        }
    }
    None
}

/// Probes runtime VA-API availability and supported encode codecs without keeping any handle open.
pub fn probe_vaapi_codecs(probe: &DrmProbe) -> std::result::Result<Vec<VideoCodec>, String> {
    let render = probe
        .render_node
        .as_deref()
        .ok_or_else(|| "no /dev/dri/renderD* node present".to_string())?;
    let lib = VaApiLib::load().map_err(|e| format!("{e:#}"))?;
    let file = File::open(render).map_err(|e| format!("cannot open {}: {e}", render.display()))?;
    let dpy = unsafe { (lib.va_get_display_drm)(file.as_raw_fd()) };
    if dpy.is_null() {
        return Err(format!("vaGetDisplayDRM({}) returned null", render.display()));
    }
    let mut maj = 0;
    let mut min = 0;
    let st = unsafe { (lib.va_initialize)(dpy, &raw mut maj, &raw mut min) };
    if st != VA_STATUS_SUCCESS {
        return Err(format!(
            "vaInitialize failed on {} (VAStatus={st})",
            render.display()
        ));
    }

    let mut codecs = Vec::new();
    for c in [VideoCodec::Hevc, VideoCodec::H264] {
        if pick_encode_profile(&lib, dpy, c).is_some() {
            codecs.push(c);
        }
    }
    unsafe {
        let _ = (lib.va_terminate)(dpy);
    }
    if codecs.is_empty() {
        return Err(format!(
            "VA-API {} initialized (v{maj}.{min}) but exposed no H264/HEVC encode entrypoint",
            render.display()
        ));
    }
    Ok(codecs)
}

pub struct VaapiEncoder {
    lib: VaApiLib,
    _drm_file: File,
    dpy: VADisplay,
    codec: VideoCodec,
    width: u16,
    height: u16,
    fps: u16,
    gop_len: u32,
    enc_w: u16,
    enc_h: u16,
    profile: i32,
    packed_headers_caps: u32,
    enc_cfg: VAConfigID,
    enc_ctx: VAContextID,
    input_surf: VASurfaceID,
    recon_surfs: [VASurfaceID; 2],
    coded_buf: VABufferID,
    nv12_format: VAImageFormat,
    vpp_cfg: Option<VAConfigID>,
    vpp_ctx: Option<VAContextID>,
    current_bitrate_kbps: u32,
    frame_idx: u64,
    idr_seq: u16,
    ref_slot: usize,
    last_ref_surf: Option<(VASurfaceID, u32, i32)>,
}

unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    pub fn new(
        probe: &DrmProbe,
        codec: VideoCodec,
        width: u16,
        height: u16,
        fps: u16,
        bitrate_kbps: u32,
        prefer: QualityPreference,
    ) -> Result<Self> {
        let render = probe
            .render_node
            .as_deref()
            .ok_or_else(|| anyhow!("no DRM render node found for VA-API"))?;
        let lib = VaApiLib::load()?;
        let drm_file = File::open(render)
            .with_context(|| format!("opening DRM render node {}", render.display()))?;
        let dpy = unsafe { (lib.va_get_display_drm)(drm_file.as_raw_fd()) };
        if dpy.is_null() {
            bail!("vaGetDisplayDRM returned null");
        }
        let mut maj = 0;
        let mut min = 0;
        let st = unsafe { (lib.va_initialize)(dpy, &raw mut maj, &raw mut min) };
        if st != VA_STATUS_SUCCESS {
            bail!("vaInitialize failed with status {st}");
        }

        let Some((profile, entrypoint)) = pick_encode_profile(&lib, dpy, codec) else {
            unsafe {
                let _ = (lib.va_terminate)(dpy);
            }
            bail!("VA-API device does not support {codec} encoding");
        };

        let mut query_attribs = [
            VAConfigAttrib {
                r#type: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: 0,
            },
            VAConfigAttrib {
                r#type: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: 0,
            },
        ];
        unsafe {
            let _ = (lib.va_get_config_attributes)(
                dpy,
                profile,
                entrypoint,
                query_attribs.as_mut_ptr(),
                query_attribs.len() as i32,
            );
        }

        let rc_mode = if query_attribs[0].value != VA_ATTRIB_NOT_SUPPORTED
            && (query_attribs[0].value & VA_RC_CBR) != 0
        {
            VA_RC_CBR
        } else {
            query_attribs[0].value & 0x1e
        };
        let want_packed = if query_attribs[1].value != VA_ATTRIB_NOT_SUPPORTED {
            query_attribs[1].value & (VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE)
        } else {
            VA_ENC_PACKED_HEADER_NONE
        };

        let mut create_attribs = vec![VAConfigAttrib {
            r#type: VA_CONFIG_ATTRIB_RT_FORMAT,
            value: VA_RT_FORMAT_YUV420,
        }];
        if rc_mode != 0 {
            create_attribs.push(VAConfigAttrib {
                r#type: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: rc_mode,
            });
        }
        if want_packed != 0 {
            create_attribs.push(VAConfigAttrib {
                r#type: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: want_packed,
            });
        }

        let mut enc_cfg = VA_INVALID_ID;
        let st = unsafe {
            (lib.va_create_config)(
                dpy,
                profile,
                entrypoint,
                create_attribs.as_mut_ptr(),
                create_attribs.len() as i32,
                &raw mut enc_cfg,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                let _ = (lib.va_terminate)(dpy);
            }
            bail!("vaCreateConfig failed (profile={profile}, ep={entrypoint}, status={st})");
        }

        let (align_w, align_h) = match codec {
            VideoCodec::H264 => (16u16, 16u16),
            VideoCodec::Hevc | VideoCodec::Av1 => (32u16, 32u16),
        };
        let enc_w = width.max(64).next_multiple_of(align_w);
        let enc_h = height.max(64).next_multiple_of(align_h);

        let mut pix_attr = VASurfaceAttrib {
            r#type: VA_SURFACE_ATTRIB_PIXEL_FORMAT,
            flags: VA_SURFACE_ATTRIB_SETTABLE,
            value: VAGenericValue {
                r#type: VA_GENERIC_VALUE_TYPE_INTEGER,
                value: VAGenericValueUnion {
                    i: VA_FOURCC_NV12 as i32,
                },
            },
        };
        let mut surfs = [VA_INVALID_SURFACE; 3];
        let st = unsafe {
            (lib.va_create_surfaces)(
                dpy,
                VA_RT_FORMAT_YUV420,
                enc_w as u32,
                enc_h as u32,
                surfs.as_mut_ptr(),
                3,
                &raw mut pix_attr,
                1,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                let _ = (lib.va_destroy_config)(dpy, enc_cfg);
                let _ = (lib.va_terminate)(dpy);
            }
            bail!("vaCreateSurfaces NV12 failed (status={st})");
        }

        let mut enc_ctx = VA_INVALID_ID;
        let st = unsafe {
            (lib.va_create_context)(
                dpy,
                enc_cfg,
                enc_w as i32,
                enc_h as i32,
                1, // VA_PROGRESSIVE
                surfs.as_mut_ptr(),
                3,
                &raw mut enc_ctx,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                let _ = (lib.va_destroy_surfaces)(dpy, surfs.as_mut_ptr(), 3);
                let _ = (lib.va_destroy_config)(dpy, enc_cfg);
                let _ = (lib.va_terminate)(dpy);
            }
            bail!("vaCreateContext encode failed (status={st})");
        }

        let coded_size = ((enc_w as u32) * (enc_h as u32) * 3 / 2).max(1024 * 1024);
        let mut coded_buf = VA_INVALID_ID;
        let st = unsafe {
            (lib.va_create_buffer)(
                dpy,
                enc_ctx,
                VA_ENC_CODED_BUFFER_TYPE,
                coded_size,
                1,
                std::ptr::null_mut(),
                &raw mut coded_buf,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                let _ = (lib.va_destroy_context)(dpy, enc_ctx);
                let _ = (lib.va_destroy_surfaces)(dpy, surfs.as_mut_ptr(), 3);
                let _ = (lib.va_destroy_config)(dpy, enc_cfg);
                let _ = (lib.va_terminate)(dpy);
            }
            bail!("vaCreateBuffer coded_buf failed (status={st})");
        }

        let nv12_format = query_nv12_image_format(&lib, dpy);
        let (vpp_cfg, vpp_ctx) = create_vpp_context(&lib, dpy, enc_w, enc_h);

        let gop_len = match prefer {
            QualityPreference::Latency => (fps as u32).max(30),
            QualityPreference::Quality => (fps as u32 * 2).max(60),
        };

        tracing::info!(
            render = %render.display(),
            va_version = format_args!("{maj}.{min}"),
            %codec,
            width,
            height,
            enc_w,
            enc_h,
            vpp_zero_copy = vpp_ctx.is_some(),
            bitrate_kbps,
            "VA-API runtime hardware encoder active (zero ffmpeg dependency)"
        );

        Ok(Self {
            lib,
            _drm_file: drm_file,
            dpy,
            codec,
            width,
            height,
            fps: fps.max(1),
            gop_len,
            enc_w,
            enc_h,
            profile,
            packed_headers_caps: want_packed,
            enc_cfg,
            enc_ctx,
            input_surf: surfs[0],
            recon_surfs: [surfs[1], surfs[2]],
            coded_buf,
            nv12_format,
            vpp_cfg,
            vpp_ctx,
            current_bitrate_kbps: bitrate_kbps.max(500),
            frame_idx: 0,
            idr_seq: 0,
            ref_slot: 0,
            last_ref_surf: None,
        })
    }

    pub fn update_bitrate_kbps(&mut self, bitrate_kbps: u32) {
        self.current_bitrate_kbps = bitrate_kbps.max(500);
    }

    pub fn supports_dmabuf_zero_copy(&self) -> bool {
        self.vpp_ctx.is_some()
    }

    pub fn encode_dmabuf(&mut self, dmabuf: &DmaBufFrame, force_idr: bool) -> Result<(Vec<u8>, bool)> {
        let vpp_ctx = self
            .vpp_ctx
            .ok_or_else(|| anyhow!("VA-API VPP unavailable for zero-copy DMA-BUF import"))?;
        let import_surf = import_dmabuf_surface(&self.lib, self.dpy, dmabuf)?;
        let conv_res = run_vpp_blit(
            &self.lib,
            self.dpy,
            vpp_ctx,
            import_surf,
            self.input_surf,
            dmabuf.fourcc == FOURCC_NV12,
        );
        unsafe {
            let mut s = import_surf;
            let _ = (self.lib.va_destroy_surfaces)(self.dpy, &raw mut s, 1);
        }
        conv_res?;
        self.encode_current_surface(force_idr)
    }

    pub fn encode_nv12(&mut self, y: &[u8], uv: &[u8], force_idr: bool) -> Result<(Vec<u8>, bool)> {
        upload_nv12_image(
            &self.lib,
            self.dpy,
            self.input_surf,
            &self.nv12_format,
            self.width as usize,
            self.height as usize,
            y,
            uv,
        )?;
        self.encode_current_surface(force_idr)
    }

    fn encode_current_surface(&mut self, force_idr: bool) -> Result<(Vec<u8>, bool)> {
        let is_idr =
            force_idr || self.frame_idx == 0 || self.frame_idx.is_multiple_of(self.gop_len as u64);
        let recon_surf = self.recon_surfs[self.ref_slot];
        let mut buffers = Vec::<VABufferID>::with_capacity(8);

        let submit_res = match self.codec {
            VideoCodec::H264 => self.submit_h264(&mut buffers, recon_surf, is_idr),
            VideoCodec::Hevc => self.submit_hevc(&mut buffers, recon_surf, is_idr),
            VideoCodec::Av1 => bail!("VA-API AV1 encoding not supported by native backend"),
        };
        if let Err(err) = submit_res {
            for b in buffers {
                unsafe {
                    let _ = (self.lib.va_destroy_buffer)(self.dpy, b);
                }
            }
            return Err(err);
        }

        let st = unsafe { (self.lib.va_begin_picture)(self.dpy, self.enc_ctx, self.input_surf) };
        if st != VA_STATUS_SUCCESS {
            for b in buffers {
                unsafe {
                    let _ = (self.lib.va_destroy_buffer)(self.dpy, b);
                }
            }
            bail!("vaBeginPicture encode failed (status={st})");
        }

        let st = unsafe {
            (self.lib.va_render_picture)(
                self.dpy,
                self.enc_ctx,
                buffers.as_mut_ptr(),
                buffers.len() as i32,
            )
        };
        let st_end = unsafe { (self.lib.va_end_picture)(self.dpy, self.enc_ctx) };
        for b in buffers {
            unsafe {
                let _ = (self.lib.va_destroy_buffer)(self.dpy, b);
            }
        }
        if st != VA_STATUS_SUCCESS {
            bail!("vaRenderPicture encode failed (status={st})");
        }
        if st_end != VA_STATUS_SUCCESS {
            bail!("vaEndPicture encode failed (status={st_end})");
        }

        let st = unsafe { (self.lib.va_sync_surface)(self.dpy, self.input_surf) };
        if st != VA_STATUS_SUCCESS {
            bail!("vaSyncSurface failed (status={st})");
        }

        let encoded = read_coded_buffer(&self.lib, self.dpy, self.coded_buf)?;

        let pic_idx = if is_idr { 0 } else { (self.frame_idx as u32).max(1) };
        let poc = (pic_idx as i32) * 2;
        self.last_ref_surf = Some((recon_surf, pic_idx, poc));
        self.ref_slot = 1 - self.ref_slot;
        self.frame_idx += 1;

        Ok((encoded, is_idr))
    }

    fn create_buf<T>(&self, buf_type: u32, val: &mut T) -> Result<VABufferID> {
        let mut id = VA_INVALID_ID;
        let st = unsafe {
            (self.lib.va_create_buffer)(
                self.dpy,
                self.enc_ctx,
                buf_type,
                std::mem::size_of::<T>() as u32,
                1,
                (val as *mut T).cast::<c_void>(),
                &raw mut id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            bail!("vaCreateBuffer(type={buf_type}) failed (status={st})");
        }
        Ok(id)
    }

    fn submit_packed_header(&self, buffers: &mut Vec<VABufferID>, hdr_type: u32, nalu: &[u8]) -> Result<()> {
        let mut param = VAEncPackedHeaderParameterBuffer {
            r#type: hdr_type,
            bit_length: (nalu.len() * 8) as u32,
            has_emulation_bytes: 0,
            va_reserved: [0; 4],
        };
        buffers.push(self.create_buf(VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE, &mut param)?);
        let mut id = VA_INVALID_ID;
        let mut bytes = nalu.to_vec();
        let st = unsafe {
            (self.lib.va_create_buffer)(
                self.dpy,
                self.enc_ctx,
                VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE,
                bytes.len() as u32,
                1,
                bytes.as_mut_ptr().cast(),
                &raw mut id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            bail!("vaCreateBuffer packed header data failed (status={st})");
        }
        buffers.push(id);
        Ok(())
    }

    fn submit_misc_rc(&self, buffers: &mut Vec<VABufferID>, reset_rc: bool) -> Result<()> {
        let bps = self.current_bitrate_kbps.saturating_mul(1000);
        let mut rc = VAEncMiscRateControlBuf {
            misc_type: VA_ENC_MISC_PARAM_RATE_CONTROL,
            bits_per_second: bps,
            target_percentage: 100,
            window_size: 500,
            initial_qp: 26,
            min_qp: 18,
            basic_unit_size: 0,
            rc_flags: if reset_rc { 1 } else { 0 } | (1 << 1), // reset | disable_frame_skip
            icq_quality_factor: 0,
            max_qp: 46,
            quality_factor: 0,
            target_frame_size: 0,
            va_reserved: [0; 4],
        };
        buffers.push(self.create_buf(VA_ENC_MISC_PARAMETER_BUFFER_TYPE, &mut rc)?);

        let cpb_bits = (bps / self.fps as u32).saturating_mul(2).max(128_000);
        let mut hrd = VAEncMiscHrdBuf {
            misc_type: VA_ENC_MISC_PARAM_HRD,
            initial_buffer_fullness: cpb_bits / 2,
            buffer_size: cpb_bits,
            va_reserved: [0; 4],
        };
        buffers.push(self.create_buf(VA_ENC_MISC_PARAMETER_BUFFER_TYPE, &mut hrd)?);

        let mut fr = VAEncMiscFrameRateBuf {
            misc_type: VA_ENC_MISC_PARAM_FRAME_RATE,
            framerate: self.fps as u32,
            framerate_flags: 0,
            va_reserved: [0; 4],
        };
        buffers.push(self.create_buf(VA_ENC_MISC_PARAMETER_BUFFER_TYPE, &mut fr)?);
        Ok(())
    }

    fn submit_h264(
        &mut self,
        buffers: &mut Vec<VABufferID>,
        recon_surf: VASurfaceID,
        is_idr: bool,
    ) -> Result<()> {
        if is_idr {
            self.idr_seq = self.idr_seq.wrapping_add(1);
            let mb_w = self.enc_w / 16;
            let mb_h = self.enc_h / 16;
            let crop_r = (self.enc_w - self.width) as u32 / 2;
            let crop_b = (self.enc_h - self.height) as u32 / 2;
            // seq_fields bits: chroma_format_idc=1(2b) | frame_mbs_only_flag=1(1b) | direct_8x8=1(bit5)
            // | log2_max_frame_num_minus4=4(bits 6..9) | pic_order_cnt_type=2(bits 10..11)
            let seq_fields = 1u32 | (1u32 << 2) | (1u32 << 5) | (4u32 << 6) | (2u32 << 10);
            let mut seq = VAEncSequenceParameterBufferH264 {
                seq_parameter_set_id: 0,
                level_idc: 51,
                intra_period: self.gop_len,
                intra_idr_period: self.gop_len,
                ip_period: 1,
                bits_per_second: self.current_bitrate_kbps.saturating_mul(1000),
                max_num_ref_frames: 1,
                picture_width_in_mbs: mb_w,
                picture_height_in_mbs: mb_h,
                seq_fields,
                bit_depth_luma_minus8: 0,
                bit_depth_chroma_minus8: 0,
                num_ref_frames_in_pic_order_cnt_cycle: 0,
                offset_for_non_ref_pic: 0,
                offset_for_top_to_bottom_field: 0,
                offset_for_ref_frame: [0; 256],
                frame_cropping_flag: u8::from(crop_r > 0 || crop_b > 0),
                frame_crop_left_offset: 0,
                frame_crop_right_offset: crop_r,
                frame_crop_top_offset: 0,
                frame_crop_bottom_offset: crop_b,
                vui_parameters_present_flag: 1,
                vui_fields: (1 << 1) | (1 << 2) | (1 << 13), // timing_info + restriction + fixed_fps
                aspect_ratio_idc: 0,
                sar_width: 0,
                sar_height: 0,
                num_units_in_tick: 1,
                time_scale: self.fps as u32 * 2,
                va_reserved: [0; 4],
            };
            buffers.push(self.create_buf(VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE, &mut seq)?);
            if (self.packed_headers_caps & VA_ENC_PACKED_HEADER_SEQUENCE) != 0 {
                let sps = build_h264_sps_nalu(self.width, self.height, self.fps, self.profile);
                self.submit_packed_header(buffers, VA_PACKED_HEADER_SEQUENCE, &sps)?;
            }
        }

        self.submit_misc_rc(buffers, self.frame_idx == 0)?;

        let frame_num = if is_idr {
            0
        } else {
            ((self.frame_idx % 256) as u16).max(1)
        };
        let poc = (frame_num as i32) * 2;
        let curr_pic = VAPictureH264 {
            picture_id: recon_surf,
            frame_idx: frame_num as u32,
            flags: 0,
            top_field_order_cnt: poc,
            bottom_field_order_cnt: poc,
            va_reserved: [0; 4],
        };
        let mut ref_frames = [VAPictureH264::default(); 16];
        let mut ref_list0 = [VAPictureH264::default(); 32];
        if !is_idr
            && let Some((ref_surf, ref_idx, ref_poc)) = self.last_ref_surf
        {
            let r = VAPictureH264 {
                picture_id: ref_surf,
                frame_idx: ref_idx,
                flags: 0x0000_0008, // VA_PICTURE_H264_SHORT_TERM_REFERENCE
                top_field_order_cnt: ref_poc,
                bottom_field_order_cnt: ref_poc,
                va_reserved: [0; 4],
            };
            ref_frames[0] = r;
            ref_list0[0] = r;
        }

        let cabac = u32::from(self.profile != VA_PROFILE_H264_CONSTRAINED_BASELINE);
        // pic_fields: idr(b0) | ref_pic_flag=1(b1..2) | cabac(b3) | deblocking=1(b9)
        let pic_fields = u32::from(is_idr) | (1u32 << 1) | (cabac << 3) | (1u32 << 9);
        let mut pic = VAEncPictureParameterBufferH264 {
            curr_pic,
            reference_frames: ref_frames,
            coded_buf: self.coded_buf,
            pic_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            last_picture: 0,
            frame_num,
            pic_init_qp: 26,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            chroma_qp_index_offset: 0,
            second_chroma_qp_index_offset: 0,
            pic_fields,
            va_reserved: [0; 4],
        };
        buffers.push(self.create_buf(VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE, &mut pic)?);
        if is_idr && (self.packed_headers_caps & VA_ENC_PACKED_HEADER_PICTURE) != 0 {
            let pps = build_h264_pps_nalu(cabac != 0);
            self.submit_packed_header(buffers, VA_PACKED_HEADER_PICTURE, &pps)?;
        }

        let num_mbs = (self.enc_w as u32 / 16) * (self.enc_h as u32 / 16);
        let mut slice = VAEncSliceParameterBufferH264 {
            macroblock_address: 0,
            num_macroblocks: num_mbs,
            macroblock_info: VA_INVALID_ID,
            slice_type: if is_idr { 2 } else { 0 }, // H264: 2=I, 0=P
            pic_parameter_set_id: 0,
            idr_pic_id: self.idr_seq,
            pic_order_cnt_lsb: (poc as u16) & 0xff,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0; 2],
            direct_spatial_mv_pred_flag: 1,
            num_ref_idx_active_override_flag: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            ref_pic_list0: ref_list0,
            ref_pic_list1: [VAPictureH264::default(); 32],
            luma_log2_weight_denom: 0,
            chroma_log2_weight_denom: 0,
            luma_weight_l0_flag: 0,
            luma_weight_l0: [0; 32],
            luma_offset_l0: [0; 32],
            chroma_weight_l0_flag: 0,
            chroma_weight_l0: [[0; 2]; 32],
            chroma_offset_l0: [[0; 2]; 32],
            luma_weight_l1_flag: 0,
            luma_weight_l1: [0; 32],
            luma_offset_l1: [0; 32],
            chroma_weight_l1_flag: 0,
            chroma_weight_l1: [[0; 2]; 32],
            chroma_offset_l1: [[0; 2]; 32],
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            va_reserved: [0; 4],
        };
        buffers.push(self.create_buf(VA_ENC_SLICE_PARAMETER_BUFFER_TYPE, &mut slice)?);
        Ok(())
    }

    fn submit_hevc(
        &mut self,
        buffers: &mut Vec<VABufferID>,
        recon_surf: VASurfaceID,
        is_idr: bool,
    ) -> Result<()> {
        if is_idr {
            // chroma_format_idc=1(b0..1), strong_intra_smoothing=1(b10), sao=0(b12), sps_temporal_mvp=1(b15), low_delay_seq=1(b16)
            let seq_fields = 1u32 | (1u32 << 10) | (1u32 << 15) | (1u32 << 16);
            let mut seq = VAEncSequenceParameterBufferHEVC {
                general_profile_idc: 1, // Main
                general_level_idc: 153, // Level 5.1 (5.1 * 30 = 153)
                general_tier_flag: 0,
                intra_period: self.gop_len,
                intra_idr_period: self.gop_len,
                ip_period: 1,
                bits_per_second: self.current_bitrate_kbps.saturating_mul(1000),
                pic_width_in_luma_samples: self.enc_w,
                pic_height_in_luma_samples: self.enc_h,
                seq_fields,
                log2_min_luma_coding_block_size_minus3: 0, // min CU = 8
                log2_diff_max_min_luma_coding_block_size: 2, // max CU (CTU) = 32
                log2_min_transform_block_size_minus2: 0, // min TB = 4
                log2_diff_max_min_transform_block_size: 3, // max TB = 32
                max_transform_hierarchy_depth_inter: 2,
                max_transform_hierarchy_depth_intra: 2,
                pcm_sample_bit_depth_luma_minus1: 7,
                pcm_sample_bit_depth_chroma_minus1: 7,
                log2_min_pcm_luma_coding_block_size_minus3: 0,
                log2_max_pcm_luma_coding_block_size_minus3: 2,
                vui_parameters_present_flag: 1,
                vui_fields: (1 << 3) | (1 << 4), // vui_timing_info_present + bitstream_restriction
                aspect_ratio_idc: 0,
                sar_width: 0,
                sar_height: 0,
                vui_num_units_in_tick: 1,
                vui_time_scale: self.fps as u32,
                min_spatial_segmentation_idc: 0,
                max_bytes_per_pic_denom: 0,
                max_bits_per_min_cu_denom: 0,
                scc_fields: 0,
                va_reserved: [0; 7],
            };
            buffers.push(self.create_buf(VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE, &mut seq)?);
            if (self.packed_headers_caps & VA_ENC_PACKED_HEADER_SEQUENCE) != 0 {
                let mut vps_sps = build_hevc_vps_nalu(self.fps);
                vps_sps.extend_from_slice(&build_hevc_sps_nalu(
                    self.enc_w,
                    self.enc_h,
                    self.width,
                    self.height,
                    self.fps,
                ));
                self.submit_packed_header(buffers, VA_PACKED_HEADER_SEQUENCE, &vps_sps)?;
            }
        }

        self.submit_misc_rc(buffers, self.frame_idx == 0)?;

        let poc = if is_idr { 0i32 } else { (self.frame_idx as i32).max(1) };
        let curr_pic = VAPictureHEVC {
            picture_id: recon_surf,
            pic_order_cnt: poc,
            flags: 0,
            va_reserved: [0; 4],
        };
        let mut ref_frames = [VAPictureHEVC::default(); 15];
        let mut ref_list0 = [VAPictureHEVC::default(); 15];
        if !is_idr
            && let Some((ref_surf, _, ref_poc)) = self.last_ref_surf
        {
            let r = VAPictureHEVC {
                picture_id: ref_surf,
                pic_order_cnt: ref_poc,
                flags: 0x0000_0008, // VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
                va_reserved: [0; 4],
            };
            ref_frames[0] = r;
            ref_list0[0] = r;
        }

        // coding_type bits 1..3: 1=I, 2=P. idr_pic_flag=bit0, reference_pic_flag=bit4.
        let coding_type = if is_idr { 1u32 } else { 2u32 };
        let pic_fields = u32::from(is_idr) | (coding_type << 1) | (1u32 << 4);
        let mut pic = VAEncPictureParameterBufferHEVC {
            decoded_curr_pic: curr_pic,
            reference_frames: ref_frames,
            coded_buf: self.coded_buf,
            collocated_ref_pic_index: 0,
            last_picture: 0,
            pic_init_qp: 26,
            diff_cu_qp_delta_depth: 0,
            pps_cb_qp_offset: 0,
            pps_cr_qp_offset: 0,
            num_tile_columns_minus1: 0,
            num_tile_rows_minus1: 0,
            column_width_minus1: [0; 19],
            row_height_minus1: [0; 21],
            log2_parallel_merge_level_minus2: 0,
            ctu_max_bitsize_allowed: 0,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            slice_pic_parameter_set_id: 0,
            nal_unit_type: if is_idr { 19 } else { 1 }, // IDR_W_RADL=19, TRAIL_R=1
            pic_fields,
            hierarchical_level_plus1: 0,
            va_byte_reserved: 0,
            scc_fields: 0,
            va_reserved: [0; 15],
        };
        buffers.push(self.create_buf(VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE, &mut pic)?);
        if is_idr && (self.packed_headers_caps & VA_ENC_PACKED_HEADER_PICTURE) != 0 {
            let pps = build_hevc_pps_nalu();
            self.submit_packed_header(buffers, VA_PACKED_HEADER_PICTURE, &pps)?;
        }

        let num_ctus = (self.enc_w as u32).div_ceil(32) * (self.enc_h as u32).div_ceil(32);
        // slice_fields: last_slice_of_pic_flag=1(b0), slice_temporal_mvp_enabled=1(b4), collocated_from_l0=1(b13)
        let slice_fields = 1u32 | (1u32 << 4) | (1u32 << 13);
        let mut slice = VAEncSliceParameterBufferHEVC {
            slice_segment_address: 0,
            num_ctu_in_slice: num_ctus,
            slice_type: if is_idr { 2 } else { 1 }, // HEVC: 2=I, 1=P
            slice_pic_parameter_set_id: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            ref_pic_list0: ref_list0,
            ref_pic_list1: [VAPictureHEVC::default(); 15],
            luma_log2_weight_denom: 0,
            delta_chroma_log2_weight_denom: 0,
            delta_luma_weight_l0: [0; 15],
            luma_offset_l0: [0; 15],
            delta_chroma_weight_l0: [[0; 2]; 15],
            chroma_offset_l0: [[0; 2]; 15],
            delta_luma_weight_l1: [0; 15],
            luma_offset_l1: [0; 15],
            delta_chroma_weight_l1: [[0; 2]; 15],
            chroma_offset_l1: [[0; 2]; 15],
            max_num_merge_cand: 5,
            slice_qp_delta: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            slice_beta_offset_div2: 0,
            slice_tc_offset_div2: 0,
            slice_fields,
            pred_weight_table_bit_offset: 0,
            pred_weight_table_bit_length: 0,
            va_reserved: [0; 6],
        };
        buffers.push(self.create_buf(VA_ENC_SLICE_PARAMETER_BUFFER_TYPE, &mut slice)?);
        Ok(())
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.lib.va_destroy_buffer)(self.dpy, self.coded_buf);
            let _ = (self.lib.va_destroy_context)(self.dpy, self.enc_ctx);
            if let Some(ctx) = self.vpp_ctx {
                let _ = (self.lib.va_destroy_context)(self.dpy, ctx);
            }
            if let Some(cfg) = self.vpp_cfg {
                let _ = (self.lib.va_destroy_config)(self.dpy, cfg);
            }
            let mut all_surfs = [self.input_surf, self.recon_surfs[0], self.recon_surfs[1]];
            let _ = (self.lib.va_destroy_surfaces)(self.dpy, all_surfs.as_mut_ptr(), 3);
            let _ = (self.lib.va_destroy_config)(self.dpy, self.enc_cfg);
            let _ = (self.lib.va_terminate)(self.dpy);
        }
    }
}

fn query_nv12_image_format(lib: &VaApiLib, dpy: VADisplay) -> VAImageFormat {
    let max_fmts = unsafe { (lib.va_max_num_image_formats)(dpy) }.max(0) as usize;
    if max_fmts > 0 {
        let mut fmts = vec![VAImageFormat::default(); max_fmts];
        let mut count = 0;
        if unsafe { (lib.va_query_image_formats)(dpy, fmts.as_mut_ptr(), &raw mut count) }
            == VA_STATUS_SUCCESS
        {
            for fmt in fmts.into_iter().take(count.max(0) as usize) {
                if fmt.fourcc == VA_FOURCC_NV12 {
                    return fmt;
                }
            }
        }
    }
    VAImageFormat {
        fourcc: VA_FOURCC_NV12,
        byte_order: 1,
        bits_per_pixel: 12,
        depth: 0,
        ..Default::default()
    }
}

fn create_vpp_context(
    lib: &VaApiLib,
    dpy: VADisplay,
    enc_w: u16,
    enc_h: u16,
) -> (Option<VAConfigID>, Option<VAContextID>) {
    let mut cfg = VA_INVALID_ID;
    let st = unsafe {
        (lib.va_create_config)(
            dpy,
            VA_PROFILE_NONE,
            VA_ENTRYPOINT_VIDEO_PROC,
            std::ptr::null_mut(),
            0,
            &raw mut cfg,
        )
    };
    if st != VA_STATUS_SUCCESS {
        return (None, None);
    }
    let mut ctx = VA_INVALID_ID;
    let st = unsafe {
        (lib.va_create_context)(
            dpy,
            cfg,
            enc_w as i32,
            enc_h as i32,
            0,
            std::ptr::null_mut(),
            0,
            &raw mut ctx,
        )
    };
    if st != VA_STATUS_SUCCESS {
        unsafe {
            let _ = (lib.va_destroy_config)(dpy, cfg);
        }
        return (None, None);
    }
    (Some(cfg), Some(ctx))
}

fn import_dmabuf_surface(lib: &VaApiLib, dpy: VADisplay, dmabuf: &DmaBufFrame) -> Result<VASurfaceID> {
    let mut desc = VADRMPRIMESurfaceDescriptor {
        fourcc: dmabuf.fourcc,
        width: dmabuf.width,
        height: dmabuf.height,
        num_objects: dmabuf.planes.len().min(4) as u32,
        objects: [VADrmPrimeObject::default(); 4],
        num_layers: 1,
        layers: [VADrmPrimeLayer::default(); 4],
    };
    desc.layers[0].drm_format = dmabuf.fourcc;
    desc.layers[0].num_planes = dmabuf.planes.len().min(4) as u32;
    for (i, p) in dmabuf.planes.iter().take(4).enumerate() {
        desc.objects[i].fd = p.fd.as_raw_fd();
        desc.objects[i].size = p.offset.saturating_add(p.pitch.saturating_mul(dmabuf.height));
        desc.objects[i].drm_format_modifier = dmabuf.modifier;
        desc.layers[0].object_index[i] = i as u32;
        desc.layers[0].offset[i] = p.offset;
        desc.layers[0].pitch[i] = p.pitch;
    }

    let rt_fmt = if dmabuf.fourcc == FOURCC_NV12 {
        VA_RT_FORMAT_YUV420
    } else {
        VA_RT_FORMAT_RGB32
    };
    let mut attribs = [
        VASurfaceAttrib {
            r#type: VA_SURFACE_ATTRIB_MEMORY_TYPE,
            flags: VA_SURFACE_ATTRIB_SETTABLE,
            value: VAGenericValue {
                r#type: VA_GENERIC_VALUE_TYPE_INTEGER,
                value: VAGenericValueUnion {
                    i: VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                },
            },
        },
        VASurfaceAttrib {
            r#type: VA_SURFACE_ATTRIB_EXT_BUF_DESCRIPTOR,
            flags: VA_SURFACE_ATTRIB_SETTABLE,
            value: VAGenericValue {
                r#type: VA_GENERIC_VALUE_TYPE_POINTER,
                value: VAGenericValueUnion {
                    p: (&raw mut desc).cast(),
                },
            },
        },
    ];
    let mut surf = VA_INVALID_SURFACE;
    let st = unsafe {
        (lib.va_create_surfaces)(
            dpy,
            rt_fmt,
            dmabuf.width,
            dmabuf.height,
            &raw mut surf,
            1,
            attribs.as_mut_ptr(),
            attribs.len() as u32,
        )
    };
    if st != VA_STATUS_SUCCESS {
        bail!("vaCreateSurfaces DRM_PRIME_2 import failed (status={st})");
    }
    Ok(surf)
}

fn run_vpp_blit(
    lib: &VaApiLib,
    dpy: VADisplay,
    vpp_ctx: VAContextID,
    src_surf: VASurfaceID,
    dst_surf: VASurfaceID,
    src_is_nv12: bool,
) -> Result<()> {
    let mut param = VAProcPipelineParameterBuffer {
        surface: src_surf,
        surface_region: std::ptr::null(),
        surface_color_standard: if src_is_nv12 {
            VA_PROC_COLOR_STANDARD_BT709
        } else {
            VA_PROC_COLOR_STANDARD_SRGB
        },
        output_region: std::ptr::null(),
        output_background_color: 0xff00_0000,
        output_color_standard: VA_PROC_COLOR_STANDARD_BT709,
        pipeline_flags: 0,
        filter_flags: 0,
        filters: std::ptr::null_mut(),
        num_filters: 0,
        forward_references: std::ptr::null_mut(),
        num_forward_references: 0,
        backward_references: std::ptr::null_mut(),
        num_backward_references: 0,
        rotation_state: 0,
        blend_state: std::ptr::null(),
        mirror_state: 0,
        additional_outputs: std::ptr::null_mut(),
        num_additional_outputs: 0,
        input_surface_flag: 0,
        output_surface_flag: 0,
        input_color_properties: VAProcColorProperties {
            chroma_sample_location: 0,
            color_range: if src_is_nv12 { 1 } else { 2 },
            colour_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 1,
            reserved: [0; 3],
        },
        output_color_properties: VAProcColorProperties {
            chroma_sample_location: 0,
            color_range: 1, // limited [16..235]
            colour_primaries: 1, // BT.709
            transfer_characteristics: 1,
            matrix_coefficients: 1,
            reserved: [0; 3],
        },
        processing_mode: 0,
        output_hdr_metadata: std::ptr::null_mut(),
        va_reserved: [0; 16],
    };

    let mut buf_id = VA_INVALID_ID;
    let st = unsafe {
        (lib.va_create_buffer)(
            dpy,
            vpp_ctx,
            VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE,
            std::mem::size_of::<VAProcPipelineParameterBuffer>() as u32,
            1,
            (&raw mut param).cast(),
            &raw mut buf_id,
        )
    };
    if st != VA_STATUS_SUCCESS {
        bail!("VPP vaCreateBuffer failed (status={st})");
    }

    let st_beg = unsafe { (lib.va_begin_picture)(dpy, vpp_ctx, dst_surf) };
    let st_ren = if st_beg == VA_STATUS_SUCCESS {
        unsafe { (lib.va_render_picture)(dpy, vpp_ctx, &raw mut buf_id, 1) }
    } else {
        st_beg
    };
    let st_end = unsafe { (lib.va_end_picture)(dpy, vpp_ctx) };
    unsafe {
        let _ = (lib.va_destroy_buffer)(dpy, buf_id);
    }
    if st_beg != VA_STATUS_SUCCESS || st_ren != VA_STATUS_SUCCESS || st_end != VA_STATUS_SUCCESS {
        bail!("VPP blit failed (beg={st_beg}, ren={st_ren}, end={st_end})");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn upload_nv12_image(
    lib: &VaApiLib,
    dpy: VADisplay,
    surf: VASurfaceID,
    fmt: &VAImageFormat,
    width: usize,
    height: usize,
    y: &[u8],
    uv: &[u8],
) -> Result<()> {
    let mut fmt_copy = *fmt;
    let mut img = VAImage::default();
    let st = unsafe {
        (lib.va_create_image)(dpy, &raw mut fmt_copy, width as i32, height as i32, &raw mut img)
    };
    if st != VA_STATUS_SUCCESS {
        bail!("vaCreateImage NV12 failed (status={st})");
    }

    let mut ptr: *mut c_void = std::ptr::null_mut();
    let st_map = unsafe { (lib.va_map_buffer)(dpy, img.buf, &raw mut ptr) };
    if st_map != VA_STATUS_SUCCESS || ptr.is_null() {
        unsafe {
            let _ = (lib.va_destroy_image)(dpy, img.image_id);
        }
        bail!("vaMapBuffer NV12 upload failed (status={st_map})");
    }

    let y_off = img.offsets[0] as usize;
    let y_pitch = (img.pitches[0] as usize).max(width);
    let uv_off = img.offsets[1] as usize;
    let uv_pitch = (img.pitches[1] as usize).max(width);
    unsafe {
        let base = ptr.cast::<u8>();
        for row in 0..height {
            std::ptr::copy_nonoverlapping(
                y.as_ptr().add(row * width),
                base.add(y_off + row * y_pitch),
                width,
            );
        }
        for row in 0..(height / 2) {
            std::ptr::copy_nonoverlapping(
                uv.as_ptr().add(row * width),
                base.add(uv_off + row * uv_pitch),
                width,
            );
        }
        let _ = (lib.va_unmap_buffer)(dpy, img.buf);
    }

    let st_put = unsafe {
        (lib.va_put_image)(
            dpy,
            surf,
            img.image_id,
            0,
            0,
            width as u32,
            height as u32,
            0,
            0,
            width as u32,
            height as u32,
        )
    };
    unsafe {
        let _ = (lib.va_destroy_image)(dpy, img.image_id);
    }
    if st_put != VA_STATUS_SUCCESS {
        bail!("vaPutImage NV12 failed (status={st_put})");
    }
    Ok(())
}

fn read_coded_buffer(lib: &VaApiLib, dpy: VADisplay, coded_buf: VABufferID) -> Result<Vec<u8>> {
    let mut seg_ptr: *mut c_void = std::ptr::null_mut();
    let st = unsafe { (lib.va_map_buffer)(dpy, coded_buf, &raw mut seg_ptr) };
    if st != VA_STATUS_SUCCESS || seg_ptr.is_null() {
        bail!("vaMapBuffer coded_buf failed (status={st})");
    }
    let mut out = Vec::new();
    let mut cur = seg_ptr.cast::<VACodedBufferSegment>();
    while !cur.is_null() {
        let seg = unsafe { &*cur };
        if !seg.buf.is_null() && seg.size > 0 {
            let slice = unsafe { std::slice::from_raw_parts(seg.buf.cast::<u8>(), seg.size as usize) };
            out.extend_from_slice(slice);
        }
        cur = seg.next;
    }
    unsafe {
        let _ = (lib.va_unmap_buffer)(dpy, coded_buf);
    }
    Ok(out)
}

// ---------- Annex-B Exp-Golomb RBSP BitWriter for packed VA-API headers ----------

struct BitWriter {
    rbsp: Vec<u8>,
    cur_byte: u8,
    bits_used: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            rbsp: Vec::with_capacity(64),
            cur_byte: 0,
            bits_used: 0,
        }
    }

    fn write_bits(&mut self, val: u32, count: u8) {
        for shift in (0..count).rev() {
            let bit = ((val >> shift) & 1) as u8;
            self.cur_byte = (self.cur_byte << 1) | bit;
            self.bits_used += 1;
            if self.bits_used == 8 {
                self.rbsp.push(self.cur_byte);
                self.cur_byte = 0;
                self.bits_used = 0;
            }
        }
    }

    fn write_bit(&mut self, b: bool) {
        self.write_bits(u32::from(b), 1);
    }

    fn write_ue(&mut self, val: u32) {
        let code = val + 1;
        let len = 32 - code.leading_zeros();
        if len > 1 {
            self.write_bits(0, (len - 1) as u8);
        }
        self.write_bits(code, len as u8);
    }

    fn write_se(&mut self, val: i32) {
        let mapped = if val <= 0 {
            ((-val) * 2) as u32
        } else {
            (val * 2 - 1) as u32
        };
        self.write_ue(mapped);
    }

    fn write_rbsp_trailing_bits(&mut self) {
        self.write_bit(true);
        if self.bits_used > 0 {
            let pad = 8 - self.bits_used;
            self.write_bits(0, pad);
        }
    }

    fn into_annexb_nalu(mut self) -> Vec<u8> {
        self.write_rbsp_trailing_bits();
        let mut out = Vec::with_capacity(self.rbsp.len() + 8);
        out.extend_from_slice(&[0, 0, 0, 1]);
        let mut zero_run = 0u8;
        for b in self.rbsp {
            if zero_run >= 2 && b <= 3 {
                out.push(0x03);
                zero_run = 0;
            }
            out.push(b);
            if b == 0 {
                zero_run = zero_run.saturating_add(1);
            } else {
                zero_run = 0;
            }
        }
        out
    }
}

pub fn build_h264_sps_nalu(width: u16, height: u16, fps: u16, profile: i32) -> Vec<u8> {
    let mb_w = (width as u32).div_ceil(16);
    let mb_h = (height as u32).div_ceil(16);
    let crop_r = (mb_w * 16 - width as u32) / 2;
    let crop_b = (mb_h * 16 - height as u32) / 2;
    let (profile_idc, constraint_set) = match profile {
        VA_PROFILE_H264_CONSTRAINED_BASELINE => (66u32, 0xE0u32),
        VA_PROFILE_H264_MAIN => (77u32, 0x40u32),
        _ => (100u32, 0x00u32),
    };

    let mut w = BitWriter::new();
    // NAL header: forbidden_zero_bit=0 | nal_ref_idc=3 | nal_unit_type=7 (SPS)
    w.write_bits(0x67, 8);
    w.write_bits(profile_idc, 8);
    w.write_bits(constraint_set, 8);
    w.write_bits(51, 8); // level 5.1
    w.write_ue(0); // seq_parameter_set_id
    if profile_idc == 100 {
        w.write_ue(1); // chroma_format_idc = 4:2:0
        w.write_ue(0); // bit_depth_luma_minus8
        w.write_ue(0); // bit_depth_chroma_minus8
        w.write_bit(false); // qpprime_y_zero_transform_bypass_flag
        w.write_bit(false); // seq_scaling_matrix_present_flag
    }
    w.write_ue(4); // log2_max_frame_num_minus4
    w.write_ue(2); // pic_order_cnt_type = 2
    w.write_ue(1); // max_num_ref_frames
    w.write_bit(false); // gaps_in_frame_num_value_allowed_flag
    w.write_ue(mb_w - 1);
    w.write_ue(mb_h - 1);
    w.write_bit(true); // frame_mbs_only_flag
    w.write_bit(true); // direct_8x8_inference_flag
    if crop_r > 0 || crop_b > 0 {
        w.write_bit(true);
        w.write_ue(0);
        w.write_ue(crop_r);
        w.write_ue(0);
        w.write_ue(crop_b);
    } else {
        w.write_bit(false);
    }
    w.write_bit(true); // vui_parameters_present_flag
    w.write_bit(false); // aspect_ratio_info_present_flag
    w.write_bit(false); // overscan_info_present_flag
    w.write_bit(true); // video_signal_type_present_flag
    w.write_bits(5, 3); // unspecified video_format
    w.write_bit(false); // video_full_range_flag = limited
    w.write_bit(true); // colour_description_present_flag
    w.write_bits(1, 8); // BT.709 primaries
    w.write_bits(1, 8); // BT.709 transfer
    w.write_bits(1, 8); // BT.709 matrix
    w.write_bit(false); // chroma_loc_info_present_flag
    w.write_bit(true); // timing_info_present_flag
    w.write_bits(1, 32); // num_units_in_tick
    w.write_bits(fps.max(1) as u32 * 2, 32); // time_scale
    w.write_bit(true); // fixed_frame_rate_flag
    w.write_bit(false); // nal_hrd_parameters_present_flag
    w.write_bit(false); // vcl_hrd_parameters_present_flag
    w.write_bit(false); // pic_struct_present_flag
    w.write_bit(false); // bitstream_restriction_flag
    w.into_annexb_nalu()
}

pub fn build_h264_pps_nalu(cabac: bool) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(0x68, 8); // nal_ref_idc=3, nal_unit_type=8 (PPS)
    w.write_ue(0); // pic_parameter_set_id
    w.write_ue(0); // seq_parameter_set_id
    w.write_bit(cabac); // entropy_coding_mode_flag
    w.write_bit(false); // bottom_field_pic_order_in_frame_present_flag
    w.write_ue(0); // num_slice_groups_minus1
    w.write_ue(0); // num_ref_idx_l0_default_active_minus1
    w.write_ue(0); // num_ref_idx_l1_default_active_minus1
    w.write_bit(false); // weighted_pred_flag
    w.write_bits(0, 2); // weighted_bipred_idc
    w.write_se(0); // pic_init_qp_minus26
    w.write_se(0); // pic_init_qs_minus26
    w.write_se(0); // chroma_qp_index_offset
    w.write_bit(true); // deblocking_filter_control_present_flag
    w.write_bit(false); // constrained_intra_pred_flag
    w.write_bit(false); // redundant_pic_cnt_present_flag
    w.into_annexb_nalu()
}

fn write_hevc_profile_tier_level(w: &mut BitWriter) {
    w.write_bits(0, 2); // general_profile_space
    w.write_bit(false); // general_tier_flag
    w.write_bits(1, 5); // general_profile_idc = Main
    w.write_bits(1u32 << 30, 32); // compatibility flag bit 1 (Main)
    w.write_bit(true); // general_progressive_source_flag
    w.write_bit(false); // general_interlaced_source_flag
    w.write_bit(false); // non_packed_constraint
    w.write_bit(true); // general_frame_only_constraint_flag
    w.write_bits(0, 13);
    w.write_bits(0, 31); // 44 bits of constraint zero padding: 13 + 31 = 44
    w.write_bits(153, 8); // general_level_idc = Level 5.1 (153)
}

pub fn build_hevc_vps_nalu(fps: u16) -> Vec<u8> {
    let mut w = BitWriter::new();
    // HEVC NAL header: forbidden=0(1) | nal_unit_type=32(6b) | layer=0(6b) | tid+1=1(3b) => 0x40, 0x01
    w.write_bits(0x40, 8);
    w.write_bits(0x01, 8);
    w.write_bits(0, 4); // vps_video_parameter_set_id
    w.write_bits(0b11, 2); // vps_base_layer_internal + vps_base_layer_available
    w.write_bits(0, 6); // vps_max_layers_minus1
    w.write_bits(0, 3); // vps_max_sub_layers_minus1
    w.write_bit(true); // vps_temporal_id_nesting_flag
    w.write_bits(0xffff, 16); // vps_reserved_0xffff_16bits
    write_hevc_profile_tier_level(&mut w);
    w.write_bit(true); // vps_sub_layer_ordering_info_present_flag
    w.write_ue(1); // vps_max_dec_pic_buffering_minus1[0]
    w.write_ue(0); // vps_max_num_reorder_pics[0]
    w.write_ue(0); // vps_max_latency_increase_plus1[0]
    w.write_bits(0, 6); // vps_max_layer_id
    w.write_ue(0); // vps_num_layer_sets_minus1
    w.write_bit(true); // vps_timing_info_present_flag
    w.write_bits(1, 32); // vps_num_units_in_tick
    w.write_bits(fps.max(1) as u32, 32); // vps_time_scale
    w.write_bit(false); // vps_poc_proportional_to_timing_flag
    w.write_ue(0); // vps_num_hrd_parameters
    w.write_bit(false); // vps_extension_flag
    w.into_annexb_nalu()
}

pub fn build_hevc_sps_nalu(
    coded_w: u16,
    coded_h: u16,
    disp_w: u16,
    disp_h: u16,
    fps: u16,
) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(0x42, 8); // nal_unit_type = 33 (SPS)
    w.write_bits(0x01, 8);
    w.write_bits(0, 4); // sps_video_parameter_set_id
    w.write_bits(0, 3); // sps_max_sub_layers_minus1
    w.write_bit(true); // sps_temporal_id_nesting_flag
    write_hevc_profile_tier_level(&mut w);
    w.write_ue(0); // sps_seq_parameter_set_id
    w.write_ue(1); // chroma_format_idc = 4:2:0
    w.write_ue(coded_w as u32);
    w.write_ue(coded_h as u32);
    let cw_r = coded_w.saturating_sub(disp_w) as u32 / 2;
    let cw_b = coded_h.saturating_sub(disp_h) as u32 / 2;
    if cw_r > 0 || cw_b > 0 {
        w.write_bit(true);
        w.write_ue(0);
        w.write_ue(cw_r);
        w.write_ue(0);
        w.write_ue(cw_b);
    } else {
        w.write_bit(false);
    }
    w.write_ue(0); // bit_depth_luma_minus8
    w.write_ue(0); // bit_depth_chroma_minus8
    w.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4
    w.write_bit(true); // sps_sub_layer_ordering_info_present_flag
    w.write_ue(1);
    w.write_ue(0);
    w.write_ue(0);
    w.write_ue(0); // log2_min_luma_coding_block_size_minus3
    w.write_ue(2); // log2_diff_max_min_luma_coding_block_size
    w.write_ue(0); // log2_min_luma_transform_block_size_minus2
    w.write_ue(3); // log2_diff_max_min_transform_block_size
    w.write_ue(2); // max_transform_hierarchy_depth_inter
    w.write_ue(2); // max_transform_hierarchy_depth_intra
    w.write_bit(false); // scaling_list_enabled_flag
    w.write_bit(false); // amp_enabled_flag
    w.write_bit(false); // sample_adaptive_offset_enabled_flag
    w.write_bit(false); // pcm_enabled_flag
    w.write_ue(0); // num_short_term_ref_pic_sets
    w.write_bit(false); // long_term_ref_pics_present_flag
    w.write_bit(true); // sps_temporal_mvp_enabled_flag
    w.write_bit(true); // strong_intra_smoothing_enabled_flag
    w.write_bit(true); // vui_parameters_present_flag
    w.write_bit(false); // aspect_ratio_info_present_flag
    w.write_bit(false); // overscan_info_present_flag
    w.write_bit(true); // video_signal_type_present_flag
    w.write_bits(5, 3);
    w.write_bit(false); // video_full_range_flag = limited
    w.write_bit(true); // colour_description_present_flag
    w.write_bits(1, 8);
    w.write_bits(1, 8);
    w.write_bits(1, 8);
    w.write_bit(false); // chroma_loc_info_present_flag
    w.write_bit(false); // neutral_chroma_indication_flag
    w.write_bit(false); // field_seq_flag
    w.write_bit(false); // frame_field_info_present_flag
    w.write_bit(false); // default_display_window_flag
    w.write_bit(true); // vui_timing_info_present_flag
    w.write_bits(1, 32);
    w.write_bits(fps.max(1) as u32, 32);
    w.write_bit(false); // vui_poc_proportional_to_timing_flag
    w.write_bit(false); // vui_hrd_parameters_present_flag
    w.write_bit(false); // bitstream_restriction_flag
    w.write_bit(false); // sps_extension_present_flag
    w.into_annexb_nalu()
}

pub fn build_hevc_pps_nalu() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(0x44, 8); // nal_unit_type = 34 (PPS)
    w.write_bits(0x01, 8);
    w.write_ue(0); // pps_pic_parameter_set_id
    w.write_ue(0); // pps_seq_parameter_set_id
    w.write_bit(false); // dependent_slice_segments_enabled_flag
    w.write_bit(false); // output_flag_present_flag
    w.write_bits(0, 3); // num_extra_slice_header_bits
    w.write_bit(false); // sign_data_hiding_enabled_flag
    w.write_bit(false); // cabac_init_present_flag
    w.write_ue(0); // num_ref_idx_l0_default_active_minus1
    w.write_ue(0); // num_ref_idx_l1_default_active_minus1
    w.write_se(0); // init_qp_minus26
    w.write_bit(false); // constrained_intra_pred_flag
    w.write_bit(false); // transform_skip_enabled_flag
    w.write_bit(false); // cu_qp_delta_enabled_flag
    w.write_se(0); // pps_cb_qp_offset
    w.write_se(0); // pps_cr_qp_offset
    w.write_bit(false); // pps_slice_chroma_qp_offsets_present_flag
    w.write_bit(false); // weighted_pred_flag
    w.write_bit(false); // weighted_bipred_flag
    w.write_bit(false); // transquant_bypass_enabled_flag
    w.write_bit(false); // tiles_enabled_flag
    w.write_bit(false); // entropy_coding_sync_enabled_flag
    w.write_bit(true); // pps_loop_filter_across_slices_enabled_flag
    w.write_bit(false); // deblocking_filter_control_present_flag
    w.write_bit(false); // pps_scaling_list_data_present_flag
    w.write_bit(false); // lists_modification_present_flag
    w.write_ue(0); // log2_parallel_merge_level_minus2
    w.write_bit(false); // slice_segment_header_extension_present_flag
    w.write_bit(false); // pps_extension_present_flag
    w.into_annexb_nalu()
}

#[cfg(test)]
mod tests {
    use super::*;
    use usher_proto::annexb::{AnnexBSplitter, is_keyframe_annexb};

    #[test]
    fn vaapi_bitwriters_produce_valid_annexb_headers() {
        let mut h264 = build_h264_sps_nalu(1920, 1080, 60, VA_PROFILE_H264_CONSTRAINED_BASELINE);
        h264.extend_from_slice(&build_h264_pps_nalu(false));
        assert!(h264.starts_with(&[0, 0, 0, 1, 0x67]));
        assert!(h264.windows(5).any(|w| w == [0, 0, 0, 1, 0x68]));
        let mut sp = AnnexBSplitter::new(VideoCodec::H264);
        assert!(!is_keyframe_annexb(VideoCodec::H264, &h264));
        let _ = sp.push_bytes(&h264);

        let mut hevc = build_hevc_vps_nalu(120);
        hevc.extend_from_slice(&build_hevc_sps_nalu(1920, 1088, 1920, 1080, 120));
        hevc.extend_from_slice(&build_hevc_pps_nalu());
        assert!(hevc.starts_with(&[0, 0, 0, 1, 0x40, 0x01]));
        assert!(hevc.windows(6).any(|w| w == [0, 0, 0, 1, 0x42, 0x01]));
        assert!(hevc.windows(6).any(|w| w == [0, 0, 0, 1, 0x44, 0x01]));
    }
}
