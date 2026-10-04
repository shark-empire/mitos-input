//! Integration tests for `mitos_input::pointer`, exercised entirely through
//! the crate's public API.

use std::time::Duration;

use mitos_input::device::DeviceId;
use mitos_input::events::Event;
use mitos_input::input::InputDevice;
use mitos_input::pointer::{AccelProfile, Button, Mouse, PointerEvent, ScrollAxis, ScrollSource};

#[test]
fn motion_accumulates_across_axis_updates_and_flushes_once() {
    let mut mouse = Mouse::new(DeviceId::new());
    let t = Duration::from_secs(0);

    assert!(mouse.flush_motion(t).is_none(), "nothing accumulated yet");

    mouse.accumulate_motion(2.0, 3.0);
    mouse.accumulate_motion(1.0, -1.0);
    match mouse.flush_motion(t) {
        Some(PointerEvent::Motion { dx, dy, .. }) => {
            assert_eq!(dx, 3.0);
            assert_eq!(dy, 2.0);
        }
        other => panic!("expected Motion, got {other:?}"),
    }
    assert_eq!(mouse.position(), (3.0, 2.0));
    assert!(mouse.flush_motion(t).is_none(), "flushing again with nothing new");
}

#[test]
fn flat_accel_profile_scales_motion() {
    let mut mouse = Mouse::new(DeviceId::new());
    mouse.set_accel_profile(AccelProfile::Flat(2.0));
    mouse.accumulate_motion(3.0, 0.0);
    match mouse.flush_motion(Duration::from_secs(0)) {
        Some(PointerEvent::Motion { dx, .. }) => assert_eq!(dx, 6.0),
        other => panic!("expected Motion, got {other:?}"),
    }
}

#[test]
fn input_device_trait_dispatches_scroll_and_buttons() {
    use mitos_input::evdev::{EV_KEY, EV_REL, REL_WHEEL};

    let mut mouse = Mouse::new(DeviceId::new());
    let t = Duration::from_secs(0);

    let events = mouse.handle_event(EV_REL, REL_WHEEL, 1, t);
    assert_eq!(events.len(), 1);
    match &events[0] {
        Event::Pointer(PointerEvent::Scroll { axis, value, source, .. }) => {
            assert_eq!(*axis, ScrollAxis::Vertical);
            assert_eq!(*source, ScrollSource::Wheel);
            // evdev's wheel-up-is-positive is flipped to "positive is down".
            assert_eq!(*value, -1.0);
        }
        other => panic!("expected Scroll, got {other:?}"),
    }

    let events = mouse.handle_event(EV_KEY, 0x110 /* BTN_LEFT */, 1, t);
    assert_eq!(events.len(), 1);
    match &events[0] {
        Event::Pointer(PointerEvent::Button { button, pressed, .. }) => {
            assert_eq!(*button, Button::Left);
            assert!(*pressed);
        }
        other => panic!("expected Button, got {other:?}"),
    }
    assert!(mouse.is_pressed(Button::Left));

    let events = mouse.handle_event(EV_KEY, 0x110, 0, t);
    match &events[0] {
        Event::Pointer(PointerEvent::Button { pressed, .. }) => assert!(!pressed),
        other => panic!("expected Button, got {other:?}"),
    }
    assert!(!mouse.is_pressed(Button::Left));
}

#[test]
fn unknown_button_code_round_trips_as_other() {
    let button = Button::from_evdev_code(0x999);
    assert_eq!(button, Button::Other(0x999));
}
