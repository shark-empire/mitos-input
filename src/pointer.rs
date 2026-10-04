//! Pointing devices: [`Mouse`], [`Button`], [`PointerEvent`], motion
//! acceleration, and scroll handling.

use crate::bitflags_type;
use crate::device::DeviceId;
use crate::events::{Event, Timestamp};
use crate::input::InputDevice;

/// A pointer button. `Other` carries the raw evdev `BTN_*` code for buttons
/// without a named variant (extra side buttons, etc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Right,
    Middle,
    Side,
    Extra,
    Forward,
    Back,
    Other(u16),
}

impl Button {
    /// Map a raw evdev `BTN_*` code (from `EV_KEY` events on a pointer
    /// device) to a [`Button`].
    pub fn from_evdev_code(code: u16) -> Button {
        match code {
            0x110 => Button::Left,
            0x111 => Button::Right,
            0x112 => Button::Middle,
            0x113 => Button::Side,
            0x114 => Button::Extra,
            0x115 => Button::Forward,
            0x116 => Button::Back,
            other => Button::Other(other),
        }
    }
}

bitflags_type! {
    /// Which buttons are currently held down.
    pub struct ButtonState: u8 {
        const LEFT    = 0b0000_0001;
        const RIGHT   = 0b0000_0010;
        const MIDDLE  = 0b0000_0100;
        const SIDE    = 0b0000_1000;
        const EXTRA   = 0b0001_0000;
        const FORWARD = 0b0010_0000;
        const BACK    = 0b0100_0000;
    }
}

impl ButtonState {
    fn bit_for(button: Button) -> Option<ButtonState> {
        match button {
            Button::Left => Some(ButtonState::LEFT),
            Button::Right => Some(ButtonState::RIGHT),
            Button::Middle => Some(ButtonState::MIDDLE),
            Button::Side => Some(ButtonState::SIDE),
            Button::Extra => Some(ButtonState::EXTRA),
            Button::Forward => Some(ButtonState::FORWARD),
            Button::Back => Some(ButtonState::BACK),
            Button::Other(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSource {
    /// A discrete wheel click.
    Wheel,
    /// A continuous touchpad/trackpoint finger scroll.
    Finger,
    /// Free-running/kinetic scroll (post-flick momentum).
    Continuous,
}

/// A pointer event, in the coordinate/units convention of its source: deltas
/// for relative motion, normalized `0.0..=1.0` for absolute motion.
#[derive(Debug, Clone)]
pub enum PointerEvent {
    /// Relative motion (typical mouse/trackball).
    Motion {
        device: DeviceId,
        time: Timestamp,
        dx: f64,
        dy: f64,
    },
    /// Absolute motion (tablets-as-pointer, some touchscreens acting as a
    /// pointer, VNC/remote-desktop input).
    MotionAbsolute {
        device: DeviceId,
        time: Timestamp,
        x: f64,
        y: f64,
    },
    Button {
        device: DeviceId,
        time: Timestamp,
        button: Button,
        pressed: bool,
    },
    Scroll {
        device: DeviceId,
        time: Timestamp,
        axis: ScrollAxis,
        /// Positive is down/right, matching evdev's `REL_WHEEL`/`REL_HWHEEL`
        /// sign convention.
        value: f64,
        source: ScrollSource,
    },
}

/// Simple pointer acceleration curves.
#[derive(Debug, Clone, Copy)]
pub enum AccelProfile {
    /// Constant multiplier.
    Flat(f64),
    /// Multiplier grows with instantaneous speed, capped at `1 + max_boost`.
    Adaptive { sensitivity: f64, max_boost: f64 },
}

impl Default for AccelProfile {
    fn default() -> Self {
        AccelProfile::Flat(1.0)
    }
}

/// Stateful representation of a relative or absolute pointing device.
pub struct Mouse {
    device: DeviceId,
    /// Accumulated virtual position (useful even for relative devices, e.g.
    /// to clamp against a virtual screen size upstream).
    position: (f64, f64),
    buttons: ButtonState,
    accel: AccelProfile,
    /// Pending relative deltas accumulated between `EV_SYN` reports (evdev
    /// may deliver `REL_X`/`REL_Y` as separate events before the `SYN_REPORT`
    /// that terminates a single hardware sample).
    pending_dx: f64,
    pending_dy: f64,
}

impl Mouse {
    pub fn new(device: DeviceId) -> Self {
        Mouse {
            device,
            position: (0.0, 0.0),
            buttons: ButtonState::empty(),
            accel: AccelProfile::default(),
            pending_dx: 0.0,
            pending_dy: 0.0,
        }
    }

    pub fn device_id(&self) -> DeviceId {
        self.device
    }

    pub fn set_accel_profile(&mut self, profile: AccelProfile) {
        self.accel = profile;
    }

    pub fn position(&self) -> (f64, f64) {
        self.position
    }

    pub fn buttons(&self) -> ButtonState {
        self.buttons
    }

    pub fn is_pressed(&self, button: Button) -> bool {
        match ButtonState::bit_for(button) {
            Some(bit) => self.buttons.contains(bit),
            None => false,
        }
    }

    fn apply_accel(&self, dx: f64, dy: f64) -> (f64, f64) {
        match self.accel {
            AccelProfile::Flat(scale) => (dx * scale, dy * scale),
            AccelProfile::Adaptive {
                sensitivity,
                max_boost,
            } => {
                let speed = (dx * dx + dy * dy).sqrt();
                let factor = sensitivity * (1.0 + (speed / 10.0).min(max_boost));
                (dx * factor, dy * factor)
            }
        }
    }

    /// Accumulate a raw relative delta (call once per `REL_X`/`REL_Y`).
    pub fn accumulate_motion(&mut self, dx: f64, dy: f64) {
        self.pending_dx += dx;
        self.pending_dy += dy;
    }

    /// Flush accumulated relative motion into a [`PointerEvent::Motion`] at
    /// `SYN_REPORT`. Returns `None` if nothing moved since the last flush.
    pub fn flush_motion(&mut self, time: Timestamp) -> Option<PointerEvent> {
        if self.pending_dx == 0.0 && self.pending_dy == 0.0 {
            return None;
        }
        let (dx, dy) = self.apply_accel(self.pending_dx, self.pending_dy);
        self.position.0 += dx;
        self.position.1 += dy;
        self.pending_dx = 0.0;
        self.pending_dy = 0.0;
        Some(PointerEvent::Motion {
            device: self.device,
            time,
            dx,
            dy,
        })
    }

    pub fn set_absolute_position(&mut self, x: f64, y: f64, time: Timestamp) -> PointerEvent {
        self.position = (x, y);
        PointerEvent::MotionAbsolute {
            device: self.device,
            time,
            x,
            y,
        }
    }

    pub fn set_button(&mut self, button: Button, pressed: bool, time: Timestamp) -> PointerEvent {
        if let Some(bit) = ButtonState::bit_for(button) {
            if pressed {
                self.buttons.insert(bit);
            } else {
                self.buttons.remove(bit);
            }
        }
        PointerEvent::Button {
            device: self.device,
            time,
            button,
            pressed,
        }
    }

    pub fn scroll(
        &mut self,
        axis: ScrollAxis,
        value: f64,
        source: ScrollSource,
        time: Timestamp,
    ) -> PointerEvent {
        PointerEvent::Scroll {
            device: self.device,
            time,
            axis,
            value,
            source,
        }
    }
}

impl InputDevice for Mouse {
    fn id(&self) -> DeviceId {
        self.device
    }

    fn handle_event(&mut self, ev_type: u16, code: u16, value: i32, time: Timestamp) -> Vec<Event> {
        use crate::evdev::{
            EV_KEY, EV_REL, EV_SYN, REL_HWHEEL, REL_HWHEEL_HI_RES, REL_WHEEL, REL_WHEEL_HI_RES,
            REL_X, REL_Y, SYN_REPORT,
        };
        match ev_type {
            EV_REL => match code {
                REL_X => {
                    self.accumulate_motion(value as f64, 0.0);
                    Vec::new()
                }
                REL_Y => {
                    self.accumulate_motion(0.0, value as f64);
                    Vec::new()
                }
                REL_WHEEL => vec![Event::Pointer(self.scroll(
                    ScrollAxis::Vertical,
                    -(value as f64),
                    ScrollSource::Wheel,
                    time,
                ))],
                REL_HWHEEL => vec![Event::Pointer(self.scroll(
                    ScrollAxis::Horizontal,
                    value as f64,
                    ScrollSource::Wheel,
                    time,
                ))],
                REL_WHEEL_HI_RES => vec![Event::Pointer(self.scroll(
                    ScrollAxis::Vertical,
                    -(value as f64) / 120.0,
                    ScrollSource::Continuous,
                    time,
                ))],
                REL_HWHEEL_HI_RES => vec![Event::Pointer(self.scroll(
                    ScrollAxis::Horizontal,
                    value as f64 / 120.0,
                    ScrollSource::Continuous,
                    time,
                ))],
                _ => Vec::new(),
            },
            EV_KEY => {
                let button = Button::from_evdev_code(code);
                vec![Event::Pointer(self.set_button(button, value != 0, time))]
            }
            EV_SYN if code == SYN_REPORT => match self.flush_motion(time) {
                Some(ev) => vec![Event::Pointer(ev)],
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_and_flushes_relative_motion() {
        let mut mouse = Mouse::new(DeviceId::new());
        let t = Timestamp::from_secs(0);
        assert!(mouse.flush_motion(t).is_none());
        mouse.accumulate_motion(3.0, -2.0);
        match mouse.flush_motion(t) {
            Some(PointerEvent::Motion { dx, dy, .. }) => {
                assert_eq!(dx, 3.0);
                assert_eq!(dy, -2.0);
            }
            other => panic!("expected Motion, got {other:?}"),
        }
        assert_eq!(mouse.position(), (3.0, -2.0));
    }

    #[test]
    fn adaptive_accel_scales_with_speed() {
        let mut mouse = Mouse::new(DeviceId::new());
        mouse.set_accel_profile(AccelProfile::Adaptive {
            sensitivity: 1.0,
            max_boost: 1.0,
        });
        let t = Timestamp::from_secs(0);
        mouse.accumulate_motion(20.0, 0.0);
        match mouse.flush_motion(t) {
            Some(PointerEvent::Motion { dx, .. }) => assert!(dx > 20.0, "expected boosted dx, got {dx}"),
            other => panic!("expected Motion, got {other:?}"),
        }
    }

    #[test]
    fn tracks_button_state() {
        let mut mouse = Mouse::new(DeviceId::new());
        let t = Timestamp::from_secs(0);
        mouse.set_button(Button::Left, true, t);
        assert!(mouse.is_pressed(Button::Left));
        assert!(!mouse.is_pressed(Button::Right));
        mouse.set_button(Button::Left, false, t);
        assert!(!mouse.is_pressed(Button::Left));
    }
}
