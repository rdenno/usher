use gilrs::{Axis, Button, Gilrs};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use usher_proto::input::InputEvent;

pub fn spawn_gamepad_thread(
    input_tx: mpsc::Sender<InputEvent>,
    cancel: CancellationToken,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut gilrs = match Gilrs::new() {
            Ok(g) => g,
            Err(err) => {
                tracing::debug!("gilrs gamepad polling disabled ({err})");
                return;
            }
        };

        let mut last_state: Option<InputEvent> = None;

        while !cancel.is_cancelled() {
            while let Some(ev) = gilrs.next_event() {
                let _ = ev;
            }

            if let Some((_, gp)) = gilrs.gamepads().find(|(_, gp)| gp.is_connected()) {
                let mut buttons = 0u16;
                let btn_map = [
                    (0u16, Button::DPadUp),
                    (1, Button::DPadDown),
                    (2, Button::DPadLeft),
                    (3, Button::DPadRight),
                    (4, Button::Start),
                    (5, Button::Select),
                    (6, Button::LeftThumb),
                    (7, Button::RightThumb),
                    (8, Button::LeftTrigger),
                    (9, Button::RightTrigger),
                    (10, Button::Mode),
                    (12, Button::South),
                    (13, Button::East),
                    (14, Button::West),
                    (15, Button::North),
                ];
                for (bit, btn) in btn_map {
                    if gp.is_pressed(btn) {
                        buttons |= 1u16 << bit;
                    }
                }

                let lt = gp
                    .button_data(Button::LeftTrigger2)
                    .map(|d| (d.value().clamp(0.0, 1.0) * 255.0).round() as u8)
                    .unwrap_or_else(|| {
                        ((gp.value(Axis::LeftZ).clamp(-1.0, 1.0) + 1.0) * 127.5).round() as u8
                    });
                let rt = gp
                    .button_data(Button::RightTrigger2)
                    .map(|d| (d.value().clamp(0.0, 1.0) * 255.0).round() as u8)
                    .unwrap_or_else(|| {
                        ((gp.value(Axis::RightZ).clamp(-1.0, 1.0) + 1.0) * 127.5).round() as u8
                    });

                let state = InputEvent::GamepadState {
                    buttons,
                    lt,
                    rt,
                    lx: norm_axis_to_i16(gp.value(Axis::LeftStickX)),
                    ly: norm_axis_to_i16(-gp.value(Axis::LeftStickY)),
                    rx: norm_axis_to_i16(gp.value(Axis::RightStickX)),
                    ry: norm_axis_to_i16(-gp.value(Axis::RightStickY)),
                };

                if last_state.as_ref() != Some(&state) {
                    last_state = Some(state);
                    let _ = input_tx.try_send(state);
                }
            } else if last_state.take().is_some() {
                let _ = input_tx.try_send(InputEvent::GamepadState {
                    buttons: 0,
                    lt: 0,
                    rt: 0,
                    lx: 0,
                    ly: 0,
                    rx: 0,
                    ry: 0,
                });
            }

            std::thread::sleep(Duration::from_millis(8));
        }
    })
}

fn norm_axis_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}
