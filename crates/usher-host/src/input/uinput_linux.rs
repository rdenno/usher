use super::InputSink;
use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent as KernelEvent,
    InputId, KeyCode, PropType, RelativeAxisCode, UinputAbsSetup,
};
use usher_proto::input::{InputEvent, MouseButton};

pub struct UinputSink {
    keyboard: VirtualDevice,
    abs_mouse: VirtualDevice,
    rel_mouse: VirtualDevice,
    gamepad: VirtualDevice,
    last_gamepad_buttons: u16,
    wheel_accum_x: i32,
    wheel_accum_y: i32,
}

impl UinputSink {
    pub fn new() -> Result<Self> {
        let keyboard = build_keyboard().context("creating virtual keyboard")?;
        let abs_mouse = build_abs_mouse().context("creating virtual absolute pointer")?;
        let rel_mouse = build_rel_mouse().context("creating virtual relative mouse")?;
        let gamepad = build_gamepad().context("creating virtual Xbox 360 gamepad")?;
        Ok(Self {
            keyboard,
            abs_mouse,
            rel_mouse,
            gamepad,
            last_gamepad_buttons: 0,
            wheel_accum_x: 0,
            wheel_accum_y: 0,
        })
    }
}

fn build_keyboard() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in 1u16..=248 {
        keys.insert(KeyCode(code));
    }
    let dev = VirtualDevice::builder()?
        .name("Usher Virtual Keyboard")
        .input_id(InputId::new(BusType::BUS_USB, 0x1d6b, 0x0101, 1))
        .with_keys(&keys)?
        .build()?;
    Ok(dev)
}

fn build_abs_mouse() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::BTN_LEFT);
    keys.insert(KeyCode::BTN_RIGHT);
    keys.insert(KeyCode::BTN_MIDDLE);
    keys.insert(KeyCode::BTN_SIDE);
    keys.insert(KeyCode::BTN_EXTRA);

    let mut rels = AttributeSet::<RelativeAxisCode>::new();
    rels.insert(RelativeAxisCode::REL_WHEEL);
    rels.insert(RelativeAxisCode::REL_HWHEEL);
    rels.insert(RelativeAxisCode::REL_WHEEL_HI_RES);
    rels.insert(RelativeAxisCode::REL_HWHEEL_HI_RES);

    let mut props = AttributeSet::<PropType>::new();
    props.insert(PropType::DIRECT);

    let abs_x = UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, AbsInfo::new(0, 0, 65535, 0, 0, 0));
    let abs_y = UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, AbsInfo::new(0, 0, 65535, 0, 0, 0));

    let dev = VirtualDevice::builder()?
        .name("Usher Virtual Tablet")
        .input_id(InputId::new(BusType::BUS_USB, 0x1d6b, 0x0102, 1))
        .with_keys(&keys)?
        .with_relative_axes(&rels)?
        .with_properties(&props)?
        .with_absolute_axis(&abs_x)?
        .with_absolute_axis(&abs_y)?
        .build()?;
    Ok(dev)
}

fn build_rel_mouse() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::BTN_LEFT);
    keys.insert(KeyCode::BTN_RIGHT);
    keys.insert(KeyCode::BTN_MIDDLE);
    keys.insert(KeyCode::BTN_SIDE);
    keys.insert(KeyCode::BTN_EXTRA);

    let mut rels = AttributeSet::<RelativeAxisCode>::new();
    rels.insert(RelativeAxisCode::REL_X);
    rels.insert(RelativeAxisCode::REL_Y);
    rels.insert(RelativeAxisCode::REL_WHEEL);
    rels.insert(RelativeAxisCode::REL_HWHEEL);
    rels.insert(RelativeAxisCode::REL_WHEEL_HI_RES);
    rels.insert(RelativeAxisCode::REL_HWHEEL_HI_RES);

    let dev = VirtualDevice::builder()?
        .name("Usher Virtual Mouse")
        .input_id(InputId::new(BusType::BUS_USB, 0x1d6b, 0x0103, 1))
        .with_keys(&keys)?
        .with_relative_axes(&rels)?
        .build()?;
    Ok(dev)
}

fn build_gamepad() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for btn in [
        KeyCode::BTN_SOUTH,
        KeyCode::BTN_EAST,
        KeyCode::BTN_NORTH,
        KeyCode::BTN_WEST,
        KeyCode::BTN_TL,
        KeyCode::BTN_TR,
        KeyCode::BTN_SELECT,
        KeyCode::BTN_START,
        KeyCode::BTN_MODE,
        KeyCode::BTN_THUMBL,
        KeyCode::BTN_THUMBR,
    ] {
        keys.insert(btn);
    }

    let stick = AbsInfo::new(0, -32768, 32767, 16, 128, 0);
    let trigger = AbsInfo::new(0, 0, 255, 0, 0, 0);
    let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);

    let dev = VirtualDevice::builder()?
        .name("Microsoft X-Box 360 pad")
        .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0114))
        .with_keys(&keys)?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, stick))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, stick))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RX, stick))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RY, stick))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Z, trigger))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RZ, trigger))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_HAT0X, hat))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_HAT0Y, hat))?
        .build()?;
    Ok(dev)
}

fn mouse_button_code(btn: MouseButton) -> KeyCode {
    match btn {
        MouseButton::Left => KeyCode::BTN_LEFT,
        MouseButton::Right => KeyCode::BTN_RIGHT,
        MouseButton::Middle => KeyCode::BTN_MIDDLE,
        MouseButton::Side => KeyCode::BTN_SIDE,
        MouseButton::Extra => KeyCode::BTN_EXTRA,
    }
}

impl InputSink for UinputSink {
    fn handle(&mut self, event: InputEvent) {
        let res = match event {
            InputEvent::Key {
                evdev_code,
                pressed,
            } => self.keyboard.emit(&[KernelEvent::new(
                EventType::KEY.0,
                evdev_code,
                i32::from(pressed),
            )]),
            InputEvent::MouseMoveRel { dx, dy } => self.rel_mouse.emit(&[
                KernelEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, dx),
                KernelEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, dy),
            ]),
            InputEvent::MouseMoveAbs { x, y } => self.abs_mouse.emit(&[
                KernelEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, x as i32),
                KernelEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, y as i32),
            ]),
            InputEvent::MouseButton { button, pressed } => {
                let code = mouse_button_code(button).0;
                self.rel_mouse
                    .emit(&[KernelEvent::new(EventType::KEY.0, code, i32::from(pressed))])
            }
            InputEvent::MouseWheel { dx, dy } => {
                let mut batch = Vec::with_capacity(4);
                if dx != 0 {
                    let dx_i32 = dx as i32;
                    batch.push(KernelEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_HWHEEL.0,
                        dx_i32,
                    ));
                    batch.push(KernelEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_HWHEEL_HI_RES.0,
                        dx_i32.saturating_mul(120),
                    ));
                }
                if dy != 0 {
                    let dy_i32 = dy as i32;
                    batch.push(KernelEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_WHEEL.0,
                        dy_i32,
                    ));
                    batch.push(KernelEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_WHEEL_HI_RES.0,
                        dy_i32.saturating_mul(120),
                    ));
                }
                if batch.is_empty() {
                    Ok(())
                } else {
                    self.rel_mouse.emit(&batch)
                }
            }
            InputEvent::MouseWheelHiRes { dx, dy } => {
                let mut batch = Vec::with_capacity(4);
                if dx != 0 {
                    batch.push(KernelEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_HWHEEL_HI_RES.0,
                        dx as i32,
                    ));
                    let detents = accumulate_hi_res_detents(&mut self.wheel_accum_x, dx);
                    if detents != 0 {
                        batch.push(KernelEvent::new(
                            EventType::RELATIVE.0,
                            RelativeAxisCode::REL_HWHEEL.0,
                            detents,
                        ));
                    }
                }
                if dy != 0 {
                    batch.push(KernelEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_WHEEL_HI_RES.0,
                        dy as i32,
                    ));
                    let detents = accumulate_hi_res_detents(&mut self.wheel_accum_y, dy);
                    if detents != 0 {
                        batch.push(KernelEvent::new(
                            EventType::RELATIVE.0,
                            RelativeAxisCode::REL_WHEEL.0,
                            detents,
                        ));
                    }
                }
                if batch.is_empty() {
                    Ok(())
                } else {
                    self.rel_mouse.emit(&batch)
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
                let mut batch = Vec::with_capacity(19);
                let button_bits = [
                    (4u16, KeyCode::BTN_START),
                    (5, KeyCode::BTN_SELECT),
                    (6, KeyCode::BTN_THUMBL),
                    (7, KeyCode::BTN_THUMBR),
                    (8, KeyCode::BTN_TL),
                    (9, KeyCode::BTN_TR),
                    (10, KeyCode::BTN_MODE),
                    (12, KeyCode::BTN_SOUTH),
                    (13, KeyCode::BTN_EAST),
                    (14, KeyCode::BTN_WEST),
                    (15, KeyCode::BTN_NORTH),
                ];
                let changed = buttons ^ self.last_gamepad_buttons;
                for (bit, code) in button_bits {
                    let mask = 1u16 << bit;
                    if (changed & mask) != 0 {
                        batch.push(KernelEvent::new(
                            EventType::KEY.0,
                            code.0,
                            i32::from((buttons & mask) != 0),
                        ));
                    }
                }
                self.last_gamepad_buttons = buttons;

                let hat_x = i32::from((buttons & (1 << 3)) != 0) - i32::from((buttons & (1 << 2)) != 0);
                let hat_y = i32::from((buttons & (1 << 1)) != 0) - i32::from((buttons & (1 << 0)) != 0);

                batch.extend_from_slice(&[
                    KernelEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_HAT0X.0, hat_x),
                    KernelEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_HAT0Y.0, hat_y),
                    KernelEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Z.0, lt as i32),
                    KernelEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_RZ.0, rt as i32),
                    KernelEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, lx as i32),
                    KernelEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, ly as i32),
                    KernelEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_RX.0, rx as i32),
                    KernelEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_RY.0, ry as i32),
                ]);
                self.gamepad.emit(&batch)
            }
        };

        if let Err(err) = res {
            tracing::warn!("uinput emit error: {err}");
        }
    }
}

fn accumulate_hi_res_detents(accum: &mut i32, delta: i16) -> i32 {
    let delta_i32 = delta as i32;
    if delta_i32 != 0 && *accum != 0 && delta_i32.signum() != accum.signum() {
        *accum = 0;
    }
    *accum += delta_i32;
    let detents = *accum / 120;
    *accum %= 120;
    detents
}

#[cfg(test)]
mod tests {
    use super::accumulate_hi_res_detents;

    #[test]
    fn accumulates_hi_res_scroll_into_legacy_detents() {
        let mut acc = 0i32;
        assert_eq!(accumulate_hi_res_detents(&mut acc, -30), 0);
        assert_eq!(acc, -30);
        assert_eq!(accumulate_hi_res_detents(&mut acc, -60), 0);
        assert_eq!(acc, -90);
        assert_eq!(accumulate_hi_res_detents(&mut acc, -60), -1);
        assert_eq!(acc, -30);

        // Direction reversal clears stale opposite remainder cleanly.
        assert_eq!(accumulate_hi_res_detents(&mut acc, 120), 1);
        assert_eq!(acc, 0);
    }
}
