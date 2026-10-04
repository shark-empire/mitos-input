//! The event model: every concrete event payload ([`KeyEvent`],
//! [`TouchEvent`], [`GamepadEvent`], ...) plus the unifying [`Event`] enum
//! that flows from device backends ([`crate::evdev`], [`crate::bluetooth`])
//! through [`crate::input::InputManager`] to IPC subscribers.
//!
//! [`PointerEvent`] is defined in [`crate::pointer`] (it's central enough to
//! that module to live there) and re-exported here so `events::*` still
//! covers the full event surface.

use crate::device::{Device, DeviceId};
use crate::gamepad::{GamepadAxis, GamepadButton};
use crate::gestures::SwipeDirection;
use crate::hotkeys::{HotkeyAction, HotkeyId};
use crate::keyboard::{KeyCode, KeyState, Modifiers};
pub use crate::pointer::PointerEvent;
use crate::touch::TouchPoint;

/// Monotonic timestamp (time since an arbitrary fixed point, typically
/// `CLOCK_MONOTONIC`/`Instant::now()` at startup) attached to every event.
pub type Timestamp = std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    pub device: DeviceId,
    pub time: Timestamp,
    pub key: KeyCode,
    pub state: KeyState,
    pub modifiers: Modifiers,
    pub repeat: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TouchEvent {
    pub device: DeviceId,
    pub time: Timestamp,
    pub phase: TouchPhase,
    pub point: TouchPoint,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabletEvent {
    pub device: DeviceId,
    pub time: Timestamp,
    pub x: f64,
    pub y: f64,
    pub pressure: f64,
    pub tilt_x: f32,
    pub tilt_y: f32,
    pub in_proximity: bool,
    pub in_contact: bool,
    /// Raw [`crate::tablet::PenButtons`] bits.
    pub buttons: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GamepadEvent {
    Button {
        device: DeviceId,
        time: Timestamp,
        button: GamepadButton,
        pressed: bool,
    },
    Axis {
        device: DeviceId,
        time: Timestamp,
        axis: GamepadAxis,
        value: f32,
    },
    Connected {
        device: DeviceId,
    },
    Disconnected {
        device: DeviceId,
    },
}

#[derive(Debug, Clone)]
pub enum DeviceEvent {
    Added(Device),
    Removed(DeviceId),
    CapabilitiesChanged(DeviceId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum GestureEvent {
    Swipe {
        device: DeviceId,
        time: Timestamp,
        fingers: u8,
        direction: SwipeDirection,
        dx: f64,
        dy: f64,
    },
    Pinch {
        device: DeviceId,
        time: Timestamp,
        fingers: u8,
        scale: f64,
        rotation: f64,
    },
    Tap {
        device: DeviceId,
        time: Timestamp,
        fingers: u8,
        x: f64,
        y: f64,
    },
    Hold {
        device: DeviceId,
        time: Timestamp,
        fingers: u8,
        x: f64,
        y: f64,
    },
}

#[derive(Debug, Clone)]
pub struct HotkeyEvent {
    pub id: HotkeyId,
    pub time: Timestamp,
    pub action: HotkeyAction,
}

/// Every event kind `mitos-input` can produce, in one envelope. This is what
/// travels over the internal channel from sources to
/// [`crate::input::InputManager`], and what gets broadcast to IPC
/// subscribers.
#[derive(Debug, Clone)]
pub enum Event {
    Key(KeyEvent),
    Pointer(PointerEvent),
    Touch(TouchEvent),
    Tablet(TabletEvent),
    Gamepad(GamepadEvent),
    Device(DeviceEvent),
    Gesture(GestureEvent),
    Hotkey(HotkeyEvent),
}

/// Coarse category of an [`Event`], for subscription filtering without
/// matching on the full enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Key,
    Pointer,
    Touch,
    Tablet,
    Gamepad,
    Device,
    Gesture,
    Hotkey,
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self {
            Event::Key(_) => EventKind::Key,
            Event::Pointer(_) => EventKind::Pointer,
            Event::Touch(_) => EventKind::Touch,
            Event::Tablet(_) => EventKind::Tablet,
            Event::Gamepad(_) => EventKind::Gamepad,
            Event::Device(_) => EventKind::Device,
            Event::Gesture(_) => EventKind::Gesture,
            Event::Hotkey(_) => EventKind::Hotkey,
        }
    }

    /// The device that produced this event, if applicable (device
    /// add/remove events aren't tied to a single source device in the same
    /// way, though `Added`/`CapabilitiesChanged` do carry one).
    pub fn device_id(&self) -> Option<DeviceId> {
        match self {
            Event::Key(e) => Some(e.device),
            Event::Pointer(e) => Some(pointer_device_id(e)),
            Event::Touch(e) => Some(e.device),
            Event::Tablet(e) => Some(e.device),
            Event::Gamepad(e) => Some(gamepad_device_id(e)),
            Event::Device(DeviceEvent::Added(d)) => Some(d.id),
            Event::Device(DeviceEvent::Removed(id)) => Some(*id),
            Event::Device(DeviceEvent::CapabilitiesChanged(id)) => Some(*id),
            Event::Gesture(e) => Some(gesture_device_id(e)),
            Event::Hotkey(_) => None,
        }
    }
}

fn pointer_device_id(e: &PointerEvent) -> DeviceId {
    match e {
        PointerEvent::Motion { device, .. }
        | PointerEvent::MotionAbsolute { device, .. }
        | PointerEvent::Button { device, .. }
        | PointerEvent::Scroll { device, .. } => *device,
    }
}

fn gamepad_device_id(e: &GamepadEvent) -> DeviceId {
    match e {
        GamepadEvent::Button { device, .. }
        | GamepadEvent::Axis { device, .. }
        | GamepadEvent::Connected { device }
        | GamepadEvent::Disconnected { device } => *device,
    }
}

fn gesture_device_id(e: &GestureEvent) -> DeviceId {
    match e {
        GestureEvent::Swipe { device, .. }
        | GestureEvent::Pinch { device, .. }
        | GestureEvent::Tap { device, .. }
        | GestureEvent::Hold { device, .. } => *device,
    }
}
