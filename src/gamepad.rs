//! Game controllers: [`Gamepad`], buttons, axes, triggers, and the raw
//! evdev code mapping (`BTN_SOUTH`/`ABS_X`/etc, per the standard Linux
//! joystick-as-evdev convention used by `xpad` and most modern drivers).

use std::collections::HashMap;

use crate::device::DeviceId;
use crate::events::{Event, GamepadEvent, Timestamp};
use crate::input::InputDevice;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GamepadButton {
    /// "A" (Xbox) / Cross (PlayStation).
    South,
    /// "B" / Circle.
    East,
    /// "X" / Square.
    North,
    /// "Y" / Triangle.
    West,
    LeftBumper,
    RightBumper,
    /// Digital click of the left trigger, on pads that have one.
    LeftTrigger,
    RightTrigger,
    Select,
    Start,
    Guide,
    LeftStick,
    RightStick,
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GamepadAxis {
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
    LeftTrigger,
    RightTrigger,
}

/// Maps raw evdev `BTN_*`/`ABS_*` codes to [`GamepadButton`]/[`GamepadAxis`],
/// plus each axis's raw range (needed to normalize to `-1.0..=1.0` or, for
/// triggers, `0.0..=1.0`).
pub struct GamepadMapping {
    buttons: HashMap<u16, GamepadButton>,
    axes: HashMap<u16, GamepadAxis>,
    axis_range: HashMap<u16, (i32, i32)>,
    hat_x_code: Option<u16>,
    hat_y_code: Option<u16>,
}

impl GamepadMapping {
    /// The common Linux `xpad`-style mapping used by most Xbox-layout
    /// controllers (evdev `BTN_SOUTH`/`ABS_X` etc, not USB HID usage ids).
    pub fn standard_xbox() -> Self {
        let mut buttons = HashMap::new();
        buttons.insert(0x130, GamepadButton::South); // BTN_SOUTH / BTN_A
        buttons.insert(0x131, GamepadButton::East); // BTN_EAST / BTN_B
        buttons.insert(0x133, GamepadButton::North); // BTN_NORTH / BTN_X
        buttons.insert(0x134, GamepadButton::West); // BTN_WEST / BTN_Y
        buttons.insert(0x136, GamepadButton::LeftBumper); // BTN_TL
        buttons.insert(0x137, GamepadButton::RightBumper); // BTN_TR
        buttons.insert(0x138, GamepadButton::LeftTrigger); // BTN_TL2
        buttons.insert(0x139, GamepadButton::RightTrigger); // BTN_TR2
        buttons.insert(0x13a, GamepadButton::Select); // BTN_SELECT
        buttons.insert(0x13b, GamepadButton::Start); // BTN_START
        buttons.insert(0x13c, GamepadButton::Guide); // BTN_MODE
        buttons.insert(0x13d, GamepadButton::LeftStick); // BTN_THUMBL
        buttons.insert(0x13e, GamepadButton::RightStick); // BTN_THUMBR

        let mut axes = HashMap::new();
        let mut axis_range = HashMap::new();
        axes.insert(0x00, GamepadAxis::LeftStickX); // ABS_X
        axis_range.insert(0x00, (-32768, 32767));
        axes.insert(0x01, GamepadAxis::LeftStickY); // ABS_Y
        axis_range.insert(0x01, (-32768, 32767));
        axes.insert(0x03, GamepadAxis::RightStickX); // ABS_RX
        axis_range.insert(0x03, (-32768, 32767));
        axes.insert(0x04, GamepadAxis::RightStickY); // ABS_RY
        axis_range.insert(0x04, (-32768, 32767));
        axes.insert(0x02, GamepadAxis::LeftTrigger); // ABS_Z
        axis_range.insert(0x02, (0, 255));
        axes.insert(0x05, GamepadAxis::RightTrigger); // ABS_RZ
        axis_range.insert(0x05, (0, 255));

        GamepadMapping {
            buttons,
            axes,
            axis_range,
            hat_x_code: Some(0x10), // ABS_HAT0X
            hat_y_code: Some(0x11), // ABS_HAT0Y
        }
    }
}

impl Default for GamepadMapping {
    fn default() -> Self {
        Self::standard_xbox()
    }
}

/// Stateful representation of a game controller: button/axis values plus
/// deadzone handling and D-pad-as-hat-axis translation.
pub struct Gamepad {
    device: DeviceId,
    mapping: GamepadMapping,
    buttons: HashMap<GamepadButton, bool>,
    axes: HashMap<GamepadAxis, f32>,
    deadzone: f32,
}

impl Gamepad {
    pub fn new(device: DeviceId, mapping: GamepadMapping) -> Self {
        Gamepad {
            device,
            mapping,
            buttons: HashMap::new(),
            axes: HashMap::new(),
            deadzone: 0.12,
        }
    }

    pub fn with_deadzone(mut self, deadzone: f32) -> Self {
        self.deadzone = deadzone.clamp(0.0, 0.9);
        self
    }

    pub fn device_id(&self) -> DeviceId {
        self.device
    }

    pub fn is_pressed(&self, button: GamepadButton) -> bool {
        *self.buttons.get(&button).unwrap_or(&false)
    }

    pub fn axis_value(&self, axis: GamepadAxis) -> f32 {
        *self.axes.get(&axis).unwrap_or(&0.0)
    }

    /// Handle a raw `EV_KEY` button code. Returns `None` if unmapped or
    /// unchanged.
    pub fn set_button_raw(&mut self, code: u16, pressed: bool, time: Timestamp) -> Option<GamepadEvent> {
        let button = *self.mapping.buttons.get(&code)?;
        self.set_button(button, pressed, time)
    }

    pub fn set_button(
        &mut self,
        button: GamepadButton,
        pressed: bool,
        time: Timestamp,
    ) -> Option<GamepadEvent> {
        // A button absent from the map has never been reported, which is
        // equivalent to "released" -- so setting it to `false` for the first
        // time is not a change worth an event.
        let previous = self.buttons.insert(button, pressed).unwrap_or(false);
        if previous == pressed {
            return None;
        }
        Some(GamepadEvent::Button {
            device: self.device,
            time,
            button,
            pressed,
        })
    }

    /// Handle a raw `EV_ABS` axis code, including the hat-switch D-pad,
    /// which is reported as an axis but surfaced here as D-pad buttons.
    /// Returns every event produced (a hat axis can imply up to two button
    /// transitions: releasing the previous direction, pressing the new one).
    pub fn set_axis_raw(&mut self, code: u16, raw_value: i32, time: Timestamp) -> Vec<GamepadEvent> {
        if Some(code) == self.mapping.hat_x_code {
            return self.set_hat(raw_value, GamepadButton::DPadLeft, GamepadButton::DPadRight, time);
        }
        if Some(code) == self.mapping.hat_y_code {
            return self.set_hat(raw_value, GamepadButton::DPadUp, GamepadButton::DPadDown, time);
        }
        let Some(axis) = self.mapping.axes.get(&code).copied() else {
            return Vec::new();
        };
        let (min, max) = *self.mapping.axis_range.get(&code).unwrap_or(&(-32768, 32767));
        let normalized = normalize_axis(raw_value, min, max, axis);
        let deadzoned = apply_deadzone(normalized, self.deadzone);
        let prev = self.axes.insert(axis, deadzoned);
        if prev == Some(deadzoned) {
            return Vec::new();
        }
        vec![GamepadEvent::Axis {
            device: self.device,
            time,
            axis,
            value: deadzoned,
        }]
    }

    fn set_hat(
        &mut self,
        raw_value: i32,
        negative: GamepadButton,
        positive: GamepadButton,
        time: Timestamp,
    ) -> Vec<GamepadEvent> {
        let mut events = Vec::new();
        let want_negative = raw_value < 0;
        let want_positive = raw_value > 0;
        if let Some(ev) = self.set_button(negative, want_negative, time) {
            events.push(ev);
        }
        if let Some(ev) = self.set_button(positive, want_positive, time) {
            events.push(ev);
        }
        events
    }
}

/// Normalize a raw axis value to `-1.0..=1.0` for sticks, or `0.0..=1.0` for
/// triggers (which only move in one direction from `min`).
fn normalize_axis(raw: i32, min: i32, max: i32, axis: GamepadAxis) -> f32 {
    let span = (max - min).max(1) as f32;
    match axis {
        GamepadAxis::LeftTrigger | GamepadAxis::RightTrigger => {
            ((raw - min) as f32 / span).clamp(0.0, 1.0)
        }
        _ => {
            let mid = (max as f32 + min as f32) / 2.0;
            let half_span = span / 2.0;
            ((raw as f32 - mid) / half_span).clamp(-1.0, 1.0)
        }
    }
}

fn apply_deadzone(value: f32, deadzone: f32) -> f32 {
    if value.abs() < deadzone {
        0.0
    } else {
        // Rescale so the value still reaches +/-1.0 at the edge of travel,
        // rather than jumping straight from 0 to `deadzone`.
        let sign = value.signum();
        sign * (value.abs() - deadzone) / (1.0 - deadzone)
    }
}

impl InputDevice for Gamepad {
    fn id(&self) -> DeviceId {
        self.device
    }

    fn handle_event(&mut self, ev_type: u16, code: u16, value: i32, time: Timestamp) -> Vec<Event> {
        use crate::evdev::{EV_ABS, EV_KEY};
        match ev_type {
            EV_KEY => self
                .set_button_raw(code, value != 0, time)
                .into_iter()
                .map(Event::Gamepad)
                .collect(),
            EV_ABS => self
                .set_axis_raw(code, value, time)
                .into_iter()
                .map(Event::Gamepad)
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_standard_buttons() {
        let mut pad = Gamepad::new(DeviceId::new(), GamepadMapping::standard_xbox());
        let t = Timestamp::from_secs(0);
        let ev = pad.set_button_raw(0x130, true, t).unwrap();
        assert!(matches!(
            ev,
            GamepadEvent::Button {
                button: GamepadButton::South,
                pressed: true,
                ..
            }
        ));
        assert!(pad.is_pressed(GamepadButton::South));
    }

    #[test]
    fn deadzone_and_axis_normalization() {
        let mut pad = Gamepad::new(DeviceId::new(), GamepadMapping::standard_xbox()).with_deadzone(0.2);
        let t = Timestamp::from_secs(0);
        // Small stick movement inside the deadzone should normalize to 0.
        let small = pad.set_axis_raw(0x00, 1000, t); // ABS_X, near center
        assert!(small.is_empty() || matches!(small[0], GamepadEvent::Axis { value, .. } if value == 0.0));

        // Full deflection should approach +/-1.0.
        pad.set_axis_raw(0x00, -32768, t);
        assert!((pad.axis_value(GamepadAxis::LeftStickX) - (-1.0)).abs() < 0.01);

        // Trigger at rest is 0.0, fully pulled is 1.0.
        pad.set_axis_raw(0x02, 255, t); // ABS_Z
        assert!((pad.axis_value(GamepadAxis::LeftTrigger) - 1.0).abs() < 0.01);
    }

    #[test]
    fn hat_switch_maps_to_dpad_buttons() {
        let mut pad = Gamepad::new(DeviceId::new(), GamepadMapping::standard_xbox());
        let t = Timestamp::from_secs(0);
        let events = pad.set_axis_raw(0x10, -1, t); // ABS_HAT0X left
        assert!(pad.is_pressed(GamepadButton::DPadLeft));
        assert!(!pad.is_pressed(GamepadButton::DPadRight));
        assert_eq!(events.len(), 1);

        let events = pad.set_axis_raw(0x10, 0, t); // release
        assert!(!pad.is_pressed(GamepadButton::DPadLeft));
        assert_eq!(events.len(), 1);
    }
}
