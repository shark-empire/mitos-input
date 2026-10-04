//! Integration tests for `mitos_input::touch`, exercised entirely through
//! the crate's public API.

use std::time::Duration;

use mitos_input::device::DeviceId;
use mitos_input::events::{Event, TouchPhase};
use mitos_input::input::InputDevice;
use mitos_input::touch::TouchDevice;

#[test]
fn full_contact_lifecycle_through_input_device_trait() {
    use mitos_input::evdev::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_ABS, EV_SYN,
        SYN_REPORT,
    };

    let mut touch = TouchDevice::new(DeviceId::new(), 5, 4095, 4095);
    let t = Duration::from_secs(0);

    touch.handle_event(EV_ABS, ABS_MT_SLOT, 0, t);
    touch.handle_event(EV_ABS, ABS_MT_TRACKING_ID, 1, t);
    touch.handle_event(EV_ABS, ABS_MT_POSITION_X, 2048, t);
    touch.handle_event(EV_ABS, ABS_MT_POSITION_Y, 2048, t);
    let down = touch.handle_event(EV_SYN, SYN_REPORT, 0, t);

    assert_eq!(down.len(), 1);
    match &down[0] {
        Event::Touch(e) => {
            assert_eq!(e.phase, TouchPhase::Down);
            assert!((e.point.x - 0.5).abs() < 0.01);
            assert!((e.point.y - 0.5).abs() < 0.01);
        }
        other => panic!("expected Touch, got {other:?}"),
    }
    assert_eq!(touch.point_count(), 1);

    touch.handle_event(EV_ABS, ABS_MT_TRACKING_ID, -1, t);
    let up = touch.handle_event(EV_SYN, SYN_REPORT, 0, t);
    assert_eq!(up.len(), 1);
    match &up[0] {
        Event::Touch(e) => assert_eq!(e.phase, TouchPhase::Up),
        other => panic!("expected Touch, got {other:?}"),
    }
    assert_eq!(touch.point_count(), 0);
}

#[test]
fn multiple_slots_track_independently() {
    let mut touch = TouchDevice::new(DeviceId::new(), 3, 1000, 1000);
    touch.select_slot(0);
    touch.set_tracking_id(1);
    touch.set_raw_x(100);

    touch.select_slot(1);
    touch.set_tracking_id(2);
    touch.set_raw_x(900);

    assert_eq!(touch.point_count(), 2);
    assert!(touch.point(0).unwrap().x < touch.point(1).unwrap().x);

    touch.select_slot(0);
    touch.set_tracking_id(-1);
    assert_eq!(touch.point_count(), 1);
    assert!(touch.point(1).is_some());
}

#[test]
fn pressure_is_zero_until_a_range_is_configured() {
    let mut unconfigured = TouchDevice::new(DeviceId::new(), 1, 1000, 1000);
    unconfigured.select_slot(0);
    unconfigured.set_tracking_id(1);
    unconfigured.set_pressure(500);
    assert_eq!(unconfigured.point(0).unwrap().pressure, 0.0);

    let mut configured = TouchDevice::new(DeviceId::new(), 1, 1000, 1000).with_pressure_range(1000);
    configured.select_slot(0);
    configured.set_tracking_id(1);
    configured.set_pressure(500);
    assert!((configured.point(0).unwrap().pressure - 0.5).abs() < 0.01);
}

#[test]
fn two_finger_swipe_surfaces_as_a_gesture_event() {
    use mitos_input::evdev::{
        ABS_MT_POSITION_X, ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_ABS, EV_SYN, SYN_REPORT,
    };
    use mitos_input::events::GestureEvent;

    let mut touch = TouchDevice::new(DeviceId::new(), 5, 1000, 1000);
    let t = Duration::from_secs(0);
    let report = |touch: &mut TouchDevice| touch.handle_event(EV_SYN, SYN_REPORT, 0, t);

    // Two fingers down.
    touch.handle_event(EV_ABS, ABS_MT_SLOT, 0, t);
    touch.handle_event(EV_ABS, ABS_MT_TRACKING_ID, 1, t);
    touch.handle_event(EV_ABS, ABS_MT_POSITION_X, 300, t);
    touch.handle_event(EV_ABS, ABS_MT_SLOT, 1, t);
    touch.handle_event(EV_ABS, ABS_MT_TRACKING_ID, 2, t);
    touch.handle_event(EV_ABS, ABS_MT_POSITION_X, 600, t);
    report(&mut touch);

    // Lockstep rightward motion, one frame at a time: the gesture engine
    // needs one frame to establish a position/spacing baseline before it
    // can compare a later frame against it, so the Swipe should appear by
    // the second iteration here at the latest. Looping (rather than
    // asserting on a fixed iteration) keeps this robust to that detail.
    for x_shift in [50, 100, 150] {
        touch.handle_event(EV_ABS, ABS_MT_SLOT, 0, t);
        touch.handle_event(EV_ABS, ABS_MT_POSITION_X, 300 + x_shift, t);
        touch.handle_event(EV_ABS, ABS_MT_SLOT, 1, t);
        touch.handle_event(EV_ABS, ABS_MT_POSITION_X, 600 + x_shift, t);
        let events = report(&mut touch);
        if let Some(Event::Gesture(GestureEvent::Swipe { fingers, .. })) =
            events.iter().find(|e| matches!(e, Event::Gesture(_)))
        {
            assert_eq!(*fingers, 2);
            return; // recognized -- test passes
        }
    }
    panic!("expected a Swipe gesture within three lockstep frames");
}
