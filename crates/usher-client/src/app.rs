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
use usher_proto::input::{InputEvent, MouseButton as UsherMouseButton};
use usher_proto::packet::ControlMessage;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
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
    pub input_tx: mpsc::Sender<InputEvent>,
    pub control_tx: mpsc::Sender<ControlMessage>,
    pub frame_slot: Arc<LatestSlot<crate::decoder::Yuv420Frame>>,
    pub telemetry: Arc<Telemetry>,
    pub audio_muted: Arc<AtomicBool>,
    pub overlay: OverlayState,
    pub _audio_player: Option<AudioPlayer>,
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
    ctrl_down: bool,
    alt_down: bool,
    rel_accum_x: f64,
    rel_accum_y: f64,
    cursor_pos: (f64, f64),
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
            ctrl_down: false,
            alt_down: false,
            rel_accum_x: 0.0,
            rel_accum_y: 0.0,
            cursor_pos: (0.0, 0.0),
            last_title_update: Instant::now(),
            pending_resize: None,
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
        if lock_cursor {
            win.set_cursor_visible(false);
            if win.set_cursor_grab(CursorGrabMode::Locked).is_err() {
                let _ = win.set_cursor_grab(CursorGrabMode::Confined);
            }
        } else {
            let _ = win.set_cursor_grab(CursorGrabMode::None);
            win.set_cursor_visible(true);
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

    fn shutdown(&self, event_loop: &ActiveEventLoop) {
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
        let mut attrs = Window::default_attributes()
            .with_title(format!("Usher — {} ({}x{})", sess.host_label, sess.video_w, sess.video_h))
            .with_inner_size(PhysicalSize::new(sess.video_w as u32, sess.video_h as u32))
            .with_window_icon(win_icon.clone());
        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_taskbar_icon(win_icon);
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
                        self.window = Some(win.clone());
                        self.renderer = Some(renderer);
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
                    self.ctrl_down = false;
                    self.alt_down = false;
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
                        KeyCode::ControlLeft | KeyCode::ControlRight => self.ctrl_down = pressed,
                        KeyCode::AltLeft | KeyCode::AltRight => self.alt_down = pressed,
                        KeyCode::KeyS if pressed && self.ctrl_down && self.alt_down => {
                            if let Some(sess) = &mut self.session {
                                sess.overlay.toggle_open();
                            }
                            self.apply_mouse_mode();
                            if let Some(win) = &self.window {
                                win.request_redraw();
                            }
                            return;
                        }
                        KeyCode::KeyG if pressed && self.ctrl_down && self.alt_down => {
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
                        KeyCode::KeyF if pressed && self.ctrl_down && self.alt_down => {
                            self.toggle_fullscreen();
                            return;
                        }
                        KeyCode::KeyQ if pressed && self.ctrl_down && self.alt_down => {
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
                    {
                        let _ = sess.input_tx.try_send(InputEvent::Key {
                            evdev_code,
                            pressed,
                        });
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x, position.y);
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
                let _ = sess.input_tx.try_send(InputEvent::MouseButton {
                    button: btn,
                    pressed: state == ElementState::Pressed,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let Some(sess) = &self.session else {
                    return;
                };
                if sess.overlay.open || !sess.input_enabled {
                    return;
                }
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x.round() as i16, y.round() as i16),
                    MouseScrollDelta::PixelDelta(pos) => (
                        (pos.x / 16.0).round() as i16,
                        (pos.y / 16.0).round() as i16,
                    ),
                };
                if dx != 0 || dy != 0 {
                    let _ = sess.input_tx.try_send(InputEvent::MouseWheel { dx, dy });
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
