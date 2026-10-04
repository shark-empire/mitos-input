//! Keyboards: [`KeyCode`], [`KeyState`], [`Modifiers`], keymap translation,
//! and the stateful [`Keyboard`] device.

use std::collections::{HashMap, HashSet};

use crate::bitflags_type;
use crate::device::DeviceId;
use crate::events::{Event, KeyEvent, Timestamp};
use crate::input::InputDevice;

/// A physical key, identified by its Linux `input-event-codes.h` `KEY_*`
/// number. Using the raw evdev numbering (rather than a hand-enumerated
/// `enum`) means every key the kernel can report is representable, even ones
/// without a named constant below -- just wrap the raw code: `KeyCode(200)`.
///
/// Named constants are provided for the common ~90% of a keyboard. More can
/// be added following the same pattern; the numbering is a stable kernel
/// ABI and has not changed in decades.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyCode(pub u16);

impl KeyCode {
    pub const ESC: KeyCode = KeyCode(1);
    pub const KEY_1: KeyCode = KeyCode(2);
    pub const KEY_2: KeyCode = KeyCode(3);
    pub const KEY_3: KeyCode = KeyCode(4);
    pub const KEY_4: KeyCode = KeyCode(5);
    pub const KEY_5: KeyCode = KeyCode(6);
    pub const KEY_6: KeyCode = KeyCode(7);
    pub const KEY_7: KeyCode = KeyCode(8);
    pub const KEY_8: KeyCode = KeyCode(9);
    pub const KEY_9: KeyCode = KeyCode(10);
    pub const KEY_0: KeyCode = KeyCode(11);
    pub const MINUS: KeyCode = KeyCode(12);
    pub const EQUAL: KeyCode = KeyCode(13);
    pub const BACKSPACE: KeyCode = KeyCode(14);
    pub const TAB: KeyCode = KeyCode(15);
    pub const Q: KeyCode = KeyCode(16);
    pub const W: KeyCode = KeyCode(17);
    pub const E: KeyCode = KeyCode(18);
    pub const R: KeyCode = KeyCode(19);
    pub const T: KeyCode = KeyCode(20);
    pub const Y: KeyCode = KeyCode(21);
    pub const U: KeyCode = KeyCode(22);
    pub const I: KeyCode = KeyCode(23);
    pub const O: KeyCode = KeyCode(24);
    pub const P: KeyCode = KeyCode(25);
    pub const LEFT_BRACE: KeyCode = KeyCode(26);
    pub const RIGHT_BRACE: KeyCode = KeyCode(27);
    pub const ENTER: KeyCode = KeyCode(28);
    pub const LEFT_CTRL: KeyCode = KeyCode(29);
    pub const A: KeyCode = KeyCode(30);
    pub const S: KeyCode = KeyCode(31);
    pub const D: KeyCode = KeyCode(32);
    pub const F: KeyCode = KeyCode(33);
    pub const G: KeyCode = KeyCode(34);
    pub const H: KeyCode = KeyCode(35);
    pub const J: KeyCode = KeyCode(36);
    pub const K: KeyCode = KeyCode(37);
    pub const L: KeyCode = KeyCode(38);
    pub const SEMICOLON: KeyCode = KeyCode(39);
    pub const APOSTROPHE: KeyCode = KeyCode(40);
    pub const GRAVE: KeyCode = KeyCode(41);
    pub const LEFT_SHIFT: KeyCode = KeyCode(42);
    pub const BACKSLASH: KeyCode = KeyCode(43);
    pub const Z: KeyCode = KeyCode(44);
    pub const X: KeyCode = KeyCode(45);
    pub const C: KeyCode = KeyCode(46);
    pub const V: KeyCode = KeyCode(47);
    pub const B: KeyCode = KeyCode(48);
    pub const N: KeyCode = KeyCode(49);
    pub const M: KeyCode = KeyCode(50);
    pub const COMMA: KeyCode = KeyCode(51);
    pub const DOT: KeyCode = KeyCode(52);
    pub const SLASH: KeyCode = KeyCode(53);
    pub const RIGHT_SHIFT: KeyCode = KeyCode(54);
    pub const KP_ASTERISK: KeyCode = KeyCode(55);
    pub const LEFT_ALT: KeyCode = KeyCode(56);
    pub const SPACE: KeyCode = KeyCode(57);
    pub const CAPS_LOCK: KeyCode = KeyCode(58);
    pub const F1: KeyCode = KeyCode(59);
    pub const F2: KeyCode = KeyCode(60);
    pub const F3: KeyCode = KeyCode(61);
    pub const F4: KeyCode = KeyCode(62);
    pub const F5: KeyCode = KeyCode(63);
    pub const F6: KeyCode = KeyCode(64);
    pub const F7: KeyCode = KeyCode(65);
    pub const F8: KeyCode = KeyCode(66);
    pub const F9: KeyCode = KeyCode(67);
    pub const F10: KeyCode = KeyCode(68);
    pub const NUM_LOCK: KeyCode = KeyCode(69);
    pub const SCROLL_LOCK: KeyCode = KeyCode(70);
    pub const KP7: KeyCode = KeyCode(71);
    pub const KP8: KeyCode = KeyCode(72);
    pub const KP9: KeyCode = KeyCode(73);
    pub const KP_MINUS: KeyCode = KeyCode(74);
    pub const KP4: KeyCode = KeyCode(75);
    pub const KP5: KeyCode = KeyCode(76);
    pub const KP6: KeyCode = KeyCode(77);
    pub const KP_PLUS: KeyCode = KeyCode(78);
    pub const KP1: KeyCode = KeyCode(79);
    pub const KP2: KeyCode = KeyCode(80);
    pub const KP3: KeyCode = KeyCode(81);
    pub const KP0: KeyCode = KeyCode(82);
    pub const KP_DOT: KeyCode = KeyCode(83);
    pub const F11: KeyCode = KeyCode(87);
    pub const F12: KeyCode = KeyCode(88);
    pub const KP_ENTER: KeyCode = KeyCode(96);
    pub const RIGHT_CTRL: KeyCode = KeyCode(97);
    pub const KP_SLASH: KeyCode = KeyCode(98);
    pub const SYSRQ: KeyCode = KeyCode(99);
    pub const PRINT_SCREEN: KeyCode = KeyCode(99);
    pub const RIGHT_ALT: KeyCode = KeyCode(100);
    pub const HOME: KeyCode = KeyCode(102);
    pub const UP: KeyCode = KeyCode(103);
    pub const PAGE_UP: KeyCode = KeyCode(104);
    pub const LEFT: KeyCode = KeyCode(105);
    pub const RIGHT: KeyCode = KeyCode(106);
    pub const END: KeyCode = KeyCode(107);
    pub const DOWN: KeyCode = KeyCode(108);
    pub const PAGE_DOWN: KeyCode = KeyCode(109);
    pub const INSERT: KeyCode = KeyCode(110);
    pub const DELETE: KeyCode = KeyCode(111);
    pub const MUTE: KeyCode = KeyCode(113);
    pub const VOLUME_DOWN: KeyCode = KeyCode(114);
    pub const VOLUME_UP: KeyCode = KeyCode(115);
    pub const POWER: KeyCode = KeyCode(116);
    pub const KP_EQUAL: KeyCode = KeyCode(117);
    pub const PAUSE: KeyCode = KeyCode(119);
    pub const LEFT_META: KeyCode = KeyCode(125);
    pub const RIGHT_META: KeyCode = KeyCode(126);
    pub const COMPOSE: KeyCode = KeyCode(127);
    pub const F13: KeyCode = KeyCode(183);
    pub const F14: KeyCode = KeyCode(184);
    pub const F15: KeyCode = KeyCode(185);
    pub const F16: KeyCode = KeyCode(186);
    pub const F17: KeyCode = KeyCode(187);
    pub const F18: KeyCode = KeyCode(188);
    pub const F19: KeyCode = KeyCode(189);
    pub const F20: KeyCode = KeyCode(190);
    pub const F21: KeyCode = KeyCode(191);
    pub const F22: KeyCode = KeyCode(192);
    pub const F23: KeyCode = KeyCode(193);
    pub const F24: KeyCode = KeyCode(194);
    pub const NEXT_TRACK: KeyCode = KeyCode(163);
    pub const PLAY_PAUSE: KeyCode = KeyCode(164);
    pub const PREV_TRACK: KeyCode = KeyCode(165);

    /// The `KEY_1`..`KEY_9`,`KEY_0` row, addressed as digits `1..=9, 0`.
    /// Returns `None` outside `0..=9`.
    pub const fn digit(n: u8) -> Option<KeyCode> {
        match n {
            1 => Some(KeyCode::KEY_1),
            2 => Some(KeyCode::KEY_2),
            3 => Some(KeyCode::KEY_3),
            4 => Some(KeyCode::KEY_4),
            5 => Some(KeyCode::KEY_5),
            6 => Some(KeyCode::KEY_6),
            7 => Some(KeyCode::KEY_7),
            8 => Some(KeyCode::KEY_8),
            9 => Some(KeyCode::KEY_9),
            0 => Some(KeyCode::KEY_0),
            _ => None,
        }
    }

    /// Best-effort symbolic name for debugging/logging; falls back to the
    /// raw numeric code for anything not in the named table above.
    pub fn name(&self) -> String {
        let s = match *self {
            KeyCode::ESC => "ESC",
            KeyCode::TAB => "TAB",
            KeyCode::ENTER => "ENTER",
            KeyCode::BACKSPACE => "BACKSPACE",
            KeyCode::SPACE => "SPACE",
            KeyCode::LEFT_CTRL => "LEFT_CTRL",
            KeyCode::RIGHT_CTRL => "RIGHT_CTRL",
            KeyCode::LEFT_SHIFT => "LEFT_SHIFT",
            KeyCode::RIGHT_SHIFT => "RIGHT_SHIFT",
            KeyCode::LEFT_ALT => "LEFT_ALT",
            KeyCode::RIGHT_ALT => "RIGHT_ALT",
            KeyCode::LEFT_META => "LEFT_META",
            KeyCode::RIGHT_META => "RIGHT_META",
            KeyCode::CAPS_LOCK => "CAPS_LOCK",
            KeyCode::NUM_LOCK => "NUM_LOCK",
            KeyCode::UP => "UP",
            KeyCode::DOWN => "DOWN",
            KeyCode::LEFT => "LEFT",
            KeyCode::RIGHT => "RIGHT",
            KeyCode::A => "A",
            KeyCode::B => "B",
            KeyCode::C => "C",
            KeyCode::D => "D",
            KeyCode::E => "E",
            KeyCode::F => "F",
            KeyCode::G => "G",
            KeyCode::H => "H",
            KeyCode::I => "I",
            KeyCode::J => "J",
            KeyCode::K => "K",
            KeyCode::L => "L",
            KeyCode::M => "M",
            KeyCode::N => "N",
            KeyCode::O => "O",
            KeyCode::P => "P",
            KeyCode::Q => "Q",
            KeyCode::R => "R",
            KeyCode::S => "S",
            KeyCode::T => "T",
            KeyCode::U => "U",
            KeyCode::V => "V",
            KeyCode::W => "W",
            KeyCode::X => "X",
            KeyCode::Y => "Y",
            KeyCode::Z => "Z",
            _ => "",
        };
        if s.is_empty() {
            format!("KEY({})", self.0)
        } else {
            s.to_string()
        }
    }
}

impl std::fmt::Debug for KeyCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// The three states the kernel reports for `EV_KEY` (`value` field of the
/// raw `input_event`): released, pressed, and auto-repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Released,
    Pressed,
    Repeat,
}

impl KeyState {
    /// Decode the raw evdev `value` field of an `EV_KEY` event.
    pub fn from_evdev_value(value: i32) -> Option<KeyState> {
        match value {
            0 => Some(KeyState::Released),
            1 => Some(KeyState::Pressed),
            2 => Some(KeyState::Repeat),
            _ => None,
        }
    }
}

bitflags_type! {
    /// Currently-held modifier keys, tracked per [`Keyboard`].
    pub struct Modifiers: u8 {
        const SHIFT     = 0b0000_0001;
        const CTRL      = 0b0000_0010;
        const ALT       = 0b0000_0100;
        /// The "MITOS key" (Super/Windows/Command on other systems).
        const SUPER     = 0b0000_1000;
        /// Right Alt, when used as a distinct AltGr modifier.
        const ALT_GR    = 0b0001_0000;
        const CAPS_LOCK = 0b0010_0000;
        const NUM_LOCK  = 0b0100_0000;
    }
}

/// A simple, static US-QWERTY keymap: enough to turn key + modifier state
/// into a `char` for basic text entry. A full layout engine (dead keys,
/// compose sequences, non-Latin layouts) is out of scope here; swap in a
/// different [`Keymap`] per-device as needed.
pub struct Keymap {
    table: HashMap<KeyCode, (char, char)>,
}

impl Keymap {
    /// A minimal US-QWERTY layout covering letters, digits, and standard
    /// punctuation.
    pub fn us_qwerty() -> Self {
        let mut table = HashMap::new();
        let letters = [
            (KeyCode::A, 'a'),
            (KeyCode::B, 'b'),
            (KeyCode::C, 'c'),
            (KeyCode::D, 'd'),
            (KeyCode::E, 'e'),
            (KeyCode::F, 'f'),
            (KeyCode::G, 'g'),
            (KeyCode::H, 'h'),
            (KeyCode::I, 'i'),
            (KeyCode::J, 'j'),
            (KeyCode::K, 'k'),
            (KeyCode::L, 'l'),
            (KeyCode::M, 'm'),
            (KeyCode::N, 'n'),
            (KeyCode::O, 'o'),
            (KeyCode::P, 'p'),
            (KeyCode::Q, 'q'),
            (KeyCode::R, 'r'),
            (KeyCode::S, 's'),
            (KeyCode::T, 't'),
            (KeyCode::U, 'u'),
            (KeyCode::V, 'v'),
            (KeyCode::W, 'w'),
            (KeyCode::X, 'x'),
            (KeyCode::Y, 'y'),
            (KeyCode::Z, 'z'),
        ];
        for (k, c) in letters {
            table.insert(k, (c, c.to_ascii_uppercase()));
        }
        let symbols: &[(KeyCode, char, char)] = &[
            (KeyCode::KEY_1, '1', '!'),
            (KeyCode::KEY_2, '2', '@'),
            (KeyCode::KEY_3, '3', '#'),
            (KeyCode::KEY_4, '4', '$'),
            (KeyCode::KEY_5, '5', '%'),
            (KeyCode::KEY_6, '6', '^'),
            (KeyCode::KEY_7, '7', '&'),
            (KeyCode::KEY_8, '8', '*'),
            (KeyCode::KEY_9, '9', '('),
            (KeyCode::KEY_0, '0', ')'),
            (KeyCode::SPACE, ' ', ' '),
            (KeyCode::MINUS, '-', '_'),
            (KeyCode::EQUAL, '=', '+'),
            (KeyCode::LEFT_BRACE, '[', '{'),
            (KeyCode::RIGHT_BRACE, ']', '}'),
            (KeyCode::SEMICOLON, ';', ':'),
            (KeyCode::APOSTROPHE, '\'', '"'),
            (KeyCode::GRAVE, '`', '~'),
            (KeyCode::BACKSLASH, '\\', '|'),
            (KeyCode::COMMA, ',', '<'),
            (KeyCode::DOT, '.', '>'),
            (KeyCode::SLASH, '/', '?'),
        ];
        for (k, base, shifted) in symbols.iter().copied() {
            table.insert(k, (base, shifted));
        }
        Keymap { table }
    }

    /// Translate a key press into a `char`, given the currently-held
    /// modifiers. Returns `None` for keys with no textual representation
    /// (arrows, function keys, modifiers themselves, ...).
    pub fn translate(&self, key: KeyCode, mods: Modifiers) -> Option<char> {
        let (base, shifted) = *self.table.get(&key)?;
        let shift = mods.contains(Modifiers::SHIFT);
        let caps = mods.contains(Modifiers::CAPS_LOCK);
        // Caps Lock only affects alphabetic keys; Shift affects everything.
        // The two "cancel out" on letters when both are active.
        let use_shifted = shift ^ (caps && base.is_alphabetic());
        Some(if use_shifted { shifted } else { base })
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self::us_qwerty()
    }
}

/// Stateful representation of one physical (or virtual) keyboard: which keys
/// are currently held, the live modifier mask, and a keymap for text
/// translation.
pub struct Keyboard {
    device: DeviceId,
    pressed: HashSet<KeyCode>,
    modifiers: Modifiers,
    keymap: Keymap,
}

impl Keyboard {
    pub fn new(device: DeviceId) -> Self {
        Keyboard {
            device,
            pressed: HashSet::new(),
            modifiers: Modifiers::empty(),
            keymap: Keymap::default(),
        }
    }

    pub fn with_keymap(mut self, keymap: Keymap) -> Self {
        self.keymap = keymap;
        self
    }

    pub fn device_id(&self) -> DeviceId {
        self.device
    }

    pub fn is_pressed(&self, key: KeyCode) -> bool {
        self.pressed.contains(&key)
    }

    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    pub fn translate(&self, key: KeyCode) -> Option<char> {
        self.keymap.translate(key, self.modifiers)
    }

    /// Core state-update entry point: feed a decoded key + state, get back
    /// the [`KeyEvent`] to publish. Updates the held-key set and modifier
    /// mask as a side effect.
    pub fn process_key(&mut self, key: KeyCode, state: KeyState, time: Timestamp) -> KeyEvent {
        match state {
            KeyState::Pressed | KeyState::Repeat => {
                self.pressed.insert(key);
            }
            KeyState::Released => {
                self.pressed.remove(&key);
            }
        }
        self.update_modifiers(key, state);
        KeyEvent {
            device: self.device,
            time,
            key,
            state,
            modifiers: self.modifiers,
            repeat: state == KeyState::Repeat,
        }
    }

    fn update_modifiers(&mut self, key: KeyCode, state: KeyState) {
        let held = matches!(state, KeyState::Pressed | KeyState::Repeat);
        match key {
            KeyCode::LEFT_SHIFT | KeyCode::RIGHT_SHIFT => self.set_mod(Modifiers::SHIFT, held),
            KeyCode::LEFT_CTRL | KeyCode::RIGHT_CTRL => self.set_mod(Modifiers::CTRL, held),
            KeyCode::LEFT_ALT => self.set_mod(Modifiers::ALT, held),
            KeyCode::RIGHT_ALT => self.set_mod(Modifiers::ALT_GR, held),
            KeyCode::LEFT_META | KeyCode::RIGHT_META => self.set_mod(Modifiers::SUPER, held),
            // Lock keys toggle on the down-stroke only.
            KeyCode::CAPS_LOCK if state == KeyState::Pressed => {
                self.modifiers.toggle(Modifiers::CAPS_LOCK)
            }
            KeyCode::NUM_LOCK if state == KeyState::Pressed => {
                self.modifiers.toggle(Modifiers::NUM_LOCK)
            }
            _ => {}
        }
    }

    fn set_mod(&mut self, flag: Modifiers, on: bool) {
        if on {
            self.modifiers.insert(flag);
        } else {
            self.modifiers.remove(flag);
        }
    }
}

impl InputDevice for Keyboard {
    fn id(&self) -> DeviceId {
        self.device
    }

    fn handle_event(&mut self, ev_type: u16, code: u16, value: i32, time: Timestamp) -> Vec<Event> {
        use crate::evdev::EV_KEY;
        if ev_type != EV_KEY {
            return Vec::new();
        }
        match KeyState::from_evdev_value(value) {
            Some(state) => vec![Event::Key(self.process_key(KeyCode(code), state, time))],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_modifiers_and_pressed_keys() {
        let mut kb = Keyboard::new(DeviceId::new());
        let t = Timestamp::from_secs(0);

        kb.process_key(KeyCode::LEFT_SHIFT, KeyState::Pressed, t);
        assert!(kb.modifiers().contains(Modifiers::SHIFT));

        let ev = kb.process_key(KeyCode::A, KeyState::Pressed, t);
        assert!(kb.is_pressed(KeyCode::A));
        assert_eq!(ev.key, KeyCode::A);
        assert!(ev.modifiers.contains(Modifiers::SHIFT));
        assert_eq!(kb.translate(KeyCode::A), Some('A'));

        kb.process_key(KeyCode::A, KeyState::Released, t);
        assert!(!kb.is_pressed(KeyCode::A));

        kb.process_key(KeyCode::LEFT_SHIFT, KeyState::Released, t);
        assert!(!kb.modifiers().contains(Modifiers::SHIFT));
        assert_eq!(kb.translate(KeyCode::A), Some('a'));
    }

    #[test]
    fn caps_lock_toggles_and_only_affects_letters() {
        let mut kb = Keyboard::new(DeviceId::new());
        let t = Timestamp::from_secs(0);
        kb.process_key(KeyCode::CAPS_LOCK, KeyState::Pressed, t);
        kb.process_key(KeyCode::CAPS_LOCK, KeyState::Released, t);
        assert!(kb.modifiers().contains(Modifiers::CAPS_LOCK));
        assert_eq!(kb.translate(KeyCode::A), Some('A'));
        assert_eq!(kb.translate(KeyCode::KEY_1), Some('1'));
    }
}
