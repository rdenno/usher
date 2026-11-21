#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgrx,
    Bgra,
    Rgbx,
    Rgba,
    Nv12,
}

#[derive(Debug, Clone)]
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}

#[inline]
fn rgb_to_y_bt709(r: i32, g: i32, b: i32) -> u8 {
    // BT.709 limited-range [16..235]:
    // Y = 16 + ((47*R + 157*G + 16*B) >> 8)
    let y = 16 + ((47 * r + 157 * g + 16 * b + 128) >> 8);
    y.clamp(16, 235) as u8
}

#[inline]
fn rgb_to_uv_bt709(r: i32, g: i32, b: i32) -> (u8, u8) {
    // BT.709 limited-range [16..240]:
    // U = 128 + ((-26*R - 86*G + 112*B) >> 8)
    // V = 128 + ((112*R - 102*G - 10*B) >> 8)
    let u = 128 + ((-26 * r - 86 * g + 112 * b + 128) >> 8);
    let v = 128 + ((112 * r - 102 * g - 10 * b + 128) >> 8);
    (u.clamp(16, 240) as u8, v.clamp(16, 240) as u8)
}

#[inline]
fn load_rgb(pixel: &[u8], is_bgr: bool) -> (i32, i32, i32) {
    if is_bgr {
        (pixel[2] as i32, pixel[1] as i32, pixel[0] as i32)
    } else {
        (pixel[0] as i32, pixel[1] as i32, pixel[2] as i32)
    }
}

/// Converts 32-bit RGBx/BGRx/RGBA/BGRA into BT.709 NV12 (planar Y + interleaved UV).
/// When `src_w != dst_w` or `src_h != dst_h`, uses fast nearest-neighbor scaling.
#[allow(clippy::too_many_arguments)]
pub fn rgba_to_nv12(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    src_stride: usize,
    is_bgr: bool,
    dst_w: usize,
    dst_h: usize,
    dst_y: &mut [u8],
    dst_uv: &mut [u8],
) -> Result<()> {
    let dw = dst_w & !1;
    let dh = dst_h & !1;
    if src_w == 0 || src_h == 0 || dw < 2 || dh < 2 {
        bail!("invalid dimensions for rgba_to_nv12: {src_w}x{src_h} -> {dst_w}x{dst_h}");
    }
    let min_stride = src_w * 4;
    if src_stride < min_stride {
        bail!("source stride {src_stride} smaller than {min_stride}");
    }
    let needed_src = (src_h - 1) * src_stride + min_stride;
    if src.len() < needed_src {
        bail!(
            "source buffer {}B smaller than required {}B",
            src.len(),
            needed_src
        );
    }
    if dst_y.len() < dw * dh || dst_uv.len() < dw * (dh / 2) {
        bail!("destination NV12 buffers too small for {dw}x{dh}");
    }

    if src_w == dw && src_h == dh {
        for y in (0..dh).step_by(2) {
            let row0 = &src[y * src_stride..y * src_stride + min_stride];
            let row1 = &src[(y + 1) * src_stride..(y + 1) * src_stride + min_stride];
            let (dst_y0, dst_y1) = dst_y[y * dw..(y + 2) * dw].split_at_mut(dw);
            let uv_row = &mut dst_uv[(y / 2) * dw..(y / 2 + 1) * dw];

            for x in (0..dw).step_by(2) {
                let off0 = x * 4;
                let off1 = off0 + 4;
                let (r00, g00, b00) = load_rgb(&row0[off0..], is_bgr);
                let (r01, g01, b01) = load_rgb(&row0[off1..], is_bgr);
                let (r10, g10, b10) = load_rgb(&row1[off0..], is_bgr);
                let (r11, g11, b11) = load_rgb(&row1[off1..], is_bgr);

                dst_y0[x] = rgb_to_y_bt709(r00, g00, b00);
                dst_y0[x + 1] = rgb_to_y_bt709(r01, g01, b01);
                dst_y1[x] = rgb_to_y_bt709(r10, g10, b10);
                dst_y1[x + 1] = rgb_to_y_bt709(r11, g11, b11);

                let r_avg = (r00 + r01 + r10 + r11 + 2) >> 2;
                let g_avg = (g00 + g01 + g10 + g11 + 2) >> 2;
                let b_avg = (b00 + b01 + b10 + b11 + 2) >> 2;
                let (u, v) = rgb_to_uv_bt709(r_avg, g_avg, b_avg);
                uv_row[x] = u;
                uv_row[x + 1] = v;
            }
        }
        return Ok(());
    }

    let x_map: Vec<usize> = (0..dw).map(|x| (x * src_w) / dw).collect();
    for y in (0..dh).step_by(2) {
        let sy0 = ((y * src_h) / dh).min(src_h - 1);
        let sy1 = (((y + 1) * src_h) / dh).min(src_h - 1);
        let row0 = &src[sy0 * src_stride..sy0 * src_stride + min_stride];
        let row1 = &src[sy1 * src_stride..sy1 * src_stride + min_stride];
        let (dst_y0, dst_y1) = dst_y[y * dw..(y + 2) * dw].split_at_mut(dw);
        let uv_row = &mut dst_uv[(y / 2) * dw..(y / 2 + 1) * dw];

        for x in (0..dw).step_by(2) {
            let sx0 = x_map[x] * 4;
            let sx1 = x_map[x + 1] * 4;
            let (r00, g00, b00) = load_rgb(&row0[sx0..], is_bgr);
            let (r01, g01, b01) = load_rgb(&row0[sx1..], is_bgr);
            let (r10, g10, b10) = load_rgb(&row1[sx0..], is_bgr);
            let (r11, g11, b11) = load_rgb(&row1[sx1..], is_bgr);

            dst_y0[x] = rgb_to_y_bt709(r00, g00, b00);
            dst_y0[x + 1] = rgb_to_y_bt709(r01, g01, b01);
            dst_y1[x] = rgb_to_y_bt709(r10, g10, b10);
            dst_y1[x + 1] = rgb_to_y_bt709(r11, g11, b11);

            let r_avg = (r00 + r01 + r10 + r11 + 2) >> 2;
            let g_avg = (g00 + g01 + g10 + g11 + 2) >> 2;
            let b_avg = (b00 + b01 + b10 + b11 + 2) >> 2;
            let (u, v) = rgb_to_uv_bt709(r_avg, g_avg, b_avg);
            uv_row[x] = u;
            uv_row[x + 1] = v;
        }
    }
    Ok(())
}

/// Converts 32-bit RGBx/BGRx/RGBA/BGRA into BT.709 I420 (planar Y, U, V).
/// When `src_w != dst_w` or `src_h != dst_h`, uses fast nearest-neighbor scaling.
#[allow(clippy::too_many_arguments)]
pub fn rgba_to_i420(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    src_stride: usize,
    is_bgr: bool,
    dst_w: usize,
    dst_h: usize,
    dst_y: &mut [u8],
    dst_u: &mut [u8],
    dst_v: &mut [u8],
) -> Result<()> {
    let dw = dst_w & !1;
    let dh = dst_h & !1;
    let cw = dw / 2;
    let ch = dh / 2;
    if src_w == 0 || src_h == 0 || dw < 2 || dh < 2 {
        bail!("invalid dimensions for rgba_to_i420: {src_w}x{src_h} -> {dst_w}x{dst_h}");
    }
    let min_stride = src_w * 4;
    if src_stride < min_stride {
        bail!("source stride {src_stride} smaller than {min_stride}");
    }
    let needed_src = (src_h - 1) * src_stride + min_stride;
    if src.len() < needed_src {
        bail!(
            "source buffer {}B smaller than required {}B",
            src.len(),
            needed_src
        );
    }
    if dst_y.len() < dw * dh || dst_u.len() < cw * ch || dst_v.len() < cw * ch {
        bail!("destination I420 buffers too small for {dw}x{dh}");
    }

    if src_w == dw && src_h == dh {
        for y in (0..dh).step_by(2) {
            let row0 = &src[y * src_stride..y * src_stride + min_stride];
            let row1 = &src[(y + 1) * src_stride..(y + 1) * src_stride + min_stride];
            let (dst_y0, dst_y1) = dst_y[y * dw..(y + 2) * dw].split_at_mut(dw);
            let cy = y / 2;
            let u_row = &mut dst_u[cy * cw..(cy + 1) * cw];
            let v_row = &mut dst_v[cy * cw..(cy + 1) * cw];

            for cx in 0..cw {
                let x = cx * 2;
                let off0 = x * 4;
                let off1 = off0 + 4;
                let (r00, g00, b00) = load_rgb(&row0[off0..], is_bgr);
                let (r01, g01, b01) = load_rgb(&row0[off1..], is_bgr);
                let (r10, g10, b10) = load_rgb(&row1[off0..], is_bgr);
                let (r11, g11, b11) = load_rgb(&row1[off1..], is_bgr);

                dst_y0[x] = rgb_to_y_bt709(r00, g00, b00);
                dst_y0[x + 1] = rgb_to_y_bt709(r01, g01, b01);
                dst_y1[x] = rgb_to_y_bt709(r10, g10, b10);
                dst_y1[x + 1] = rgb_to_y_bt709(r11, g11, b11);

                let r_avg = (r00 + r01 + r10 + r11 + 2) >> 2;
                let g_avg = (g00 + g01 + g10 + g11 + 2) >> 2;
                let b_avg = (b00 + b01 + b10 + b11 + 2) >> 2;
                let (u, v) = rgb_to_uv_bt709(r_avg, g_avg, b_avg);
                u_row[cx] = u;
                v_row[cx] = v;
            }
        }
        return Ok(());
    }

    let x_map: Vec<usize> = (0..dw).map(|x| (x * src_w) / dw).collect();
    for y in (0..dh).step_by(2) {
        let sy0 = ((y * src_h) / dh).min(src_h - 1);
        let sy1 = (((y + 1) * src_h) / dh).min(src_h - 1);
        let row0 = &src[sy0 * src_stride..sy0 * src_stride + min_stride];
        let row1 = &src[sy1 * src_stride..sy1 * src_stride + min_stride];
        let (dst_y0, dst_y1) = dst_y[y * dw..(y + 2) * dw].split_at_mut(dw);
        let cy = y / 2;
        let u_row = &mut dst_u[cy * cw..(cy + 1) * cw];
        let v_row = &mut dst_v[cy * cw..(cy + 1) * cw];

        for cx in 0..cw {
            let x = cx * 2;
            let sx0 = x_map[x] * 4;
            let sx1 = x_map[x + 1] * 4;
            let (r00, g00, b00) = load_rgb(&row0[sx0..], is_bgr);
            let (r01, g01, b01) = load_rgb(&row0[sx1..], is_bgr);
            let (r10, g10, b10) = load_rgb(&row1[sx0..], is_bgr);
            let (r11, g11, b11) = load_rgb(&row1[sx1..], is_bgr);

            dst_y0[x] = rgb_to_y_bt709(r00, g00, b00);
            dst_y0[x + 1] = rgb_to_y_bt709(r01, g01, b01);
            dst_y1[x] = rgb_to_y_bt709(r10, g10, b10);
            dst_y1[x + 1] = rgb_to_y_bt709(r11, g11, b11);

            let r_avg = (r00 + r01 + r10 + r11 + 2) >> 2;
            let g_avg = (g00 + g01 + g10 + g11 + 2) >> 2;
            let b_avg = (b00 + b01 + b10 + b11 + 2) >> 2;
            let (u, v) = rgb_to_uv_bt709(r_avg, g_avg, b_avg);
            u_row[cx] = u;
            v_row[cx] = v;
        }
    }
    Ok(())
}

/// Converts planar NV12 (`src_stride` padded rows for Y then UV) into I420.
#[allow(clippy::too_many_arguments)]
pub fn nv12_to_i420(
    src_nv12: &[u8],
    src_w: usize,
    src_h: usize,
    src_stride: usize,
    dst_w: usize,
    dst_h: usize,
    dst_y: &mut [u8],
    dst_u: &mut [u8],
    dst_v: &mut [u8],
) -> Result<()> {
    let dw = dst_w & !1;
    let dh = dst_h & !1;
    let cw = dw / 2;
    let ch = dh / 2;
    if src_w < 2 || src_h < 2 || dw < 2 || dh < 2 {
        bail!("invalid dimensions for nv12_to_i420: {src_w}x{src_h} -> {dst_w}x{dst_h}");
    }
    if src_stride < src_w {
        bail!("NV12 source stride {src_stride} smaller than {src_w}");
    }
    let uv_base = src_h * src_stride;
    let needed = uv_base + (src_h / 2) * src_stride;
    if src_nv12.len() < needed {
        bail!(
            "NV12 buffer {}B smaller than required {}B",
            src_nv12.len(),
            needed
        );
    }
    if dst_y.len() < dw * dh || dst_u.len() < cw * ch || dst_v.len() < cw * ch {
        bail!("destination I420 buffers too small for {dw}x{dh}");
    }

    let uv_src = &src_nv12[uv_base..];
    if src_w == dw && src_h == dh {
        for y in 0..dh {
            dst_y[y * dw..(y + 1) * dw]
                .copy_from_slice(&src_nv12[y * src_stride..y * src_stride + dw]);
        }
        for cy in 0..ch {
            let src_uv = &uv_src[cy * src_stride..cy * src_stride + dw];
            let u_row = &mut dst_u[cy * cw..(cy + 1) * cw];
            let v_row = &mut dst_v[cy * cw..(cy + 1) * cw];
            for cx in 0..cw {
                u_row[cx] = src_uv[cx * 2];
                v_row[cx] = src_uv[cx * 2 + 1];
            }
        }
        return Ok(());
    }

    for y in 0..dh {
        let sy = (y * src_h) / dh;
        let srow = &src_nv12[sy * src_stride..sy * src_stride + src_w];
        let drow = &mut dst_y[y * dw..(y + 1) * dw];
        for (x, dst_px) in drow.iter_mut().enumerate() {
            *dst_px = srow[(x * src_w) / dw];
        }
    }
    let src_cw = (src_w / 2).max(1);
    let src_ch = (src_h / 2).max(1);
    for cy in 0..ch {
        let scy = (cy * src_ch) / ch;
        let src_uv = &uv_src[scy * src_stride..];
        let u_row = &mut dst_u[cy * cw..(cy + 1) * cw];
        let v_row = &mut dst_v[cy * cw..(cy + 1) * cw];
        for cx in 0..cw {
            let scx = (cx * src_cw) / cw;
            u_row[cx] = src_uv[scx * 2];
            v_row[cx] = src_uv[scx * 2 + 1];
        }
    }
    Ok(())
}

pub fn cpu_frame_to_nv12(
    frame: &CpuFrame,
    dst_w: usize,
    dst_h: usize,
    dst_y: &mut [u8],
    dst_uv: &mut [u8],
) -> Result<()> {
    let sw = frame.width as usize;
    let sh = frame.height as usize;
    let dw = dst_w & !1;
    let dh = dst_h & !1;
    match frame.format {
        PixelFormat::Bgrx | PixelFormat::Bgra => {
            rgba_to_nv12(&frame.data, sw, sh, frame.stride, true, dw, dh, dst_y, dst_uv)
        }
        PixelFormat::Rgbx | PixelFormat::Rgba => {
            rgba_to_nv12(&frame.data, sw, sh, frame.stride, false, dw, dh, dst_y, dst_uv)
        }
        PixelFormat::Nv12 => {
            if frame.stride < sw {
                bail!("NV12 stride {} smaller than {sw}", frame.stride);
            }
            let uv_base = sh * frame.stride;
            let needed = uv_base + (sh / 2) * frame.stride;
            if frame.data.len() < needed {
                bail!("NV12 frame truncated");
            }
            if dst_y.len() < dw * dh || dst_uv.len() < dw * (dh / 2) {
                bail!("destination NV12 buffers too small");
            }
            let uv_src = &frame.data[uv_base..];
            if sw == dw && sh == dh {
                for y in 0..dh {
                    dst_y[y * dw..(y + 1) * dw]
                        .copy_from_slice(&frame.data[y * frame.stride..y * frame.stride + dw]);
                }
                for cy in 0..(dh / 2) {
                    dst_uv[cy * dw..(cy + 1) * dw]
                        .copy_from_slice(&uv_src[cy * frame.stride..cy * frame.stride + dw]);
                }
                Ok(())
            } else {
                for y in 0..dh {
                    let sy = (y * sh) / dh;
                    let srow = &frame.data[sy * frame.stride..sy * frame.stride + sw];
                    let drow = &mut dst_y[y * dw..(y + 1) * dw];
                    for (x, dst_px) in drow.iter_mut().enumerate() {
                        *dst_px = srow[(x * sw) / dw];
                    }
                }
                let src_cw = (sw / 2).max(1);
                let src_ch = (sh / 2).max(1);
                let dst_cw = dw / 2;
                let dst_ch = dh / 2;
                for cy in 0..dst_ch {
                    let scy = (cy * src_ch) / dst_ch;
                    let srow = &uv_src[scy * frame.stride..];
                    let drow = &mut dst_uv[cy * dw..(cy + 1) * dw];
                    for cx in 0..dst_cw {
                        let scx = (cx * src_cw) / dst_cw;
                        drow[cx * 2] = srow[scx * 2];
                        drow[cx * 2 + 1] = srow[scx * 2 + 1];
                    }
                }
                Ok(())
            }
        }
    }
}

pub fn cpu_frame_to_i420(
    frame: &CpuFrame,
    dst_w: usize,
    dst_h: usize,
    dst_y: &mut [u8],
    dst_u: &mut [u8],
    dst_v: &mut [u8],
) -> Result<()> {
    let sw = frame.width as usize;
    let sh = frame.height as usize;
    let dw = dst_w & !1;
    let dh = dst_h & !1;
    match frame.format {
        PixelFormat::Bgrx | PixelFormat::Bgra => {
            rgba_to_i420(&frame.data, sw, sh, frame.stride, true, dw, dh, dst_y, dst_u, dst_v)
        }
        PixelFormat::Rgbx | PixelFormat::Rgba => {
            rgba_to_i420(&frame.data, sw, sh, frame.stride, false, dw, dh, dst_y, dst_u, dst_v)
        }
        PixelFormat::Nv12 => nv12_to_i420(
            &frame.data,
            sw,
            sh,
            frame.stride,
            dw,
            dh,
            dst_y,
            dst_u,
            dst_v,
        ),
    }
}

/// Converts 32-bit RGBx/BGRx/RGBA/BGRA into BT.709 planar YUV 4:4:4 (full-resolution Y, U, V planes).
/// When `src_w != dst_w` or `src_h != dst_h`, uses fast nearest-neighbor scaling.
#[allow(clippy::too_many_arguments)]
pub fn rgba_to_yuv444(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    src_stride: usize,
    is_bgr: bool,
    dst_w: usize,
    dst_h: usize,
    dst_y: &mut [u8],
    dst_u: &mut [u8],
    dst_v: &mut [u8],
) -> Result<()> {
    let dw = dst_w & !1;
    let dh = dst_h & !1;
    if src_w == 0 || src_h == 0 || dw < 2 || dh < 2 {
        bail!("invalid dimensions for rgba_to_yuv444: {src_w}x{src_h} -> {dst_w}x{dst_h}");
    }
    let min_stride = src_w * 4;
    if src_stride < min_stride {
        bail!("source stride {src_stride} smaller than {min_stride}");
    }
    let needed_src = (src_h - 1) * src_stride + min_stride;
    if src.len() < needed_src {
        bail!(
            "source buffer {}B smaller than required {}B",
            src.len(),
            needed_src
        );
    }
    let plane_len = dw * dh;
    if dst_y.len() < plane_len || dst_u.len() < plane_len || dst_v.len() < plane_len {
        bail!("destination YUV444 buffers too small for {dw}x{dh}");
    }

    if src_w == dw && src_h == dh {
        for y in 0..dh {
            let row = &src[y * src_stride..y * src_stride + min_stride];
            let y_row = &mut dst_y[y * dw..(y + 1) * dw];
            let u_row = &mut dst_u[y * dw..(y + 1) * dw];
            let v_row = &mut dst_v[y * dw..(y + 1) * dw];
            for x in 0..dw {
                let (r, g, b) = load_rgb(&row[x * 4..], is_bgr);
                y_row[x] = rgb_to_y_bt709(r, g, b);
                let (u, v) = rgb_to_uv_bt709(r, g, b);
                u_row[x] = u;
                v_row[x] = v;
            }
        }
        return Ok(());
    }

    let x_map: Vec<usize> = (0..dw).map(|x| ((x * src_w) / dw) * 4).collect();
    for y in 0..dh {
        let sy = ((y * src_h) / dh).min(src_h - 1);
        let row = &src[sy * src_stride..sy * src_stride + min_stride];
        let y_row = &mut dst_y[y * dw..(y + 1) * dw];
        let u_row = &mut dst_u[y * dw..(y + 1) * dw];
        let v_row = &mut dst_v[y * dw..(y + 1) * dw];
        for (x, &sx_off) in x_map.iter().enumerate() {
            let (r, g, b) = load_rgb(&row[sx_off..], is_bgr);
            y_row[x] = rgb_to_y_bt709(r, g, b);
            let (u, v) = rgb_to_uv_bt709(r, g, b);
            u_row[x] = u;
            v_row[x] = v;
        }
    }
    Ok(())
}

/// Converts a captured `CpuFrame` into full-resolution BT.709 planar YUV 4:4:4.
pub fn cpu_frame_to_yuv444(
    frame: &CpuFrame,
    dst_w: usize,
    dst_h: usize,
    dst_y: &mut [u8],
    dst_u: &mut [u8],
    dst_v: &mut [u8],
) -> Result<()> {
    let sw = frame.width as usize;
    let sh = frame.height as usize;
    let dw = dst_w & !1;
    let dh = dst_h & !1;
    match frame.format {
        PixelFormat::Bgrx | PixelFormat::Bgra => {
            rgba_to_yuv444(&frame.data, sw, sh, frame.stride, true, dw, dh, dst_y, dst_u, dst_v)
        }
        PixelFormat::Rgbx | PixelFormat::Rgba => {
            rgba_to_yuv444(&frame.data, sw, sh, frame.stride, false, dw, dh, dst_y, dst_u, dst_v)
        }
        PixelFormat::Nv12 => {
            if frame.stride < sw || sw < 2 || sh < 2 || dw < 2 || dh < 2 {
                bail!("invalid NV12 frame dimensions/stride for YUV444");
            }
            let uv_base = sh * frame.stride;
            let needed = uv_base + (sh / 2) * frame.stride;
            if frame.data.len() < needed {
                bail!("NV12 frame truncated");
            }
            let plane_len = dw * dh;
            if dst_y.len() < plane_len || dst_u.len() < plane_len || dst_v.len() < plane_len {
                bail!("destination YUV444 buffers too small for {dw}x{dh}");
            }
            let uv_src = &frame.data[uv_base..];
            for y in 0..dh {
                let sy = (y * sh) / dh;
                let srow = &frame.data[sy * frame.stride..sy * frame.stride + sw];
                let suv = &uv_src[(sy / 2) * frame.stride..];
                let y_row = &mut dst_y[y * dw..(y + 1) * dw];
                let u_row = &mut dst_u[y * dw..(y + 1) * dw];
                let v_row = &mut dst_v[y * dw..(y + 1) * dw];
                for x in 0..dw {
                    let sx = (x * sw) / dw;
                    let cx = (sx / 2) * 2;
                    y_row[x] = srow[sx];
                    u_row[x] = suv[cx];
                    v_row[x] = suv[cx + 1];
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt709_primary_colors_match_limited_range_nv12_and_i420() {
        // 4x2 BGRA image: left half pure white (255,255,255), right half pure black (0,0,0)
        let w = 4usize;
        let h = 2usize;
        let mut bgra = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let off = (y * w + x) * 4;
                if x < 2 {
                    bgra[off..off + 4].copy_from_slice(&[255, 255, 255, 255]);
                } else {
                    bgra[off..off + 4].copy_from_slice(&[0, 0, 0, 255]);
                }
            }
        }

        let mut y_nv12 = vec![0u8; w * h];
        let mut uv_nv12 = vec![0u8; w * (h / 2)];
        rgba_to_nv12(&bgra, w, h, w * 4, true, w, h, &mut y_nv12, &mut uv_nv12).unwrap();
        assert_eq!(y_nv12[0], 235);
        assert_eq!(y_nv12[1], 235);
        assert_eq!(y_nv12[2], 16);
        assert_eq!(y_nv12[3], 16);
        assert_eq!(&uv_nv12[..], &[128, 128, 128, 128]);

        // Pure red in RGBA: R=255, G=0, B=0
        let rgba_red = [255u8, 0, 0, 255].repeat(4 * 2);
        let mut y_i = vec![0u8; 4 * 2];
        let mut u_i = vec![0u8; 2];
        let mut v_i = vec![0u8; 2];
        rgba_to_i420(&rgba_red, 4, 2, 16, false, 4, 2, &mut y_i, &mut u_i, &mut v_i).unwrap();
        // BT.709 red: Y~63, U~102, V~240
        assert!((y_i[0] as i32 - 63).abs() <= 1);
        assert!((u_i[0] as i32 - 102).abs() <= 1);
        assert!((v_i[0] as i32 - 240).abs() <= 1);
    }

    #[test]
    fn downscales_2x_cleanly() {
        let mut rgb = vec![0u8; 8 * 4 * 4];
        for px in rgb.as_chunks_mut::<4>().0 {
            // Pure green in BGRX: B=0, G=255, R=0
            px.copy_from_slice(&[0, 255, 0, 255]);
        }
        let frame = CpuFrame {
            width: 8,
            height: 4,
            stride: 32,
            format: PixelFormat::Bgrx,
            data: rgb,
        };
        let mut y = vec![0u8; 4 * 2];
        let mut uv = vec![0u8; 4];
        cpu_frame_to_nv12(&frame, 4, 2, &mut y, &mut uv).unwrap();
        assert!(y.iter().all(|&v| (v as i32 - 173).abs() <= 1));
        assert_eq!(uv[0], uv[2]);
        assert!(uv[0] < 60 && uv[1] < 40);
    }

    #[test]
    fn yuv444_preserves_single_pixel_red_blue_subpixel_edges() {
        // 4x2 BGRA image alternating pure red and pure blue every single column.
        let w = 4usize;
        let h = 2usize;
        let mut bgra = Vec::with_capacity(w * h * 4);
        for _y in 0..h {
            for x in 0..w {
                if x % 2 == 0 {
                    bgra.extend_from_slice(&[0, 0, 255, 255]); // red in BGRA
                } else {
                    bgra.extend_from_slice(&[255, 0, 0, 255]); // blue in BGRA
                }
            }
        }
        let frame = CpuFrame {
            width: w as u32,
            height: h as u32,
            stride: w * 4,
            format: PixelFormat::Bgra,
            data: bgra,
        };
        let mut y = vec![0u8; w * h];
        let mut u = vec![0u8; w * h];
        let mut v = vec![0u8; w * h];
        cpu_frame_to_yuv444(&frame, w, h, &mut y, &mut u, &mut v).unwrap();

        // Full-res chroma must preserve distinct U/V on adjacent pixels without averaging to gray!
        assert!((v[0] as i32 - 240).abs() <= 1); // red pixel V~240
        assert!((u[0] as i32 - 102).abs() <= 1); // red pixel U~102
        assert!((u[1] as i32 - 240).abs() <= 1); // blue pixel U~240
        assert!((v[1] as i32 - 118).abs() <= 1); // blue pixel V~118
    }
}
