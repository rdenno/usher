use crate::keymap::winit_keycode_to_evdev;
use crate::renderer::{Renderer, video_letterbox_rect};
use crate::transport::{AppEvent, LatestSlot, Telemetry};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::input::{InputEvent, MouseButton as UsherMouseButton};
use usher_proto::packet::ControlMessage;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

pub struct UsherApp {
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
    pub cancel: CancellationToken,

    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    focused: bool,
    ctrl_down: bool,
    alt_down: bool,
    rel_accum_x: f64,
    rel_accum_y: f64,
    last_title_update: Instant,
}

impl UsherApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host_label: String,
        video_w: u16,
        video_h: u16,
        initial_fullscreen: bool,
        relative_mouse: bool,
        input_enabled: bool,
        input_tx: mpsc::Sender<InputEvent>,
        control_tx: mpsc::Sender<ControlMessage>,
        frame_slot: Arc<LatestSlot<crate::decoder::Yuv420Frame>>,
        telemetry: Arc<Telemetry>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            host_label,
            video_w,
            video_h,
            initial_fullscreen,
            relative_mouse,
            input_enabled,
            input_tx,
            control_tx,
            frame_slot,
            telemetry,
            cancel,
            window: None,
            renderer: None,
            focused: true,
            ctrl_down: false,
            alt_down: false,
            rel_accum_x: 0.0,
            rel_accum_y: 0.0,
            last_title_update: Instant::now(),
        }
    }

    fn apply_mouse_mode(&self) {
        let Some(win) = &self.window else {
            return;
        };
        if self.relative_mouse && self.focused {
            win.set_cursor_visible(false);
            if win.set_cursor_grab(CursorGrabMode::Locked).is_err() {
                let _ = win.set_cursor_grab(CursorGrabMode::Confined);
            }
        } else {
            let _ = win.set_cursor_grab(CursorGrabMode::None);
            win.set_cursor_visible(true);
        }
        let _ = self.control_tx.try_send(ControlMessage::SetPointerMode {
            relative: self.relative_mouse,
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

    fn update_title(&mut self) {
        if self.last_title_update.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_title_update = Instant::now();
        let Some(win) = &self.window else {
            return;
        };

        let fps = self.telemetry.fps_x10.load(Ordering::Acquire) as f32 / 10.0;
        let rtt_ms = self.telemetry.rtt_us.load(Ordering::Acquire) as f32 / 1000.0;
        let mbps = self.telemetry.kbps.load(Ordering::Acquire) as f32 / 1000.0;
        let fec = self.telemetry.fec_recovered.load(Ordering::Acquire);
        let mode = if self.relative_mouse { "REL" } else { "ABS" };

        win.set_title(&format!(
            "Usher — {} | {:.0} fps | {:.1} ms RTT | {:.1} Mbps | FEC: {} | [{}] (Ctrl+Alt+G/F/Q)",
            self.host_label, fps, rtt_ms, mbps, fec, mode
        ));
    }

    fn shutdown(&self, event_loop: &ActiveEventLoop) {
        let _ = self.control_tx.try_send(ControlMessage::Disconnect);
        self.cancel.cancel();
        self.frame_slot.close();
        event_loop.exit();
    }
}

impl ApplicationHandler<AppEvent> for UsherApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let mut attrs = Window::default_attributes()
            .with_title(format!("Usher — {} ({}x{})", self.host_label, self.video_w, self.video_h))
            .with_inner_size(LogicalSize::new(self.video_w as f64, self.video_h as f64));
        if self.initial_fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }

        match event_loop.create_window(attrs) {
            Ok(win) => {
                let win = Arc::new(win);
                match Renderer::new(win.clone(), self.video_w as u32, self.video_h as u32) {
                    Ok(renderer) => {
                        self.window = Some(win);
                        self.renderer = Some(renderer);
                        self.apply_mouse_mode();
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
            AppEvent::Shutdown => {
                self.shutdown(event_loop);
            }
        }
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
            }
            WindowEvent::RedrawRequested => {
                if let Some(renderer) = &mut self.renderer {
                    let frame = self.frame_slot.try_take();
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
                        KeyCode::KeyG if pressed && self.ctrl_down && self.alt_down => {
                            self.relative_mouse = !self.relative_mouse;
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

                    if self.input_enabled
                        && !event.repeat
                        && let Some(evdev_code) = winit_keycode_to_evdev(code)
                    {
                        let _ = self.input_tx.try_send(InputEvent::Key {
                            evdev_code,
                            pressed,
                        });
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if !self.input_enabled || self.relative_mouse {
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
                    let _ = self.input_tx.try_send(InputEvent::MouseMoveAbs { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if !self.input_enabled {
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
                let _ = self.input_tx.try_send(InputEvent::MouseButton {
                    button: btn,
                    pressed: state == ElementState::Pressed,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if !self.input_enabled {
                    return;
                }
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        (x.round() as i16, y.round() as i16)
                    }
                    MouseScrollDelta::PixelDelta(pos) => (
                        (pos.x / 16.0).round() as i16,
                        (pos.y / 16.0).round() as i16,
                    ),
                };
                if dx != 0 || dy != 0 {
                    let _ = self.input_tx.try_send(InputEvent::MouseWheel { dx, dy });
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
        if !self.input_enabled || !self.relative_mouse || !self.focused {
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
                let _ = self
                    .input_tx
                    .try_send(InputEvent::MouseMoveRel { dx: ix, dy: iy });
            }
        }
    }
}
