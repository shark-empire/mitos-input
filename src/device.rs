//! Device identity and metadata: [`Device`], [`DeviceId`], [`DeviceType`],
//! [`DeviceCapabilities`].
//!
//! This module also defines [`bitflags_type!`], a tiny hand-rolled
//! replacement for the `bitflags` crate. It's used here and in
//! [`crate::keyboard`], [`crate::pointer`], and [`crate::tablet`] so the
//! whole crate needs exactly one external dependency (`libc`, for raw
//! syscalls in [`crate::evdev`] / [`crate::uinput`]).

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Defines a small bitflags-style newtype: named constants, bitwise
/// operators, and `contains`/`insert`/`remove`/`toggle`. Deliberately
/// minimal -- just enough for the flag types in this crate.
#[macro_export]
macro_rules! bitflags_type {
    (
        $(#[$meta:meta])*
        pub struct $name:ident : $ty:ty {
            $( $(#[$fmeta:meta])* const $flag:ident = $value:expr; )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name($ty);

        #[allow(dead_code)]
        impl $name {
            $( $(#[$fmeta])* pub const $flag: $name = $name($value); )*

            /// The empty flag set.
            pub const fn empty() -> Self { $name(0) }
            /// The raw bit pattern.
            pub const fn bits(&self) -> $ty { self.0 }
            /// Build a flag set directly from raw bits (no masking).
            pub const fn from_bits(bits: $ty) -> Self { $name(bits) }
            /// Whether every bit set in `other` is also set in `self`.
            pub const fn contains(&self, other: Self) -> bool { (self.0 & other.0) == other.0 }
            /// Whether no bits are set.
            pub const fn is_empty(&self) -> bool { self.0 == 0 }
            /// Set the bits in `other`.
            pub fn insert(&mut self, other: Self) { self.0 |= other.0; }
            /// Clear the bits in `other`.
            pub fn remove(&mut self, other: Self) { self.0 &= !other.0; }
            /// Flip the bits in `other`.
            pub fn toggle(&mut self, other: Self) { self.0 ^= other.0; }
        }

        impl std::ops::BitOr for $name {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self { $name(self.0 | rhs.0) }
        }
        impl std::ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: Self) { self.0 |= rhs.0; }
        }
        impl std::ops::BitAnd for $name {
            type Output = Self;
            fn bitand(self, rhs: Self) -> Self { $name(self.0 & rhs.0) }
        }
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}(0x{:x})", stringify!($name), self.0)
            }
        }
    };
}

static NEXT_DEVICE_ID: AtomicU64 = AtomicU64::new(1);

/// Opaque, process-unique identifier for a device.
///
/// IDs are allocated in-process (via [`DeviceId::new`]) and are stable for
/// the lifetime of the device, but are **not** stable across restarts --
/// use [`Device::sys_path`] / [`Device::node_path`] for persistent identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(u64);

impl DeviceId {
    /// Allocate a fresh, process-unique device id.
    pub fn new() -> Self {
        DeviceId(NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// The raw numeric value, e.g. for wire encoding.
    pub const fn raw(&self) -> u64 {
        self.0
    }

    /// Reconstruct a `DeviceId` from a raw value (e.g. decoded off the wire).
    pub const fn from_raw(raw: u64) -> Self {
        DeviceId(raw)
    }
}

impl Default for DeviceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dev:{}", self.0)
    }
}

/// Broad classification of a device, used for routing and for clients that
/// want to filter by kind without inspecting capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceType {
    Keyboard,
    Mouse,
    Touchpad,
    Touchscreen,
    Tablet,
    Gamepad,
    Switch,
    Unknown,
}

impl fmt::Display for DeviceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DeviceType::Keyboard => "keyboard",
            DeviceType::Mouse => "mouse",
            DeviceType::Touchpad => "touchpad",
            DeviceType::Touchscreen => "touchscreen",
            DeviceType::Tablet => "tablet",
            DeviceType::Gamepad => "gamepad",
            DeviceType::Switch => "switch",
            DeviceType::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

bitflags_type! {
    /// What kinds of events a device can produce, independent of its
    /// [`DeviceType`] (a tablet and a touchscreen might both report
    /// `PRESSURE`, for instance).
    pub struct DeviceCapabilities: u32 {
        const KEYS         = 0b0000_0000_0001;
        const REL_MOTION   = 0b0000_0000_0010;
        const ABS_MOTION   = 0b0000_0000_0100;
        const BUTTONS      = 0b0000_0000_1000;
        const SCROLL       = 0b0000_0001_0000;
        const MULTITOUCH   = 0b0000_0010_0000;
        const PRESSURE     = 0b0000_0100_0000;
        const TILT         = 0b0000_1000_0000;
        const RUMBLE       = 0b0001_0000_0000;
        const LEDS         = 0b0010_0000_0000;
        const SWITCHES     = 0b0100_0000_0000;
        const VIRTUAL      = 0b1000_0000_0000;
    }
}

/// Metadata describing a single input device, real or virtual.
#[derive(Debug, Clone)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
    pub device_type: DeviceType,
    pub capabilities: DeviceCapabilities,
    pub vendor_id: u16,
    pub product_id: u16,
    pub bus_type: u16,
    /// `/sys/class/input/...` path, if known.
    pub sys_path: Option<String>,
    /// `/dev/input/eventN` (or similar) path, if backed by a real node.
    pub node_path: Option<String>,
    /// Logical seat this device belongs to (see [`crate::seat`]).
    pub seat: String,
}

impl Device {
    /// Start building a new [`Device`] with a fresh id and no capabilities.
    pub fn new(name: impl Into<String>, device_type: DeviceType) -> Self {
        Device {
            id: DeviceId::new(),
            name: name.into(),
            device_type,
            capabilities: DeviceCapabilities::empty(),
            vendor_id: 0,
            product_id: 0,
            bus_type: 0,
            sys_path: None,
            node_path: None,
            seat: "seat0".to_string(),
        }
    }

    pub fn with_capabilities(mut self, caps: DeviceCapabilities) -> Self {
        self.capabilities = caps;
        self
    }

    pub fn with_ids(mut self, bus_type: u16, vendor_id: u16, product_id: u16) -> Self {
        self.bus_type = bus_type;
        self.vendor_id = vendor_id;
        self.product_id = product_id;
        self
    }

    pub fn with_node_path(mut self, path: impl Into<String>) -> Self {
        self.node_path = Some(path.into());
        self
    }

    pub fn with_seat(mut self, seat: impl Into<String>) -> Self {
        self.seat = seat.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_are_unique_and_monotonic() {
        let a = DeviceId::new();
        let b = DeviceId::new();
        assert_ne!(a, b);
        assert!(b.raw() > a.raw());
    }

    #[test]
    fn capabilities_bitops() {
        let mut caps = DeviceCapabilities::KEYS | DeviceCapabilities::REL_MOTION;
        assert!(caps.contains(DeviceCapabilities::KEYS));
        assert!(!caps.contains(DeviceCapabilities::PRESSURE));
        caps.insert(DeviceCapabilities::PRESSURE);
        assert!(caps.contains(DeviceCapabilities::PRESSURE));
        caps.remove(DeviceCapabilities::KEYS);
        assert!(!caps.contains(DeviceCapabilities::KEYS));
    }
}
