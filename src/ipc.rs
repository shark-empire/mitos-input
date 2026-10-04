//! IPC: the Unix-socket server through which other MITOS components (the
//! compositor, an on-screen keyboard, accessibility tools, ...) subscribe to
//! input/device events and inject synthetic input.
//!
//! # Security
//! Anyone who can connect to this socket can **observe every keystroke on the
//! system** (via `SUBSCRIBE`) and **inject arbitrary input** (via `INJECT-*`).
//! The socket file is therefore created owner-only (`0600`) by default; widen
//! it deliberately with [`IpcServer::with_socket_mode`] (e.g. `0660` plus a
//! dedicated group) rather than leaving it world-connectable.
//!
//! # Protocol
//! Newline-delimited text, in the spirit of `libinput debug-events`. Clients
//! send one command per line; the server sends one reply per command
//! (`OK`, `ERR <message>`, `PONG`, or for `LIST-DEVICES` a run of `DEVICE ...`
//! lines terminated by `END`) and, once subscribed, event lines. Replies and
//! event lines share one stream, so **clients must dispatch on the first
//! token of each line** -- an event line may arrive before a reply.
//!
//! Commands (case-insensitive keywords):
//! - `PING` -> `PONG`
//! - `LIST-DEVICES` -> `DEVICE ...` lines, then `END`
//! - `SUBSCRIBE [all | kind[,kind...]]` -> `OK` (kinds: `key pointer touch
//!   tablet gamepad device gesture hotkey`); by the time `OK` is received
//!   the subscription is active, so no later event is missed
//! - `UNSUBSCRIBE` -> `OK`
//! - `INJECT-KEY <code> <press|release>`
//! - `INJECT-MOTION <dx> <dy>`
//! - `INJECT-BUTTON <left|right|middle|side|extra|forward|back> <press|release>`
//! - `INJECT-SCROLL <vertical> <horizontal>`
//! - `QUIT`
//!
//! Free-text values in server output (device names, custom hotkey actions)
//! are emitted as double-quoted, escaped strings: quotes, backslashes and all
//! control characters (including Unicode line separators) are escaped, so a
//! hostile device name can never inject a fake protocol line.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use crate::device::{Device, DeviceId};
use crate::error::{InputError, Result};
use crate::evdev::KEY_MAX;
use crate::events::{
    DeviceEvent, Event, EventKind, GamepadEvent, GestureEvent, TouchPhase, Timestamp,
};
use crate::gestures::SwipeDirection;
use crate::hotkeys::HotkeyAction;
use crate::keyboard::KeyState;
use crate::pointer::{Button, PointerEvent, ScrollAxis, ScrollSource};
use crate::uinput::VirtualDeviceManager;

pub const DEFAULT_SOCKET_PATH: &str = "/run/mitos/input.sock";

/// Registry of currently-known devices, shared between the
/// [`crate::input::InputManager`] (which keeps it current) and this server
/// (which serves `LIST-DEVICES` from it).
pub type SharedDeviceTable = Arc<Mutex<HashMap<DeviceId, Device>>>;

type SharedStream = Arc<Mutex<UnixStream>>;

/// Upper bound on concurrent clients (each gets a handler thread).
const MAX_CLIENTS: usize = 64;
/// Upper bound on a single command line, so a client can't grow our buffer forever.
const MAX_LINE: usize = 4096;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A handler thread panicking while holding a lock must not take the
    // whole IPC layer down with it.
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Bind a non-blocking Unix listener at `path`, restricted to `mode`
/// permissions from the very first instant (also used by
/// [`crate::bluetooth`], whose socket is just as sensitive). It:
/// - refuses to take over a path that a live process is still listening on;
/// - clears a stale socket left by a crash, but refuses to delete anything
///   at `path` that isn't a socket;
/// - binds under a restrictive umask, so the socket is never -- even for an
///   instant -- connectable by more users than `mode` intends.
pub(crate) fn bind_private_socket(path: &str, mode: u32) -> io::Result<UnixListener> {
    if UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another instance is already listening on {path}"),
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            let _ = std::fs::remove_file(path);
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{path} already exists and is not a socket"),
            ));
        }
        Err(_) => {} // nothing there yet
    }
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // SAFETY: `umask` has no preconditions; it only swaps a process-wide mask,
    // which we restore immediately after binding.
    let old_umask = unsafe { libc::umask(0o177) };
    let bound = UnixListener::bind(path);
    unsafe { libc::umask(old_umask) };
    let listener = bound?;

    if mode != 0o600 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    listener.set_nonblocking(true)?;
    Ok(listener)
}

struct Subscriber {
    id: u64,
    writer: SharedStream,
    /// `None` subscribes to every kind.
    kinds: Option<HashSet<EventKind>>,
}

impl Subscriber {
    fn wants(&self, kind: EventKind) -> bool {
        match &self.kinds {
            None => true,
            Some(set) => set.contains(&kind),
        }
    }
}

struct ClientGuard(Arc<AtomicUsize>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct ClientCtx {
    id: u64,
    devices: SharedDeviceTable,
    virtual_devices: Arc<Mutex<VirtualDeviceManager>>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    running: Arc<AtomicBool>,
}

/// The IPC server. Create with [`IpcServer::new`], [`IpcServer::start`] it,
/// then call [`IpcServer::broadcast`] for every event to fan out.
pub struct IpcServer {
    socket_path: String,
    socket_mode: u32,
    devices: SharedDeviceTable,
    virtual_devices: Arc<Mutex<VirtualDeviceManager>>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    running: Arc<AtomicBool>,
    clients: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl IpcServer {
    pub fn new(
        socket_path: impl Into<String>,
        devices: SharedDeviceTable,
        virtual_devices: Arc<Mutex<VirtualDeviceManager>>,
    ) -> Self {
        IpcServer {
            socket_path: socket_path.into(),
            socket_mode: 0o600,
            devices,
            virtual_devices,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            running: Arc::new(AtomicBool::new(false)),
            clients: Arc::new(AtomicUsize::new(0)),
            thread: None,
        }
    }

    /// Permission bits for the socket file (default `0o600`, owner only).
    /// See the module-level security note before widening this.
    pub fn with_socket_mode(mut self, mode: u32) -> Self {
        self.socket_mode = mode;
        self
    }

    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }

    pub fn subscriber_count(&self) -> usize {
        lock(&self.subscribers).len()
    }

    pub fn client_count(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    pub fn start(&mut self) -> Result<()> {
        let listener = bind_private_socket(&self.socket_path, self.socket_mode).map_err(|e| {
            InputError::Ipc(format!("failed to listen on {}: {e}", self.socket_path))
        })?;

        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let clients = self.clients.clone();
        let devices = self.devices.clone();
        let virtual_devices = self.virtual_devices.clone();
        let subscribers = self.subscribers.clone();

        self.thread = Some(thread::spawn(move || {
            let mut next_id: u64 = 1;
            while running.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        let _ = stream.set_nonblocking(false);
                        if clients.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
                            clients.fetch_sub(1, Ordering::SeqCst);
                            let mut s = &stream;
                            let _ = s.write_all(b"ERR too many clients\n");
                            continue;
                        }
                        let guard = ClientGuard(clients.clone());
                        let ctx = ClientCtx {
                            id: next_id,
                            devices: devices.clone(),
                            virtual_devices: virtual_devices.clone(),
                            subscribers: subscribers.clone(),
                            running: running.clone(),
                        };
                        next_id += 1;
                        thread::spawn(move || {
                            let _guard = guard;
                            handle_client(stream, ctx);
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        }));
        Ok(())
    }

    /// Send `event` to every subscriber whose filter matches. Subscribers
    /// whose connection has failed (or stalled past the write timeout) are
    /// dropped rather than allowed to hold up delivery to everyone else.
    pub fn broadcast(&self, event: &Event) {
        let kind = event.kind();
        let mut subs = lock(&self.subscribers);
        if subs.is_empty() {
            return;
        }
        let line = encode_event(event);
        subs.retain(|sub| {
            if !sub.wants(kind) {
                return true;
            }
            let mut writer = lock(&sub.writer);
            writer.write_all(line.as_bytes()).is_ok()
        });
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
            // Only remove the socket if we actually created it.
            let _ = std::fs::remove_file(&self.socket_path);
        }
        lock(&self.subscribers).clear();
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------
// Client handling
// ---------------------------------------------------------------------

fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Write one complete reply under the client's writer lock (so it can't be
/// interleaved with a broadcast line). Returns `false` if the write failed.
fn send(writer: &SharedStream, text: &str) -> bool {
    let mut w = lock(writer);
    w.write_all(text.as_bytes()).is_ok()
}

fn remove_subscriber(subscribers: &Mutex<Vec<Subscriber>>, id: u64) {
    lock(subscribers).retain(|s| s.id != id);
}

fn handle_client(stream: UnixStream, ctx: ClientCtx) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let writer_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    // Bound how long a stalled client can block a broadcast.
    let _ = writer_stream.set_write_timeout(Some(Duration::from_millis(200)));
    let writer: SharedStream = Arc::new(Mutex::new(writer_stream));

    let mut reader = BufReader::new(stream);
    let mut buf: Vec<u8> = Vec::new();

    loop {
        if !ctx.running.load(Ordering::SeqCst) {
            break;
        }
        if buf.len() >= MAX_LINE {
            let _ = send(&writer, "ERR command line too long\n");
            break;
        }
        let budget = (MAX_LINE - buf.len()) as u64;
        match reader.by_ref().take(budget).read_until(b'\n', &mut buf) {
            Ok(0) => break, // EOF (budget is always > 0 here)
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    let line = String::from_utf8_lossy(&buf).trim().to_string();
                    buf.clear();
                    if !line.is_empty() && handle_command(&line, &ctx, &writer) == Flow::Quit {
                        break;
                    }
                }
                // No newline yet: either more is coming, or the budget check
                // at the top of the loop rejects an over-long line.
            }
            // Keep any partial line in `buf` across a read timeout.
            Err(e) if is_timeout(&e) => continue,
            Err(_) => break,
        }
    }
    remove_subscriber(&ctx.subscribers, ctx.id);
}

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

fn handle_command(line: &str, ctx: &ClientCtx, writer: &SharedStream) -> Flow {
    let mut parts = line.split_whitespace();
    let cmd = match parts.next() {
        Some(c) => c.to_ascii_uppercase(),
        None => return Flow::Continue,
    };
    let args: Vec<&str> = parts.collect();

    let (reply, flow) = match cmd.as_str() {
        "PING" => ("PONG\n".to_string(), Flow::Continue),
        "QUIT" => ("OK\n".to_string(), Flow::Quit),
        "LIST-DEVICES" => (list_devices(ctx), Flow::Continue),
        "SUBSCRIBE" => (subscribe(ctx, writer, &args), Flow::Continue),
        "UNSUBSCRIBE" => {
            remove_subscriber(&ctx.subscribers, ctx.id);
            ("OK\n".to_string(), Flow::Continue)
        }
        "INJECT-KEY" => (inject_key(ctx, &args), Flow::Continue),
        "INJECT-MOTION" => (inject_motion(ctx, &args), Flow::Continue),
        "INJECT-BUTTON" => (inject_button(ctx, &args), Flow::Continue),
        "INJECT-SCROLL" => (inject_scroll(ctx, &args), Flow::Continue),
        other => (
            format!("ERR unknown command {}\n", quote(other)),
            Flow::Continue,
        ),
    };
    if !send(writer, &reply) {
        return Flow::Quit;
    }
    flow
}

fn parse_kinds(args: &[&str]) -> std::result::Result<Option<HashSet<EventKind>>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut set = HashSet::new();
    for arg in args {
        for name in arg.split(',') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let kind = match name.to_ascii_lowercase().as_str() {
                "all" => return Ok(None),
                "key" => EventKind::Key,
                "pointer" => EventKind::Pointer,
                "touch" => EventKind::Touch,
                "tablet" => EventKind::Tablet,
                "gamepad" => EventKind::Gamepad,
                "device" => EventKind::Device,
                "gesture" => EventKind::Gesture,
                "hotkey" => EventKind::Hotkey,
                other => return Err(format!("unknown event kind {}", quote(other))),
            };
            set.insert(kind);
        }
    }
    if set.is_empty() {
        Err("no event kinds given".to_string())
    } else {
        Ok(Some(set))
    }
}

fn subscribe(ctx: &ClientCtx, writer: &SharedStream, args: &[&str]) -> String {
    let kinds = match parse_kinds(args) {
        Ok(k) => k,
        Err(msg) => return format!("ERR {msg}\n"),
    };
    // Register *before* the caller sends `OK`, so a client that has seen
    // `OK` is guaranteed not to miss any later event.
    let mut subs = lock(&ctx.subscribers);
    subs.retain(|s| s.id != ctx.id);
    subs.push(Subscriber {
        id: ctx.id,
        writer: Arc::clone(writer),
        kinds,
    });
    "OK\n".to_string()
}

fn list_devices(ctx: &ClientCtx) -> String {
    let devices = lock(&ctx.devices);
    let mut list: Vec<&Device> = devices.values().collect();
    list.sort_by_key(|d| d.id);
    let mut out = String::new();
    for d in list {
        out.push_str(&encode_device_line("DEVICE", d));
    }
    out.push_str("END\n");
    out
}

fn parse_press(word: &str) -> Option<bool> {
    match word.to_ascii_lowercase().as_str() {
        "press" | "down" | "1" => Some(true),
        "release" | "up" | "0" => Some(false),
        _ => None,
    }
}

fn err_line(msg: &str) -> String {
    format!("ERR {}\n", one_line(msg))
}

fn inject_key(ctx: &ClientCtx, args: &[&str]) -> String {
    if args.len() != 2 {
        return "ERR usage: INJECT-KEY <code> <press|release>\n".to_string();
    }
    let code: u16 = match args[0].parse() {
        Ok(c) if c <= KEY_MAX => c,
        _ => return "ERR key code out of range\n".to_string(),
    };
    let Some(pressed) = parse_press(args[1]) else {
        return "ERR expected press or release\n".to_string();
    };
    match lock(&ctx.virtual_devices).inject_key(code, pressed) {
        Ok(()) => "OK\n".to_string(),
        Err(e) => err_line(&e.to_string()),
    }
}

fn inject_motion(ctx: &ClientCtx, args: &[&str]) -> String {
    if args.len() != 2 {
        return "ERR usage: INJECT-MOTION <dx> <dy>\n".to_string();
    }
    let (Ok(dx), Ok(dy)) = (args[0].parse::<i32>(), args[1].parse::<i32>()) else {
        return "ERR dx and dy must be integers\n".to_string();
    };
    match lock(&ctx.virtual_devices).inject_motion(dx, dy) {
        Ok(()) => "OK\n".to_string(),
        Err(e) => err_line(&e.to_string()),
    }
}

fn parse_button(word: &str) -> Option<Button> {
    match word.to_ascii_lowercase().as_str() {
        "left" => Some(Button::Left),
        "right" => Some(Button::Right),
        "middle" => Some(Button::Middle),
        "side" => Some(Button::Side),
        "extra" => Some(Button::Extra),
        "forward" => Some(Button::Forward),
        "back" => Some(Button::Back),
        _ => None,
    }
}

fn inject_button(ctx: &ClientCtx, args: &[&str]) -> String {
    if args.len() != 2 {
        return "ERR usage: INJECT-BUTTON <button> <press|release>\n".to_string();
    }
    let Some(button) = parse_button(args[0]) else {
        return "ERR unknown button\n".to_string();
    };
    let Some(pressed) = parse_press(args[1]) else {
        return "ERR expected press or release\n".to_string();
    };
    match lock(&ctx.virtual_devices).inject_button(button, pressed) {
        Ok(()) => "OK\n".to_string(),
        Err(e) => err_line(&e.to_string()),
    }
}

fn inject_scroll(ctx: &ClientCtx, args: &[&str]) -> String {
    if args.len() != 2 {
        return "ERR usage: INJECT-SCROLL <vertical> <horizontal>\n".to_string();
    }
    let (Ok(v), Ok(h)) = (args[0].parse::<i32>(), args[1].parse::<i32>()) else {
        return "ERR scroll amounts must be integers\n".to_string();
    };
    match lock(&ctx.virtual_devices).inject_scroll(v, h) {
        Ok(()) => "OK\n".to_string(),
        Err(e) => err_line(&e.to_string()),
    }
}

// ---------------------------------------------------------------------
// Wire encoding
// ---------------------------------------------------------------------

fn needs_escape(c: char) -> bool {
    // `is_control` covers U+0000-001F and U+007F-009F (incl. NEL); the two
    // Unicode line/paragraph separators aren't "control" but many
    // line-splitting libraries treat them as newlines.
    c.is_control() || c == '\u{2028}' || c == '\u{2029}'
}

/// Double-quote `s`, escaping everything that could break out of the value
/// or the line it sits on.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if needs_escape(c) => {
                let cp = c as u32;
                if cp < 0x80 {
                    out.push_str(&format!("\\x{cp:02x}"));
                } else {
                    out.push_str(&format!("\\u{{{cp:04x}}}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Flatten arbitrary text onto a single line (for error messages).
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if needs_escape(c) { ' ' } else { c })
        .collect()
}

fn secs(t: Timestamp) -> f64 {
    t.as_secs_f64()
}

fn pressed_str(pressed: bool) -> &'static str {
    if pressed {
        "pressed"
    } else {
        "released"
    }
}

fn key_state_str(state: KeyState) -> &'static str {
    match state {
        KeyState::Released => "released",
        KeyState::Pressed => "pressed",
        KeyState::Repeat => "repeat",
    }
}

fn button_name(button: Button) -> String {
    match button {
        Button::Left => "left".to_string(),
        Button::Right => "right".to_string(),
        Button::Middle => "middle".to_string(),
        Button::Side => "side".to_string(),
        Button::Extra => "extra".to_string(),
        Button::Forward => "forward".to_string(),
        Button::Back => "back".to_string(),
        Button::Other(code) => format!("other:{code}"),
    }
}

fn direction_str(d: SwipeDirection) -> &'static str {
    match d {
        SwipeDirection::Up => "up",
        SwipeDirection::Down => "down",
        SwipeDirection::Left => "left",
        SwipeDirection::Right => "right",
    }
}

fn phase_str(p: TouchPhase) -> &'static str {
    match p {
        TouchPhase::Down => "down",
        TouchPhase::Move => "move",
        TouchPhase::Up => "up",
        TouchPhase::Cancel => "cancel",
    }
}

fn action_str(action: &HotkeyAction) -> String {
    match action {
        HotkeyAction::ShowLauncher => "show-launcher".to_string(),
        HotkeyAction::ShowDesktop => "show-desktop".to_string(),
        HotkeyAction::SwitchWindow => "switch-window".to_string(),
        HotkeyAction::SwitchWindowReverse => "switch-window-reverse".to_string(),
        HotkeyAction::SwitchWorkspace(n) => format!("switch-workspace:{n}"),
        HotkeyAction::CloseWindow => "close-window".to_string(),
        HotkeyAction::Lock => "lock".to_string(),
        HotkeyAction::Screenshot => "screenshot".to_string(),
        HotkeyAction::ScreenshotRegion => "screenshot-region".to_string(),
        HotkeyAction::ToggleOverview => "toggle-overview".to_string(),
        HotkeyAction::VolumeUp => "volume-up".to_string(),
        HotkeyAction::VolumeDown => "volume-down".to_string(),
        HotkeyAction::VolumeMute => "volume-mute".to_string(),
        HotkeyAction::Custom(name) => format!("custom:{}", quote(name)),
    }
}

fn encode_device_line(prefix: &str, d: &Device) -> String {
    let node = match &d.node_path {
        Some(p) => quote(p),
        None => "-".to_string(),
    };
    format!(
        "{prefix} id={} type={} name={} vendor=0x{:04x} product=0x{:04x} bus=0x{:04x} seat={} caps=0x{:x} node={}\n",
        d.id.raw(),
        d.device_type,
        quote(&d.name),
        d.vendor_id,
        d.product_id,
        d.bus_type,
        quote(&d.seat),
        d.capabilities.bits(),
        node,
    )
}

fn encode_pointer(p: &PointerEvent) -> String {
    match p {
        PointerEvent::Motion { device, time, dx, dy } => format!(
            "POINTER-MOTION dev={} t={:.6} dx={:.4} dy={:.4}\n",
            device.raw(),
            secs(*time),
            dx,
            dy
        ),
        PointerEvent::MotionAbsolute { device, time, x, y } => format!(
            "POINTER-MOTION-ABS dev={} t={:.6} x={:.5} y={:.5}\n",
            device.raw(),
            secs(*time),
            x,
            y
        ),
        PointerEvent::Button { device, time, button, pressed } => format!(
            "POINTER-BUTTON dev={} t={:.6} button={} state={}\n",
            device.raw(),
            secs(*time),
            button_name(*button),
            pressed_str(*pressed)
        ),
        PointerEvent::Scroll { device, time, axis, value, source } => format!(
            "POINTER-SCROLL dev={} t={:.6} axis={} value={:.4} source={}\n",
            device.raw(),
            secs(*time),
            match axis {
                ScrollAxis::Vertical => "vertical",
                ScrollAxis::Horizontal => "horizontal",
            },
            value,
            match source {
                ScrollSource::Wheel => "wheel",
                ScrollSource::Finger => "finger",
                ScrollSource::Continuous => "continuous",
            }
        ),
    }
}

fn encode_gamepad(g: &GamepadEvent) -> String {
    match g {
        GamepadEvent::Button { device, time, button, pressed } => format!(
            "GAMEPAD-BUTTON dev={} t={:.6} button={:?} state={}\n",
            device.raw(),
            secs(*time),
            button,
            pressed_str(*pressed)
        ),
        GamepadEvent::Axis { device, time, axis, value } => format!(
            "GAMEPAD-AXIS dev={} t={:.6} axis={:?} value={:.4}\n",
            device.raw(),
            secs(*time),
            axis,
            value
        ),
        GamepadEvent::Connected { device } => format!("GAMEPAD-CONNECTED dev={}\n", device.raw()),
        GamepadEvent::Disconnected { device } => {
            format!("GAMEPAD-DISCONNECTED dev={}\n", device.raw())
        }
    }
}

fn encode_gesture(g: &GestureEvent) -> String {
    match g {
        GestureEvent::Swipe { device, time, fingers, direction, dx, dy } => format!(
            "GESTURE-SWIPE dev={} t={:.6} fingers={} dir={} dx={:.5} dy={:.5}\n",
            device.raw(),
            secs(*time),
            fingers,
            direction_str(*direction),
            dx,
            dy
        ),
        GestureEvent::Pinch { device, time, fingers, scale, rotation } => format!(
            "GESTURE-PINCH dev={} t={:.6} fingers={} scale={:.4} rotation={:.4}\n",
            device.raw(),
            secs(*time),
            fingers,
            scale,
            rotation
        ),
        GestureEvent::Tap { device, time, fingers, x, y } => format!(
            "GESTURE-TAP dev={} t={:.6} fingers={} x={:.5} y={:.5}\n",
            device.raw(),
            secs(*time),
            fingers,
            x,
            y
        ),
        GestureEvent::Hold { device, time, fingers, x, y } => format!(
            "GESTURE-HOLD dev={} t={:.6} fingers={} x={:.5} y={:.5}\n",
            device.raw(),
            secs(*time),
            fingers,
            x,
            y
        ),
    }
}

/// Encode one event as a single protocol line (including the trailing
/// newline). Public so tools built on this crate (and the `mitos-input`
/// binary's own debug output) can share the exact wire format.
pub fn encode_event(event: &Event) -> String {
    match event {
        Event::Key(e) => format!(
            "KEY dev={} t={:.6} code={} key={} state={} mods=0x{:02x} repeat={}\n",
            e.device.raw(),
            secs(e.time),
            e.key.0,
            e.key.name(),
            key_state_str(e.state),
            e.modifiers.bits(),
            e.repeat as u8
        ),
        Event::Pointer(p) => encode_pointer(p),
        Event::Touch(t) => format!(
            "TOUCH dev={} t={:.6} phase={} slot={} id={} x={:.5} y={:.5} pressure={:.4}\n",
            t.device.raw(),
            secs(t.time),
            phase_str(t.phase),
            t.point.slot,
            t.point.tracking_id,
            t.point.x,
            t.point.y,
            t.point.pressure
        ),
        Event::Tablet(t) => format!(
            "TABLET dev={} t={:.6} x={:.5} y={:.5} pressure={:.4} tilt_x={:.1} tilt_y={:.1} prox={} contact={} buttons=0x{:02x}\n",
            t.device.raw(),
            secs(t.time),
            t.x,
            t.y,
            t.pressure,
            t.tilt_x,
            t.tilt_y,
            t.in_proximity as u8,
            t.in_contact as u8,
            t.buttons
        ),
        Event::Gamepad(g) => encode_gamepad(g),
        Event::Device(DeviceEvent::Added(d)) => encode_device_line("DEVICE-ADDED", d),
        Event::Device(DeviceEvent::Removed(id)) => format!("DEVICE-REMOVED id={}\n", id.raw()),
        Event::Device(DeviceEvent::CapabilitiesChanged(id)) => {
            format!("DEVICE-CAPS id={}\n", id.raw())
        }
        Event::Gesture(g) => encode_gesture(g),
        Event::Hotkey(h) => format!(
            "HOTKEY id={} t={:.6} action={}\n",
            h.id,
            secs(h.time),
            action_str(&h.action)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceType;
    use crate::events::KeyEvent;
    use crate::keyboard::{KeyCode, Modifiers};

    fn sample_key_event() -> KeyEvent {
        KeyEvent {
            device: DeviceId::new(),
            time: Duration::from_millis(1500),
            key: KeyCode::A,
            state: KeyState::Pressed,
            modifiers: Modifiers::SHIFT,
            repeat: false,
        }
    }

    fn unique_socket_path() -> String {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!(
            "{}/mitos-input-ipc-test-{}-{}.sock",
            std::env::temp_dir().display(),
            std::process::id(),
            n
        )
    }

    fn start_server() -> (IpcServer, String, SharedDeviceTable) {
        let path = unique_socket_path();
        let devices: SharedDeviceTable = Arc::new(Mutex::new(HashMap::new()));
        let vd = Arc::new(Mutex::new(VirtualDeviceManager::new()));
        let mut server = IpcServer::new(path.clone(), devices.clone(), vd);
        server.start().expect("server should start");
        (server, path, devices)
    }

    fn connect(path: &str) -> (UnixStream, BufReader<UnixStream>) {
        let stream = UnixStream::connect(path).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let writer = stream.try_clone().unwrap();
        (writer, BufReader::new(stream))
    }

    fn read_line(reader: &mut BufReader<UnixStream>) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read_line");
        line
    }

    #[test]
    fn quote_escapes_everything_dangerous() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(quote("line1\nline2"), "\"line1\\x0aline2\"");
        assert_eq!(quote("\u{2028}"), "\"\\u{2028}\"");
        assert_eq!(quote("\u{0085}"), "\"\\u{0085}\"");
        // Nothing in the output may itself be a line break.
        assert!(!quote("x\r\ny\u{2029}z").contains(|c: char| needs_escape(c)));
    }

    #[test]
    fn key_event_encodes_as_one_line() {
        let line = encode_event(&Event::Key(sample_key_event()));
        assert!(line.starts_with("KEY "));
        assert!(line.contains("code=30"));
        assert!(line.contains("key=A"));
        assert!(line.contains("state=pressed"));
        assert!(line.contains("mods=0x01"));
        assert!(line.contains("t=1.500000"));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn parse_kinds_handles_lists_all_and_errors() {
        assert!(parse_kinds(&[]).unwrap().is_none());
        assert!(parse_kinds(&["all"]).unwrap().is_none());
        let set = parse_kinds(&["key,pointer", "TOUCH"]).unwrap().unwrap();
        assert_eq!(set.len(), 3);
        assert!(set.contains(&EventKind::Key));
        assert!(set.contains(&EventKind::Touch));
        assert!(parse_kinds(&["bogus"]).is_err());
        assert!(parse_kinds(&[","]).is_err());
    }

    #[test]
    fn ping_subscribe_and_filtered_broadcast() {
        let (mut server, path, _devices) = start_server();
        let (mut writer, mut reader) = connect(&path);

        writer.write_all(b"PING\n").unwrap();
        assert_eq!(read_line(&mut reader), "PONG\n");

        writer.write_all(b"SUBSCRIBE key\n").unwrap();
        assert_eq!(read_line(&mut reader), "OK\n");
        assert_eq!(server.subscriber_count(), 1);

        // A device event doesn't match the `key` filter and must not be
        // delivered; the key event that follows must be.
        server.broadcast(&Event::Device(DeviceEvent::Removed(DeviceId::new())));
        server.broadcast(&Event::Key(sample_key_event()));
        let line = read_line(&mut reader);
        assert!(line.starts_with("KEY "), "expected KEY line first, got {line:?}");

        // Nothing else is queued: the next line is the reply to this PING.
        writer.write_all(b"PING\n").unwrap();
        assert_eq!(read_line(&mut reader), "PONG\n");

        server.stop();
        assert!(!Path::new(&path).exists());
    }

    #[test]
    fn invalid_commands_get_errors_without_touching_uinput() {
        let (mut server, path, _devices) = start_server();
        let (mut writer, mut reader) = connect(&path);

        for (cmd, expected_prefix) in [
            ("FROBNICATE\n", "ERR unknown command"),
            ("INJECT-KEY 99999 press\n", "ERR key code out of range"),
            ("INJECT-KEY abc press\n", "ERR key code out of range"),
            ("INJECT-KEY 30 maybe\n", "ERR expected press or release"),
            ("INJECT-BUTTON nope press\n", "ERR unknown button"),
            ("INJECT-MOTION 1\n", "ERR usage"),
            ("SUBSCRIBE bogus\n", "ERR unknown event kind"),
        ] {
            writer.write_all(cmd.as_bytes()).unwrap();
            let reply = read_line(&mut reader);
            assert!(
                reply.starts_with(expected_prefix),
                "for {cmd:?} expected prefix {expected_prefix:?}, got {reply:?}"
            );
        }
        server.stop();
    }

    #[test]
    fn list_devices_escapes_hostile_names() {
        let (mut server, path, devices) = start_server();
        let dev = Device::new("Evil \"Board\"\nDEVICE-ADDED id=999", DeviceType::Keyboard);
        lock(&devices).insert(dev.id, dev.clone());

        let (mut writer, mut reader) = connect(&path);
        writer.write_all(b"LIST-DEVICES\n").unwrap();

        let first = read_line(&mut reader);
        assert!(first.starts_with("DEVICE id="), "got {first:?}");
        assert!(first.contains("type=keyboard"));
        assert!(first.contains(r#"name="Evil \"Board\"\x0aDEVICE-ADDED id=999""#));
        // The embedded newline must have been escaped, not emitted.
        assert_eq!(first.matches('\n').count(), 1);
        assert_eq!(read_line(&mut reader), "END\n");
        server.stop();
    }

    #[test]
    fn socket_is_owner_only_by_default() {
        let (mut server, path, _devices) = start_server();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        server.stop();
    }

    #[test]
    fn bind_refuses_to_clobber_a_regular_file() {
        let path = unique_socket_path();
        std::fs::write(&path, b"precious").unwrap();
        let err = bind_private_socket(&path, 0o600).expect_err("must not replace a regular file");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"precious");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn second_server_refuses_a_live_socket() {
        let (mut first, path, devices) = start_server();
        let vd = Arc::new(Mutex::new(VirtualDeviceManager::new()));
        let mut second = IpcServer::new(path.clone(), devices, vd);
        assert!(second.start().is_err());
        // The failed second start must not have removed the first's socket.
        assert!(Path::new(&path).exists());
        first.stop();
    }
}
