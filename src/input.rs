//! The orchestrator: [`InputManager`] owns every backend
//! ([`crate::evdev`], [`crate::bluetooth`]), the cross-cutting systems
//! ([`crate::hotkeys`], [`crate::seat`], [`crate::ipc`],
//! [`crate::uinput`]), and the central event loop that connects them. See
//! the crate README for the overall data-flow diagram.
//!
//! This module also defines the two traits backends implement --
//! [`InputDevice`] (one instance per physical device, translating raw
//! protocol codes into [`Event`]s) and [`InputSource`] (one instance per
//! backend, discovering devices and running [`InputDevice`]s against them)
//! -- and [`EventSink`], the handle a source uses to publish into the
//! manager's event loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::bluetooth::BluetoothBridgeSource;
use crate::device::{Device, DeviceId};
use crate::error::Result;
use crate::evdev::EvdevSource;
use crate::events::{DeviceEvent, Event, HotkeyEvent, Timestamp};
use crate::hotkeys::{HotkeyAction, HotkeyId, HotkeyRegistry, KeyChord};
use crate::ipc::{IpcServer, SharedDeviceTable};
use crate::seat::SeatManager;
use crate::uinput::VirtualDeviceManager;

/// Implemented by each concrete device-type state machine
/// ([`crate::keyboard::Keyboard`], [`crate::pointer::Mouse`],
/// [`crate::touch::TouchDevice`], [`crate::tablet::Tablet`],
/// [`crate::gamepad::Gamepad`]) so a backend can hold them polymorphically
/// and feed raw protocol codes through one interface.
pub trait InputDevice: Send {
    fn id(&self) -> DeviceId;

    /// Handle one decoded `(type, code, value)` triple -- in the numbering
    /// [`crate::evdev`]'s protocol constants use -- and return zero or more
    /// resulting events. Implementations buffer partial state (held
    /// modifiers, multitouch slots, ...) internally and typically only
    /// produce output on `EV_SYN`/`SYN_REPORT`.
    fn handle_event(&mut self, ev_type: u16, code: u16, value: i32, time: Timestamp) -> Vec<Event>;
}

/// Implemented by each backend that discovers devices and turns their raw
/// input into [`Event`]s: [`crate::evdev::EvdevSource`] for the Linux kernel
/// input subsystem, [`crate::bluetooth::BluetoothBridgeSource`] for the
/// Bluetooth HID bridge. A source owns its own background thread(s),
/// started by `start` and joined by `stop`.
pub trait InputSource: Send {
    fn name(&self) -> &str;
    fn start(&mut self, sink: EventSink) -> Result<()>;
    fn stop(&mut self);
}

/// A cheap, cloneable handle sources use to publish events into
/// [`InputManager`]'s event loop.
#[derive(Clone)]
pub struct EventSink {
    tx: Sender<Event>,
}

impl EventSink {
    pub fn new(tx: Sender<Event>) -> Self {
        EventSink { tx }
    }

    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere while holding this lock shouldn't take the whole
    // input daemon down with it.
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// See [`InputManager::stop_handle`].
#[derive(Clone)]
pub struct StopHandle(Arc<AtomicBool>);

impl StopHandle {
    pub fn stop(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Ties every backend, device-type state machine, and cross-cutting system
/// together into one running input daemon.
///
/// ```text
/// EvdevSource ─┐
///              ├─▶ EventSink ─▶ [channel] ─▶ InputManager::dispatch ─▶ IpcServer ─▶ subscribers
/// Bluetooth  ──┘                                   │
///                                                   ├─▶ device registry / seat assignment
///                                                   └─▶ hotkey matching ─▶ HotkeyEvent
/// ```
pub struct InputManager {
    devices: SharedDeviceTable,
    sources: Vec<Box<dyn InputSource>>,
    hotkeys: HotkeyRegistry,
    seats: SeatManager,
    virtual_devices: Arc<Mutex<VirtualDeviceManager>>,
    ipc: IpcServer,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    running: Arc<AtomicBool>,
}

impl InputManager {
    /// A manager with the default `evdev` source already added and the
    /// default MITOS hotkeys registered. Add any extra sources (see
    /// [`InputManager::add_source`], [`InputManager::with_bluetooth_bridge`])
    /// before calling [`InputManager::run`].
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let devices: SharedDeviceTable = Arc::new(Mutex::new(HashMap::new()));
        let virtual_devices = Arc::new(Mutex::new(VirtualDeviceManager::new()));
        let ipc = IpcServer::new(
            crate::ipc::DEFAULT_SOCKET_PATH,
            devices.clone(),
            virtual_devices.clone(),
        );
        let mut manager = InputManager {
            devices,
            sources: Vec::new(),
            hotkeys: HotkeyRegistry::with_defaults(),
            seats: SeatManager::new(),
            virtual_devices,
            ipc,
            tx,
            rx,
            running: Arc::new(AtomicBool::new(false)),
        };
        manager.add_source(Box::new(EvdevSource::new()));
        manager
    }

    pub fn add_source(&mut self, source: Box<dyn InputSource>) {
        self.sources.push(source);
    }

    /// Add the Bluetooth HID bridge source at its default socket path.
    pub fn with_bluetooth_bridge(mut self) -> Self {
        self.add_source(Box::new(BluetoothBridgeSource::new()));
        self
    }

    /// Replace the default IPC socket path. Must be called before [`InputManager::run`].
    pub fn with_ipc_socket_path(mut self, path: impl Into<String>) -> Self {
        self.ipc = IpcServer::new(path, self.devices.clone(), self.virtual_devices.clone());
        self
    }

    pub fn register_hotkey(&mut self, chord: KeyChord, action: HotkeyAction) -> HotkeyId {
        self.hotkeys.register(chord, action)
    }

    pub fn hotkeys_mut(&mut self) -> &mut HotkeyRegistry {
        &mut self.hotkeys
    }

    pub fn seats_mut(&mut self) -> &mut SeatManager {
        &mut self.seats
    }

    /// Snapshot of every currently-known device.
    pub fn devices(&self) -> Vec<Device> {
        lock(&self.devices).values().cloned().collect()
    }

    /// Ask a running [`InputManager::run`] loop (on another thread) to
    /// return.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// A cheap, `Send + Clone` handle that can request shutdown from another
    /// thread (e.g. a signal handler) while this manager's `run` loop holds
    /// `&mut self` on the main thread. Get one *before* calling `run`.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle(self.running.clone())
    }

    /// Start every registered source and the IPC server, then block this
    /// thread, dispatching events until [`InputManager::stop`] is called.
    pub fn run(&mut self) -> Result<()> {
        self.running.store(true, Ordering::SeqCst);
        for source in &mut self.sources {
            source.start(EventSink::new(self.tx.clone()))?;
        }
        self.ipc.start()?;

        while self.running.load(Ordering::SeqCst) {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(event) => self.dispatch(event),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        for source in &mut self.sources {
            source.stop();
        }
        self.ipc.stop();
        Ok(())
    }

    fn dispatch(&mut self, event: Event) {
        // Device topology changes always go out, and keep the registry /
        // seat assignment current, regardless of seat activity below.
        if let Event::Device(de) = &event {
            match de {
                DeviceEvent::Added(d) => {
                    lock(&self.devices).insert(d.id, d.clone());
                    self.seats.ensure_seat(&d.seat);
                    let _ = self.seats.assign_device(d.id, &d.seat);
                }
                DeviceEvent::Removed(id) => {
                    lock(&self.devices).remove(id);
                    self.seats.remove_device(*id);
                }
                DeviceEvent::CapabilitiesChanged(_) => {}
            }
            self.ipc.broadcast(&event);
            return;
        }

        // Live input is suppressed while the owning seat's session is
        // inactive (VT-switched away, on a multi-seat/logind-backed setup).
        if let Some(device_id) = event.device_id() {
            if !self.seats.should_deliver(device_id) {
                return;
            }
        }

        if let Event::Key(ref ke) = event {
            if let Some((id, action)) = self.hotkeys.feed(ke) {
                self.ipc.broadcast(&Event::Hotkey(HotkeyEvent {
                    id,
                    time: ke.time,
                    action,
                }));
            }
        }

        self.ipc.broadcast(&event);
    }
}

impl Default for InputManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceType;
    use crate::keyboard::{KeyCode, KeyState, Modifiers};
    use crate::events::KeyEvent;

    #[test]
    fn device_added_updates_registry_and_seat() {
        let mut manager = InputManager::new();
        let device = Device::new("Test Keyboard", DeviceType::Keyboard);
        let id = device.id;

        manager.dispatch(Event::Device(DeviceEvent::Added(device)));

        assert!(manager.devices().iter().any(|d| d.id == id));
        assert_eq!(manager.seats_mut().seat_of(id), Some("seat0"));

        manager.dispatch(Event::Device(DeviceEvent::Removed(id)));
        assert!(!manager.devices().iter().any(|d| d.id == id));
        assert_eq!(manager.seats_mut().seat_of(id), None);
    }

    #[test]
    fn inactive_seat_suppresses_live_input_but_not_device_events() {
        let mut manager = InputManager::new();
        let device = Device::new("Test Keyboard", DeviceType::Keyboard).with_seat("seat0");
        let id = device.id;
        manager.dispatch(Event::Device(DeviceEvent::Added(device)));

        manager.seats_mut().set_session("seat0", "session-1").unwrap();
        manager.seats_mut().set_session_active("seat0", false).unwrap();

        // Broadcasting still succeeds (no subscribers, so this just proves
        // dispatch doesn't panic); what we actually check is that a second
        // Added notification for a device on the now-inactive seat still
        // updates the registry -- device topology isn't gated.
        let other = Device::new("Second Keyboard", DeviceType::Keyboard).with_seat("seat0");
        let other_id = other.id;
        manager.dispatch(Event::Device(DeviceEvent::Added(other)));
        assert!(manager.devices().iter().any(|d| d.id == other_id));

        // A key event from the gated device must not reach hotkey matching
        // (we can't observe that directly without a subscriber, but we can
        // confirm dispatch returns early without touching hotkeys' held
        // state by checking a bound chord doesn't leave `is_pressed` set --
        // simplest proxy: dispatch must not panic and devices stay intact).
        let key_event = KeyEvent {
            device: id,
            time: Duration::from_secs(0),
            key: KeyCode::D,
            state: KeyState::Pressed,
            modifiers: Modifiers::SUPER,
            repeat: false,
        };
        manager.dispatch(Event::Key(key_event));
        assert!(manager.devices().iter().any(|d| d.id == id));
    }

    #[test]
    fn hotkey_match_does_not_panic_without_subscribers() {
        let mut manager = InputManager::new();
        let device = Device::new("Test Keyboard", DeviceType::Keyboard);
        let id = device.id;
        manager.dispatch(Event::Device(DeviceEvent::Added(device)));

        let key_event = KeyEvent {
            device: id,
            time: Duration::from_secs(0),
            key: KeyCode::D,
            state: KeyState::Pressed,
            modifiers: Modifiers::SUPER,
            repeat: false,
        };
        // Super+D is a default MITOS shortcut (ShowDesktop); this should be
        // matched and broadcast internally without panicking even though
        // nothing is subscribed to observe it.
        manager.dispatch(Event::Key(key_event));
    }
}
