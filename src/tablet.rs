//! Graphics tablets: [`Tablet`], pen [`PenState`], pressure and tilt
//! normalization.

use crate::bitflags_type;
use crate::device::DeviceId;
use crate::events::{Event, TabletEvent, Timestamp};
use crate::input::InputDevice;

bitflags_type! {
    /// Buttons on the pen barrel (not the tablet's own hardware buttons).
    pub struct PenButtons: u8 {
        const PRIMARY   = 0b001;
        const SECONDARY = 0b010;
        const ERASER    = 0b100;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenTool {
    Pen,
    Eraser,
    Brush,
    Pencil,
    Airbrush,
    Unknown,
}

impl PenTool {
    /// Map a `BTN_TOOL_*` evdev code to a [`PenTool`].
    pub fn from_evdev_code(code: u16) -> Option<PenTool> {
        match code {
            0x140 => Some(PenTool::Pen),
            0x141 => Some(PenTool::Eraser),
            0x142 => Some(PenTool::Brush),
            0x143 => Some(PenTool::Pencil),
            0x144 => Some(PenTool::Airbrush),
            _ => None,
        }
    }
}

/// Instantaneous state of the pen in contact with (or hovering over) a tablet.
#[derive(Debug, Clone, Copy)]
pub struct PenState {
    /// Normalized `0.0..=1.0` position within the tablet's active area.
    pub x: f64,
    pub y: f64,
    /// Normalized `0.0..=1.0` pressure; `0.0` while hovering/not in contact.
    pub pressure: f64,
    /// Degrees, roughly `-90.0..=90.0`, if the hardware reports tilt.
    pub tilt_x: f32,
    pub tilt_y: f32,
    /// Normalized hover distance, `0.0` at the surface.
    pub distance: f64,
    pub in_proximity: bool,
    pub in_contact: bool,
    pub buttons: PenButtons,
    pub tool: PenTool,
}

impl Default for PenState {
    fn default() -> Self {
        PenState {
            x: 0.0,
            y: 0.0,
            pressure: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            distance: 0.0,
            in_proximity: false,
            in_contact: false,
            buttons: PenButtons::empty(),
            tool: PenTool::Unknown,
        }
    }
}

/// Stateful representation of a graphics tablet and the pen interacting
/// with it. Raw axis ranges are supplied at construction (normally read via
/// `EVIOCGABS` in [`crate::evdev`]) so pressure/tilt can be normalized here.
pub struct Tablet {
    device: DeviceId,
    pub pen: PenState,
    x_max: i32,
    y_max: i32,
    pressure_max: i32,
    tilt_x_range: (i32, i32),
    tilt_y_range: (i32, i32),
    distance_max: i32,
}

impl Tablet {
    pub fn new(device: DeviceId, x_max: i32, y_max: i32, pressure_max: i32) -> Self {
        Tablet {
            device,
            pen: PenState::default(),
            x_max: x_max.max(1),
            y_max: y_max.max(1),
            pressure_max: pressure_max.max(1),
            tilt_x_range: (-90, 90),
            tilt_y_range: (-90, 90),
            distance_max: 1,
        }
    }

    pub fn with_tilt_ranges(mut self, x_range: (i32, i32), y_range: (i32, i32)) -> Self {
        self.tilt_x_range = x_range;
        self.tilt_y_range = y_range;
        self
    }

    pub fn with_distance_max(mut self, max: i32) -> Self {
        self.distance_max = max.max(1);
        self
    }

    pub fn device_id(&self) -> DeviceId {
        self.device
    }

    pub fn update_position(&mut self, raw_x: i32, raw_y: i32) {
        self.update_x(raw_x);
        self.update_y(raw_y);
    }

    pub fn update_x(&mut self, raw_x: i32) {
        self.pen.x = (raw_x as f64 / self.x_max as f64).clamp(0.0, 1.0);
    }

    pub fn update_y(&mut self, raw_y: i32) {
        self.pen.y = (raw_y as f64 / self.y_max as f64).clamp(0.0, 1.0);
    }

    pub fn set_pressure(&mut self, raw: i32) {
        self.pen.pressure = (raw as f64 / self.pressure_max as f64).clamp(0.0, 1.0);
        self.pen.in_contact = self.pen.pressure > 0.0;
    }

    pub fn set_tilt(&mut self, raw_x: i32, raw_y: i32) {
        self.pen.tilt_x = normalize_signed(raw_x, self.tilt_x_range);
        self.pen.tilt_y = normalize_signed(raw_y, self.tilt_y_range);
    }

    pub fn set_distance(&mut self, raw: i32) {
        self.pen.distance = (raw as f64 / self.distance_max as f64).clamp(0.0, 1.0);
    }

    pub fn set_proximity(&mut self, in_proximity: bool) {
        self.pen.in_proximity = in_proximity;
        if !in_proximity {
            self.pen.in_contact = false;
            self.pen.pressure = 0.0;
        }
    }

    pub fn set_tool(&mut self, tool: PenTool) {
        self.pen.tool = tool;
    }

    pub fn set_button(&mut self, button: PenButtons, pressed: bool) {
        if pressed {
            self.pen.buttons.insert(button);
        } else {
            self.pen.buttons.remove(button);
        }
    }

    pub fn to_event(&self, time: Timestamp) -> TabletEvent {
        TabletEvent {
            device: self.device,
            time,
            x: self.pen.x,
            y: self.pen.y,
            pressure: self.pen.pressure,
            tilt_x: self.pen.tilt_x,
            tilt_y: self.pen.tilt_y,
            in_proximity: self.pen.in_proximity,
            in_contact: self.pen.in_contact,
            buttons: self.pen.buttons.bits(),
        }
    }
}

/// Map a raw value in `range` (which may be signed, e.g. `(-90, 90)`) to a
/// `f32` in that same unit (tilt is reported in already-meaningful degrees
/// by most hardware, so this mostly just clamps/converts).
fn normalize_signed(raw: i32, range: (i32, i32)) -> f32 {
    raw.clamp(range.0, range.1) as f32
}

impl InputDevice for Tablet {
    fn id(&self) -> DeviceId {
        self.device
    }

    fn handle_event(&mut self, ev_type: u16, code: u16, value: i32, time: Timestamp) -> Vec<Event> {
        use crate::evdev::{
            ABS_DISTANCE, ABS_PRESSURE, ABS_TILT_X, ABS_TILT_Y, ABS_X, ABS_Y, BTN_STYLUS,
            BTN_STYLUS2, BTN_TOUCH, EV_ABS, EV_KEY, EV_SYN, SYN_REPORT,
        };
        match ev_type {
            EV_ABS => {
                match code {
                    ABS_X => self.update_x(value),
                    ABS_Y => self.update_y(value),
                    ABS_PRESSURE => self.set_pressure(value),
                    ABS_TILT_X => {
                        let ty = self.pen.tilt_y as i32;
                        self.set_tilt(value, ty)
                    }
                    ABS_TILT_Y => {
                        let tx = self.pen.tilt_x as i32;
                        self.set_tilt(tx, value)
                    }
                    ABS_DISTANCE => self.set_distance(value),
                    _ => {}
                }
                Vec::new()
            }
            EV_KEY => {
                match code {
                    BTN_TOUCH => self.set_proximity(value != 0),
                    BTN_STYLUS => self.set_button(PenButtons::PRIMARY, value != 0),
                    BTN_STYLUS2 => self.set_button(PenButtons::SECONDARY, value != 0),
                    code if PenTool::from_evdev_code(code).is_some() => {
                        if value != 0 {
                            self.set_proximity(true);
                            self.set_tool(PenTool::from_evdev_code(code).unwrap());
                        } else {
                            self.set_proximity(false);
                        }
                    }
                    _ => {}
                }
                Vec::new()
            }
            EV_SYN if code == SYN_REPORT => vec![Event::Tablet(self.to_event(time))],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_position_and_pressure() {
        let mut tablet = Tablet::new(DeviceId::new(), 10000, 6000, 2048);
        tablet.update_position(5000, 3000);
        assert!((tablet.pen.x - 0.5).abs() < 0.001);
        assert!((tablet.pen.y - 0.5).abs() < 0.001);

        tablet.set_pressure(1024);
        assert!((tablet.pen.pressure - 0.5).abs() < 0.001);
        assert!(tablet.pen.in_contact);

        tablet.set_pressure(0);
        assert!(!tablet.pen.in_contact);
    }

    #[test]
    fn proximity_clears_contact() {
        let mut tablet = Tablet::new(DeviceId::new(), 1000, 1000, 100);
        tablet.set_proximity(true);
        tablet.set_pressure(50);
        assert!(tablet.pen.in_contact);
        tablet.set_proximity(false);
        assert!(!tablet.pen.in_contact);
        assert_eq!(tablet.pen.pressure, 0.0);
    }
}
