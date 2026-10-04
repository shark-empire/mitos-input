//! Integration tests for `mitos_input::gamepad`, exercised entirely through
//! the crate's public API.

use std::time::Duration;

use mitos_input::device::DeviceId;
use mitos_input::events::{Event, GamepadEvent};
use mitos_input::gamepad::{Gamepad, GamepadAxis, GamepadButton, GamepadMapping};
use mitos_input::input::InputDevice;

#[test]
fn standard_mapping_recognizes_face_buttons() {
    use mitos_input::evdev::EV_KEY;

    let mut pad = Gamepad::new(DeviceId::new(), GamepadMapping::standard_xbox());
    let t = Duration::from_secs(0);

    let events = pad.handle_event(EV_KEY, 0x130 /* BTN_SOUTH */, 1, t);
    assert_eq!(events.len(), 1);
    match &events[0] {
        Event::Gamepad(GamepadEvent::Button { button, pressed, .. }) => {
            assert_eq!(*button, GamepadButton::South);
            assert!(*pressed);
        }
        other => panic!("expected Button South pressed, got {other:?}"),
    }
    assert!(pad.is_pressed(GamepadButton::South));

    pad.handle_event(EV_KEY, 0x130, 0, t);
    assert!(!pad.is_pressed(GamepadButton::South));
}

#[test]
fn full_stick_deflection_normalizes_to_unit_range() {
    use mitos_input::evdev::EV_ABS;

    let mut pad = Gamepad::new(DeviceId::new(), GamepadMapping::standard_xbox());
    pad.handle_event(EV_ABS, 0x00 /* ABS_X */, 32767, Duration::from_secs(0));
    assert!((pad.axis_value(GamepadAxis::LeftStickX) - 1.0).abs() < 0.01);

    let mut pad2 = Gamepad::new(DeviceId::new(), GamepadMapping::standard_xbox());
    pad2.handle_event(EV_ABS, 0x00, -32768, Duration::from_secs(0));
    assert!((pad2.axis_value(GamepadAxis::LeftStickX) - (-1.0)).abs() < 0.01);
}

#[test]
fn trigger_axis_normalizes_zero_to_one_not_signed() {
    use mitos_input::evdev::EV_ABS;

    let mut pad = Gamepad::new(DeviceId::new(), GamepadMapping::standard_xbox());
    pad.handle_event(EV_ABS, 0x02 /* ABS_Z, left trigger */, 255, Duration::from_secs(0));
    assert!((pad.axis_value(GamepadAxis::LeftTrigger) - 1.0).abs() < 0.01);
}

#[test]
fn custom_deadzone_widens_the_dead_region() {
    use mitos_input::evdev::EV_ABS;
    let t = Duration::from_secs(0);
    // Raw value chosen to normalize to roughly 0.30: outside the default
    // deadzone (0.12) but inside a widened one (0.5).
    const RAW: i32 = 9830;

    let mut default_dz = Gamepad::new(DeviceId::new(), GamepadMapping::standard_xbox());
    default_dz.handle_event(EV_ABS, 0x00, RAW, t);
    assert!(
        default_dz.axis_value(GamepadAxis::LeftStickX) > 0.0,
        "default deadzone (0.12) should let a ~0.30 deflection through"
    );

    let mut wide_dz = Gamepad::new(DeviceId::new(), GamepadMapping::standard_xbox()).with_deadzone(0.5);
    wide_dz.handle_event(EV_ABS, 0x00, RAW, t);
    assert_eq!(
        wide_dz.axis_value(GamepadAxis::LeftStickX),
        0.0,
        "a 0.5 deadzone should swallow the same ~0.30 deflection"
    );
}

#[test]
fn hat_switch_dpad_reports_exactly_one_transition_per_press_and_release() {
    use mitos_input::evdev::EV_ABS;
    let t = Duration::from_secs(0);
    let mut pad = Gamepad::new(DeviceId::new(), GamepadMapping::standard_xbox());

    let events = pad.handle_event(EV_ABS, 0x10 /* ABS_HAT0X */, -1, t);
    assert_eq!(events.len(), 1, "pressing left should not also emit a spurious right-release");
    assert!(pad.is_pressed(GamepadButton::DPadLeft));
    assert!(!pad.is_pressed(GamepadButton::DPadRight));

    let events = pad.handle_event(EV_ABS, 0x10, 0, t);
    assert_eq!(events.len(), 1);
    assert!(!pad.is_pressed(GamepadButton::DPadLeft));
}
