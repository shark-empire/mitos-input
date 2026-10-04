//! Bluetooth HID bridge.
//!
//! `mitos-input` does not speak the Bluetooth HID-over-GATT/HID-Host
//! protocols itself -- that's the job of a separate `mitos-bluetooth`
//! service (pairing, BlueZ/D-Bus interaction, reconnection policy). This
//! module is the receiving end: a small Unix-socket protocol carrying
//! already-negotiated HID boot-protocol reports, which get diffed and fed
//! through the same [`crate::keyboard::Keyboard`] / [`crate::pointer::Mouse`]
//! state machines the evdev backend uses, so the rest of the system sees
//! identical [`Event`]s regardless of transport:
//!
//! ```text
//! Bluetooth Keyboard -> mitos-bluetooth -> (this module) -> KeyEvent -> ...
//! ```
//!
//! ## Wire format
//! One byte frame-type tag, then a fixed layout per type (all integers are
//! single bytes; there's no multi-byte integer in this protocol, so
//! endianness doesn't come up):
//! - `0x01` Connected: `mac[6] name_len[1] name[name_len] kind[1]` (`kind`: 0=keyboard, 1=mouse)
//! - `0x02` Disconnected: `mac[6]`
//! - `0x03` KeyboardReport: `mac[6] report[8]` (standard USB HID boot keyboard report)
//! - `0x04` MouseReport: `mac[6] buttons[1] dx[1,signed] dy[1,signed] wheel[1,signed]` (boot mouse report)

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::device::{Device, DeviceCapabilities, DeviceId, DeviceType};
use crate::error::{InputError, Result};
use crate::events::{DeviceEvent, Event, Timestamp};
use crate::input::{EventSink, InputSource};
use crate::ipc::bind_private_socket;
use crate::keyboard::{KeyCode, KeyState, Keyboard};
use crate::pointer::{Button, Mouse, ScrollAxis, ScrollSource};

pub const DEFAULT_SOCKET_PATH: &str = "/run/mitos/bluetooth-bridge.sock";

const FRAME_CONNECTED: u8 = 0x01;
const FRAME_DISCONNECTED: u8 = 0x02;
const FRAME_KEYBOARD_REPORT: u8 = 0x03;
const FRAME_MOUSE_REPORT: u8 = 0x04;

const KIND_MOUSE: u8 = 1;

type MacAddr = [u8; 6];

enum Frame {
    Connected { mac: MacAddr, name: String, kind: u8 },
    Disconnected { mac: MacAddr },
    KeyboardReport { mac: MacAddr, modifiers: u8, keys: [u8; 6] },
    MouseReport { mac: MacAddr, buttons: u8, dx: i8, dy: i8, wheel: i8 },
}

enum DeviceState {
    Keyboard {
        id: DeviceId,
        keyboard: Keyboard,
        last_modifiers: u8,
        last_keys: [u8; 6],
    },
    Mouse {
        id: DeviceId,
        mouse: Mouse,
        last_buttons: u8,
    },
}

impl DeviceState {
    fn id(&self) -> DeviceId {
        match self {
            DeviceState::Keyboard { id, .. } => *id,
            DeviceState::Mouse { id, .. } => *id,
        }
    }
}

type SharedDevices = Arc<Mutex<HashMap<MacAddr, DeviceState>>>;

/// Reads a `/dev/input`-independent stream of HID reports over a Unix
/// socket and bridges them into the normal [`Event`] flow.
pub struct BluetoothBridgeSource {
    socket_path: String,
    /// Unix permission bits for the socket file; owner-only (`0o600`) by
    /// default since anyone connected can inject keystrokes. The
    /// `mitos-bluetooth` companion process must run as the same user (or be
    /// granted access via a wider mode set deliberately) to connect.
    socket_mode: u32,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl BluetoothBridgeSource {
    pub fn new() -> Self {
        BluetoothBridgeSource {
            socket_path: DEFAULT_SOCKET_PATH.to_string(),
            socket_mode: 0o600,
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    pub fn with_socket_path(mut self, path: impl Into<String>) -> Self {
        self.socket_path = path.into();
        self
    }

    pub fn with_socket_mode(mut self, mode: u32) -> Self {
        self.socket_mode = mode;
        self
    }
}

impl Default for BluetoothBridgeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSource for BluetoothBridgeSource {
    fn name(&self) -> &str {
        "bluetooth-bridge"
    }

    fn start(&mut self, sink: EventSink) -> Result<()> {
        // Anyone who can connect here can inject keystrokes as if from a
        // paired keyboard, so the socket is owner-only unless widened.
        let listener = bind_private_socket(&self.socket_path, self.socket_mode).map_err(|e| {
            InputError::Bluetooth(format!("failed to listen on {}: {e}", self.socket_path))
        })?;

        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let devices: SharedDevices = Arc::new(Mutex::new(HashMap::new()));
        let start = Instant::now();

        self.thread = Some(thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        let sink = sink.clone();
                        let devices = devices.clone();
                        let running = running.clone();
                        thread::spawn(move || handle_connection(stream, sink, devices, running, start));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => break,
                }
            }
        }));
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
            // Only remove the socket if this instance actually bound it: if
            // `start` failed (e.g. another instance was already listening),
            // `thread` is still `None` here and there's nothing of ours to
            // clean up.
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

fn handle_connection(
    mut stream: UnixStream,
    sink: EventSink,
    devices: SharedDevices,
    running: Arc<AtomicBool>,
    start: Instant,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    while running.load(Ordering::SeqCst) {
        match read_frame(&mut stream) {
            Ok(Some(frame)) => {
                let time = start.elapsed();
                for ev in process_frame(frame, &devices, time) {
                    sink.send(ev);
                }
            }
            Ok(None) => break, // peer closed the connection cleanly
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
}

/// Reads one byte; distinguishes "stream closed before any new frame" (a
/// normal disconnect, `Ok(false)`) from a genuine I/O error.
fn read_or_eof(stream: &mut UnixStream, buf: &mut [u8]) -> io::Result<bool> {
    match stream.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

fn read_frame(stream: &mut UnixStream) -> io::Result<Option<Frame>> {
    let mut ty = [0u8; 1];
    if !read_or_eof(stream, &mut ty)? {
        return Ok(None);
    }
    match ty[0] {
        FRAME_CONNECTED => {
            let mut mac = [0u8; 6];
            stream.read_exact(&mut mac)?;
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf)?;
            let mut name_buf = vec![0u8; len_buf[0] as usize];
            stream.read_exact(&mut name_buf)?;
            let mut kind_buf = [0u8; 1];
            stream.read_exact(&mut kind_buf)?;
            Ok(Some(Frame::Connected {
                mac,
                name: String::from_utf8_lossy(&name_buf).into_owned(),
                kind: kind_buf[0],
            }))
        }
        FRAME_DISCONNECTED => {
            let mut mac = [0u8; 6];
            stream.read_exact(&mut mac)?;
            Ok(Some(Frame::Disconnected { mac }))
        }
        FRAME_KEYBOARD_REPORT => {
            let mut mac = [0u8; 6];
            stream.read_exact(&mut mac)?;
            let mut report = [0u8; 8];
            stream.read_exact(&mut report)?;
            Ok(Some(Frame::KeyboardReport {
                mac,
                modifiers: report[0],
                keys: [report[2], report[3], report[4], report[5], report[6], report[7]],
            }))
        }
        FRAME_MOUSE_REPORT => {
            let mut mac = [0u8; 6];
            stream.read_exact(&mut mac)?;
            let mut rep = [0u8; 4];
            stream.read_exact(&mut rep)?;
            Ok(Some(Frame::MouseReport {
                mac,
                buttons: rep[0],
                dx: rep[1] as i8,
                dy: rep[2] as i8,
                wheel: rep[3] as i8,
            }))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown bluetooth bridge frame type {other:#x}"),
        )),
    }
}

fn process_frame(frame: Frame, devices: &SharedDevices, time: Timestamp) -> Vec<Event> {
    let mut map = match devices.lock() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    match frame {
        Frame::Connected { mac, name, kind } => {
            let id = DeviceId::new();
            let (device_type, caps) = if kind == KIND_MOUSE {
                (
                    DeviceType::Mouse,
                    DeviceCapabilities::REL_MOTION | DeviceCapabilities::BUTTONS | DeviceCapabilities::SCROLL,
                )
            } else {
                (DeviceType::Keyboard, DeviceCapabilities::KEYS)
            };
            let display_name = if name.is_empty() {
                "Bluetooth HID device".to_string()
            } else {
                name
            };
            let mut info = Device::new(display_name, device_type)
                .with_capabilities(caps)
                .with_seat("seat0");
            info.id = id;

            if kind == KIND_MOUSE {
                map.insert(
                    mac,
                    DeviceState::Mouse {
                        id,
                        mouse: Mouse::new(id),
                        last_buttons: 0,
                    },
                );
            } else {
                map.insert(
                    mac,
                    DeviceState::Keyboard {
                        id,
                        keyboard: Keyboard::new(id),
                        last_modifiers: 0,
                        last_keys: [0; 6],
                    },
                );
            }
            vec![Event::Device(DeviceEvent::Added(info))]
        }
        Frame::Disconnected { mac } => match map.remove(&mac) {
            Some(state) => vec![Event::Device(DeviceEvent::Removed(state.id()))],
            None => Vec::new(),
        },
        Frame::KeyboardReport { mac, modifiers, keys } => match map.get_mut(&mac) {
            Some(DeviceState::Keyboard {
                keyboard,
                last_modifiers,
                last_keys,
                ..
            }) => diff_keyboard_report(keyboard, last_modifiers, last_keys, modifiers, keys, time),
            _ => Vec::new(),
        },
        Frame::MouseReport {
            mac,
            buttons,
            dx,
            dy,
            wheel,
        } => match map.get_mut(&mac) {
            Some(DeviceState::Mouse { mouse, last_buttons, .. }) => {
                diff_mouse_report(mouse, last_buttons, buttons, dx, dy, wheel, time)
            }
            _ => Vec::new(),
        },
    }
}

const MOD_BITS: [(u8, KeyCode); 8] = [
    (0x01, KeyCode::LEFT_CTRL),
    (0x02, KeyCode::LEFT_SHIFT),
    (0x04, KeyCode::LEFT_ALT),
    (0x08, KeyCode::LEFT_META),
    (0x10, KeyCode::RIGHT_CTRL),
    (0x20, KeyCode::RIGHT_SHIFT),
    (0x40, KeyCode::RIGHT_ALT),
    (0x80, KeyCode::RIGHT_META),
];

fn diff_keyboard_report(
    keyboard: &mut Keyboard,
    last_modifiers: &mut u8,
    last_keys: &mut [u8; 6],
    modifiers: u8,
    keys: [u8; 6],
    time: Timestamp,
) -> Vec<Event> {
    let mut events = Vec::new();

    for (bit, code) in MOD_BITS {
        let was = *last_modifiers & bit != 0;
        let now = modifiers & bit != 0;
        if was != now {
            let state = if now { KeyState::Pressed } else { KeyState::Released };
            events.push(Event::Key(keyboard.process_key(code, state, time)));
        }
    }

    for &old_id in last_keys.iter() {
        if old_id != 0 && !keys.contains(&old_id) {
            if let Some(code) = hid_usage_to_keycode(old_id) {
                events.push(Event::Key(keyboard.process_key(code, KeyState::Released, time)));
            }
        }
    }
    for &new_id in keys.iter() {
        if new_id != 0 && !last_keys.contains(&new_id) {
            if let Some(code) = hid_usage_to_keycode(new_id) {
                events.push(Event::Key(keyboard.process_key(code, KeyState::Pressed, time)));
            }
        }
    }

    *last_modifiers = modifiers;
    *last_keys = keys;
    events
}

fn diff_mouse_report(
    mouse: &mut Mouse,
    last_buttons: &mut u8,
    buttons: u8,
    dx: i8,
    dy: i8,
    wheel: i8,
    time: Timestamp,
) -> Vec<Event> {
    let mut events = Vec::new();
    const BTN_BITS: [(u8, Button); 3] = [(0x01, Button::Left), (0x02, Button::Right), (0x04, Button::Middle)];
    for (bit, button) in BTN_BITS {
        let was = *last_buttons & bit != 0;
        let now = buttons & bit != 0;
        if was != now {
            events.push(Event::Pointer(mouse.set_button(button, now, time)));
        }
    }
    if dx != 0 || dy != 0 {
        mouse.accumulate_motion(dx as f64, dy as f64);
        if let Some(ev) = mouse.flush_motion(time) {
            events.push(Event::Pointer(ev));
        }
    }
    if wheel != 0 {
        events.push(Event::Pointer(mouse.scroll(
            ScrollAxis::Vertical,
            wheel as f64,
            ScrollSource::Wheel,
            time,
        )));
    }
    *last_buttons = buttons;
    events
}

/// Maps a USB HID "Keyboard/Keypad Page" (0x07) usage id -- as carried in a
/// boot-protocol report -- to the equivalent Linux [`KeyCode`]. Covers the
/// common alphanumeric/navigation/function-key range; extend following the
/// same table (USB HID Usage Tables, Keyboard/Keypad Page) for less common
/// keys.
fn hid_usage_to_keycode(usage: u8) -> Option<KeyCode> {
    use KeyCode as K;
    Some(match usage {
        0x04 => K::A,
        0x05 => K::B,
        0x06 => K::C,
        0x07 => K::D,
        0x08 => K::E,
        0x09 => K::F,
        0x0A => K::G,
        0x0B => K::H,
        0x0C => K::I,
        0x0D => K::J,
        0x0E => K::K,
        0x0F => K::L,
        0x10 => K::M,
        0x11 => K::N,
        0x12 => K::O,
        0x13 => K::P,
        0x14 => K::Q,
        0x15 => K::R,
        0x16 => K::S,
        0x17 => K::T,
        0x18 => K::U,
        0x19 => K::V,
        0x1A => K::W,
        0x1B => K::X,
        0x1C => K::Y,
        0x1D => K::Z,
        0x1E => K::KEY_1,
        0x1F => K::KEY_2,
        0x20 => K::KEY_3,
        0x21 => K::KEY_4,
        0x22 => K::KEY_5,
        0x23 => K::KEY_6,
        0x24 => K::KEY_7,
        0x25 => K::KEY_8,
        0x26 => K::KEY_9,
        0x27 => K::KEY_0,
        0x28 => K::ENTER,
        0x29 => K::ESC,
        0x2A => K::BACKSPACE,
        0x2B => K::TAB,
        0x2C => K::SPACE,
        0x2D => K::MINUS,
        0x2E => K::EQUAL,
        0x2F => K::LEFT_BRACE,
        0x30 => K::RIGHT_BRACE,
        0x31 => K::BACKSLASH,
        0x33 => K::SEMICOLON,
        0x34 => K::APOSTROPHE,
        0x35 => K::GRAVE,
        0x36 => K::COMMA,
        0x37 => K::DOT,
        0x38 => K::SLASH,
        0x39 => K::CAPS_LOCK,
        0x3A => K::F1,
        0x3B => K::F2,
        0x3C => K::F3,
        0x3D => K::F4,
        0x3E => K::F5,
        0x3F => K::F6,
        0x40 => K::F7,
        0x41 => K::F8,
        0x42 => K::F9,
        0x43 => K::F10,
        0x44 => K::F11,
        0x45 => K::F12,
        0x46 => K::PRINT_SCREEN,
        0x47 => K::SCROLL_LOCK,
        0x48 => K::PAUSE,
        0x49 => K::INSERT,
        0x4A => K::HOME,
        0x4B => K::PAGE_UP,
        0x4C => K::DELETE,
        0x4D => K::END,
        0x4E => K::PAGE_DOWN,
        0x4F => K::RIGHT,
        0x50 => K::LEFT,
        0x51 => K::DOWN,
        0x52 => K::UP,
        0x53 => K::NUM_LOCK,
        0x54 => K::KP_SLASH,
        0x55 => K::KP_ASTERISK,
        0x56 => K::KP_MINUS,
        0x57 => K::KP_PLUS,
        0x58 => K::KP_ENTER,
        0x59 => K::KP1,
        0x5A => K::KP2,
        0x5B => K::KP3,
        0x5C => K::KP4,
        0x5D => K::KP5,
        0x5E => K::KP6,
        0x5F => K::KP7,
        0x60 => K::KP8,
        0x61 => K::KP9,
        0x62 => K::KP0,
        0x63 => K::KP_DOT,
        0x65 => K::COMPOSE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_report_diff_emits_press_and_release() {
        let mut kb = Keyboard::new(DeviceId::new());
        let mut last_mods = 0u8;
        let mut last_keys = [0u8; 6];
        let t = Timestamp::from_secs(0);

        // Press 'a' (usage 0x04) with left shift held (modifier bit 0x02).
        let events = diff_keyboard_report(&mut kb, &mut last_mods, &mut last_keys, 0x02, [0x04, 0, 0, 0, 0, 0], t);
        // Expect: LEFT_SHIFT pressed, then 'a' pressed = 2 events.
        assert_eq!(events.len(), 2);
        assert!(kb.is_pressed(KeyCode::A));
        assert!(kb.modifiers().contains(crate::keyboard::Modifiers::SHIFT));

        // Release everything.
        let events = diff_keyboard_report(&mut kb, &mut last_mods, &mut last_keys, 0x00, [0, 0, 0, 0, 0, 0], t);
        assert_eq!(events.len(), 2);
        assert!(!kb.is_pressed(KeyCode::A));
    }

    #[test]
    fn mouse_report_diff_emits_button_and_motion() {
        let mut mouse = Mouse::new(DeviceId::new());
        let mut last_buttons = 0u8;
        let t = Timestamp::from_secs(0);
        let events = diff_mouse_report(&mut mouse, &mut last_buttons, 0x01, 5, -3, 0, t);
        // One button-down event plus one motion event.
        assert_eq!(events.len(), 2);
        assert!(mouse.is_pressed(Button::Left));
    }

    #[test]
    fn hid_usage_table_covers_letters_and_digits() {
        assert_eq!(hid_usage_to_keycode(0x04), Some(KeyCode::A));
        assert_eq!(hid_usage_to_keycode(0x1D), Some(KeyCode::Z));
        assert_eq!(hid_usage_to_keycode(0x27), Some(KeyCode::KEY_0));
        assert_eq!(hid_usage_to_keycode(0x00), None);
    }
}
