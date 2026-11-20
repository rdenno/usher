use anyhow::{Result, bail};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    Side = 3,
    Extra = 4,
}

impl MouseButton {
    pub fn from_u8(val: u8) -> Result<Self> {
        match val {
            0 => Ok(Self::Left),
            1 => Ok(Self::Right),
            2 => Ok(Self::Middle),
            3 => Ok(Self::Side),
            4 => Ok(Self::Extra),
            other => bail!("unknown mouse button {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    Key {
        evdev_code: u16,
        pressed: bool,
    },
    MouseMoveRel {
        dx: i32,
        dy: i32,
    },
    /// Normalized screen coordinates `0..=65535`.
    MouseMoveAbs {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// Whole mouse wheel detents (`+dy` = scroll up, `+dx` = scroll right).
    MouseWheel {
        dx: i16,
        dy: i16,
    },
    /// High-resolution mouse wheel deltas in `1/120`th detent units
    /// (`120` = one standard detent; `+dy` = scroll up, `+dx` = scroll right).
    MouseWheelHiRes {
        dx: i16,
        dy: i16,
    },
    GamepadState {
        /// Bitmask matching standard XInput layout:
        /// 0: DPadUp, 1: DPadDown, 2: DPadLeft, 3: DPadRight,
        /// 4: Start, 5: Back/Select, 6: LeftThumb, 7: RightThumb,
        /// 8: LB, 9: RB, 10: Guide, 12: A/South, 13: B/East, 14: X/West, 15: Y/North
        buttons: u16,
        lt: u8,
        rt: u8,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
    },
    /// Idempotently releases all remotely-held keys, mouse buttons, and gamepad state.
    ReleaseAll,
}

impl InputEvent {
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            InputEvent::Key {
                evdev_code,
                pressed,
            } => {
                let mut out = Vec::with_capacity(4);
                out.push(0x01);
                out.extend_from_slice(&evdev_code.to_le_bytes());
                out.push(u8::from(pressed));
                out
            }
            InputEvent::MouseMoveRel { dx, dy } => {
                let mut out = Vec::with_capacity(9);
                out.push(0x02);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
                out
            }
            InputEvent::MouseMoveAbs { x, y } => {
                let mut out = Vec::with_capacity(5);
                out.push(0x03);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                out
            }
            InputEvent::MouseButton { button, pressed } => {
                vec![0x04, button as u8, u8::from(pressed)]
            }
            InputEvent::MouseWheel { dx, dy } => {
                let mut out = Vec::with_capacity(5);
                out.push(0x05);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
                out
            }
            InputEvent::MouseWheelHiRes { dx, dy } => {
                let mut out = Vec::with_capacity(5);
                out.push(0x07);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
                out
            }
            InputEvent::GamepadState {
                buttons,
                lt,
                rt,
                lx,
                ly,
                rx,
                ry,
            } => {
                let mut out = Vec::with_capacity(13);
                out.push(0x06);
                out.extend_from_slice(&buttons.to_le_bytes());
                out.push(lt);
                out.push(rt);
                out.extend_from_slice(&lx.to_le_bytes());
                out.extend_from_slice(&ly.to_le_bytes());
                out.extend_from_slice(&rx.to_le_bytes());
                out.extend_from_slice(&ry.to_le_bytes());
                out
            }
            InputEvent::ReleaseAll => vec![0x08],
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (&tag, rest) = bytes
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("empty input packet"))?;
        match tag {
            0x01 => {
                if rest.len() != 3 {
                    bail!("invalid Key event len {}", rest.len());
                }
                Ok(Self::Key {
                    evdev_code: u16::from_le_bytes([rest[0], rest[1]]),
                    pressed: rest[2] != 0,
                })
            }
            0x02 => {
                if rest.len() != 8 {
                    bail!("invalid MouseMoveRel len {}", rest.len());
                }
                Ok(Self::MouseMoveRel {
                    dx: i32::from_le_bytes(rest[0..4].try_into().expect("4 bytes")),
                    dy: i32::from_le_bytes(rest[4..8].try_into().expect("4 bytes")),
                })
            }
            0x03 => {
                if rest.len() != 4 {
                    bail!("invalid MouseMoveAbs len {}", rest.len());
                }
                Ok(Self::MouseMoveAbs {
                    x: u16::from_le_bytes([rest[0], rest[1]]),
                    y: u16::from_le_bytes([rest[2], rest[3]]),
                })
            }
            0x04 => {
                if rest.len() != 2 {
                    bail!("invalid MouseButton len {}", rest.len());
                }
                Ok(Self::MouseButton {
                    button: MouseButton::from_u8(rest[0])?,
                    pressed: rest[1] != 0,
                })
            }
            0x05 => {
                if rest.len() != 4 {
                    bail!("invalid MouseWheel len {}", rest.len());
                }
                Ok(Self::MouseWheel {
                    dx: i16::from_le_bytes([rest[0], rest[1]]),
                    dy: i16::from_le_bytes([rest[2], rest[3]]),
                })
            }
            0x06 => {
                if rest.len() != 12 {
                    bail!("invalid GamepadState len {}", rest.len());
                }
                Ok(Self::GamepadState {
                    buttons: u16::from_le_bytes([rest[0], rest[1]]),
                    lt: rest[2],
                    rt: rest[3],
                    lx: i16::from_le_bytes([rest[4], rest[5]]),
                    ly: i16::from_le_bytes([rest[6], rest[7]]),
                    rx: i16::from_le_bytes([rest[8], rest[9]]),
                    ry: i16::from_le_bytes([rest[10], rest[11]]),
                })
            }
            0x07 => {
                if rest.len() != 4 {
                    bail!("invalid MouseWheelHiRes len {}", rest.len());
                }
                Ok(Self::MouseWheelHiRes {
                    dx: i16::from_le_bytes([rest[0], rest[1]]),
                    dy: i16::from_le_bytes([rest[2], rest[3]]),
                })
            }
            0x08 => {
                if !rest.is_empty() {
                    bail!("invalid ReleaseAll len {}", rest.len());
                }
                Ok(Self::ReleaseAll)
            }
            other => bail!("unknown input event tag {other}"),
        }
    }
}

/// Returns the order in which held keys should be released during input cleanup:
/// `0` = non-modifier keys (released first), `1` = Alt/Meta (released while Ctrl/Shift
/// are still down to avoid menu/Start activation), `2` = Shift/Ctrl (released last).
pub fn key_release_priority(evdev_code: u16) -> u8 {
    match evdev_code {
        42 | 54 | 29 | 97 => 2,
        56 | 100 | 125 | 126 => 1,
        _ => 0,
    }
}

#[derive(Debug, Default)]
pub struct HeldInputTracker {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<MouseButton>,
    gamepad_active: bool,
}

impl HeldInputTracker {
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty() && !self.gamepad_active
    }

    pub fn should_forward_key(&mut self, evdev_code: u16, pressed: bool) -> bool {
        if pressed {
            self.keys.insert(evdev_code)
        } else {
            self.keys.remove(&evdev_code)
        }
    }

    pub fn should_forward_button(&mut self, button: MouseButton, pressed: bool) -> bool {
        if pressed {
            self.buttons.insert(button)
        } else {
            self.buttons.remove(&button)
        }
    }

    pub fn record(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Key {
                evdev_code,
                pressed,
            } => {
                if *pressed {
                    self.keys.insert(*evdev_code);
                } else {
                    self.keys.remove(evdev_code);
                }
            }
            InputEvent::MouseButton { button, pressed } => {
                if *pressed {
                    self.buttons.insert(*button);
                } else {
                    self.buttons.remove(button);
                }
            }
            InputEvent::GamepadState {
                buttons,
                lt,
                rt,
                lx,
                ly,
                rx,
                ry,
            } => {
                self.gamepad_active = *buttons != 0
                    || *lt != 0
                    || *rt != 0
                    || *lx != 0
                    || *ly != 0
                    || *rx != 0
                    || *ry != 0;
            }
            InputEvent::ReleaseAll => {
                self.keys.clear();
                self.buttons.clear();
                self.gamepad_active = false;
            }
            InputEvent::MouseMoveRel { .. }
            | InputEvent::MouseMoveAbs { .. }
            | InputEvent::MouseWheel { .. }
            | InputEvent::MouseWheelHiRes { .. } => {}
        }
    }

    pub fn drain_release_events(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::with_capacity(
            self.buttons.len() + self.keys.len() + usize::from(self.gamepad_active),
        );
        let buttons = std::mem::take(&mut self.buttons);
        for button in buttons {
            out.push(InputEvent::MouseButton {
                button,
                pressed: false,
            });
        }
        let mut keys: Vec<u16> = std::mem::take(&mut self.keys).into_iter().collect();
        keys.sort_by_key(|&code| (key_release_priority(code), code));
        for evdev_code in keys {
            out.push(InputEvent::Key {
                evdev_code,
                pressed: false,
            });
        }
        if self.gamepad_active {
            self.gamepad_active = false;
            out.push(InputEvent::GamepadState {
                buttons: 0,
                lt: 0,
                rt: 0,
                lx: 0,
                ly: 0,
                rx: 0,
                ry: 0,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_input_events() {
        let events = [
            InputEvent::Key {
                evdev_code: 30,
                pressed: true,
            },
            InputEvent::MouseMoveRel { dx: -14, dy: 28 },
            InputEvent::MouseMoveAbs {
                x: 32768,
                y: 65535,
            },
            InputEvent::MouseButton {
                button: MouseButton::Right,
                pressed: false,
            },
            InputEvent::MouseWheel { dx: 0, dy: -120 },
            InputEvent::MouseWheelHiRes { dx: -15, dy: 24 },
            InputEvent::GamepadState {
                buttons: 0b1010_0000_0001_0001,
                lt: 200,
                rt: 255,
                lx: -12345,
                ly: 23456,
                rx: 100,
                ry: -32768,
            },
            InputEvent::ReleaseAll,
        ];
        for ev in events {
            let enc = ev.encode();
            let dec = InputEvent::decode(&enc).unwrap();
            assert_eq!(dec, ev);
        }
    }

    #[test]
    fn held_tracker_drains_modifiers_in_safe_order_and_suppresses_stale_ups() {
        let mut tracker = HeldInputTracker::default();
        assert!(tracker.is_empty());

        // Suppress unmatched release before press and duplicate press.
        assert!(!tracker.should_forward_key(29, false));
        assert!(tracker.should_forward_key(29, true)); // CtrlLeft
        assert!(!tracker.should_forward_key(29, true));
        assert!(tracker.should_forward_key(56, true)); // AltLeft
        assert!(tracker.should_forward_key(42, true)); // ShiftLeft
        assert!(tracker.should_forward_key(125, true)); // SuperLeft
        assert!(tracker.should_forward_key(30, true)); // KeyA
        assert!(tracker.should_forward_key(16, true)); // KeyQ
        assert!(tracker.should_forward_button(MouseButton::Left, true));
        assert!(!tracker.should_forward_button(MouseButton::Left, true));
        assert!(!tracker.should_forward_button(MouseButton::Right, false));

        tracker.record(&InputEvent::GamepadState {
            buttons: 0x1000,
            lt: 0,
            rt: 0,
            lx: 1000,
            ly: 0,
            rx: 0,
            ry: 0,
        });
        assert!(!tracker.is_empty());

        let drained = tracker.drain_release_events();
        assert_eq!(
            drained,
            vec![
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: false,
                },
                InputEvent::Key {
                    evdev_code: 16,
                    pressed: false,
                },
                InputEvent::Key {
                    evdev_code: 30,
                    pressed: false,
                },
                InputEvent::Key {
                    evdev_code: 56,
                    pressed: false,
                },
                InputEvent::Key {
                    evdev_code: 125,
                    pressed: false,
                },
                InputEvent::Key {
                    evdev_code: 29,
                    pressed: false,
                },
                InputEvent::Key {
                    evdev_code: 42,
                    pressed: false,
                },
                InputEvent::GamepadState {
                    buttons: 0,
                    lt: 0,
                    rt: 0,
                    lx: 0,
                    ly: 0,
                    rx: 0,
                    ry: 0,
                },
            ]
        );
        assert!(tracker.is_empty());
        assert!(tracker.drain_release_events().is_empty());

        // Subsequent physical key-ups/button-ups after ReleaseAll must be suppressed.
        assert!(!tracker.should_forward_key(16, false));
        assert!(!tracker.should_forward_key(56, false));
        assert!(!tracker.should_forward_key(29, false));
        assert!(!tracker.should_forward_button(MouseButton::Left, false));
    }
}
