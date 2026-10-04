//! Linux evdev backend: talks directly to `/dev/input/eventN` nodes via
//! raw `ioctl`s (no `libinput`/`libevdev` dependency), discovers and
//! classifies devices, and turns raw kernel `input_event`s into the typed
//! [`crate::events::Event`] stream.
//!
//! This is the only module (besides [`crate::uinput`]) that touches `libc`
//! directly. The ioctl request numbers are computed with the same bit
//! layout as the kernel's `<asm-generic/ioctl.h>` `_IOR`/`_IOW` macros
//! rather than hard-coded, so they're easy to audit and extend.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crate::device::{Device, DeviceCapabilities, DeviceId, DeviceType};
use crate::error::{ioctl_error, InputError, Result};
use crate::events::{DeviceEvent, Event};
use crate::gamepad::{Gamepad, GamepadMapping};
use crate::input::{EventSink, InputDevice, InputSource};
use crate::keyboard::Keyboard;
use crate::pointer::Mouse;
use crate::tablet::Tablet;
use crate::touch::TouchDevice;

// ---------------------------------------------------------------------
// Linux input protocol constants (linux/input-event-codes.h). Stable
// kernel UAPI; only a practical subset is named here.
// ---------------------------------------------------------------------

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
pub const EV_SW: u16 = 0x05;
pub const EV_LED: u16 = 0x11;
pub const EV_MAX: u16 = 0x1f;
pub const EV_CNT: u16 = EV_MAX + 1;

pub const SYN_REPORT: u16 = 0;

pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_WHEEL_HI_RES: u16 = 0x0b;
pub const REL_HWHEEL_HI_RES: u16 = 0x0c;
pub const REL_MAX: u16 = 0x0f;
pub const REL_CNT: u16 = REL_MAX + 1;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_PRESSURE: u16 = 0x18;
pub const ABS_DISTANCE: u16 = 0x19;
pub const ABS_TILT_X: u16 = 0x1a;
pub const ABS_TILT_Y: u16 = 0x1b;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
pub const ABS_MT_TOUCH_MINOR: u16 = 0x31;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_PRESSURE: u16 = 0x3a;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_MAX: u16 = 0x3f;
pub const ABS_CNT: u16 = ABS_MAX + 1;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;
pub const BTN_FORWARD: u16 = 0x115;
pub const BTN_BACK: u16 = 0x116;
pub const BTN_TOOL_PEN: u16 = 0x140;
pub const BTN_STYLUS: u16 = 0x14b;
pub const BTN_STYLUS2: u16 = 0x14c;
pub const BTN_TOUCH: u16 = 0x14a;
pub const BTN_GAMEPAD: u16 = 0x130; // == BTN_SOUTH

pub const KEY_A_CODE: u16 = 30;
pub const KEY_MAX: u16 = 0x2ff;
pub const KEY_CNT: u16 = KEY_MAX + 1;

pub const INPUT_PROP_DIRECT: u16 = 0x01;
pub const INPUT_PROP_MAX: u16 = 0x1f;
pub const INPUT_PROP_CNT: u16 = INPUT_PROP_MAX + 1;

// ---------------------------------------------------------------------
// Raw kernel structs (must match the C layout exactly).
// ---------------------------------------------------------------------

/// Mirrors `struct input_event` from `linux/input.h`. On 64-bit Linux this
/// is 24 bytes: two 8-byte time fields, then two `u16`s and an `i32`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RawInputEvent {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

/// Mirrors `struct input_id`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// Mirrors `struct input_absinfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

// ---------------------------------------------------------------------
// ioctl number encoding, mirroring <asm-generic/ioctl.h>.
// ---------------------------------------------------------------------

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

const IOC_SIZEMASK: usize = (1 << IOC_SIZEBITS) - 1; // 0x3fff

const fn ioc(dir: u64, ty: u8, nr: u8, size: usize) -> u64 {
    (dir << IOC_DIRSHIFT)
        | ((ty as u64) << IOC_TYPESHIFT)
        | ((nr as u64) << IOC_NRSHIFT)
        | (((size & IOC_SIZEMASK) as u64) << IOC_SIZESHIFT)
}

const fn ior(ty: u8, nr: u8, size: usize) -> u64 {
    ioc(IOC_READ, ty, nr, size)
}

const fn iow(ty: u8, nr: u8, size: usize) -> u64 {
    ioc(IOC_WRITE, ty, nr, size)
}

// Fixed-size evdev ioctls. Sanity-checked against known values in tests below
// (e.g. EVIOCGID == 0x80084502, a widely cited constant).
fn eviocgid() -> u64 {
    ior(b'E', 0x02, std::mem::size_of::<InputId>())
}
fn eviocgrab() -> u64 {
    iow(b'E', 0x90, std::mem::size_of::<i32>())
}
fn eviocgname(len: usize) -> u64 {
    ior(b'E', 0x06, len)
}
fn eviocgbit(ev: u8, len: usize) -> u64 {
    ior(b'E', 0x20 + ev, len)
}
fn eviocgabs(abs: u8) -> u64 {
    ior(b'E', 0x40 + abs, std::mem::size_of::<InputAbsInfo>())
}
fn eviocgprop(len: usize) -> u64 {
    ior(b'E', 0x09, len)
}

unsafe fn ioctl_ptr<T>(fd: RawFd, request: u64, arg: *mut T, call: &'static str) -> Result<i32> {
    let ret = libc::ioctl(fd, request as _, arg);
    if ret < 0 {
        Err(ioctl_error(call))
    } else {
        Ok(ret)
    }
}

fn bytes_for_bits(bits: u16) -> usize {
    (bits as usize + 7) / 8
}

fn test_bit(bitmap: &[u8], bit: usize) -> bool {
    let byte = bit / 8;
    let shift = bit % 8;
    bitmap.get(byte).map(|b| b & (1 << shift) != 0).unwrap_or(false)
}

fn read_bits(fd: RawFd, ev: u16, count: u16) -> Vec<u8> {
    let mut buf = vec![0u8; bytes_for_bits(count)];
    unsafe {
        let _ = ioctl_ptr(fd, eviocgbit(ev as u8, buf.len()), buf.as_mut_ptr(), "EVIOCGBIT");
    }
    buf
}

fn read_props(fd: RawFd) -> Vec<u8> {
    let mut buf = vec![0u8; bytes_for_bits(INPUT_PROP_CNT)];
    unsafe {
        let _ = ioctl_ptr(fd, eviocgprop(buf.len()), buf.as_mut_ptr(), "EVIOCGPROP");
    }
    buf
}

fn read_name(fd: RawFd) -> String {
    let mut buf = vec![0u8; 256];
    let ret = unsafe { ioctl_ptr(fd, eviocgname(buf.len()), buf.as_mut_ptr(), "EVIOCGNAME") };
    match ret {
        Ok(n) if n > 0 => {
            let len = (n as usize).min(buf.len());
            let end = buf[..len].iter().position(|&b| b == 0).unwrap_or(len);
            String::from_utf8_lossy(&buf[..end]).into_owned()
        }
        _ => "unknown input device".to_string(),
    }
}

fn read_id(fd: RawFd) -> InputId {
    let mut id = InputId::default();
    unsafe {
        let _ = ioctl_ptr(fd, eviocgid(), &mut id as *mut InputId, "EVIOCGID");
    }
    id
}

fn read_abs_info(fd: RawFd, code: u16) -> Option<InputAbsInfo> {
    let mut info = InputAbsInfo::default();
    let ret = unsafe { ioctl_ptr(fd, eviocgabs(code as u8), &mut info as *mut InputAbsInfo, "EVIOCGABS") };
    ret.ok().map(|_| info)
}

/// Grab (or release) exclusive access to a device, so events stop reaching
/// any other listener (the console, another compositor, ...). Left as an
/// explicit opt-in call for the embedding compositor to make once it's
/// ready to own input, rather than grabbed unconditionally at discovery.
pub fn set_grab(fd: RawFd, grab: bool) -> Result<()> {
    let mut val: i32 = if grab { 1 } else { 0 };
    unsafe {
        ioctl_ptr(fd, eviocgrab(), &mut val as *mut i32, "EVIOCGRAB")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Device classification
// ---------------------------------------------------------------------

struct Bitmaps {
    ev: Vec<u8>,
    key: Vec<u8>,
    rel: Vec<u8>,
    abs: Vec<u8>,
    props: Vec<u8>,
}

impl Bitmaps {
    fn query(fd: RawFd) -> Self {
        let ev = read_bits(fd, 0, EV_CNT);
        let has_ev = |t: u16| test_bit(&ev, t as usize);
        let key = if has_ev(EV_KEY) { read_bits(fd, EV_KEY, KEY_CNT) } else { Vec::new() };
        let rel = if has_ev(EV_REL) { read_bits(fd, EV_REL, REL_CNT) } else { Vec::new() };
        let abs = if has_ev(EV_ABS) { read_bits(fd, EV_ABS, ABS_CNT) } else { Vec::new() };
        let props = read_props(fd);
        Bitmaps { ev, key, rel, abs, props }
    }

    fn has_ev(&self, t: u16) -> bool {
        test_bit(&self.ev, t as usize)
    }
    fn has_key(&self, c: u16) -> bool {
        test_bit(&self.key, c as usize)
    }
    fn has_rel(&self, c: u16) -> bool {
        test_bit(&self.rel, c as usize)
    }
    fn has_abs(&self, c: u16) -> bool {
        test_bit(&self.abs, c as usize)
    }
    fn has_prop(&self, c: u16) -> bool {
        test_bit(&self.props, c as usize)
    }
}

fn classify(bm: &Bitmaps) -> DeviceType {
    // Most specific first: a device can have EV_KEY set for entirely
    // different reasons (a mouse has BTN_LEFT; a tablet has BTN_TOOL_PEN).
    if bm.has_key(BTN_GAMEPAD) || bm.has_abs(ABS_HAT0X) {
        return DeviceType::Gamepad;
    }
    if bm.has_key(BTN_TOOL_PEN) || bm.has_key(BTN_STYLUS) {
        return DeviceType::Tablet;
    }
    if bm.has_abs(ABS_MT_SLOT) || bm.has_abs(ABS_MT_POSITION_X) {
        return if bm.has_prop(INPUT_PROP_DIRECT) {
            DeviceType::Touchscreen
        } else {
            DeviceType::Touchpad
        };
    }
    if bm.has_rel(REL_X) && bm.has_key(BTN_LEFT) {
        return DeviceType::Mouse;
    }
    if bm.has_key(KEY_A_CODE) {
        return DeviceType::Keyboard;
    }
    DeviceType::Unknown
}

fn capabilities_from(bm: &Bitmaps) -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::empty();
    if bm.has_ev(EV_KEY) {
        caps.insert(DeviceCapabilities::KEYS);
    }
    if bm.has_ev(EV_REL) {
        caps.insert(DeviceCapabilities::REL_MOTION);
    }
    if bm.has_ev(EV_ABS) {
        caps.insert(DeviceCapabilities::ABS_MOTION);
    }
    if bm.has_key(BTN_LEFT) {
        caps.insert(DeviceCapabilities::BUTTONS);
    }
    if bm.has_rel(REL_WHEEL) || bm.has_rel(REL_HWHEEL) {
        caps.insert(DeviceCapabilities::SCROLL);
    }
    if bm.has_abs(ABS_MT_SLOT) {
        caps.insert(DeviceCapabilities::MULTITOUCH);
    }
    if bm.has_abs(ABS_PRESSURE) {
        caps.insert(DeviceCapabilities::PRESSURE);
    }
    if bm.has_abs(ABS_TILT_X) {
        caps.insert(DeviceCapabilities::TILT);
    }
    caps
}

/// An opened, classified evdev node ready to be pumped by the poll loop.
struct OpenDevice {
    file: File,
    handler: Box<dyn InputDevice>,
}

fn open_and_classify(path: &Path) -> Result<(OpenDevice, Device)> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .or_else(|_| OpenOptions::new().read(true).open(path))
        .map_err(|e| InputError::DeviceOpen {
            path: path.display().to_string(),
            source: e,
        })?;
    let fd = file.as_raw_fd();

    let bitmaps = Bitmaps::query(fd);
    let device_type = classify(&bitmaps);
    let caps = capabilities_from(&bitmaps);
    let name = read_name(fd);
    let id = read_id(fd);
    let device_id = DeviceId::new();

    let handler: Box<dyn InputDevice> = match device_type {
        DeviceType::Keyboard | DeviceType::Switch | DeviceType::Unknown => {
            Box::new(Keyboard::new(device_id))
        }
        DeviceType::Mouse => Box::new(Mouse::new(device_id)),
        DeviceType::Touchpad | DeviceType::Touchscreen => {
            let slot_count = read_abs_info(fd, ABS_MT_SLOT)
                .map(|a| (a.maximum + 1).max(1) as usize)
                .unwrap_or(10);
            let x_max = read_abs_info(fd, ABS_MT_POSITION_X).map(|a| a.maximum).unwrap_or(4095);
            let y_max = read_abs_info(fd, ABS_MT_POSITION_Y).map(|a| a.maximum).unwrap_or(4095);
            let mut td = TouchDevice::new(device_id, slot_count, x_max, y_max);
            if let Some(p) = read_abs_info(fd, ABS_MT_PRESSURE) {
                td = td.with_pressure_range(p.maximum);
            }
            if let Some(s) = read_abs_info(fd, ABS_MT_TOUCH_MAJOR) {
                td = td.with_size_range(s.maximum);
            }
            Box::new(td)
        }
        DeviceType::Tablet => {
            let x_max = read_abs_info(fd, ABS_X).map(|a| a.maximum).unwrap_or(32767);
            let y_max = read_abs_info(fd, ABS_Y).map(|a| a.maximum).unwrap_or(32767);
            let p_max = read_abs_info(fd, ABS_PRESSURE).map(|a| a.maximum).unwrap_or(2047);
            Box::new(Tablet::new(device_id, x_max, y_max, p_max))
        }
        DeviceType::Gamepad => Box::new(Gamepad::new(device_id, GamepadMapping::standard_xbox())),
    };

    let mut info = Device::new(name, device_type)
        .with_capabilities(caps)
        .with_ids(id.bustype, id.vendor, id.product)
        .with_node_path(path.display().to_string());
    info.id = device_id;

    Ok((OpenDevice { file, handler }, info))
}

fn scan_event_nodes(dir: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix("event") {
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                    paths.push(entry.path());
                }
            }
        }
    }
    paths
}

/// Bookkeeping for the discovery loop: which fds are open, which
/// [`DeviceId`] and source path each corresponds to, and which paths we've
/// already claimed (so a rescan doesn't try to reopen them).
#[derive(Default)]
struct ScanState {
    devices: HashMap<RawFd, OpenDevice>,
    ids: HashMap<RawFd, DeviceId>,
    fd_to_path: HashMap<RawFd, PathBuf>,
    open_paths: std::collections::HashSet<PathBuf>,
}

/// Open every not-yet-tracked `/dev/input/eventN` node, classify it, and
/// publish a [`DeviceEvent::Added`] for each. Nodes that fail to open
/// (typically a permissions error) are skipped silently and retried on the
/// next call -- permissions can change at runtime via udev/ACLs.
fn rescan_devices(input_dir: &str, state: &mut ScanState, sink: &EventSink) {
    for path in scan_event_nodes(input_dir) {
        if state.open_paths.contains(&path) {
            continue;
        }
        if let Ok((opened, info)) = open_and_classify(&path) {
            let fd = opened.file.as_raw_fd();
            let id = info.id;
            state.open_paths.insert(path.clone());
            state.fd_to_path.insert(fd, path);
            state.ids.insert(fd, id);
            state.devices.insert(fd, opened);
            sink.send(Event::Device(DeviceEvent::Added(info)));
        }
    }
}

// ---------------------------------------------------------------------
// inotify-based hotplug watch (best-effort; a periodic rescan still runs so
// a missed inotify event doesn't permanently hide a device).
// ---------------------------------------------------------------------

fn init_inotify_watch(dir: &str) -> Option<File> {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let c_path = CString::new(dir).ok()?;
    let wd = unsafe {
        libc::inotify_add_watch(fd, c_path.as_ptr(), libc::IN_CREATE | libc::IN_DELETE)
    };
    if wd < 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    // SAFETY: `fd` was just returned by `inotify_init1` and we own it.
    Some(unsafe { File::from_raw_fd(fd) })
}

// ---------------------------------------------------------------------
// InputSource implementation
// ---------------------------------------------------------------------

/// Discovers and reads Linux `/dev/input/eventN` devices, translating raw
/// kernel events into [`Event`]s.
pub struct EvdevSource {
    input_dir: String,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl EvdevSource {
    pub fn new() -> Self {
        EvdevSource {
            input_dir: "/dev/input".to_string(),
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    pub fn with_input_dir(mut self, dir: impl Into<String>) -> Self {
        self.input_dir = dir.into();
        self
    }

    fn run_loop(input_dir: String, running: Arc<AtomicBool>, sink: EventSink) {
        let start = Instant::now();
        let mut state = ScanState::default();
        let inotify = init_inotify_watch(&input_dir);

        rescan_devices(&input_dir, &mut state, &sink);

        let mut last_periodic_rescan = Instant::now();
        while running.load(Ordering::SeqCst) {
            let mut pollfds: Vec<libc::pollfd> = Vec::new();
            let mut fd_order: Vec<RawFd> = Vec::new();
            if let Some(inotify_file) = &inotify {
                pollfds.push(libc::pollfd {
                    fd: inotify_file.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            for fd in state.devices.keys() {
                pollfds.push(libc::pollfd {
                    fd: *fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
                fd_order.push(*fd);
            }

            let n = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 200) };
            if n < 0 {
                // EINTR or similar; loop and re-check `running`.
                continue;
            }

            let mut idx = 0;
            let mut need_rescan = false;
            if let Some(inotify_file) = &inotify {
                if pollfds[idx].revents & libc::POLLIN != 0 {
                    let mut discard = [0u8; 4096];
                    unsafe {
                        libc::read(
                            inotify_file.as_raw_fd(),
                            discard.as_mut_ptr() as *mut libc::c_void,
                            discard.len(),
                        );
                    }
                    need_rescan = true;
                }
                idx += 1;
            }

            let mut dead: Vec<RawFd> = Vec::new();
            for fd in &fd_order {
                let revents = pollfds[idx].revents;
                idx += 1;
                if revents == 0 {
                    continue;
                }
                if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    dead.push(*fd);
                    continue;
                }
                if revents & libc::POLLIN == 0 {
                    continue;
                }
                if let Some(open_dev) = state.devices.get_mut(fd) {
                    match read_events(&mut open_dev.file) {
                        Ok(raw_events) => {
                            for raw in raw_events {
                                let time = start.elapsed();
                                let out = open_dev.handler.handle_event(
                                    raw.type_,
                                    raw.code,
                                    raw.value,
                                    time,
                                );
                                for ev in out {
                                    sink.send(ev);
                                }
                            }
                        }
                        Err(_) => dead.push(*fd),
                    }
                }
            }

            for fd in dead {
                state.devices.remove(&fd);
                if let Some(path) = state.fd_to_path.remove(&fd) {
                    state.open_paths.remove(&path);
                }
                if let Some(id) = state.ids.remove(&fd) {
                    sink.send(Event::Device(DeviceEvent::Removed(id)));
                }
            }

            if need_rescan || last_periodic_rescan.elapsed() > std::time::Duration::from_secs(2) {
                // Drop bookkeeping for nodes that no longer exist so a
                // physically removed-then-replugged device (same path) can
                // be picked up again.
                state.open_paths.retain(|p| p.exists());
                rescan_devices(&input_dir, &mut state, &sink);
                last_periodic_rescan = Instant::now();
            }
        }
    }
}

impl Default for EvdevSource {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSource for EvdevSource {
    fn name(&self) -> &str {
        "evdev"
    }

    fn start(&mut self, sink: EventSink) -> Result<()> {
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let dir = self.input_dir.clone();
        self.thread = Some(thread::spawn(move || {
            EvdevSource::run_loop(dir, running, sink);
        }));
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Read every complete `input_event` currently available on `file` without
/// blocking past what's already buffered by the kernel (the caller only
/// calls this after `poll` indicated readability).
fn read_events(file: &mut File) -> io::Result<Vec<RawInputEvent>> {
    const EVENT_SIZE: usize = std::mem::size_of::<RawInputEvent>();
    let mut buf = [0u8; EVENT_SIZE * 64];
    let n = file.read(&mut buf)?;
    let mut out = Vec::with_capacity(n / EVENT_SIZE);
    let mut offset = 0;
    while offset + EVENT_SIZE <= n {
        // SAFETY: RawInputEvent is `#[repr(C)]` and made only of integer
        // fields, so any byte pattern of the right length is valid; the
        // buffer is at least EVENT_SIZE bytes at `offset` by the loop guard.
        let raw: RawInputEvent = unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr() as *const RawInputEvent) };
        out.push(raw);
        offset += EVENT_SIZE;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_known_kernel_constants() {
        // EVIOCGID is widely documented/observed as 0x80084502 on 64-bit
        // Linux; this is a strong sanity check on the whole _IOC encoding.
        assert_eq!(eviocgid(), 0x8008_4502);
    }

    #[test]
    fn bit_test_reads_expected_bits() {
        // bit 3 and bit 9 set: byte0 = 0b0000_1000, byte1 = 0b0000_0010
        let bitmap = [0b0000_1000u8, 0b0000_0010u8];
        assert!(test_bit(&bitmap, 3));
        assert!(test_bit(&bitmap, 9));
        assert!(!test_bit(&bitmap, 0));
        assert!(!test_bit(&bitmap, 100)); // out of range is just "unset"
    }

    #[test]
    fn classify_prefers_gamepad_over_generic_key() {
        let mut bm = Bitmaps {
            ev: vec![0xff; 4],
            key: vec![0u8; bytes_for_bits(KEY_CNT)],
            rel: vec![0u8; bytes_for_bits(REL_CNT)],
            abs: vec![0u8; bytes_for_bits(ABS_CNT)],
            props: vec![0u8; bytes_for_bits(INPUT_PROP_CNT)],
        };
        let byte = BTN_GAMEPAD as usize / 8;
        let shift = BTN_GAMEPAD as usize % 8;
        bm.key[byte] |= 1 << shift;
        assert_eq!(classify(&bm), DeviceType::Gamepad);
    }
}
