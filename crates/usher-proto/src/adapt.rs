use crate::handshake::QualityPreference;
use crate::packet::ControlMessage;
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_BITRATE_KBPS: u32 = 60_000;
pub const MIN_BITRATE_KBPS: u32 = 1_500;
pub const STEP_DOWN_FLOOR_INTERVALS: u32 = 20;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdaptiveAction {
    pub new_bitrate_kbps: Option<u32>,
    pub new_fec_percent: Option<u8>,
    pub step_down_resolution: Option<(u16, u16)>,
    pub status_changed: bool,
}

#[derive(Debug, Clone)]
pub struct AdaptiveController {
    pub enabled: bool,
    pub max_bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub target_bitrate_kbps: u32,
    pub applied_bitrate_kbps: u32,
    pub fec_percent: u8,
    pub prefer: QualityPreference,
    pub baseline_rtt_us: u32,
    pub high_rtt_streak: u32,
    pub clean_streak: u32,
    pub fec_clean_streak: u32,
    pub floor_congestion_streak: u32,
    pub resolution_stepped_down: bool,
    pub last_restart_at: Option<Instant>,
}

impl AdaptiveController {
    pub fn new(
        max_bitrate_kbps: u32,
        initial_bitrate_kbps: Option<u32>,
        initial_fec: u8,
        prefer: QualityPreference,
        enabled: bool,
    ) -> Self {
        let max_br = max_bitrate_kbps.max(MIN_BITRATE_KBPS);
        let min_br = MIN_BITRATE_KBPS.min(max_br);
        let target = initial_bitrate_kbps
            .unwrap_or_else(|| (max_br / 2).clamp(min_br, 25_000))
            .clamp(min_br, max_br);
        let (fec_min, fec_max) = fec_bounds(prefer);
        let fec = if enabled {
            initial_fec.clamp(fec_min, fec_max)
        } else {
            initial_fec.min(50)
        };
        Self {
            enabled,
            max_bitrate_kbps: max_br,
            min_bitrate_kbps: min_br,
            target_bitrate_kbps: target,
            applied_bitrate_kbps: target,
            fec_percent: fec,
            prefer,
            baseline_rtt_us: 0,
            high_rtt_streak: 0,
            clean_streak: 0,
            fec_clean_streak: 0,
            floor_congestion_streak: 0,
            resolution_stepped_down: false,
            last_restart_at: None,
        }
    }

    pub fn set_max_bitrate(&mut self, max_kbps: u32) -> Option<u32> {
        self.max_bitrate_kbps = max_kbps.max(self.min_bitrate_kbps);
        if self.target_bitrate_kbps > self.max_bitrate_kbps {
            self.target_bitrate_kbps = self.max_bitrate_kbps;
        }
        if !self.enabled {
            self.target_bitrate_kbps = self.max_bitrate_kbps;
        }
        if self.applied_bitrate_kbps > self.target_bitrate_kbps
            || (!self.enabled && self.applied_bitrate_kbps != self.target_bitrate_kbps)
        {
            self.applied_bitrate_kbps = self.target_bitrate_kbps;
            Some(self.target_bitrate_kbps)
        } else {
            None
        }
    }

    pub fn set_prefer(&mut self, prefer: QualityPreference) -> Option<u8> {
        self.prefer = prefer;
        if !self.enabled {
            return None;
        }
        let (fec_min, fec_max) = fec_bounds(prefer);
        let clamped = self.fec_percent.clamp(fec_min, fec_max);
        if clamped != self.fec_percent {
            self.fec_percent = clamped;
            Some(clamped)
        } else {
            None
        }
    }

    pub fn reset_resolution_step_down(&mut self) {
        self.resolution_stepped_down = false;
        self.floor_congestion_streak = 0;
    }

    pub fn on_feedback(
        &mut self,
        msg: &ControlMessage,
        now: Instant,
        in_place_encoder: bool,
        cur_w: u16,
        cur_h: u16,
    ) -> AdaptiveAction {
        let ControlMessage::NetworkFeedback {
            loss_after_fec_bps,
            fec_recovery_bps,
            rtt_us,
            late_frames,
            recv_kbps: _,
        } = *msg
        else {
            return AdaptiveAction::default();
        };

        let mut action = AdaptiveAction::default();
        if !self.enabled {
            return action;
        }

        if rtt_us > 0 {
            if self.baseline_rtt_us == 0 {
                self.baseline_rtt_us = rtt_us;
            } else if rtt_us < self.baseline_rtt_us {
                self.baseline_rtt_us = (self.baseline_rtt_us * 3 + rtt_us) / 4;
            } else {
                self.baseline_rtt_us = (self.baseline_rtt_us * 31 + rtt_us) / 32;
            }
        }

        let rtt_threshold = self
            .baseline_rtt_us
            .saturating_add((self.baseline_rtt_us / 2).max(12_000));
        let rtt_inflated = rtt_us > 0 && self.baseline_rtt_us > 0 && rtt_us > rtt_threshold;
        if rtt_inflated {
            self.high_rtt_streak += 1;
        } else {
            self.high_rtt_streak = 0;
        }

        let unrecovered_loss = loss_after_fec_bps > 0;
        let high_recovery = fec_recovery_bps >= 100;
        let late_spike = late_frames >= 2;
        let sustained_rtt = self.high_rtt_streak >= 2;
        let congested = unrecovered_loss || late_spike || sustained_rtt;

        // 1) Adaptive FEC (zero restart cost)
        let (fec_min, fec_max) = fec_bounds(self.prefer);
        let desired_fec = if unrecovered_loss {
            self.fec_clean_streak = 0;
            self.fec_percent.saturating_add(10).min(fec_max)
        } else if high_recovery {
            self.fec_clean_streak = 0;
            self.fec_percent.saturating_add(5).min(fec_max)
        } else if fec_recovery_bps == 0 {
            self.fec_clean_streak += 1;
            if self.fec_clean_streak >= 6 {
                self.fec_clean_streak = 0;
                self.fec_percent.saturating_sub(2).max(fec_min)
            } else {
                self.fec_percent
            }
        } else {
            self.fec_percent
        };
        if desired_fec != self.fec_percent {
            self.fec_percent = desired_fec;
            action.new_fec_percent = Some(desired_fec);
            action.status_changed = true;
        }

        // 2) AIMD Bitrate adaptation (cut fast, grow slow)
        if unrecovered_loss {
            self.clean_streak = 0;
            let pct = if loss_after_fec_bps >= 300 { 65 } else { 75 };
            self.target_bitrate_kbps = ((self.target_bitrate_kbps as u64 * pct) / 100) as u32;
        } else if late_spike || sustained_rtt {
            self.clean_streak = 0;
            let pct = match self.prefer {
                QualityPreference::Latency => 82,
                QualityPreference::Quality => 86,
            };
            self.target_bitrate_kbps = ((self.target_bitrate_kbps as u64 * pct) / 100) as u32;
        } else if !high_recovery {
            self.clean_streak += 1;
            if self.clean_streak >= 2 {
                self.clean_streak = 0;
                let step = match self.prefer {
                    QualityPreference::Latency => (self.target_bitrate_kbps / 20).max(500),
                    QualityPreference::Quality => (self.target_bitrate_kbps / 14).max(750),
                };
                self.target_bitrate_kbps = self.target_bitrate_kbps.saturating_add(step);
            }
        } else {
            self.clean_streak = 0;
        }

        self.target_bitrate_kbps = self
            .target_bitrate_kbps
            .clamp(self.min_bitrate_kbps, self.max_bitrate_kbps);

        // 3) Rate-limit hardware subprocess restarts; apply OpenH264 changes immediately
        let prev_applied = self.applied_bitrate_kbps;
        let should_apply = if in_place_encoder {
            let diff = self.target_bitrate_kbps.abs_diff(prev_applied);
            self.target_bitrate_kbps != prev_applied
                && (diff >= prev_applied / 20
                    || self.target_bitrate_kbps == self.min_bitrate_kbps
                    || self.target_bitrate_kbps == self.max_bitrate_kbps)
        } else {
            let big_drop = self.target_bitrate_kbps <= ((prev_applied as u64 * 80) / 100) as u32;
            let severe_drop =
                self.target_bitrate_kbps <= ((prev_applied as u64 * 65) / 100) as u32;
            let big_grow = self.target_bitrate_kbps >= ((prev_applied as u64 * 125) / 100) as u32
                || (self.target_bitrate_kbps == self.max_bitrate_kbps
                    && prev_applied < self.max_bitrate_kbps
                    && self.target_bitrate_kbps.saturating_sub(prev_applied) >= prev_applied / 10);
            let cooldown = if severe_drop {
                Duration::from_secs(2)
            } else {
                Duration::from_secs(4)
            };
            let cooldown_ok = self
                .last_restart_at
                .is_none_or(|t| now.saturating_duration_since(t) >= cooldown);
            (big_drop || big_grow) && cooldown_ok
        };

        if should_apply && self.target_bitrate_kbps != self.applied_bitrate_kbps {
            self.applied_bitrate_kbps = self.target_bitrate_kbps;
            self.last_restart_at = Some(now);
            action.new_bitrate_kbps = Some(self.target_bitrate_kbps);
            action.status_changed = true;
        }

        // 4) Optional single-notch resolution step-down only after prolonged congestion at floor
        if congested && self.target_bitrate_kbps <= self.min_bitrate_kbps + 200 {
            self.floor_congestion_streak += 1;
        } else if !congested && !high_recovery {
            self.floor_congestion_streak = 0;
        }

        if !self.resolution_stepped_down
            && self.floor_congestion_streak >= STEP_DOWN_FLOOR_INTERVALS
            && cur_h > 720
        {
            let next_h = if cur_h > 1080 { 1080u16 } else { 720u16 };
            let next_w =
                ((((cur_w as u32) * (next_h as u32)) / cur_h as u32).max(64) as u16) & !1;
            self.resolution_stepped_down = true;
            self.floor_congestion_streak = 0;
            action.step_down_resolution = Some((next_w, next_h));
            action.status_changed = true;
        }

        action
    }
}

fn fec_bounds(prefer: QualityPreference) -> (u8, u8) {
    match prefer {
        QualityPreference::Latency => (5, 40),
        QualityPreference::Quality => (0, 30),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb(loss_bps: u16, rec_bps: u16, rtt_us: u32, late: u16) -> ControlMessage {
        ControlMessage::NetworkFeedback {
            loss_after_fec_bps: loss_bps,
            fec_recovery_bps: rec_bps,
            rtt_us,
            late_frames: late,
            recv_kbps: 15_000,
        }
    }

    #[test]
    fn aimd_cuts_fast_on_loss_and_grows_slow_on_clean_link() {
        let t0 = Instant::now();
        let mut ctrl = AdaptiveController::new(
            60_000,
            Some(20_000),
            10,
            QualityPreference::Latency,
            true,
        );

        let a1 = ctrl.on_feedback(&fb(150, 50, 8_000, 0), t0, true, 1920, 1080);
        assert_eq!(a1.new_bitrate_kbps, Some(15_000));
        assert_eq!(a1.new_fec_percent, Some(20));

        // Clean intervals grow slowly, capped at ceiling
        let a2 = ctrl.on_feedback(
            &fb(0, 0, 8_000, 0),
            t0 + Duration::from_millis(500),
            true,
            1920,
            1080,
        );
        assert_eq!(a2.new_bitrate_kbps, None);
        let a3 = ctrl.on_feedback(
            &fb(0, 0, 8_000, 0),
            t0 + Duration::from_secs(1),
            true,
            1920,
            1080,
        );
        assert_eq!(a3.new_bitrate_kbps, Some(15_750));
    }

    #[test]
    fn rtt_inflation_cuts_bitrate_without_packet_loss() {
        let t0 = Instant::now();
        let mut ctrl = AdaptiveController::new(
            60_000,
            Some(30_000),
            10,
            QualityPreference::Latency,
            true,
        );
        ctrl.on_feedback(&fb(0, 0, 6_000, 0), t0, true, 1920, 1080);
        // Sustained RTT jump > baseline + 12ms
        ctrl.on_feedback(
            &fb(0, 0, 28_000, 0),
            t0 + Duration::from_millis(500),
            true,
            1920,
            1080,
        );
        let act = ctrl.on_feedback(
            &fb(0, 0, 30_000, 0),
            t0 + Duration::from_secs(1),
            true,
            1920,
            1080,
        );
        assert!(act.new_bitrate_kbps.is_some());
        assert!(ctrl.applied_bitrate_kbps < 30_000);
    }

    #[test]
    fn subprocess_rate_limits_restarts_and_fec_decays_when_clean() {
        let t0 = Instant::now();
        let mut ctrl = AdaptiveController::new(
            60_000,
            Some(20_000),
            20,
            QualityPreference::Quality,
            true,
        );
        // 25% cut triggers first subprocess update immediately
        let first = ctrl.on_feedback(&fb(100, 0, 5_000, 0), t0, false, 1920, 1080);
        assert!(first.new_bitrate_kbps.is_some());

        // Another cut 1s later is suppressed during cooldown
        let second = ctrl.on_feedback(
            &fb(500, 0, 5_000, 0),
            t0 + Duration::from_secs(1),
            false,
            1920,
            1080,
        );
        assert_eq!(second.new_bitrate_kbps, None);

        // After cooldown (>= 4s), restart is allowed
        let third = ctrl.on_feedback(
            &fb(100, 0, 5_000, 0),
            t0 + Duration::from_secs(5),
            false,
            1920,
            1080,
        );
        assert!(third.new_bitrate_kbps.is_some());

        // FEC steps down after 6 clean intervals
        let mut decayed = false;
        for i in 1..=6 {
            let a = ctrl.on_feedback(
                &fb(0, 0, 5_000, 0),
                t0 + Duration::from_secs(5) + Duration::from_millis(500 * i),
                false,
                1920,
                1080,
            );
            if a.new_fec_percent.is_some() {
                decayed = true;
            }
        }
        assert!(decayed);
    }

    #[test]
    fn resolution_steps_down_only_after_prolonged_floor_congestion() {
        let t0 = Instant::now();
        let mut ctrl = AdaptiveController::new(
            20_000,
            Some(MIN_BITRATE_KBPS),
            10,
            QualityPreference::Latency,
            true,
        );
        for i in 0..(STEP_DOWN_FLOOR_INTERVALS - 1) {
            let a = ctrl.on_feedback(
                &fb(200, 0, 10_000, 0),
                t0 + Duration::from_millis(500 * i as u64),
                true,
                2560,
                1440,
            );
            assert!(a.step_down_resolution.is_none());
        }
        let last = ctrl.on_feedback(
            &fb(200, 0, 10_000, 0),
            t0 + Duration::from_millis(500 * STEP_DOWN_FLOOR_INTERVALS as u64),
            true,
            2560,
            1440,
        );
        assert_eq!(last.step_down_resolution, Some((1920, 1080)));
    }
}
