//! Seat management and session association.
//!
//! A *seat* is a logical grouping of input devices (plus outputs, in a full
//! desktop stack) that belongs to one user at a time -- `seat0` on a typical
//! single-user machine, more on multi-seat setups. Each seat can have a
//! *session* associated with it, and that session can be active or inactive
//! (e.g. the user switched to another virtual terminal). While a seat's
//! session is inactive, [`SeatManager::should_deliver`] reports `false` for
//! its devices so [`crate::input::InputManager`] stops forwarding their
//! events to clients.
//!
//! This module deliberately doesn't talk to `logind` or `libseat` itself.
//! The embedding compositor already receives those enable/disable
//! notifications; it simply calls [`SeatManager::set_session_active`] from
//! them. That keeps D-Bus/libseat out of this crate's dependency surface
//! while still giving the compositor complete control over when input flows.

use std::collections::{HashMap, HashSet};

use crate::device::DeviceId;
use crate::error::{InputError, Result};

/// The seat every device belongs to unless told otherwise.
pub const DEFAULT_SEAT: &str = "seat0";

#[derive(Debug, Clone)]
pub struct Seat {
    pub name: String,
    pub devices: HashSet<DeviceId>,
    /// Identifier of the session that currently owns this seat, if any.
    pub session: Option<String>,
    /// Whether that session may currently receive input. Irrelevant (and
    /// treated as `true`) while no session is associated.
    pub active: bool,
}

impl Seat {
    fn new(name: impl Into<String>) -> Self {
        Seat {
            name: name.into(),
            devices: HashSet::new(),
            session: None,
            active: true,
        }
    }
}

pub struct SeatManager {
    seats: HashMap<String, Seat>,
    device_seat: HashMap<DeviceId, String>,
}

impl SeatManager {
    /// A manager containing just the default seat, [`DEFAULT_SEAT`].
    pub fn new() -> Self {
        let mut seats = HashMap::new();
        seats.insert(DEFAULT_SEAT.to_string(), Seat::new(DEFAULT_SEAT));
        SeatManager {
            seats,
            device_seat: HashMap::new(),
        }
    }

    /// Create a new, empty seat. Errors if one with that name already exists.
    pub fn create_seat(&mut self, name: &str) -> Result<()> {
        if self.seats.contains_key(name) {
            return Err(InputError::Seat(format!("seat already exists: {name}")));
        }
        self.seats.insert(name.to_string(), Seat::new(name));
        Ok(())
    }

    /// Create the seat if it doesn't exist yet; no-op otherwise.
    pub fn ensure_seat(&mut self, name: &str) {
        self.seats
            .entry(name.to_string())
            .or_insert_with(|| Seat::new(name));
    }

    /// Remove a seat, moving any devices on it back to [`DEFAULT_SEAT`].
    /// The default seat itself can't be removed.
    pub fn remove_seat(&mut self, name: &str) -> Result<()> {
        if name == DEFAULT_SEAT {
            return Err(InputError::Seat("cannot remove the default seat".to_string()));
        }
        let removed = self
            .seats
            .remove(name)
            .ok_or_else(|| InputError::Seat(format!("no such seat: {name}")))?;
        for device in removed.devices {
            self.device_seat.insert(device, DEFAULT_SEAT.to_string());
            if let Some(default) = self.seats.get_mut(DEFAULT_SEAT) {
                default.devices.insert(device);
            }
        }
        Ok(())
    }

    /// Assign `device` to `seat`, moving it off whichever seat it was on.
    /// The seat must already exist (see [`SeatManager::ensure_seat`]).
    pub fn assign_device(&mut self, device: DeviceId, seat: &str) -> Result<()> {
        let target = self
            .seats
            .get_mut(seat)
            .ok_or_else(|| InputError::Seat(format!("no such seat: {seat}")))?;
        target.devices.insert(device);

        if let Some(previous) = self.device_seat.insert(device, seat.to_string()) {
            if previous != seat {
                if let Some(old) = self.seats.get_mut(&previous) {
                    old.devices.remove(&device);
                }
            }
        }
        Ok(())
    }

    /// Forget a device entirely (e.g. on unplug).
    pub fn remove_device(&mut self, device: DeviceId) {
        if let Some(seat) = self.device_seat.remove(&device) {
            if let Some(s) = self.seats.get_mut(&seat) {
                s.devices.remove(&device);
            }
        }
    }

    pub fn seat_of(&self, device: DeviceId) -> Option<&str> {
        self.device_seat.get(&device).map(|s| s.as_str())
    }

    pub fn seat(&self, name: &str) -> Option<&Seat> {
        self.seats.get(name)
    }

    /// Seat names in sorted order (deterministic for listings/tests).
    pub fn seat_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.seats.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// Associate a session with a seat. Leaves the seat's active flag as-is.
    pub fn set_session(&mut self, seat: &str, session: impl Into<String>) -> Result<()> {
        let s = self.seat_mut(seat)?;
        s.session = Some(session.into());
        Ok(())
    }

    /// Drop the seat's session association; the seat becomes ungated again.
    pub fn clear_session(&mut self, seat: &str) -> Result<()> {
        let s = self.seat_mut(seat)?;
        s.session = None;
        s.active = true;
        Ok(())
    }

    /// Mark the seat's session active or inactive -- call this from the
    /// compositor's libseat enable/disable (or logind pause/resume) hooks.
    pub fn set_session_active(&mut self, seat: &str, active: bool) -> Result<()> {
        let s = self.seat_mut(seat)?;
        s.active = active;
        Ok(())
    }

    /// Whether the seat may currently receive input. A seat with no session
    /// associated (or an unknown seat) is never gated.
    pub fn is_seat_active(&self, seat: &str) -> bool {
        match self.seats.get(seat) {
            Some(s) if s.session.is_some() => s.active,
            _ => true,
        }
    }

    /// Whether events from `device` should currently reach clients. Devices
    /// not assigned to any seat are never gated.
    pub fn should_deliver(&self, device: DeviceId) -> bool {
        match self.seat_of(device) {
            Some(seat) => self.is_seat_active(seat),
            None => true,
        }
    }

    fn seat_mut(&mut self, name: &str) -> Result<&mut Seat> {
        self.seats
            .get_mut(name)
            .ok_or_else(|| InputError::Seat(format!("no such seat: {name}")))
    }
}

impl Default for SeatManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_seat_exists_and_devices_can_be_assigned() {
        let mut seats = SeatManager::new();
        let dev = DeviceId::new();
        assert!(seats.seat(DEFAULT_SEAT).is_some());
        seats.assign_device(dev, DEFAULT_SEAT).unwrap();
        assert_eq!(seats.seat_of(dev), Some(DEFAULT_SEAT));
        assert!(seats.seat(DEFAULT_SEAT).unwrap().devices.contains(&dev));
    }

    #[test]
    fn moving_a_device_removes_it_from_the_old_seat() {
        let mut seats = SeatManager::new();
        seats.create_seat("seat1").unwrap();
        let dev = DeviceId::new();
        seats.assign_device(dev, DEFAULT_SEAT).unwrap();
        seats.assign_device(dev, "seat1").unwrap();
        assert_eq!(seats.seat_of(dev), Some("seat1"));
        assert!(!seats.seat(DEFAULT_SEAT).unwrap().devices.contains(&dev));
        assert!(seats.seat("seat1").unwrap().devices.contains(&dev));
    }

    #[test]
    fn unknown_seat_is_an_error() {
        let mut seats = SeatManager::new();
        let dev = DeviceId::new();
        assert!(seats.assign_device(dev, "nope").is_err());
        assert!(seats.set_session("nope", "s1").is_err());
        assert!(seats.create_seat(DEFAULT_SEAT).is_err());
    }

    #[test]
    fn removing_a_seat_falls_back_to_default() {
        let mut seats = SeatManager::new();
        seats.create_seat("seat1").unwrap();
        let dev = DeviceId::new();
        seats.assign_device(dev, "seat1").unwrap();
        seats.remove_seat("seat1").unwrap();
        assert_eq!(seats.seat_of(dev), Some(DEFAULT_SEAT));
        assert!(seats.remove_seat(DEFAULT_SEAT).is_err());
    }

    #[test]
    fn inactive_session_gates_delivery() {
        let mut seats = SeatManager::new();
        let dev = DeviceId::new();
        let unassigned = DeviceId::new();
        seats.assign_device(dev, DEFAULT_SEAT).unwrap();

        // No session associated: never gated, even if flagged inactive.
        seats.set_session_active(DEFAULT_SEAT, false).unwrap();
        assert!(seats.should_deliver(dev));
        seats.set_session_active(DEFAULT_SEAT, true).unwrap();

        seats.set_session(DEFAULT_SEAT, "session-1").unwrap();
        assert!(seats.should_deliver(dev));

        seats.set_session_active(DEFAULT_SEAT, false).unwrap();
        assert!(!seats.should_deliver(dev));
        assert!(seats.should_deliver(unassigned));

        seats.set_session_active(DEFAULT_SEAT, true).unwrap();
        assert!(seats.should_deliver(dev));

        seats.set_session_active(DEFAULT_SEAT, false).unwrap();
        seats.clear_session(DEFAULT_SEAT).unwrap();
        assert!(seats.should_deliver(dev));
    }
}
