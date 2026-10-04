//! Crate-wide error type.
//!
//! Every fallible operation in `mitos-input` returns [`Result<T>`], a thin
//! alias over `std::result::Result<T, InputError>`. We hand-roll this rather
//! than pulling in `thiserror` to keep the dependency surface at just `libc`.

use std::fmt;
use std::io;

use crate::device::DeviceId;

/// Errors produced anywhere in `mitos-input`.
#[derive(Debug)]
pub enum InputError {
    /// A generic I/O failure (opening/reading/writing a device node, socket, etc).
    Io(io::Error),

    /// A lookup by [`DeviceId`] found nothing.
    DeviceNotFound(DeviceId),

    /// Opening a specific device node failed.
    DeviceOpen { path: String, source: io::Error },

    /// A `libc::ioctl` call failed. `call` is the ioctl's symbolic name
    /// (e.g. `"EVIOCGBIT"`) for diagnostics; `errno` is `errno` at the time.
    Ioctl { call: &'static str, errno: i32 },

    /// The requested capability/feature isn't supported by this device or backend.
    Unsupported(String),

    /// Failure creating or writing to a virtual (`uinput`) device.
    UInput(String),

    /// Failure in the Bluetooth HID bridge.
    Bluetooth(String),

    /// Failure in seat/session management.
    Seat(String),

    /// Failure in the IPC server or a client connection.
    Ipc(String),

    /// A malformed message was received over the wire protocol.
    Protocol(String),

    /// The internal event channel between a source and the [`crate::input::InputManager`]
    /// event loop was closed (its receiver was dropped).
    ChannelClosed,

    /// Catch-all for anything that doesn't need its own variant.
    Other(String),
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputError::Io(e) => write!(f, "I/O error: {e}"),
            InputError::DeviceNotFound(id) => write!(f, "device not found: {id}"),
            InputError::DeviceOpen { path, source } => {
                write!(f, "failed to open device '{path}': {source}")
            }
            InputError::Ioctl { call, errno } => {
                write!(f, "ioctl {call} failed (errno {errno})")
            }
            InputError::Unsupported(s) => write!(f, "unsupported: {s}"),
            InputError::UInput(s) => write!(f, "uinput error: {s}"),
            InputError::Bluetooth(s) => write!(f, "bluetooth bridge error: {s}"),
            InputError::Seat(s) => write!(f, "seat error: {s}"),
            InputError::Ipc(s) => write!(f, "ipc error: {s}"),
            InputError::Protocol(s) => write!(f, "protocol error: {s}"),
            InputError::ChannelClosed => write!(f, "internal event channel closed"),
            InputError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for InputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InputError::Io(e) => Some(e),
            InputError::DeviceOpen { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for InputError {
    fn from(e: io::Error) -> Self {
        InputError::Io(e)
    }
}

/// Build an [`InputError::Ioctl`] from the current `errno`, for use right
/// after a `libc::ioctl` call returns a negative result.
pub fn ioctl_error(call: &'static str) -> InputError {
    InputError::Ioctl {
        call,
        errno: io::Error::last_os_error().raw_os_error().unwrap_or(-1),
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, InputError>;
