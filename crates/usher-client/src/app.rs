use crate::audio::AudioPlayer;
use crate::config::{PointerMode, ResolutionMode};
use crate::keymap::winit_keycode_to_evdev;
use crate::overlay::{OverlayAction, OverlayState};
use crate::renderer::{Renderer, video_letterbox_rect};
use crate::transport::{AppEvent, LatestSlot, Telemetry};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::input::{HeldInputTracker, InputEvent, MouseButton as UsherMouseButton};
use usher_proto::packet::ControlMessage;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{
    DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent,
};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

pub struct ConnectedSession {
    pub host_label: String,
    pub video_w: u16,
    pub video_h: u16,
    pub initial_fullscreen: bool,
    pub relative_mouse: bool,
    pub input_enabled: bool,
    pub hi_res_scroll: bool,
    pub input_tx: mpsc::Sender<InputEvent>,
    pub control_tx: mpsc::Sender<ControlMessage>,
    pub frame_slot: Arc<LatestSlot<crate::decoder::Yuv420Frame>>,
    pub telemetry: Arc<Telemetry>,
    pub audio_muted: Arc<AtomicBool>,
    pub overlay: OverlayState,
    pub _audio_player: Option<AudioPlayer>,
}

const WHEEL_DELTA_UNITS: f64 = 120.0;
const PIXELS_PER_WHEEL_NOTCH: f64 = 40.0;
const SCROLL_ACCUM_RESET_TIMEOUT: Duration = Duration::from_millis(500);

pub(crate) struct ScrollAccumulator {
    accum_x: f64,
    accum_y: f64,
    last_event: Instant,
}

impl ScrollAccumulator {
    pub(crate) fn new() -> Self {
        Self {
            accum_x: 0.0,
            accum_y: 0.0,
            last_event: Instant::now(),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.accum_x = 0.0;
        self.accum_y = 0.0;
    }

    fn accumulate_axis(accum: &mut f64, delta: f64) {
        if !delta.is_finite() || delta == 0.0 {
            return;
        }
        if *accum != 0.0 && delta.signum() != accum.signum() {
            *accum = 0.0;
        }
        *accum += delta;
    }

    fn take_steps(accum: &mut f64, step_size: f64) -> i16 {
        let scaled = *accum / step_size;
        let rounded = scaled.round();
        if (scaled - rounded).abs() < 1e-4 {
            *accum = rounded * step_size;
        }
        let steps = (*accum / step_size)
            .trunc()
            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        *accum -= (steps as f64) * step_size;
        steps
    }

    pub(crate) fn push(
        &mut self,
        delta: MouseScrollDelta,
        phase: TouchPhase,
        scale_factor: f64,
        hi_res: bool,
        now: Instant,
    ) -> Option<InputEvent> {
        if phase == TouchPhase::Started
            || now.saturating_duration_since(self.last_event) >= SCROLL_ACCUM_RESET_TIMEOUT
        {
            self.reset();
        }
        self.last_event = now;

        let (raw_dx, raw_dy) = match delta {
            MouseScrollDelta::LineDelta(x, y) => {
                (-(x as f64) * WHEEL_DELTA_UNITS, (y as f64) * WHEEL_DELTA_UNITS)
            }
            MouseScrollDelta::PixelDelta(pos) => {
                let scale = if scale_factor.is_finite() {
                    scale_factor.max(0.5)
                } else {
                    1.0
                };
                let factor = WHEEL_DELTA_UNITS / (PIXELS_PER_WHEEL_NOTCH * scale);
                (-pos.x * factor, pos.y * factor)
            }
        };

        Self::accumulate_axis(&mut self.accum_x, raw_dx);
        Self::accumulate_axis(&mut self.accum_y, raw_dy);

        if hi_res {
            let dx = Self::take_steps(&mut self.accum_x, 1.0);
            let dy = Self::take_steps(&mut self.accum_y, 1.0);
            if dx != 0 || dy != 0 {
                Some(InputEvent::MouseWheelHiRes { dx, dy })
            } else {
                None
            }
        } else {
            let dx = Self::take_steps(&mut self.accum_x, WHEEL_DELTA_UNITS);
            let dy = Self::take_steps(&mut self.accum_y, WHEEL_DELTA_UNITS);
            if dx != 0 || dy != 0 {
                Some(InputEvent::MouseWheel { dx, dy })
            } else {
                None
            }
        }
    }

    pub(crate) fn restore_unsent(&mut self, ev: InputEvent) {
        let (dx, dy) = match ev {
            InputEvent::MouseWheelHiRes { dx, dy } => (dx as f64, dy as f64),
            InputEvent::MouseWheel { dx, dy } => (
                (dx as f64) * WHEEL_DELTA_UNITS,
                (dy as f64) * WHEEL_DELTA_UNITS,
            ),
            _ => return,
        };
        self.accum_x = (self.accum_x + dx).clamp(-1200.0, 1200.0);
        self.accum_y = (self.accum_y + dy).clamp(-1200.0, 1200.0);
    }
}

fn enqueue_release_all(
    remote_held: &mut HeldInputTracker,
    input_tx: &mpsc::Sender<InputEvent>,
    always_send_release_all: bool,
) {
    let events = remote_held.drain_release_events();
    for ev in &events {
        let _ = input_tx.try_send(*ev);
    }
    if !events.is_empty() {
        // Send twice as a lightweight idempotent guard against single-packet UDP drops.
        let _ = input_tx.try_send(InputEvent::ReleaseAll);
        let _ = input_tx.try_send(InputEvent::ReleaseAll);
    } else if always_send_release_all {
        let _ = input_tx.try_send(InputEvent::ReleaseAll);
    }
}

pub type ConnectFn = Box<dyn FnOnce(Option<(u16, u16, u16)>) -> anyhow::Result<ConnectedSession>>;

pub struct UsherApp {
    connect_fn: Option<ConnectFn>,
    session: Option<ConnectedSession>,
    pub cancel: CancellationToken,
    pub startup_error: Option<anyhow::Error>,

    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    focused: bool,
    ctrl_left_down: bool,
    ctrl_right_down: bool,
    alt_left_down: bool,
    alt_right_down: bool,
    remote_held: HeldInputTracker,
    rel_accum_x: f64,
    rel_accum_y: f64,
    scroll_accum: ScrollAccumulator,
    cursor_pos: (f64, f64),
    cursor_inside: bool,
    last_title_update: Instant,
    pending_resize: Option<(u16, u16, Instant)>,
}

impl UsherApp {
    pub fn new(connect_fn: ConnectFn, cancel: CancellationToken) -> Self {
        Self {
            connect_fn: Some(connect_fn),
            session: None,
            cancel,
            startup_error: None,
            window: None,
            renderer: None,
            focused: true,
            ctrl_left_down: false,
            ctrl_right_down: false,
            alt_left_down: false,
            alt_right_down: false,
            remote_held: HeldInputTracker::default(),
            rel_accum_x: 0.0,
            rel_accum_y: 0.0,
            scroll_accum: ScrollAccumulator::new(),
            cursor_pos: (0.0, 0.0),
            cursor_inside: false,
            last_title_update: Instant::now(),
            pending_resize: None,
        }
    }

    fn ctrl_down(&self) -> bool {
        self.ctrl_left_down || self.ctrl_right_down
    }

    fn alt_down(&self) -> bool {
        self.alt_left_down || self.alt_right_down
    }

    fn reset_local_modifiers(&mut self) {
        self.ctrl_left_down = false;
        self.ctrl_right_down = false;
        self.alt_left_down = false;
        self.alt_right_down = false;
    }

    fn release_all_remote_inputs(&mut self, always_send_release_all: bool) {
        if let Some(sess) = &self.session
            && sess.input_enabled
        {
            enqueue_release_all(
                &mut self.remote_held,
                &sess.input_tx,
                always_send_release_all,
            );
        } else {
            let _ = self.remote_held.drain_release_events();
        }
    }

    fn apply_mouse_mode(&self) {
        let Some(win) = &self.window else {
            return;
        };
        let Some(sess) = &self.session else {
            return;
        };
        let lock_cursor = !sess.overlay.open && sess.relative_mouse && self.focused;
        let hide_cursor =
            !sess.overlay.open && self.focused && sess.input_enabled && (lock_cursor || self.cursor_inside);
        win.set_cursor_visible(!hide_cursor);
        if lock_cursor {
            if win.set_cursor_grab(CursorGrabMode::Locked).is_err() {
                let _ = win.set_cursor_grab(CursorGrabMode::Confined);
            }
        } else {
            let _ = win.set_cursor_grab(CursorGrabMode::None);
        }
        let _ = sess.control_tx.try_send(ControlMessage::SetPointerMode {
            relative: lock_cursor,
        });
    }

    fn toggle_fullscreen(&self) {
        let Some(win) = &self.window else {
            return;
        };
        if win.fullscreen().is_some() {
            win.set_fullscreen(None);
        } else {
            win.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
    }

    fn apply_overlay_action(&mut self, act: OverlayAction) {
        let Some(sess) = &mut self.session else {
            return;
        };
        if act.close_overlay {
            sess.overlay.open = false;
            sess.overlay.mark_dirty();
        }
        if let Some(msg) = act.reconfigure {
            let _ = sess.control_tx.try_send(msg);
        }
        if let Some(audio) = act.audio_enabled {
            sess.audio_muted.store(!audio, Ordering::Release);
        }
        if let Some(pm) = act.pointer_mode {
            sess.relative_mouse = pm == PointerMode::Relative;
        }
        if let Some(mode) = act.resolution_mode
            && mode != ResolutionMode::MatchWindow
        {
            self.pending_resize = None;
        }
        if let Some(hud) = act.stats_hud {
            sess.overlay.stats_hud = hud;
        }
        if act.close_overlay || act.pointer_mode.is_some() {
            self.apply_mouse_mode();
            self.update_title();
        }
        if let Some(win) = &self.window {
            win.request_redraw();
        }
    }

    fn update_title(&mut self) {
        if self.last_title_update.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_title_update = Instant::now();
        let Some(win) = &self.window else {
            return;
        };
        let Some(sess) = &self.session else {
            return;
        };

        let fps = sess.telemetry.fps_x10.load(Ordering::Acquire) as f32 / 10.0;
        let rtt_ms = sess.telemetry.rtt_us.load(Ordering::Acquire) as f32 / 1000.0;
        let mbps = sess.telemetry.kbps.load(Ordering::Acquire) as f32 / 1000.0;
        let fec = sess.telemetry.fec_recovered.load(Ordering::Acquire);
        let mode = if sess.relative_mouse { "REL" } else { "ABS" };

        win.set_title(&format!(
            "Usher — {} | {:.0} fps | {:.1} ms RTT | {:.1} Mbps | FEC: {} | [{}] (Ctrl+Alt+S/G/F/Q)",
            sess.host_label, fps, rtt_ms, mbps, fec, mode
        ));
    }

    fn flush_debounced_resize(&mut self) {
        let Some(sess) = &self.session else {
            return;
        };
        if let Some((rw, rh, at)) = self.pending_resize
            && at.elapsed() >= Duration::from_millis(600)
        {
            self.pending_resize = None;
            if sess.overlay.resolution_mode == ResolutionMode::MatchWindow {
                let _ = sess.control_tx.try_send(ControlMessage::Resize {
                    width: rw,
                    height: rh,
                });
            }
        }
    }

    fn shutdown(&mut self, event_loop: &ActiveEventLoop) {
        self.release_all_remote_inputs(true);
        self.reset_local_modifiers();
        self.scroll_accum.reset();
        if let Some(sess) = &self.session {
            let _ = sess.control_tx.try_send(ControlMessage::Disconnect);
            sess.frame_slot.close();
        }
        self.cancel.cancel();
        event_loop.exit();
    }
}

impl ApplicationHandler<AppEvent> for UsherApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let mon_info = event_loop.primary_monitor().map(|m| {
            let sz = m.size();
            let w = (sz.width.clamp(64, 3840) as u16) & !1;
            let h = (sz.height.clamp(64, 2160) as u16) & !1;
            let fps = m
                .refresh_rate_millihertz()
                .map(|mhz| ((mhz + 500) / 1000).clamp(30, 240) as u16)
                .unwrap_or(60);
            (w, h, fps)
        });

        if self.session.is_none()
            && let Some(connect) = self.connect_fn.take()
        {
            match connect(mon_info) {
                Ok(sess) => self.session = Some(sess),
                Err(err) => {
                    self.startup_error = Some(err);
                    self.cancel.cancel();
                    event_loop.exit();
                    return;
                }
            }
        }

        let Some(sess) = &mut self.session else {
            return;
        };
        let win_icon = crate::icon::build_window_icon();
        let (init_w, init_h) = if sess.initial_fullscreen {
            (
                ((sess.video_w as u32 * 4) / 5).max(640),
                ((sess.video_h as u32 * 4) / 5).max(360),
            )
        } else {
            (sess.video_w as u32, sess.video_h as u32)
        };
        let mut attrs = Window::default_attributes()
            .with_title(format!("Usher — {} ({}x{})", sess.host_label, sess.video_w, sess.video_h))
            .with_inner_size(PhysicalSize::new(init_w, init_h))
            .with_window_icon(win_icon.clone());
        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs
                .with_taskbar_icon(win_icon)
                .with_drag_and_drop(false);
        }
        if sess.initial_fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }

        match event_loop.create_window(attrs) {
            Ok(win) => {
                let win = Arc::new(win);
                match Renderer::new(win.clone(), sess.video_w as u32, sess.video_h as u32) {
                    Ok(renderer) => {
                        let phys = win.inner_size();
                        sess.overlay.window_phys_res = (
                            (phys.width.clamp(64, 3840) as u16) & !1,
                            (phys.height.clamp(64, 2160) as u16) & !1,
                        );
                        let start_fullscreen = sess.initial_fullscreen;
                        self.window = Some(win.clone());
                        self.renderer = Some(renderer);
                        if start_fullscreen {
                            self.cursor_inside = true;
                        }
                        self.apply_mouse_mode();
                        win.request_redraw();
                    }
                    Err(err) => {
                        tracing::error!("GPU renderer initialization failed: {err:#}");
                        self.shutdown(event_loop);
                    }
                }
            }
            Err(err) => {
                tracing::error!("window creation failed: {err}");
                self.shutdown(event_loop);
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::NewFrame => {
                if let Some(win) = &self.window {
                    win.request_redraw();
                }
            }
            AppEvent::StreamStatusChanged => {
                if let Some(sess) = &mut self.session {
                    sess.overlay.sync_from_telemetry(&sess.telemetry);
                }
                if let Some(win) = &self.window {
                    win.request_redraw();
                }
            }
            AppEvent::Shutdown => {
                self.shutdown(event_loop);
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        self.flush_debounced_resize();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.shutdown(event_loop);
            }
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if !focused {
                    self.release_all_remote_inputs(true);
                    self.reset_local_modifiers();
                    self.scroll_accum.reset();
                }
                self.apply_mouse_mode();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
                if size.width >= 64
                    && size.height >= 64
                    && let Some(sess) = &mut self.session
                {
                    let rw = (size.width.min(3840) as u16) & !1;
                    let rh = (size.height.min(2160) as u16) & !1;
                    sess.overlay.window_phys_res = (rw, rh);
                    sess.overlay.mark_dirty();
                    if sess.overlay.resolution_mode == ResolutionMode::MatchWindow {
                        self.pending_resize = Some((rw, rh, Instant::now()));
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.flush_debounced_resize();
                if let (Some(renderer), Some(sess)) = (&mut self.renderer, &mut self.session) {
                    sess.overlay.tick_dirty();
                    if !sess.overlay.is_visible() {
                        renderer.update_overlay(None);
                    } else {
                        let (ww, wh) = renderer.window_dimensions();
                        if let Some(out) = sess.overlay.render_if_dirty(ww, wh, &sess.telemetry) {
                            renderer.update_overlay(Some(&out));
                        }
                    }
                    let frame = sess.frame_slot.try_take();
                    renderer.render(frame.as_ref());
                }
                self.update_title();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                if let PhysicalKey::Code(code) = event.physical_key {
                    match code {
                        KeyCode::ControlLeft => self.ctrl_left_down = pressed,
                        KeyCode::ControlRight => self.ctrl_right_down = pressed,
                        KeyCode::AltLeft => self.alt_left_down = pressed,
                        KeyCode::AltRight => self.alt_right_down = pressed,
                        KeyCode::KeyS if pressed && self.ctrl_down() && self.alt_down() => {
                            self.release_all_remote_inputs(false);
                            if let Some(sess) = &mut self.session {
                                sess.overlay.toggle_open();
                                if sess.overlay.open {
                                    self.scroll_accum.reset();
                                }
                            }
                            self.apply_mouse_mode();
                            if let Some(win) = &self.window {
                                win.request_redraw();
                            }
                            return;
                        }
                        KeyCode::KeyG if pressed && self.ctrl_down() && self.alt_down() => {
                            self.release_all_remote_inputs(false);
                            if let Some(sess) = &mut self.session {
                                sess.relative_mouse = !sess.relative_mouse;
                                sess.overlay.pointer_mode = if sess.relative_mouse {
                                    PointerMode::Relative
                                } else {
                                    PointerMode::Absolute
                                };
                                sess.overlay.mark_dirty();
                            }
                            self.apply_mouse_mode();
                            self.update_title();
                            return;
                        }
                        KeyCode::KeyF if pressed && self.ctrl_down() && self.alt_down() => {
                            self.release_all_remote_inputs(false);
                            self.toggle_fullscreen();
                            return;
                        }
                        KeyCode::KeyQ if pressed && self.ctrl_down() && self.alt_down() => {
                            self.shutdown(event_loop);
                            return;
                        }
                        _ => {}
                    }

                    let overlay_open = self.session.as_ref().is_some_and(|s| s.overlay.open);
                    if overlay_open {
                        if pressed {
                            match code {
                                KeyCode::Escape => {
                                    if let Some(sess) = &mut self.session {
                                        sess.overlay.open = false;
                                        sess.overlay.mark_dirty();
                                    }
                                    self.apply_mouse_mode();
                                }
                                KeyCode::ArrowUp => {
                                    if let Some(sess) = &mut self.session {
                                        sess.overlay.move_selection(-1);
                                    }
                                }
                                KeyCode::ArrowDown => {
                                    if let Some(sess) = &mut self.session {
                                        sess.overlay.move_selection(1);
                                    }
                                }
                                KeyCode::ArrowLeft => {
                                    if let Some(sess) = &mut self.session {
                                        let act = sess.overlay.activate_or_step(-1);
                                        self.apply_overlay_action(act);
                                    }
                                    return;
                                }
                                KeyCode::ArrowRight | KeyCode::Enter | KeyCode::Space => {
                                    if let Some(sess) = &mut self.session {
                                        let act = sess.overlay.activate_or_step(1);
                                        self.apply_overlay_action(act);
                                    }
                                    return;
                                }
                                _ => {}
                            }
                            if let Some(win) = &self.window {
                                win.request_redraw();
                            }
                        }
                        return;
                    }

                    if let Some(sess) = &self.session
                        && sess.input_enabled
                        && !event.repeat
                        && let Some(evdev_code) = winit_keycode_to_evdev(code)
                        && self.remote_held.should_forward_key(evdev_code, pressed)
                    {
                        let _ = sess.input_tx.try_send(InputEvent::Key {
                            evdev_code,
                            pressed,
                        });
                    }
                }
            }
            WindowEvent::CursorEntered { .. } => {
                self.cursor_inside = true;
                self.apply_mouse_mode();
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_inside = false;
                self.apply_mouse_mode();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x, position.y);
                if !self.cursor_inside {
                    self.cursor_inside = true;
                    self.apply_mouse_mode();
                }
                let Some(sess) = &self.session else {
                    return;
                };
                if sess.overlay.open || !sess.input_enabled || sess.relative_mouse {
                    return;
                }
                if let Some(renderer) = &self.renderer {
                    let (ww, wh) = renderer.window_dimensions();
                    let (vw, vh) = renderer.video_dimensions();
                    let (rx, ry, rw, rh) = video_letterbox_rect(ww, wh, vw, vh);
                    let nx = ((position.x - rx) / rw.max(1.0)).clamp(0.0, 1.0);
                    let ny = ((position.y - ry) / rh.max(1.0)).clamp(0.0, 1.0);
                    let x = (nx * 65535.0).round() as u16;
                    let y = (ny * 65535.0).round() as u16;
                    let _ = sess.input_tx.try_send(InputEvent::MouseMoveAbs { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let overlay_open = self.session.as_ref().is_some_and(|s| s.overlay.open);
                if overlay_open {
                    if state == ElementState::Pressed
                        && button == MouseButton::Left
                        && let Some(sess) = &mut self.session
                    {
                        let act = sess
                            .overlay
                            .handle_click(self.cursor_pos.0, self.cursor_pos.1);
                        self.apply_overlay_action(act);
                    }
                    return;
                }
                let Some(sess) = &self.session else {
                    return;
                };
                if !sess.input_enabled {
                    return;
                }
                let btn = match button {
                    MouseButton::Left => UsherMouseButton::Left,
                    MouseButton::Right => UsherMouseButton::Right,
                    MouseButton::Middle => UsherMouseButton::Middle,
                    MouseButton::Back => UsherMouseButton::Side,
                    MouseButton::Forward => UsherMouseButton::Extra,
                    MouseButton::Other(_) => return,
                };
                let pressed = state == ElementState::Pressed;
                if self.remote_held.should_forward_button(btn, pressed) {
                    let _ = sess.input_tx.try_send(InputEvent::MouseButton {
                        button: btn,
                        pressed,
                    });
                }
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                let (input_tx, hi_res_scroll) = {
                    let Some(sess) = &self.session else {
                        return;
                    };
                    if sess.overlay.open || !sess.input_enabled {
                        return;
                    }
                    (sess.input_tx.clone(), sess.hi_res_scroll)
                };
                let scale_factor = self.window.as_ref().map_or(1.0, |w| w.scale_factor());
                if let Some(ev) = self.scroll_accum.push(
                    delta,
                    phase,
                    scale_factor,
                    hi_res_scroll,
                    Instant::now(),
                ) {
                    tracing::trace!(?delta, ?phase, ?ev, "forwarding mouse wheel");
                    if input_tx.try_send(ev).is_err() {
                        self.scroll_accum.restore_unsent(ev);
                    }
                }
            }
            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        let Some(sess) = &self.session else {
            return;
        };
        if sess.overlay.open || !sess.input_enabled || !sess.relative_mouse || !self.focused {
            return;
        }
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            self.rel_accum_x += dx;
            self.rel_accum_y += dy;
            let ix = self.rel_accum_x.trunc() as i32;
            let iy = self.rel_accum_y.trunc() as i32;
            self.rel_accum_x -= ix as f64;
            self.rel_accum_y -= iy as f64;
            if ix != 0 || iy != 0 {
                let _ = sess
                    .input_tx
                    .try_send(InputEvent::MouseMoveRel { dx: ix, dy: iy });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    #[test]
    fn windows_sub_detent_scroll_never_loses_units() {
        let mut acc = ScrollAccumulator::new();
        let t0 = Instant::now();
        for n in 1i16..=120 {
            let pos = acc.push(
                MouseScrollDelta::LineDelta(0.0, n as f32 / 120.0),
                TouchPhase::Moved,
                1.0,
                true,
                t0,
            );
            assert_eq!(
                pos,
                Some(InputEvent::MouseWheelHiRes { dx: 0, dy: n }),
                "positive sub-detent {n}/120 truncated"
            );

            let neg = acc.push(
                MouseScrollDelta::LineDelta(0.0, -(n as f32) / 120.0),
                TouchPhase::Moved,
                1.0,
                true,
                t0,
            );
            assert_eq!(
                neg,
                Some(InputEvent::MouseWheelHiRes { dx: 0, dy: -n }),
                "negative sub-detent -{n}/120 truncated"
            );
        }
    }

    #[test]
    fn horizontal_scroll_inverts_winit_sign_to_right_positive() {
        let mut acc = ScrollAccumulator::new();
        let t0 = Instant::now();
        let right = acc.push(
            MouseScrollDelta::LineDelta(-0.1, 0.0),
            TouchPhase::Moved,
            1.0,
            true,
            t0,
        );
        assert_eq!(right, Some(InputEvent::MouseWheelHiRes { dx: 12, dy: 0 }));

        let left = acc.push(
            MouseScrollDelta::LineDelta(0.1, 0.0),
            TouchPhase::Moved,
            1.0,
            true,
            t0,
        );
        assert_eq!(left, Some(InputEvent::MouseWheelHiRes { dx: -12, dy: 0 }));
    }

    #[test]
    fn pixel_delta_accumulates_fractional_scroll_events() {
        let mut acc = ScrollAccumulator::new();
        let t0 = Instant::now();
        // At scale_factor = 2.0, factor = 120 / (40 * 2) = 1.5 units/px -> 0.2 px = 0.3 units.
        for _ in 0..3 {
            assert_eq!(
                acc.push(
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.2)),
                    TouchPhase::Moved,
                    2.0,
                    true,
                    t0,
                ),
                None
            );
        }
        assert_eq!(
            acc.push(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.2)),
                TouchPhase::Moved,
                2.0,
                true,
                t0,
            ),
            Some(InputEvent::MouseWheelHiRes { dx: 0, dy: 1 })
        );
    }

    #[test]
    fn legacy_fallback_accumulates_small_touchpad_events_into_detents() {
        let mut acc = ScrollAccumulator::new();
        let t0 = Instant::now();
        for _ in 0..19 {
            assert_eq!(
                acc.push(
                    MouseScrollDelta::LineDelta(0.0, -0.05),
                    TouchPhase::Moved,
                    1.0,
                    false,
                    t0,
                ),
                None
            );
        }
        assert_eq!(
            acc.push(
                MouseScrollDelta::LineDelta(0.0, -0.05),
                TouchPhase::Moved,
                1.0,
                false,
                t0,
            ),
            Some(InputEvent::MouseWheel { dx: 0, dy: -1 })
        );
    }

    #[test]
    fn reversal_start_and_timeout_clear_stale_remainder() {
        let mut acc = ScrollAccumulator::new();
        let t0 = Instant::now();

        // Accumulate 0.5 units vertically (below 1.0 threshold).
        assert_eq!(
            acc.push(
                MouseScrollDelta::LineDelta(0.0, 0.5 / 120.0),
                TouchPhase::Moved,
                1.0,
                true,
                t0,
            ),
            None
        );
        // Reversing immediately clears the positive 0.5 remainder rather than requiring 1.5 downward.
        assert_eq!(
            acc.push(
                MouseScrollDelta::LineDelta(0.0, -1.0 / 120.0),
                TouchPhase::Moved,
                1.0,
                true,
                t0,
            ),
            Some(InputEvent::MouseWheelHiRes { dx: 0, dy: -1 })
        );

        // Leave 0.6 units accumulated, then send TouchPhase::Started.
        assert_eq!(
            acc.push(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.2)),
                TouchPhase::Moved,
                1.0,
                true,
                t0,
            ),
            None
        );
        assert_eq!(
            acc.push(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.2)),
                TouchPhase::Started,
                1.0,
                true,
                t0 + Duration::from_millis(10),
            ),
            None
        );
        // After reset at Started, only 0.6 units accumulated, so 0.2 more reaches 1.2 -> dy=1 (not >1 from old sum).
        assert_eq!(
            acc.push(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.2)),
                TouchPhase::Moved,
                1.0,
                true,
                t0 + Duration::from_millis(20),
            ),
            Some(InputEvent::MouseWheelHiRes { dx: 0, dy: 1 })
        );

        // Leave 0.6 units, then wait >= 500ms timeout.
        assert_eq!(
            acc.push(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.2)),
                TouchPhase::Moved,
                1.0,
                true,
                t0 + Duration::from_millis(600),
            ),
            None
        );
    }

    #[test]
    fn ctrl_alt_shortcut_releases_alt_before_ctrl_and_suppresses_physical_ups() {
        let mut held = HeldInputTracker::default();
        let (tx, mut rx) = mpsc::channel(16);

        let ctrl = winit_keycode_to_evdev(KeyCode::ControlLeft).unwrap();
        let alt = winit_keycode_to_evdev(KeyCode::AltLeft).unwrap();
        let key_q = winit_keycode_to_evdev(KeyCode::KeyQ).unwrap();

        assert!(held.should_forward_key(ctrl, true));
        assert!(held.should_forward_key(alt, true));

        // Pressing Ctrl+Alt+Q triggers enqueue_release_all before sending KeyQ or shutting down.
        enqueue_release_all(&mut held, &tx, true);

        let mut sent = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            sent.push(ev);
        }
        assert_eq!(
            sent,
            vec![
                InputEvent::Key {
                    evdev_code: alt,
                    pressed: false,
                },
                InputEvent::Key {
                    evdev_code: ctrl,
                    pressed: false,
                },
                InputEvent::ReleaseAll,
                InputEvent::ReleaseAll,
            ]
        );

        // Physical releases after hotkey / overlay / focus-loss are suppressed cleanly.
        assert!(!held.should_forward_key(key_q, false));
        assert!(!held.should_forward_key(alt, false));
        assert!(!held.should_forward_key(ctrl, false));

        // Nothing held + always_send_release_all=false emits nothing; =true emits one ReleaseAll.
        enqueue_release_all(&mut held, &tx, false);
        assert!(rx.try_recv().is_err());
        enqueue_release_all(&mut held, &tx, true);
        assert_eq!(rx.try_recv().ok(), Some(InputEvent::ReleaseAll));
        assert!(rx.try_recv().is_err());
    }
}
