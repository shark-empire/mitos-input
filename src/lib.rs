//! `mitos-input`: the input device management subsystem for the MITOS
//! desktop environment.
//!
//! Owns everything from the raw Linux kernel input protocol up to a typed,
//! cross-process event stream: keyboards, pointers, touch, tablets and
//! gamepads; the `evdev` and `uinput` backends; a Bluetooth HID bridge;
//! global hotkeys and multi-finger gestures; seat/session gating; and the
//! IPC server other MITOS components (the compositor, accessibility tools,
//! ...) use to subscribe to events and inject synthetic input.
//!
//! See `README.md` for the architecture diagram and IPC protocol reference.
//! [`input::InputManager`] is the entry point; the `mitos-input` binary
//! (`src/main.rs`) is a thin wrapper around it.

pub mod bluetooth;
pub mod device;
pub mod error;
pub mod evdev;
pub mod events;
pub mod gamepad;
pub mod gestures;
pub mod hotkeys;
pub mod input;
pub mod ipc;
pub mod keyboard;
pub mod pointer;
pub mod seat;
pub mod tablet;
pub mod touch;
pub mod uinput;

pub use error::{InputError, Result};
pub use events::Event;
pub use input::InputManager;
