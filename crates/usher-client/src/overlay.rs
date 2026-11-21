use crate::config::{
    DisplaySelector, FpsSetting, HostSettings, PointerMode, ResolutionMode, save_host_settings,
};
use crate::transport::Telemetry;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use usher_proto::handshake::{CodecPreference, DisplayInfo, QualityPreference, VideoCodec};
use usher_proto::packet::ControlMessage;

const MAX_BITRATE_PRESETS_KBPS: &[u32] = &[
    5_000, 10_000, 15_000, 20_000, 30_000, 45_000, 60_000, 80_000, 100_000,
];
const NUM_ROWS: usize = 10;

pub const MENU_W: u32 = 512;
pub const MENU_H: u32 = 432;
pub const HUD_W: u32 = 760;
pub const HUD_H: u32 = 34;
pub const TOAST_H: u32 = 30;

#[derive(Debug, Clone, Default)]
pub struct OverlayAction {
    pub reconfigure: Option<ControlMessage>,
    pub pointer_mode: Option<PointerMode>,
    pub audio_enabled: Option<bool>,
    pub resolution_mode: Option<ResolutionMode>,
    pub stats_hud: Option<bool>,
    pub close_overlay: bool,
}

#[derive(Debug, Clone, Copy)]
struct RowHitRect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    left_arrow_max_x: u32,
}

pub struct OverlayRenderOutput {
    pub width: u32,
    pub height: u32,
    pub panel_x: u32,
    pub panel_y: u32,
    pub panel_w: u32,
    pub panel_h: u32,
    pub rgba: Vec<u8>,
}

pub struct OverlayState {
    pub open: bool,
    pub stats_hud: bool,
    pub selected_row: usize,
    pub host_alias: String,
    pub config_path: PathBuf,
    pub displays: Vec<DisplayInfo>,
    pub client_native_res: (u16, u16),
    pub client_refresh_fps: u16,
    pub window_phys_res: (u16, u16),

    // Editable settings
    pub max_bitrate_kbps: u32,
    pub resolution_mode: ResolutionMode,
    pub fps_setting: FpsSetting,
    pub codec_pref: CodecPreference,
    pub prefer: QualityPreference,
    pub audio_enabled: bool,
    pub display_index: u8,
    pub pointer_mode: PointerMode,

    dirty: bool,
    status_flash: Option<(String, Instant)>,
    toast: Option<(String, Instant)>,
    last_hud_refresh: Instant,
    row_hits: Vec<RowHitRect>,
    last_panel_origin: (u32, u32),
    last_ui_scale: f32,
}

impl OverlayState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host_alias: String,
        config_path: PathBuf,
        displays: Vec<DisplayInfo>,
        client_native_res: (u16, u16),
        client_refresh_fps: u16,
        window_phys_res: (u16, u16),
        max_bitrate_kbps: u32,
        resolution_mode: ResolutionMode,
        fps_setting: FpsSetting,
        codec_pref: CodecPreference,
        prefer: QualityPreference,
        audio_enabled: bool,
        display_index: u8,
        pointer_mode: PointerMode,
        stats_hud: bool,
    ) -> Self {
        Self {
            open: false,
            stats_hud,
            selected_row: 0,
            host_alias,
            config_path,
            displays,
            client_native_res,
            client_refresh_fps: client_refresh_fps.clamp(30, 240),
            window_phys_res,
            max_bitrate_kbps: max_bitrate_kbps.max(1_500),
            resolution_mode,
            fps_setting,
            codec_pref,
            prefer,
            audio_enabled,
            display_index,
            pointer_mode,
            dirty: stats_hud,
            status_flash: None,
            toast: None,
            last_hud_refresh: Instant::now() - Duration::from_secs(1),
            row_hits: Vec::with_capacity(NUM_ROWS),
            last_panel_origin: (0, 0),
            last_ui_scale: 1.0,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.open || self.stats_hud || self.toast.is_some()
    }

    pub fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
        self.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn toggle_open(&mut self) {
        self.open = !self.open;
        self.dirty = true;
    }

    pub fn sync_from_telemetry(&mut self, telem: &Telemetry) {
        let max_br = telem.max_bitrate_kbps.load(Ordering::Acquire);
        if max_br > 0 {
            self.max_bitrate_kbps = max_br;
        }
        self.audio_enabled = telem.audio_enabled.load(Ordering::Acquire);
        self.display_index = telem.active_display.load(Ordering::Acquire);
        if let Ok(p) = QualityPreference::from_u8(telem.active_prefer.load(Ordering::Acquire)) {
            self.prefer = p;
        }
        self.dirty = true;
    }

    pub fn tick_dirty(&mut self) {
        if let Some((_, at)) = &self.status_flash
            && at.elapsed() >= Duration::from_secs(3)
        {
            self.status_flash = None;
            self.dirty = true;
        }
        if let Some((_, at)) = &self.toast
            && at.elapsed() >= Duration::from_secs(4)
        {
            self.toast = None;
            self.dirty = true;
        }
        if self.is_visible() && self.last_hud_refresh.elapsed() >= Duration::from_millis(450) {
            self.last_hud_refresh = Instant::now();
            self.dirty = true;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let cur = self.selected_row as isize;
        self.selected_row = (cur + delta).rem_euclid(NUM_ROWS as isize) as usize;
        self.dirty = true;
    }

    pub fn activate_or_step(&mut self, delta: isize) -> OverlayAction {
        self.adjust_row(self.selected_row, delta)
    }

    pub fn handle_click(&mut self, phys_x: f64, phys_y: f64) -> OverlayAction {
        if !self.open {
            return OverlayAction::default();
        }
        let scale = self.last_ui_scale.max(1.0) as f64;
        let lx = (phys_x - self.last_panel_origin.0 as f64) / scale;
        let ly = (phys_y - self.last_panel_origin.1 as f64) / scale;
        if lx < 0.0 || ly < 0.0 || lx >= MENU_W as f64 || ly >= MENU_H as f64 {
            return OverlayAction {
                close_overlay: true,
                ..Default::default()
            };
        }
        let px = lx as u32;
        let py = ly as u32;

        for (idx, hit) in self.row_hits.iter().enumerate() {
            if px >= hit.x && px < hit.x + hit.w && py >= hit.y && py < hit.y + hit.h {
                self.selected_row = idx;
                let step = if idx != 9 && px <= hit.left_arrow_max_x {
                    -1
                } else {
                    1
                };
                return self.adjust_row(idx, step);
            }
        }
        OverlayAction::default()
    }

    fn adjust_row(&mut self, row: usize, delta: isize) -> OverlayAction {
        let mut act = OverlayAction::default();
        let mut msg = ControlMessage::reconfigure_unchanged();
        let mut send_reconf = false;

        match row {
            0 => {
                let next = cycle_u32(MAX_BITRATE_PRESETS_KBPS, self.max_bitrate_kbps, delta);
                self.max_bitrate_kbps = next;
                if let ControlMessage::Reconfigure {
                    ref mut max_bitrate_kbps,
                    ..
                } = msg
                {
                    *max_bitrate_kbps = next;
                    send_reconf = true;
                }
            }
            1 => {
                let modes = [
                    ResolutionMode::Native,
                    ResolutionMode::MatchWindow,
                    ResolutionMode::Fixed(1280, 720),
                    ResolutionMode::Fixed(1600, 900),
                    ResolutionMode::Fixed(1920, 1080),
                    ResolutionMode::Fixed(2560, 1440),
                    ResolutionMode::Fixed(3840, 2160),
                ];
                let norm_cur = match self.resolution_mode {
                    ResolutionMode::Auto => ResolutionMode::Native,
                    other => other,
                };
                let idx = modes
                    .iter()
                    .position(|m| *m == norm_cur)
                    .unwrap_or(0);
                let next = modes[(idx as isize + delta).rem_euclid(modes.len() as isize) as usize];
                self.resolution_mode = next;
                let (rw, rh) = match next {
                    ResolutionMode::Auto | ResolutionMode::Native => self
                        .displays
                        .iter()
                        .find(|d| d.index == self.display_index)
                        .or_else(|| self.displays.first())
                        .map_or(self.client_native_res, |d| (d.width, d.height)),
                    ResolutionMode::MatchWindow => self.window_phys_res,
                    ResolutionMode::Fixed(w, h) => (w, h),
                };
                if let ControlMessage::Reconfigure {
                    ref mut width,
                    ref mut height,
                    ..
                } = msg
                {
                    *width = (rw.clamp(64, 3840)) & !1;
                    *height = (rh.clamp(64, 2160)) & !1;
                    send_reconf = true;
                }
                act.resolution_mode = Some(next);
            }
            2 => {
                let choices = [
                    FpsSetting::Auto,
                    FpsSetting::Fixed(30),
                    FpsSetting::Fixed(60),
                    FpsSetting::Fixed(90),
                    FpsSetting::Fixed(120),
                ];
                let idx = choices
                    .iter()
                    .position(|c| *c == self.fps_setting)
                    .unwrap_or(0);
                let next = choices[(idx as isize + delta).rem_euclid(choices.len() as isize) as usize];
                self.fps_setting = next;
                let actual_fps = match next {
                    FpsSetting::Auto => self.client_refresh_fps,
                    FpsSetting::Fixed(f) => f,
                };
                if let ControlMessage::Reconfigure { ref mut fps, .. } = msg {
                    *fps = actual_fps;
                    send_reconf = true;
                }
            }
            3 => {
                let choices = [
                    CodecPreference::Auto,
                    CodecPreference::H264,
                    CodecPreference::Hevc,
                    CodecPreference::Av1,
                ];
                let idx = choices
                    .iter()
                    .position(|c| *c == self.codec_pref)
                    .unwrap_or(0);
                let next = choices[(idx as isize + delta).rem_euclid(choices.len() as isize) as usize];
                self.codec_pref = next;
                if let ControlMessage::Reconfigure { ref mut codec, .. } = msg {
                    *codec = next.to_reconfigure_u8();
                    send_reconf = true;
                }
            }
            4 => {
                let next = match self.prefer {
                    QualityPreference::Latency => QualityPreference::Quality,
                    QualityPreference::Quality => QualityPreference::Latency,
                };
                self.prefer = next;
                if let ControlMessage::Reconfigure { ref mut prefer, .. } = msg {
                    *prefer = next.to_reconfigure_u8();
                    send_reconf = true;
                }
            }
            5 => {
                self.audio_enabled = !self.audio_enabled;
                if let ControlMessage::Reconfigure { ref mut audio, .. } = msg {
                    *audio = if self.audio_enabled { 2 } else { 1 };
                    send_reconf = true;
                }
                act.audio_enabled = Some(self.audio_enabled);
            }
            6 => {
                let count = self.displays.len().max(1);
                let next_idx = ((self.display_index as usize as isize + delta)
                    .rem_euclid(count as isize)) as u8;
                self.display_index = next_idx;
                if let ControlMessage::Reconfigure { ref mut display, .. } = msg {
                    *display = next_idx;
                    send_reconf = true;
                }
            }
            7 => {
                self.pointer_mode = match self.pointer_mode {
                    PointerMode::Absolute => PointerMode::Relative,
                    PointerMode::Relative => PointerMode::Absolute,
                };
                act.pointer_mode = Some(self.pointer_mode);
            }
            8 => {
                self.stats_hud = !self.stats_hud;
                act.stats_hud = Some(self.stats_hud);
            }
            9 => {
                let entry = HostSettings {
                    max_bitrate_kbps: Some(self.max_bitrate_kbps),
                    resolution: Some(self.resolution_mode),
                    fps: Some(self.fps_setting),
                    codec: Some(self.codec_pref),
                    prefer: Some(self.prefer),
                    audio: Some(self.audio_enabled),
                    display: Some(DisplaySelector::Index(self.display_index)),
                    pointer_mode: Some(self.pointer_mode),
                    stats_overlay: Some(self.stats_hud),
                    ..Default::default()
                };
                match save_host_settings(&self.config_path, &self.host_alias, &entry) {
                    Ok(()) => {
                        self.status_flash =
                            Some((format!("Saved [host.{}]", self.host_alias), Instant::now()));
                    }
                    Err(err) => {
                        self.status_flash = Some((format!("Save failed: {err}"), Instant::now()));
                    }
                }
            }
            _ => {}
        }

        if send_reconf {
            act.reconfigure = Some(msg);
        }
        self.dirty = true;
        act
    }

    pub fn render_if_dirty(
        &mut self,
        win_w: u32,
        win_h: u32,
        telem: &Telemetry,
    ) -> Option<OverlayRenderOutput> {
        if !self.is_visible() {
            self.dirty = false;
            return None;
        }
        if !self.dirty {
            return None;
        }
        self.dirty = false;

        let ww = win_w.max(320);
        let wh = win_h.max(200);
        let fps = telem.fps_x10.load(Ordering::Acquire) as f32 / 10.0;
        let rtt_ms = telem.rtt_us.load(Ordering::Acquire) as f32 / 1000.0;
        let rx_mbps = telem.kbps.load(Ordering::Acquire) as f32 / 1000.0;
        let tgt_mbps = telem.target_bitrate_kbps.load(Ordering::Acquire) as f32 / 1000.0;
        let fec = telem.fec_percent.load(Ordering::Acquire);
        let loss_pct = telem.loss_after_fec_bps.load(Ordering::Acquire) as f32 / 100.0;
        let active_w = telem.active_width.load(Ordering::Acquire);
        let active_h = telem.active_height.load(Ordering::Acquire);
        let active_fps = telem.active_fps.load(Ordering::Acquire);
        let active_codec = VideoCodec::from_u8(telem.active_codec.load(Ordering::Acquire))
            .unwrap_or(VideoCodec::H264);
        let stepped_down = telem.resolution_stepped_down.load(Ordering::Acquire);
        let chroma_444 = telem.chroma_444.load(Ordering::Acquire);

        if self.open {
            let scale = ((ww as f32 / (MENU_W as f32 + 40.0))
                .min(wh as f32 / (MENU_H as f32 + 40.0)))
            .clamp(0.75, 1.5);
            self.last_ui_scale = scale;
            let panel_w = ((MENU_W as f32 * scale).round() as u32).min(ww);
            let panel_h = ((MENU_H as f32 * scale).round() as u32).min(wh);
            let panel_x = ww.saturating_sub(panel_w) / 2;
            let panel_y = wh.saturating_sub(panel_h) / 2;
            self.last_panel_origin = (panel_x, panel_y);

            let mut canvas = Canvas::new(MENU_W, MENU_H);
            canvas.fill_rounded_rect(0, 0, MENU_W, MENU_H, 12, [15, 23, 42, 232]);
            canvas.stroke_rect(0, 0, MENU_W, MENU_H, [56, 189, 248, 170]);

            // Header
            canvas.draw_text(
                18,
                14,
                2,
                &format!("USHER  //  {}", self.host_alias),
                [248, 250, 252, 255],
            );
            canvas.draw_text(
                18,
                38,
                1,
                &format!(
                    "Stream: {}x{}@{} {}  |  RX {:.1} Mbps (target {:.1} / cap {} Mbps)",
                    active_w,
                    active_h,
                    active_fps,
                    active_codec,
                    rx_mbps,
                    tgt_mbps,
                    self.max_bitrate_kbps / 1000
                ),
                [148, 163, 184, 255],
            );
            canvas.draw_text(
                18,
                52,
                1,
                &format!(
                    "RTT: {:.1} ms  |  FPS: {:.0}  |  Loss: {:.2}%  |  FEC: {}%",
                    rtt_ms, fps, loss_pct, fec
                ),
                [56, 189, 248, 255],
            );
            canvas.fill_rect(18, 68, MENU_W - 36, 1, [51, 65, 85, 255]);

            let mut y = 76u32;
            if stepped_down {
                canvas.fill_rect(18, y, MENU_W - 36, 18, [120, 53, 15, 230]);
                canvas.draw_text(
                    24,
                    y + 5,
                    1,
                    "NOTICE: Resolution stepped down due to low bandwidth",
                    [253, 224, 71, 255],
                );
                y += 22;
            }

            self.row_hits.clear();
            let labels_vals = self.build_row_strings(active_codec);
            let row_h = 28u32;
            for (idx, (label, val)) in labels_vals.iter().enumerate() {
                let is_sel = idx == self.selected_row;
                let bg = if is_sel {
                    [30, 58, 95, 235]
                } else {
                    [22, 33, 56, 175]
                };
                canvas.fill_rect(16, y, MENU_W - 32, row_h - 3, bg);
                if is_sel {
                    canvas.fill_rect(16, y, 4, row_h - 3, [56, 189, 248, 255]);
                }
                let text_col = if is_sel {
                    [248, 250, 252, 255]
                } else {
                    [203, 213, 225, 255]
                };
                canvas.draw_text(28, y + 8, 1, label, text_col);

                let display_val = if idx == 9 {
                    format!("[ {val} ]")
                } else {
                    format!("<  {val}  >")
                };
                let vx = MENU_W.saturating_sub(24 + (display_val.len() as u32 * 8));
                let val_col = if idx == 9 {
                    [52, 211, 153, 255]
                } else if is_sel {
                    [56, 189, 248, 255]
                } else {
                    [226, 232, 240, 255]
                };
                canvas.draw_text(vx, y + 8, 1, &display_val, val_col);

                self.row_hits.push(RowHitRect {
                    x: 16,
                    y,
                    w: MENU_W - 32,
                    h: row_h - 3,
                    left_arrow_max_x: vx + (display_val.len() as u32 * 4),
                });
                y += row_h;
            }

            if let Some((flash, _)) = self.status_flash.as_ref().or(self.toast.as_ref()) {
                canvas.draw_text(18, MENU_H - 38, 1, flash, [52, 211, 153, 255]);
            }
            canvas.fill_rect(18, MENU_H - 24, MENU_W - 36, 1, [51, 65, 85, 255]);
            canvas.draw_text(
                18,
                MENU_H - 16,
                1,
                "Arrows/Click: Adjust   Enter: Save   Ctrl+Alt+S / Esc: Close",
                [148, 163, 184, 255],
            );

            Some(OverlayRenderOutput {
                width: MENU_W,
                height: MENU_H,
                panel_x,
                panel_y,
                panel_w,
                panel_h,
                rgba: canvas.rgba,
            })
        } else if !self.stats_hud {
            self.last_ui_scale = 1.0;
            self.row_hits.clear();
            let (msg, _) = self.toast.as_ref()?;
            let max_w = ww.saturating_sub(24).max(180);
            let canvas_w = ((msg.len() as u32) * 8 + 24).clamp(180, max_w.min(680));
            let canvas_h = TOAST_H;
            let panel_x = 12;
            let panel_y = wh.saturating_sub(canvas_h + 12);
            let mut canvas = Canvas::new(canvas_w, canvas_h);
            canvas.fill_rounded_rect(0, 0, canvas_w, canvas_h, 6, [15, 23, 42, 225]);
            canvas.stroke_rect(0, 0, canvas_w, canvas_h, [52, 211, 153, 200]);
            canvas.draw_text(12, 11, 1, msg, [241, 245, 249, 255]);
            Some(OverlayRenderOutput {
                width: canvas_w,
                height: canvas_h,
                panel_x,
                panel_y,
                panel_w: canvas_w,
                panel_h: canvas_h,
                rgba: canvas.rgba,
            })
        } else {
            self.last_ui_scale = 1.0;
            self.row_hits.clear();
            let extra_h = if self.toast.is_some() { 6 + TOAST_H } else { 0 };
            let canvas_h = HUD_H + extra_h;
            let panel_w = HUD_W.min(ww.saturating_sub(24));
            let panel_h = canvas_h.min(wh.saturating_sub(24));
            let panel_x = 12;
            let panel_y = 12;

            let mut canvas = Canvas::new(HUD_W, canvas_h);
            let is_tcp =
                telem.transport_mode.load(Ordering::Acquire) == crate::transport::TRANSPORT_SSH_TCP;
            let border_col = if is_tcp {
                [245, 158, 11, 210]
            } else {
                [56, 189, 248, 150]
            };
            let transport_label = if is_tcp { "SSH-TCP" } else { "UDP" };
            canvas.fill_rounded_rect(0, 0, HUD_W, HUD_H, 8, [15, 23, 42, 210]);
            canvas.stroke_rect(0, 0, HUD_W, HUD_H, border_col);
            let mode = if self.pointer_mode == PointerMode::Relative {
                "REL"
            } else {
                "ABS"
            };
            let chroma_tag = if chroma_444 { " | 4:4:4" } else { "" };
            let notice = if stepped_down { " | RES-STEPDOWN" } else { "" };
            let text = format!(
                "{}x{}@{} {} | {:.0}fps | {:.1}ms | {:.1}/{}Mbps | FEC {}% | Loss {:.1}%{} | [{}] [{}]{}",
                active_w,
                active_h,
                active_fps,
                active_codec,
                fps,
                rtt_ms,
                rx_mbps,
                self.max_bitrate_kbps / 1000,
                fec,
                loss_pct,
                chroma_tag,
                mode,
                transport_label,
                notice
            );
            canvas.draw_text(12, 13, 1, &text, [241, 245, 249, 255]);

            if let Some((ref toast_msg, _)) = self.toast {
                let ty = HUD_H + 6;
                let tw = ((toast_msg.len() as u32) * 8 + 24).clamp(180, HUD_W);
                canvas.fill_rounded_rect(0, ty, tw, TOAST_H, 6, [15, 23, 42, 225]);
                canvas.stroke_rect(0, ty, tw, TOAST_H, [52, 211, 153, 200]);
                canvas.draw_text(12, ty + 11, 1, toast_msg, [241, 245, 249, 255]);
            }

            Some(OverlayRenderOutput {
                width: HUD_W,
                height: canvas_h,
                panel_x,
                panel_y,
                panel_w,
                panel_h,
                rgba: canvas.rgba,
            })
        }
    }

    fn build_row_strings(&self, active_codec: VideoCodec) -> Vec<(&'static str, String)> {
        let res_str = match self.resolution_mode {
            ResolutionMode::Auto | ResolutionMode::Native => format!(
                "Native ({}x{})",
                self.client_native_res.0, self.client_native_res.1
            ),
            ResolutionMode::MatchWindow => format!(
                "Match Window ({}x{})",
                self.window_phys_res.0, self.window_phys_res.1
            ),
            ResolutionMode::Fixed(w, h) => format!("{w}x{h}"),
        };
        let fps_str = match self.fps_setting {
            FpsSetting::Auto => format!("Auto ({})", self.client_refresh_fps),
            FpsSetting::Fixed(f) => format!("{f} fps"),
        };
        let codec_str = match self.codec_pref {
            CodecPreference::Auto => format!("Auto ({active_codec})"),
            CodecPreference::H264 => "H.264".into(),
            CodecPreference::Hevc => "HEVC".into(),
            CodecPreference::Av1 => "AV1".into(),
        };
        let prefer_str = match self.prefer {
            QualityPreference::Latency => "Latency".into(),
            QualityPreference::Quality => "Quality".into(),
        };
        let disp_str = if let Some(d) = self.displays.iter().find(|d| d.index == self.display_index)
        {
            format!("{}: {} ({}x{})", d.index + 1, d.name, d.width, d.height)
        } else {
            format!("Display {}", self.display_index + 1)
        };
        vec![
            ("Max Bitrate", format!("{} Mbps", self.max_bitrate_kbps / 1000)),
            ("Resolution", res_str),
            ("Frame Rate", fps_str),
            ("Codec", codec_str),
            ("Prefer", prefer_str),
            ("Audio", if self.audio_enabled { "On" } else { "Off" }.into()),
            ("Display", disp_str),
            (
                "Pointer Mode",
                match self.pointer_mode {
                    PointerMode::Absolute => "Absolute".into(),
                    PointerMode::Relative => "Relative".into(),
                },
            ),
            ("Stats HUD", if self.stats_hud { "On" } else { "Off" }.into()),
            ("Config File", format!("Save for {}", self.host_alias)),
        ]
    }
}

fn cycle_u32(options: &[u32], cur: u32, delta: isize) -> u32 {
    let idx = options
        .iter()
        .enumerate()
        .min_by_key(|&(_, &v)| v.abs_diff(cur))
        .map_or(0, |(i, _)| i);
    options[(idx as isize + delta).rem_euclid(options.len() as isize) as usize]
}

struct Canvas {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            rgba: vec![0u8; (w * h * 4) as usize],
        }
    }

    fn set_px(&mut self, x: u32, y: u32, col: [u8; 4]) {
        if x < self.w && y < self.h {
            let i = ((y * self.w + x) * 4) as usize;
            self.rgba[i..i + 4].copy_from_slice(&col);
        }
    }

    fn fill_rect(&mut self, x: u32, y: u32, rw: u32, rh: u32, col: [u8; 4]) {
        let x2 = (x + rw).min(self.w);
        let y2 = (y + rh).min(self.h);
        for py in y..y2 {
            for px in x..x2 {
                self.set_px(px, py, col);
            }
        }
    }

    fn fill_rounded_rect(&mut self, x: u32, y: u32, rw: u32, rh: u32, r: u32, col: [u8; 4]) {
        self.fill_rect(x, y, rw, rh, col);
        let r = r.min(rw / 2).min(rh / 2);
        for dy in 0..r {
            for dx in 0..r {
                let cx = r - dx;
                let cy = r - dy;
                if cx * cx + cy * cy > r * r {
                    self.set_px(x + dx, y + dy, [0, 0, 0, 0]);
                    self.set_px(x + rw - 1 - dx, y + dy, [0, 0, 0, 0]);
                    self.set_px(x + dx, y + rh - 1 - dy, [0, 0, 0, 0]);
                    self.set_px(x + rw - 1 - dx, y + rh - 1 - dy, [0, 0, 0, 0]);
                }
            }
        }
    }

    fn stroke_rect(&mut self, x: u32, y: u32, rw: u32, rh: u32, col: [u8; 4]) {
        self.fill_rect(x, y, rw, 1, col);
        self.fill_rect(x, y + rh.saturating_sub(1), rw, 1, col);
        self.fill_rect(x, y, 1, rh, col);
        self.fill_rect(x + rw.saturating_sub(1), y, 1, rh, col);
    }

    fn draw_text(&mut self, mut x: u32, y: u32, scale: u32, text: &str, col: [u8; 4]) {
        let s = scale.max(1);
        for ch in text.chars() {
            let glyph = glyph_bitmap(ch);
            for (row_idx, &bits) in glyph.iter().enumerate() {
                for col_idx in 0..8u32 {
                    if ((bits >> (7 - col_idx)) & 1) != 0 {
                        self.fill_rect(
                            x + col_idx * s,
                            y + (row_idx as u32) * s,
                            s,
                            s,
                            col,
                        );
                    }
                }
            }
            x += 8 * s;
        }
    }
}

fn glyph_bitmap(ch: char) -> [u8; 8] {
    match ch {
        ' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        '!' => [0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x18, 0x00],
        '"' => [0x66, 0x66, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00],
        '#' => [0x24, 0x7E, 0x24, 0x24, 0x7E, 0x24, 0x00, 0x00],
        '$' => [0x18, 0x3E, 0x58, 0x3C, 0x1A, 0x7C, 0x18, 0x00],
        '%' => [0x62, 0x64, 0x08, 0x10, 0x26, 0x46, 0x00, 0x00],
        '&' => [0x30, 0x48, 0x30, 0x4A, 0x44, 0x3A, 0x00, 0x00],
        '\'' => [0x18, 0x18, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00],
        '(' => [0x0C, 0x18, 0x30, 0x30, 0x30, 0x18, 0x0C, 0x00],
        ')' => [0x30, 0x18, 0x0C, 0x0C, 0x0C, 0x18, 0x30, 0x00],
        '*' => [0x00, 0x42, 0x24, 0x7E, 0x24, 0x42, 0x00, 0x00],
        '+' => [0x00, 0x18, 0x18, 0x7E, 0x18, 0x18, 0x00, 0x00],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x30, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x7E, 0x00, 0x00, 0x00, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00],
        '/' => [0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x00, 0x00],
        '0' => [0x3C, 0x66, 0x6E, 0x76, 0x66, 0x66, 0x3C, 0x00],
        '1' => [0x18, 0x38, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00],
        '2' => [0x3C, 0x66, 0x06, 0x1C, 0x30, 0x60, 0x7E, 0x00],
        '3' => [0x3C, 0x66, 0x06, 0x1C, 0x06, 0x66, 0x3C, 0x00],
        '4' => [0x0C, 0x1C, 0x3C, 0x6C, 0x7E, 0x0C, 0x0C, 0x00],
        '5' => [0x7E, 0x60, 0x7C, 0x06, 0x06, 0x66, 0x3C, 0x00],
        '6' => [0x1C, 0x30, 0x60, 0x7C, 0x66, 0x66, 0x3C, 0x00],
        '7' => [0x7E, 0x66, 0x0C, 0x18, 0x18, 0x18, 0x18, 0x00],
        '8' => [0x3C, 0x66, 0x66, 0x3C, 0x66, 0x66, 0x3C, 0x00],
        '9' => [0x3C, 0x66, 0x66, 0x3E, 0x06, 0x0C, 0x38, 0x00],
        ':' => [0x00, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00, 0x00],
        ';' => [0x00, 0x18, 0x18, 0x00, 0x18, 0x18, 0x30, 0x00],
        '<' => [0x06, 0x0C, 0x18, 0x30, 0x18, 0x0C, 0x06, 0x00],
        '=' => [0x00, 0x00, 0x7E, 0x00, 0x7E, 0x00, 0x00, 0x00],
        '>' => [0x60, 0x30, 0x18, 0x0C, 0x18, 0x30, 0x60, 0x00],
        '?' => [0x3C, 0x66, 0x06, 0x1C, 0x18, 0x00, 0x18, 0x00],
        '@' => [0x3C, 0x66, 0x6E, 0x6E, 0x60, 0x62, 0x3C, 0x00],
        'A' => [0x18, 0x3C, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x00],
        'B' => [0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x7C, 0x00],
        'C' => [0x3C, 0x66, 0x60, 0x60, 0x60, 0x66, 0x3C, 0x00],
        'D' => [0x78, 0x6C, 0x66, 0x66, 0x66, 0x6C, 0x78, 0x00],
        'E' => [0x7E, 0x60, 0x60, 0x78, 0x60, 0x60, 0x7E, 0x00],
        'F' => [0x7E, 0x60, 0x60, 0x78, 0x60, 0x60, 0x60, 0x00],
        'G' => [0x3C, 0x66, 0x60, 0x6E, 0x66, 0x66, 0x3E, 0x00],
        'H' => [0x66, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x00],
        'I' => [0x3C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00],
        'J' => [0x1E, 0x0C, 0x0C, 0x0C, 0x6C, 0x6C, 0x38, 0x00],
        'K' => [0x66, 0x6C, 0x78, 0x70, 0x78, 0x6C, 0x66, 0x00],
        'L' => [0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00],
        'M' => [0x63, 0x77, 0x7F, 0x6B, 0x63, 0x63, 0x63, 0x00],
        'N' => [0x66, 0x76, 0x7E, 0x7E, 0x6E, 0x66, 0x66, 0x00],
        'O' => [0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
        'P' => [0x7C, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0x00],
        'Q' => [0x3C, 0x66, 0x66, 0x66, 0x6A, 0x6C, 0x36, 0x00],
        'R' => [0x7C, 0x66, 0x66, 0x7C, 0x6C, 0x66, 0x66, 0x00],
        'S' => [0x3C, 0x66, 0x60, 0x3C, 0x06, 0x66, 0x3C, 0x00],
        'T' => [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00],
        'U' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
        'V' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x00],
        'W' => [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00],
        'X' => [0x66, 0x66, 0x3C, 0x18, 0x3C, 0x66, 0x66, 0x00],
        'Y' => [0x66, 0x66, 0x66, 0x3C, 0x18, 0x18, 0x18, 0x00],
        'Z' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x7E, 0x00],
        '[' => [0x3C, 0x30, 0x30, 0x30, 0x30, 0x30, 0x3C, 0x00],
        '\\' => [0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x00, 0x00],
        ']' => [0x3C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x3C, 0x00],
        '^' => [0x18, 0x3C, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7E, 0x00],
        '`' => [0x30, 0x18, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00],
        'a' => [0x00, 0x00, 0x3C, 0x06, 0x3E, 0x66, 0x3E, 0x00],
        'b' => [0x60, 0x60, 0x7C, 0x66, 0x66, 0x66, 0x7C, 0x00],
        'c' => [0x00, 0x00, 0x3C, 0x66, 0x60, 0x66, 0x3C, 0x00],
        'd' => [0x06, 0x06, 0x3E, 0x66, 0x66, 0x66, 0x3E, 0x00],
        'e' => [0x00, 0x00, 0x3C, 0x66, 0x7E, 0x60, 0x3C, 0x00],
        'f' => [0x1C, 0x30, 0x7C, 0x30, 0x30, 0x30, 0x30, 0x00],
        'g' => [0x00, 0x00, 0x3E, 0x66, 0x66, 0x3E, 0x06, 0x3C],
        'h' => [0x60, 0x60, 0x7C, 0x66, 0x66, 0x66, 0x66, 0x00],
        'i' => [0x18, 0x00, 0x38, 0x18, 0x18, 0x18, 0x3C, 0x00],
        'j' => [0x0C, 0x00, 0x1C, 0x0C, 0x0C, 0x6C, 0x38, 0x00],
        'k' => [0x60, 0x60, 0x66, 0x6C, 0x78, 0x6C, 0x66, 0x00],
        'l' => [0x38, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00],
        'm' => [0x00, 0x00, 0x66, 0x7F, 0x7F, 0x6B, 0x63, 0x00],
        'n' => [0x00, 0x00, 0x7C, 0x66, 0x66, 0x66, 0x66, 0x00],
        'o' => [0x00, 0x00, 0x3C, 0x66, 0x66, 0x66, 0x3C, 0x00],
        'p' => [0x00, 0x00, 0x7C, 0x66, 0x66, 0x7C, 0x60, 0x60],
        'q' => [0x00, 0x00, 0x3E, 0x66, 0x66, 0x3E, 0x06, 0x06],
        'r' => [0x00, 0x00, 0x7C, 0x66, 0x60, 0x60, 0x60, 0x00],
        's' => [0x00, 0x00, 0x3E, 0x60, 0x3C, 0x06, 0x7C, 0x00],
        't' => [0x30, 0x30, 0x7C, 0x30, 0x30, 0x30, 0x1C, 0x00],
        'u' => [0x00, 0x00, 0x66, 0x66, 0x66, 0x66, 0x3E, 0x00],
        'v' => [0x00, 0x00, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x00],
        'w' => [0x00, 0x00, 0x63, 0x63, 0x6B, 0x7F, 0x36, 0x00],
        'x' => [0x00, 0x00, 0x66, 0x3C, 0x18, 0x3C, 0x66, 0x00],
        'y' => [0x00, 0x00, 0x66, 0x66, 0x66, 0x3E, 0x06, 0x3C],
        'z' => [0x00, 0x00, 0x7E, 0x0C, 0x18, 0x30, 0x7E, 0x00],
        '{' => [0x0E, 0x18, 0x18, 0x70, 0x18, 0x18, 0x0E, 0x00],
        '|' => [0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00],
        '}' => [0x70, 0x18, 0x18, 0x0E, 0x18, 0x18, 0x70, 0x00],
        '~' => [0x32, 0x4C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        _ => [0x7E, 0x42, 0x42, 0x42, 0x42, 0x42, 0x7E, 0x00],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> OverlayState {
        OverlayState::new(
            "desktop".into(),
            std::env::temp_dir().join("usher-overlay-unit-test.toml"),
            vec![
                DisplayInfo {
                    index: 0,
                    name: "DP-1".into(),
                    width: 2560,
                    height: 1440,
                },
                DisplayInfo {
                    index: 1,
                    name: "HDMI-1".into(),
                    width: 1920,
                    height: 1080,
                },
            ],
            (1920, 1080),
            120,
            (1600, 900),
            60_000,
            ResolutionMode::Auto,
            FpsSetting::Auto,
            CodecPreference::Auto,
            QualityPreference::Latency,
            true,
            0,
            PointerMode::Absolute,
            false,
        )
    }

    #[test]
    fn cycles_settings_and_renders_overlay_and_hud() {
        let mut st = test_state();
        assert!(!st.is_visible());

        // Toggle open with Ctrl+Alt+S
        st.toggle_open();
        assert!(st.open);
        assert!(st.is_visible());

        let telem = Telemetry::default();
        telem.active_width.store(1920, Ordering::Release);
        telem.active_height.store(1080, Ordering::Release);
        telem.active_fps.store(120, Ordering::Release);
        telem.max_bitrate_kbps.store(60_000, Ordering::Release);
        telem.target_bitrate_kbps.store(45_000, Ordering::Release);

        let rendered = st.render_if_dirty(1600, 900, &telem).expect("full menu");
        assert_eq!(rendered.width, MENU_W);
        assert_eq!(rendered.height, MENU_H);
        assert!(rendered.rgba.iter().any(|&b| b != 0));

        // Row 0: Max Bitrate step right -> 80 Mbps
        let act0 = st.activate_or_step(1);
        assert_eq!(st.max_bitrate_kbps, 80_000);
        assert!(matches!(
            act0.reconfigure,
            Some(ControlMessage::Reconfigure {
                max_bitrate_kbps: 80_000,
                ..
            })
        ));

        // Row 5: Audio toggle Off
        st.selected_row = 5;
        let act5 = st.activate_or_step(1);
        assert!(!st.audio_enabled);
        assert_eq!(act5.audio_enabled, Some(false));

        // Row 8: Stats HUD toggle On, then close panel -> compact HUD renders
        st.selected_row = 8;
        let act8 = st.activate_or_step(1);
        assert_eq!(act8.stats_hud, Some(true));
        st.toggle_open();
        assert!(!st.open);
        assert!(st.is_visible());

        let hud = st.render_if_dirty(1600, 900, &telem).expect("stats hud");
        assert_eq!(hud.width, HUD_W);
        assert_eq!(hud.height, HUD_H);
        assert!(hud.rgba.iter().any(|&b| b != 0));
    }

    #[test]
    fn mouse_click_hits_row_arrows() {
        let mut st = test_state();
        st.toggle_open();
        let telem = Telemetry::default();
        let out = st.render_if_dirty(1600, 900, &telem).expect("menu");
        let scale_x = out.panel_w as f64 / MENU_W as f64;
        let scale_y = out.panel_h as f64 / MENU_H as f64;
        // Row 0 sits at logical y = 76..101; right side of "< 60 Mbps >" sits near x = MENU_W - 28.
        let row0_y = out.panel_y as f64 + 88.0 * scale_y;
        let right_arrow_x = out.panel_x as f64 + (MENU_W as f64 - 28.0) * scale_x;
        let act = st.handle_click(right_arrow_x, row0_y);
        assert!(act.reconfigure.is_some());
        assert_eq!(st.max_bitrate_kbps, 80_000);
    }
}
