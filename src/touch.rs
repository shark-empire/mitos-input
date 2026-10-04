//! Touch devices: [`TouchPoint`], [`TouchDevice`], multitouch slot tracking
//! (Linux "type B" protocol), and simple per-contact tap detection.

use std::time::{Duration, Instant};

use crate::device::DeviceId;
use crate::events::{Event, Timestamp, TouchEvent, TouchPhase};
use crate::gestures::GestureEngine;
use crate::input::InputDevice;

/// One active contact, in the type-B multitouch protocol's terms: a slot
/// index (a reusable hardware "finger tracker") holding a tracking id
/// (unique per physical touch-down-to-up) and position/pressure/size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchPoint {
    pub slot: usize,
    /// Unique per contact; assigned by the kernel, `-1` means the slot is empty.
    pub tracking_id: i32,
    /// Normalized `0.0..=1.0` within the device's reported bounds.
    pub x: f64,
    pub y: f64,
    pub pressure: f64,
    pub major: f64,
    pub minor: f64,
}

impl TouchPoint {
    fn empty(slot: usize) -> Self {
        TouchPoint {
            slot,
            tracking_id: -1,
            x: 0.0,
            y: 0.0,
            pressure: 0.0,
            major: 0.0,
            minor: 0.0,
        }
    }

    fn is_active(&self) -> bool {
        self.tracking_id >= 0
    }
}

/// Tracks a single slot's down-time and start position, for tap detection.
struct SlotGesture {
    start: Instant,
    start_x: f64,
    start_y: f64,
}

fn normalize(raw: i32, max: i32) -> f64 {
    if max <= 0 {
        0.0
    } else {
        (raw as f64 / max as f64).clamp(0.0, 1.0)
    }
}

/// Stateful representation of a multitouch device (touchscreen or touchpad),
/// implementing the Linux type-B protocol: `ABS_MT_SLOT` selects the active
/// slot, then `ABS_MT_TRACKING_ID` / `ABS_MT_POSITION_X` / `_Y` /
/// `ABS_MT_PRESSURE` update it, all flushed on `SYN_REPORT`.
///
/// Raw axis ranges (`x_max`/`y_max`/pressure/size) are supplied at
/// construction, normally read via `EVIOCGABS` in [`crate::evdev`], so
/// positions can be normalized to `0.0..=1.0` here. Pressure/size default to
/// "unsupported" (always normalizes to `0.0`) until configured with
/// [`TouchDevice::with_pressure_range`] / [`TouchDevice::with_size_range`].
pub struct TouchDevice {
    device: DeviceId,
    slots: Vec<TouchPoint>,
    active_slot: usize,
    x_max: i32,
    y_max: i32,
    pressure_max: i32,
    size_max: i32,
    gestures: Vec<Option<SlotGesture>>,
    /// Whether each slot was active as of the end of the last `SYN_REPORT`,
    /// so the next report can tell Down/Move/Up apart.
    was_active: Vec<bool>,
    tap_max_duration: Duration,
    tap_max_movement: f64,
    /// Recognizes multi-finger swipe/pinch/hold across the slots above,
    /// fed one full frame at a time from the `SYN_REPORT` handler below.
    gesture_engine: GestureEngine,
}

impl TouchDevice {
    pub fn new(device: DeviceId, slot_count: usize, x_max: i32, y_max: i32) -> Self {
        let slot_count = slot_count.max(1);
        TouchDevice {
            device,
            slots: (0..slot_count).map(TouchPoint::empty).collect(),
            active_slot: 0,
            x_max: x_max.max(1),
            y_max: y_max.max(1),
            pressure_max: 0,
            size_max: 0,
            gestures: (0..slot_count).map(|_| None).collect(),
            was_active: vec![false; slot_count],
            tap_max_duration: Duration::from_millis(200),
            tap_max_movement: 0.02, // normalized units
            gesture_engine: GestureEngine::new(),
        }
    }

    pub fn with_pressure_range(mut self, max: i32) -> Self {
        self.pressure_max = max;
        self
    }

    pub fn with_size_range(mut self, max: i32) -> Self {
        self.size_max = max;
        self
    }

    pub fn device_id(&self) -> DeviceId {
        self.device
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// `ABS_MT_SLOT`: select which slot subsequent updates apply to.
    pub fn select_slot(&mut self, slot: usize) {
        if slot < self.slots.len() {
            self.active_slot = slot;
        }
    }

    /// `ABS_MT_TRACKING_ID`: a non-negative id starts a new contact in the
    /// active slot; `-1` ends it.
    pub fn set_tracking_id(&mut self, id: i32) {
        let slot = self.active_slot;
        if id < 0 {
            // Only clear the tracking id (which is what `is_active` checks)
            // here. Position/pressure are left alone so the `SYN_REPORT`
            // handler can report the `Up` event with the last known
            // location; it resets the slot fully right after.
            self.slots[slot].tracking_id = -1;
            self.gestures[slot] = None;
        } else {
            self.slots[slot].tracking_id = id;
            self.gestures[slot] = Some(SlotGesture {
                start: Instant::now(),
                start_x: self.slots[slot].x,
                start_y: self.slots[slot].y,
            });
        }
    }

    /// `ABS_MT_POSITION_X` in raw device units.
    pub fn set_raw_x(&mut self, raw: i32) {
        let slot = self.active_slot;
        let max = self.x_max;
        self.slots[slot].x = normalize(raw, max);
    }

    /// `ABS_MT_POSITION_Y` in raw device units.
    pub fn set_raw_y(&mut self, raw: i32) {
        let slot = self.active_slot;
        let max = self.y_max;
        self.slots[slot].y = normalize(raw, max);
    }

    pub fn set_pressure(&mut self, raw: i32) {
        let slot = self.active_slot;
        let max = self.pressure_max;
        self.slots[slot].pressure = normalize(raw, max);
    }

    pub fn set_touch_major(&mut self, raw: i32) {
        let slot = self.active_slot;
        let max = self.size_max;
        self.slots[slot].major = normalize(raw, max);
    }

    pub fn set_touch_minor(&mut self, raw: i32) {
        let slot = self.active_slot;
        let max = self.size_max;
        self.slots[slot].minor = normalize(raw, max);
    }

    pub fn active_points(&self) -> impl Iterator<Item = &TouchPoint> {
        self.slots.iter().filter(|p| p.is_active())
    }

    pub fn point_count(&self) -> usize {
        self.active_points().count()
    }

    pub fn point(&self, slot: usize) -> Option<&TouchPoint> {
        self.slots.get(slot).filter(|p| p.is_active())
    }

    /// Check whether the contact that just lifted in `slot` qualifies as a
    /// tap (short duration, little movement). Call right before/at the point
    /// you clear the slot (tracking id `-1`).
    fn check_tap(&self, slot: usize) -> Option<(f64, f64)> {
        let g = self.gestures.get(slot)?.as_ref()?;
        let p = self.slots.get(slot)?;
        if g.start.elapsed() > self.tap_max_duration {
            return None;
        }
        let dx = p.x - g.start_x;
        let dy = p.y - g.start_y;
        if (dx * dx + dy * dy).sqrt() > self.tap_max_movement {
            return None;
        }
        Some((p.x, p.y))
    }
}

impl InputDevice for TouchDevice {
    fn id(&self) -> DeviceId {
        self.device
    }

    fn handle_event(&mut self, ev_type: u16, code: u16, value: i32, time: Timestamp) -> Vec<Event> {
        use crate::evdev::{
            ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_PRESSURE, ABS_MT_SLOT,
            ABS_MT_TOUCH_MAJOR, ABS_MT_TOUCH_MINOR, ABS_MT_TRACKING_ID, EV_ABS, EV_SYN,
            SYN_REPORT,
        };
        match ev_type {
            EV_ABS => {
                match code {
                    ABS_MT_SLOT => self.select_slot(value.max(0) as usize),
                    ABS_MT_TRACKING_ID => {
                        let was_active = self.slots[self.active_slot].is_active();
                        if value < 0 && was_active {
                            if let Some((x, y)) = self.check_tap(self.active_slot) {
                                self.set_tracking_id(value);
                                return vec![Event::Gesture(crate::events::GestureEvent::Tap {
                                    device: self.device,
                                    time,
                                    fingers: 1,
                                    x,
                                    y,
                                })];
                            }
                        }
                        self.set_tracking_id(value);
                    }
                    ABS_MT_POSITION_X => self.set_raw_x(value),
                    ABS_MT_POSITION_Y => self.set_raw_y(value),
                    ABS_MT_PRESSURE => self.set_pressure(value),
                    ABS_MT_TOUCH_MAJOR => self.set_touch_major(value),
                    ABS_MT_TOUCH_MINOR => self.set_touch_minor(value),
                    _ => {}
                }
                Vec::new()
            }
            EV_SYN if code == SYN_REPORT => {
                let mut events = Vec::with_capacity(self.slots.len());
                for slot in 0..self.slots.len() {
                    let now_active = self.slots[slot].is_active();
                    let phase = match (self.was_active[slot], now_active) {
                        (false, true) => Some(TouchPhase::Down),
                        (true, true) => Some(TouchPhase::Move),
                        (true, false) => Some(TouchPhase::Up),
                        (false, false) => None,
                    };
                    if let Some(phase) = phase {
                        events.push(Event::Touch(TouchEvent {
                            device: self.device,
                            time,
                            phase,
                            point: self.slots[slot],
                        }));
                    }
                    self.was_active[slot] = now_active;
                    if !now_active {
                        // Safe to fully reset now that any Up event above
                        // already captured the last known position.
                        self.slots[slot] = TouchPoint::empty(slot);
                    }
                }

                // Multi-finger gestures are recognized over the complete
                // set of contacts as of this report, not per-slot, so feed
                // the engine exactly once per frame.
                let active: Vec<TouchPoint> = self.active_points().copied().collect();
                if let Some(gesture) = self.gesture_engine.feed_frame(self.device, &active, time) {
                    events.push(Event::Gesture(gesture));
                }
                events
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evdev::{ABS_MT_POSITION_X, ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_ABS, EV_SYN, SYN_REPORT};
    use crate::input::InputDevice;

    #[test]
    fn full_contact_reports_down_move_up() {
        let mut t = TouchDevice::new(DeviceId::new(), 2, 4095, 4095);
        let time = Timestamp::from_secs(0);

        t.handle_event(EV_ABS, ABS_MT_SLOT, 0, time);
        t.handle_event(EV_ABS, ABS_MT_TRACKING_ID, 5, time);
        t.handle_event(EV_ABS, ABS_MT_POSITION_X, 2048, time);
        let down = t.handle_event(EV_SYN, SYN_REPORT, 0, time);
        assert_eq!(down.len(), 1);
        match &down[0] {
            Event::Touch(e) => assert_eq!(e.phase, TouchPhase::Down),
            other => panic!("expected Touch, got {other:?}"),
        }

        t.handle_event(EV_ABS, ABS_MT_POSITION_X, 3000, time);
        let moved = t.handle_event(EV_SYN, SYN_REPORT, 0, time);
        assert_eq!(moved.len(), 1);
        match &moved[0] {
            Event::Touch(e) => assert_eq!(e.phase, TouchPhase::Move),
            other => panic!("expected Touch, got {other:?}"),
        }

        t.handle_event(EV_ABS, ABS_MT_TRACKING_ID, -1, time);
        let up = t.handle_event(EV_SYN, SYN_REPORT, 0, time);
        assert_eq!(up.len(), 1);
        match &up[0] {
            Event::Touch(e) => assert_eq!(e.phase, TouchPhase::Up),
            other => panic!("expected Touch, got {other:?}"),
        }
    }

    #[test]
    fn tracks_multiple_slots() {
        let mut t = TouchDevice::new(DeviceId::new(), 2, 4095, 4095);
        t.select_slot(0);
        t.set_tracking_id(10);
        t.set_raw_x(2048);
        t.set_raw_y(1024);

        t.select_slot(1);
        t.set_tracking_id(11);
        t.set_raw_x(100);
        t.set_raw_y(200);

        assert_eq!(t.point_count(), 2);
        let p0 = t.point(0).unwrap();
        assert!((p0.x - 0.5).abs() < 0.01);

        t.select_slot(0);
        t.set_tracking_id(-1);
        assert_eq!(t.point_count(), 1);
        assert!(t.point(0).is_none());
        assert!(t.point(1).is_some());
    }

    #[test]
    fn quick_small_movement_is_a_tap() {
        let mut t = TouchDevice::new(DeviceId::new(), 1, 4095, 4095);
        t.select_slot(0);
        t.set_tracking_id(1);
        t.set_raw_x(2000);
        t.set_raw_y(2000);
        assert!(t.check_tap(0).is_some());
    }

    #[test]
    fn pressure_defaults_to_unsupported() {
        let mut t = TouchDevice::new(DeviceId::new(), 1, 4095, 4095);
        t.select_slot(0);
        t.set_tracking_id(1);
        t.set_pressure(500);
        assert_eq!(t.point(0).unwrap().pressure, 0.0);

        let mut t2 = TouchDevice::new(DeviceId::new(), 1, 4095, 4095).with_pressure_range(1000);
        t2.select_slot(0);
        t2.set_tracking_id(1);
        t2.set_pressure(500);
        assert!((t2.point(0).unwrap().pressure - 0.5).abs() < 0.01);
    }
}
