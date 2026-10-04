//! Integration tests for `mitos_input::keyboard`, exercised entirely through
//! the crate's public API (as an external consumer would use it).

use std::time::Duration;

use mitos_input::device::DeviceId;
use mitos_input::events::Event;
use mitos_input::input::InputDevice;
use mitos_input::keyboard::{KeyCode, KeyState, Keyboard, Keymap, Modifiers};

#[test]
fn keyboard_tracks_modifiers_and_pressed_keys() {
    let device = DeviceId::new();
    let mut kb = Keyboard::new(device);
    let t = Duration::from_secs(0);

    kb.process_key(KeyCode::LEFT_CTRL, KeyState::Pressed, t);
    assert!(kb.modifiers().contains(Modifiers::CTRL));

    let ev = kb.process_key(KeyCode::C, KeyState::Pressed, t);
    assert!(ev.modifiers.contains(Modifiers::CTRL));
    assert!(kb.is_pressed(KeyCode::C));

    kb.process_key(KeyCode::C, KeyState::Released, t);
    assert!(!kb.is_pressed(KeyCode::C));
}

#[test]
fn input_device_trait_dispatches_raw_ev_key_codes() {
    use mitos_input::evdev::{EV_KEY, EV_SYN, SYN_REPORT};

    let device = DeviceId::new();
    let mut kb = Keyboard::new(device);
    let t = Duration::from_secs(0);

    // Drive it the same way `crate::evdev`'s poll loop would: raw
    // (type, code, value) triples through the polymorphic InputDevice trait.
    let events = kb.handle_event(EV_KEY, KeyCode::A.0, 1, t);
    assert_eq!(events.len(), 1);
    match &events[0] {
        Event::Key(k) => {
            assert_eq!(k.state, KeyState::Pressed);
            assert_eq!(k.device, device);
            assert_eq!(k.key, KeyCode::A);
        }
        other => panic!("expected Event::Key, got {other:?}"),
    }

    // SYN_REPORT carries no state for a keyboard (unlike touch/gamepad).
    assert!(kb.handle_event(EV_SYN, SYN_REPORT, 0, t).is_empty());
}

#[test]
fn keymap_translates_base_and_shifted_symbols() {
    let keymap = Keymap::us_qwerty();
    assert_eq!(keymap.translate(KeyCode::KEY_1, Modifiers::empty()), Some('1'));
    assert_eq!(keymap.translate(KeyCode::KEY_1, Modifiers::SHIFT), Some('!'));
    assert_eq!(keymap.translate(KeyCode::A, Modifiers::empty()), Some('a'));
    assert_eq!(keymap.translate(KeyCode::A, Modifiers::SHIFT), Some('A'));
    // Keys with no textual representation translate to nothing.
    assert_eq!(keymap.translate(KeyCode::F1, Modifiers::empty()), None);
}

#[test]
fn caps_lock_is_a_toggle_that_only_affects_letters() {
    let mut kb = Keyboard::new(DeviceId::new());
    let t = Duration::from_secs(0);

    kb.process_key(KeyCode::CAPS_LOCK, KeyState::Pressed, t);
    kb.process_key(KeyCode::CAPS_LOCK, KeyState::Released, t);
    assert!(kb.modifiers().contains(Modifiers::CAPS_LOCK));
    assert_eq!(kb.translate(KeyCode::A), Some('A'));
    assert_eq!(kb.translate(KeyCode::KEY_1), Some('1')); // digits are unaffected

    kb.process_key(KeyCode::CAPS_LOCK, KeyState::Pressed, t);
    kb.process_key(KeyCode::CAPS_LOCK, KeyState::Released, t);
    assert!(!kb.modifiers().contains(Modifiers::CAPS_LOCK));
}
