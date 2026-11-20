use winit::window::Icon;

const ICON_SIZE: u32 = 64;

/// Renders Usher's rounded-square SVG logo badge (`assets/logo-*.svg`) into a 64x64 RGBA icon.
pub fn render_logo_badge_rgba() -> Vec<u8> {
    let u_pts = sample_u_polyline();
    let chev_pts = [(27.0f32, 21.0f32), (36.0f32, 29.0f32), (27.0f32, 37.0f32)];
    let mut rgba = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];

    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;

            let d_bg = sdf_rounded_rect(px - 32.0, py - 32.0, 32.0, 32.0, 16.0);
            let bg_cov = cov_less_than(d_bg, 0.0);
            if bg_cov <= 0.0 {
                continue;
            }

            let t_bg = ((px + py) / 128.0).clamp(0.0, 1.0);
            let mut rgb = lerp_rgb([15.0, 23.0, 42.0], [30.0, 41.0, 59.0], t_bg);

            // Inner border: rx=15.25 stroke-width=1.5 #334155 @ 0.6 opacity centered at d_bg = -0.75
            let border_cov = cov_less_than((d_bg + 0.75).abs(), 0.75) * 0.6;
            rgb = blend_over(rgb, [51.0, 65.0, 85.0], border_cov);

            // U path: diagonal gradient from (16, 15) to (48, 49) (#22D3EE -> #38BDF8 -> #6366F1)
            let d_u = dist_polyline(px, py, &u_pts);
            let u_cov = cov_less_than(d_u, 2.6);
            if u_cov > 0.0 {
                let u_color = sample_u_gradient(px, py);
                rgb = blend_over(rgb, u_color, u_cov);
            }

            // '>' chevron: stroke-width=4.4 in #38BDF8
            let d_chev = dist_polyline(px, py, &chev_pts);
            let chev_cov = cov_less_than(d_chev, 2.2);
            if chev_cov > 0.0 {
                rgb = blend_over(rgb, [56.0, 189.0, 248.0], chev_cov);
            }

            let off = ((y * ICON_SIZE + x) * 4) as usize;
            rgba[off] = rgb[0].round().clamp(0.0, 255.0) as u8;
            rgba[off + 1] = rgb[1].round().clamp(0.0, 255.0) as u8;
            rgba[off + 2] = rgb[2].round().clamp(0.0, 255.0) as u8;
            rgba[off + 3] = (bg_cov * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    rgba
}

pub fn build_window_icon() -> Option<Icon> {
    let rgba = render_logo_badge_rgba();
    Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE).ok()
}

fn cov_less_than(dist: f32, radius: f32) -> f32 {
    let edge = radius - dist;
    ((edge + 0.65) / 1.3).clamp(0.0, 1.0)
}

fn sdf_rounded_rect(x: f32, y: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = x.abs() - (hx - r);
    let qy = y.abs() - (hy - r);
    let out = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let inside = qx.max(qy).min(0.0);
    out + inside - r
}

fn dist_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;
    let t = if len_sq <= 1e-6 {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0)
    };
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

fn dist_polyline(px: f32, py: f32, pts: &[(f32, f32)]) -> f32 {
    let mut best = f32::MAX;
    for w in pts.windows(2) {
        let d = dist_segment(px, py, w[0].0, w[0].1, w[1].0, w[1].1);
        if d < best {
            best = d;
        }
    }
    best
}

fn sample_u_polyline() -> Vec<(f32, f32)> {
    let mut pts = Vec::with_capacity(48);
    pts.push((16.0, 15.0));
    pts.push((16.0, 35.0));

    let steps = 18;
    // Left cubic Bézier (shifted by -6,-6): (16,35) -> (16,44), (23,49), (32,49)
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        pts.push(cubic_bezier(
            t,
            (16.0, 35.0),
            (16.0, 44.0),
            (23.0, 49.0),
            (32.0, 49.0),
        ));
    }
    // Right cubic Bézier: (32,49) -> (41,49), (48,44), (48,35)
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        pts.push(cubic_bezier(
            t,
            (32.0, 49.0),
            (41.0, 49.0),
            (48.0, 44.0),
            (48.0, 35.0),
        ));
    }
    pts.push((48.0, 15.0));
    pts
}

fn cubic_bezier(
    t: f32,
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
) -> (f32, f32) {
    let u = 1.0 - t;
    let uu = u * u;
    let tt = t * t;
    let x = uu * u * p0.0 + 3.0 * uu * t * p1.0 + 3.0 * u * tt * p2.0 + tt * t * p3.0;
    let y = uu * u * p0.1 + 3.0 * uu * t * p1.1 + 3.0 * u * tt * p2.1 + tt * t * p3.1;
    (x, y)
}

fn sample_u_gradient(px: f32, py: f32) -> [f32; 3] {
    // Gradient vector from (16, 15) to (48, 49)
    let gx = 32.0f32;
    let gy = 34.0f32;
    let denom = gx * gx + gy * gy;
    let t = (((px - 16.0) * gx + (py - 15.0) * gy) / denom).clamp(0.0, 1.0);
    if t < 0.5 {
        lerp_rgb([34.0, 211.0, 238.0], [56.0, 189.0, 248.0], t * 2.0)
    } else {
        lerp_rgb([56.0, 189.0, 248.0], [99.0, 102.0, 241.0], (t - 0.5) * 2.0)
    }
}

fn lerp_rgb(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn blend_over(dst: [f32; 3], src: [f32; 3], alpha: f32) -> [f32; 3] {
    let a = alpha.clamp(0.0, 1.0);
    [
        dst[0] * (1.0 - a) + src[0] * a,
        dst[1] * (1.0 - a) + src[1] * a,
        dst[2] * (1.0 - a) + src[2] * a,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * ICON_SIZE + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn renders_transparent_corners_dark_badge_and_cyan_glyphs() {
        let rgba = render_logo_badge_rgba();
        assert_eq!(rgba.len(), 64 * 64 * 4);
        assert_eq!(pixel(&rgba, 0, 0)[3], 0);
        let bg = pixel(&rgba, 10, 10);
        assert_eq!(bg[3], 255);
        assert!(bg[0] < 40 && bg[1] < 50 && bg[2] < 70);

        let u_px = pixel(&rgba, 16, 24);
        assert!(u_px[2] > 180 && u_px[1] > 150);

        let chev_px = pixel(&rgba, 36, 29);
        assert!(chev_px[2] > 200 && chev_px[1] > 150);

        assert!(build_window_icon().is_some());
    }
}
