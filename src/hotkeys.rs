//! Global shortcuts: key combinations bound to system-level actions,
//! including the default MITOS shortcuts (the Super/"MITOS key" bindings).

use std::collections::HashMap;

use crate::events::KeyEvent;
use crate::keyboard::{KeyCode, KeyState, Modifiers};

pub type HotkeyId = u64;

/// A key plus the exact modifier state required for it to fire. Distinct
/// from just tracking "is Ctrl held" -- `Modifiers::SUPER` here must match
/// exactly what's currently held, so `Super+D` doesn't also fire on
/// `Super+Shift+D`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub key: KeyCode,
    pub modifiers: Modifiers,
}

impl KeyChord {
    pub fn new(key: KeyCode, modifiers: Modifiers) -> Self {
        KeyChord { key, modifiers }
    }
}

/// A system-level action a hotkey can trigger. `Custom` carries an
/// application-defined identifier for anything not built in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyAction {
    ShowLauncher,
    ShowDesktop,
    SwitchWindow,
    SwitchWindowReverse,
    /// Switch to a workspace by 1-based index.
    SwitchWorkspace(u8),
    CloseWindow,
    Lock,
    Screenshot,
    ScreenshotRegion,
    ToggleOverview,
    VolumeUp,
    VolumeDown,
    VolumeMute,
    Custom(String),
}

struct Binding {
    id: HotkeyId,
    action: HotkeyAction,
}

/// Holds every registered chord -> action binding and matches incoming key
/// presses against them.
pub struct HotkeyRegistry {
    bindings: HashMap<KeyChord, Binding>,
    next_id: HotkeyId,
}

impl HotkeyRegistry {
    /// An empty registry with none of the default MITOS shortcuts bound.
    pub fn empty() -> Self {
        HotkeyRegistry {
            bindings: HashMap::new(),
            next_id: 1,
        }
    }

    /// A registry pre-populated with the default MITOS desktop shortcuts.
    pub fn with_defaults() -> Self {
        let mut reg = Self::empty();
        reg.register_defaults();
        reg
    }

    fn register_defaults(&mut self) {
        use HotkeyAction::*;
        let super_key = Modifiers::SUPER;
        let super_shift = Modifiers::SUPER | Modifiers::SHIFT;

        self.register(KeyChord::new(KeyCode::D, super_key), ShowDesktop);
        self.register(KeyChord::new(KeyCode::TAB, super_key), SwitchWindow);
        self.register(KeyChord::new(KeyCode::TAB, super_shift), SwitchWindowReverse);
        self.register(KeyChord::new(KeyCode::L, super_key), Lock);
        self.register(KeyChord::new(KeyCode::Q, super_shift), CloseWindow);
        self.register(KeyChord::new(KeyCode::SPACE, super_key), ShowLauncher);
        self.register(KeyChord::new(KeyCode::UP, super_key), ToggleOverview);
        self.register(KeyChord::new(KeyCode::PRINT_SCREEN, Modifiers::empty()), Screenshot);
        self.register(KeyChord::new(KeyCode::PRINT_SCREEN, Modifiers::SHIFT), ScreenshotRegion);
        self.register(KeyChord::new(KeyCode::VOLUME_UP, Modifiers::empty()), VolumeUp);
        self.register(KeyChord::new(KeyCode::VOLUME_DOWN, Modifiers::empty()), VolumeDown);
        self.register(KeyChord::new(KeyCode::MUTE, Modifiers::empty()), VolumeMute);
        for n in 1..=9u8 {
            if let Some(digit) = KeyCode::digit(n) {
                self.register(KeyChord::new(digit, super_key), SwitchWorkspace(n));
            }
        }
    }

    /// Bind `chord` to `action`, returning an id that can later be passed to
    /// [`HotkeyRegistry::unregister`]. Rebinding an already-bound chord
    /// replaces the previous action.
    pub fn register(&mut self, chord: KeyChord, action: HotkeyAction) -> HotkeyId {
        let id = self.next_id;
        self.next_id += 1;
        self.bindings.insert(chord, Binding { id, action });
        id
    }

    pub fn unregister(&mut self, id: HotkeyId) {
        self.bindings.retain(|_, binding| binding.id != id);
    }

    pub fn unregister_chord(&mut self, chord: &KeyChord) {
        self.bindings.remove(chord);
    }

    pub fn is_bound(&self, chord: &KeyChord) -> bool {
        self.bindings.contains_key(chord)
    }

    /// Check whether a key event matches a registered chord. Only fires on
    /// the initial press (not release, not auto-repeat), which is standard
    /// for global shortcuts.
    pub fn feed(&self, event: &KeyEvent) -> Option<(HotkeyId, HotkeyAction)> {
        if event.state != KeyState::Pressed {
            return None;
        }
        let chord = KeyChord::new(event.key, event.modifiers);
        self.bindings.get(&chord).map(|b| (b.id, b.action.clone()))
    }
}

impl Default for HotkeyRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceId;
    use std::time::Duration;

    fn key_event(key: KeyCode, modifiers: Modifiers, state: KeyState) -> KeyEvent {
        KeyEvent {
            device: DeviceId::new(),
            time: Duration::from_secs(0),
            key,
            state,
            modifiers,
            repeat: false,
        }
    }

    #[test]
    fn default_shortcuts_fire_on_exact_match() {
        let reg = HotkeyRegistry::with_defaults();
        let ev = key_event(KeyCode::D, Modifiers::SUPER, KeyState::Pressed);
        let (_, action) = reg.feed(&ev).expect("Super+D should be bound");
        assert_eq!(action, HotkeyAction::ShowDesktop);
    }

    #[test]
    fn extra_modifier_does_not_match() {
        let reg = HotkeyRegistry::with_defaults();
        let ev = key_event(KeyCode::D, Modifiers::SUPER | Modifiers::SHIFT, KeyState::Pressed);
        assert!(reg.feed(&ev).is_none());
    }

    #[test]
    fn release_does_not_fire() {
        let reg = HotkeyRegistry::with_defaults();
        let ev = key_event(KeyCode::D, Modifiers::SUPER, KeyState::Released);
        assert!(reg.feed(&ev).is_none());
    }

    #[test]
    fn unregister_removes_binding() {
        let mut reg = HotkeyRegistry::empty();
        let chord = KeyChord::new(KeyCode::A, Modifiers::SUPER);
        let id = reg.register(chord, HotkeyAction::Custom("test".into()));
        assert!(reg.is_bound(&chord));
        reg.unregister(id);
        assert!(!reg.is_bound(&chord));
    }
}
