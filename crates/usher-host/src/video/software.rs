use super::{EncodedFrame, VideoPipelineConfig};
use anyhow::{Result, anyhow};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;
use std::os::raw::c_int;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::annexb::is_keyframe_annexb;
use usher_proto::handshake::{QualityPreference, VideoCodec};

struct PlanarI420Frame {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl PlanarI420Frame {
    fn new(width: usize, height: usize) -> Self {
        let w = width & !1;
        let h = height & !1;
        Self {
            width: w,
            height: h,
            y: vec![16u8; w * h],
            u: vec![128u8; (w / 2) * (h / 2)],
            v: vec![128u8; (w / 2) * (h / 2)],
        }
    }

    /// Renders a live SMPTE-inspired color-bar + moving sweep pattern directly in I420
    /// so end-to-end streaming, frame pacing, and FEC recovery are immediately visible.
    fn render_test_pattern(&mut self, frame_idx: u64) {
        let w = self.width;
        let h = self.height;
        let cw = w / 2;
        let ch = h / 2;

        // 8 color bars in YUV (BT.709-ish): white, yellow, cyan, green, magenta, red, blue, dark
        const BARS: [(u8, u8, u8); 8] = [
            (235, 128, 128),
            (210, 16, 146),
            (170, 166, 16),
            (145, 54, 34),
            (106, 202, 222),
            (81, 90, 240),
            (41, 240, 110),
            (20, 128, 128),
        ];

        let top_band_h = h / 4;
        let box_size = (h / 6).max(24) & !1;
        let travel_x = w.saturating_sub(box_size).max(2);
        let box_x = (((frame_idx * 8) as usize) % travel_x) & !1;
        let box_y = (h / 2).saturating_sub(box_size / 2) & !1;

        for y in 0..h {
            let row_offset = y * w;
            if y < top_band_h {
                for x in 0..w {
                    let bar = (x * BARS.len() / w).min(BARS.len() - 1);
                    self.y[row_offset + x] = BARS[bar].0;
                }
            } else {
                // Dark slate animated gradient background
                let base = (24 + ((y + frame_idx as usize * 2) % 48)) as u8;
                self.y[row_offset..row_offset + w].fill(base);
                // Moving high-contrast bright sweep block
                if y >= box_y && y < box_y + box_size {
                    let end_x = (box_x + box_size).min(w);
                    self.y[row_offset + box_x..row_offset + end_x].fill(235);
                }
            }
        }

        // Render 16-bit binary frame counter bar at top-left for instant visual frame inspection
        let bit_w = (w / 48).max(8);
        let bit_h = (top_band_h / 3).max(8);
        for bit in 0..16 {
            let is_one = ((frame_idx >> (15 - bit)) & 1) == 1;
            let luma = if is_one { 245 } else { 16 };
            let start_x = 12 + bit * (bit_w + 4);
            if start_x + bit_w >= w {
                break;
            }
            for y in 12..(12 + bit_h).min(h) {
                let row = y * w;
                self.y[row + start_x..row + start_x + bit_w].fill(luma);
            }
        }

        for cy in 0..ch {
            let crow = cy * cw;
            let full_y = cy * 2;
            if full_y < top_band_h {
                for cx in 0..cw {
                    let bar = ((cx * 2) * BARS.len() / w).min(BARS.len() - 1);
                    self.u[crow + cx] = BARS[bar].1;
                    self.v[crow + cx] = BARS[bar].2;
                }
            } else {
                self.u[crow..crow + cw].fill(128);
                self.v[crow..crow + cw].fill(128);
                if full_y >= box_y && full_y < box_y + box_size {
                    let c_start = box_x / 2;
                    let c_end = ((box_x + box_size) / 2).min(cw);
                    self.u[crow + c_start..crow + c_end].fill(60);
                    self.v[crow + c_start..crow + c_end].fill(210);
                }
            }
        }
    }
}

impl YUVSource for PlanarI420Frame {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

pub fn apply_openh264_bitrate(encoder: &mut Encoder, bitrate_kbps: u32) {
    let bps = bitrate_kbps.saturating_mul(1000).min(i32::MAX as u32) as c_int;
    let mut max_info = openh264_sys2::SBitrateInfo {
        iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
        iBitrate: bps,
    };
    let mut target_info = openh264_sys2::SBitrateInfo {
        iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
        iBitrate: bps,
    };
    unsafe {
        let raw = encoder.raw_api();
        let _ = raw.set_option(
            openh264_sys2::ENCODER_OPTION_MAX_BITRATE,
            (&raw mut max_info).cast(),
        );
        let _ = raw.set_option(
            openh264_sys2::ENCODER_OPTION_BITRATE,
            (&raw mut target_info).cast(),
        );
    }
}

pub async fn run_software_testsrc(
    cfg: VideoPipelineConfig,
    live_bitrate_kbps: Arc<AtomicU32>,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        run_blocking(cfg, live_bitrate_kbps, tx, keyframe_req, start, cancel)
    })
    .await
    .map_err(|e| anyhow!("software video worker panicked: {e}"))?
}

fn run_blocking(
    cfg: VideoPipelineConfig,
    live_bitrate_kbps: Arc<AtomicU32>,
    tx: mpsc::Sender<EncodedFrame>,
    keyframe_req: Arc<AtomicBool>,
    start: Instant,
    cancel: CancellationToken,
) -> Result<()> {
    let initial_kbps = live_bitrate_kbps.load(Ordering::Acquire).max(500);
    let complexity = match cfg.prefer {
        QualityPreference::Latency => Complexity::Low,
        QualityPreference::Quality => Complexity::Medium,
    };
    let enc_cfg = EncoderConfig::new()
        .bitrate(BitRate::from_bps(initial_kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(cfg.fps as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::ScreenContentRealTime)
        .complexity(complexity)
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        .intra_frame_period(IntraFramePeriod::from_num_frames(cfg.fps as u32 * 2))
        .skip_frames(false)
        .vui(VuiConfig::bt709());

    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), enc_cfg)
        .map_err(|e| anyhow!("failed to create OpenH264 encoder: {e}"))?;

    let mut applied_bitrate = initial_kbps;
    let mut have_encoded_first = false;
    let mut frame = PlanarI420Frame::new(cfg.width as usize, cfg.height as usize);
    let frame_period = Duration::from_micros(1_000_000 / cfg.fps.max(1) as u64);
    let mut next_tick = Instant::now();
    let mut frame_idx = 0u64;

    while !cancel.is_cancelled() {
        frame.render_test_pattern(frame_idx);
        frame_idx += 1;

        if have_encoded_first {
            let desired = live_bitrate_kbps.load(Ordering::Acquire).max(500);
            if desired != applied_bitrate {
                apply_openh264_bitrate(&mut encoder, desired);
                applied_bitrate = desired;
            }
        }

        if keyframe_req.swap(false, Ordering::AcqRel) {
            encoder.force_intra_frame();
        }

        let bs = encoder
            .encode(&frame)
            .map_err(|e| anyhow!("OpenH264 encode failed: {e}"))?;
        have_encoded_first = true;
        let data = bs.to_vec();
        if !data.is_empty() {
            let keyframe = matches!(bs.frame_type(), FrameType::IDR | FrameType::I)
                || is_keyframe_annexb(VideoCodec::H264, &data);
            let ts_us = start.elapsed().as_micros() as u32;
            let _ = tx.try_send(EncodedFrame {
                data,
                keyframe,
                timestamp_us: ts_us,
            });
        }

        next_tick += frame_period;
        let now = Instant::now();
        if next_tick > now {
            let remaining = next_tick - now;
            if remaining > Duration::from_millis(2) {
                std::thread::sleep(remaining - Duration::from_millis(1));
            }
            while Instant::now() < next_tick {
                if cancel.is_cancelled() {
                    break;
                }
                std::hint::spin_loop();
            }
        } else {
            next_tick = now;
        }
    }

    Ok(())
}
