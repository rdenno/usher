use super::InputSink;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use usher_proto::input::{InputEvent, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::WHEEL_DELTA;

static WARNED_GAMEPAD: AtomicBool = AtomicBool::new(false);

pub struct Win32InputSink {
    tx: Option<std::sync::mpsc::SyncSender<InputEvent>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Win32InputSink {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<InputEvent>(256);
        let handle = std::thread::Builder::new()
            .name("usher-win32-input".into())
            .spawn(move || {
                use windows::Win32::System::StationsAndDesktops::{
                    CloseDesktop, GetThreadDesktop, HDESK, SetThreadDesktop,
                };
                use windows::Win32::System::Threading::GetCurrentThreadId;

                let orig_desk = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok();
                let mut prev_desk: Option<HDESK> = None;
                let mut last_attach = Instant::now() - Duration::from_secs(1);

                while let Ok(event) = rx.recv() {
                    if last_attach.elapsed() >= Duration::from_millis(200) {
                        crate::windows_session::switch_thread_to_input_desktop(&mut prev_desk);
                        last_attach = Instant::now();
                    }
                    dispatch_input_event(event, &mut prev_desk, &mut last_attach);
                }

                if let Some(old) = prev_desk.take() {
                    if let Some(orig) = orig_desk {
                        let _ = unsafe { SetThreadDesktop(orig) };
                    }
                    let _ = unsafe { CloseDesktop(old) };
                }
            })
            .ok();
        Self {
            tx: handle.as_ref().map(|_| tx),
            handle,
        }
    }
}

impl Drop for Win32InputSink {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Translates a Linux `<linux/input-event-codes.h>` (`evdev`) scancode into a
/// Windows PS/2 Set 1 hardware scan code plus `is_extended` (`KEYEVENTF_EXTENDEDKEY`).
pub fn evdev_to_win32_scancode(evdev_code: u16) -> Option<(u16, bool)> {
    let pair = match evdev_code {
        // Direct 1:1 Set-1 scancodes 1..=88 (Esc .. F12, excluding extended keys below)
        1..=83 | 86..=88 => (evdev_code, false),
        // Numpad Enter, Right Ctrl, Numpad Slash, PrintScreen/SysRq, Right Alt
        96 => (0x1C, true),
        97 => (0x1D, true),
        98 => (0x35, true),
        99 => (0x37, true),
        100 => (0x38, true),
        // Navigation cluster
        102 => (0x47, true), // Home
        103 => (0x48, true), // Up
        104 => (0x49, true), // PageUp
        105 => (0x4B, true), // Left
        106 => (0x4D, true), // Right
        107 => (0x4F, true), // End
        108 => (0x50, true), // Down
        109 => (0x51, true), // PageDown
        110 => (0x52, true), // Insert
        111 => (0x53, true), // Delete
        // Super / Meta / Context Menu
        125 => (0x5B, true), // LeftMeta
        126 => (0x5C, true), // RightMeta
        127 => (0x5D, true), // Apps / ContextMenu
        // Extended function keys F13..F24
        183 => (0x64, false),
        184 => (0x65, false),
        185 => (0x66, false),
        186 => (0x67, false),
        187 => (0x68, false),
        188 => (0x69, false),
        189 => (0x6A, false),
        190 => (0x6B, false),
        191 => (0x6C, false),
        192 => (0x6D, false),
        193 => (0x6E, false),
        194 => (0x76, false),
        // Japanese / IME keys
        89 => (0x73, false),  // RO
        92 => (0x79, false),  // Henkan
        93 => (0x70, false),  // Katakana/Hiragana
        94 => (0x7B, false),  // Muhenkan
        124 => (0x7D, false), // Yen
        _ => return None,
    };
    Some(pair)
}

fn send_single(
    input: INPUT,
    prev_desk: &mut Option<windows::Win32::System::StationsAndDesktops::HDESK>,
    last_attach: &mut Instant,
) {
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        crate::windows_session::switch_thread_to_input_desktop(prev_desk);
        *last_attach = Instant::now();
        let retry = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        if retry == 0 {
            tracing::debug!("Win32 SendInput returned 0 after desktop reattach");
        }
    }
}

impl InputSink for Win32InputSink {
    fn handle(&mut self, event: InputEvent) {
        if let Some(ref tx) = self.tx {
            let _ = tx.try_send(event);
        }
    }
}

fn dispatch_input_event(
    event: InputEvent,
    prev_desk: &mut Option<windows::Win32::System::StationsAndDesktops::HDESK>,
    last_attach: &mut Instant,
) {
    match event {
        InputEvent::Key {
            evdev_code,
            pressed,
        } => {
            let Some((scan, extended)) = evdev_to_win32_scancode(evdev_code) else {
                return;
            };
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !pressed {
                flags |= KEYEVENTF_KEYUP;
            }
            send_single(
                INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: scan,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                },
                prev_desk,
                last_attach,
            );
        }
        InputEvent::MouseMoveRel { dx, dy } => {
            send_single(
                INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx,
                            dy,
                            mouseData: 0,
                            dwFlags: MOUSEEVENTF_MOVE,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                },
                prev_desk,
                last_attach,
            );
        }
        InputEvent::MouseMoveAbs { x, y } => {
            send_single(
                INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: x as i32,
                            dy: y as i32,
                            mouseData: 0,
                            dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                },
                prev_desk,
                last_attach,
            );
        }
        InputEvent::MouseButton { button, pressed } => {
            let (flags, data) = match (button, pressed) {
                (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::Side, true) => (MOUSEEVENTF_XDOWN, 1),
                (MouseButton::Side, false) => (MOUSEEVENTF_XUP, 1),
                (MouseButton::Extra, true) => (MOUSEEVENTF_XDOWN, 2),
                (MouseButton::Extra, false) => (MOUSEEVENTF_XUP, 2),
            };
            send_single(
                INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: data,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                },
                prev_desk,
                last_attach,
            );
        }
        InputEvent::MouseWheel { dx, dy } => {
            if dy != 0 {
                send_single(
                    INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dx: 0,
                                dy: 0,
                                mouseData: (dy as i32 * WHEEL_DELTA as i32) as u32,
                                dwFlags: MOUSEEVENTF_WHEEL,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    },
                    prev_desk,
                    last_attach,
                );
            }
            if dx != 0 {
                send_single(
                    INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dx: 0,
                                dy: 0,
                                mouseData: (dx as i32 * WHEEL_DELTA as i32) as u32,
                                dwFlags: MOUSEEVENTF_HWHEEL,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    },
                    prev_desk,
                    last_attach,
                );
            }
        }
        InputEvent::GamepadState { .. } => {
            if !WARNED_GAMEPAD.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    "received gamepad input on Windows host (virtual Xbox controller on Windows requires ViGEmBus; keyboard & mouse work natively)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_evdev_to_win32_ps2_scancodes() {
        assert_eq!(evdev_to_win32_scancode(1), Some((1, false))); // Esc
        assert_eq!(evdev_to_win32_scancode(30), Some((30, false))); // A
        assert_eq!(evdev_to_win32_scancode(28), Some((28, false))); // Enter
        assert_eq!(evdev_to_win32_scancode(88), Some((88, false))); // F12
        assert_eq!(evdev_to_win32_scancode(96), Some((0x1C, true))); // NumpadEnter
        assert_eq!(evdev_to_win32_scancode(97), Some((0x1D, true))); // RightCtrl
        assert_eq!(evdev_to_win32_scancode(103), Some((0x48, true))); // ArrowUp
        assert_eq!(evdev_to_win32_scancode(125), Some((0x5B, true))); // LeftMeta
    }
}
