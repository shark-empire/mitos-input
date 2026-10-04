//! Virtual devices via Linux `uinput`: [`VirtualKeyboard`], [`VirtualMouse`],
//! and [`VirtualDeviceManager`] which lazily creates them on first use (for
//! IPC-driven injection -- see [`crate::ipc`]).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::{AsRawFd, RawFd};

use crate::error::{ioctl_error, InputError, Result};
use crate::evdev::{
    BTN_BACK, BTN_EXTRA, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, EV_KEY, EV_REL,
    EV_SYN, KEY_MAX, REL_HWHEEL, REL_WHEEL, REL_X, REL_Y, SYN_REPORT,
};
use crate::keyboard::KeyCode;
use crate::pointer::Button;

pub const UINPUT_PATH: &str = "/dev/uinput";
const UINPUT_MAX_NAME_SIZE: usize = 80;

// ---------------------------------------------------------------------
// uinput ioctl numbers (linux/uinput.h). The _IOC encoding machinery below
// is computed the same way as in `crate::evdev` but kept separate/duplicated
// intentionally, since uinput's ioctl numbers (UI_SET_EVBIT, UI_DEV_CREATE,
// ...) are a distinct set from evdev's (EVIOCGBIT, EVIOCGID, ...); only the
// plain protocol constants (EV_KEY, BTN_LEFT, ...) are shared, imported
// above from `crate::evdev`.
// ---------------------------------------------------------------------

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_SIZEMASK: usize = (1 << IOC_SIZEBITS) - 1;
const IOC_NONE: u64 = 0;
const IOC_WRITE: u64 = 1;

const fn ioc(dir: u64, ty: u8, nr: u8, size: usize) -> u64 {
    (dir << IOC_DIRSHIFT)
        | ((ty as u64) << IOC_TYPESHIFT)
        | ((nr as u64) << IOC_NRSHIFT)
        | (((size & IOC_SIZEMASK) as u64) << IOC_SIZESHIFT)
}
const fn iow(ty: u8, nr: u8, size: usize) -> u64 {
    ioc(IOC_WRITE, ty, nr, size)
}
const fn io_(ty: u8, nr: u8) -> u64 {
    ioc(IOC_NONE, ty, nr, 0)
}

fn ui_set_evbit() -> u64 {
    iow(b'U', 100, std::mem::size_of::<i32>())
}
fn ui_set_keybit() -> u64 {
    iow(b'U', 101, std::mem::size_of::<i32>())
}
fn ui_set_relbit() -> u64 {
    iow(b'U', 102, std::mem::size_of::<i32>())
}
fn ui_dev_create() -> u64 {
    io_(b'U', 1)
}
fn ui_dev_destroy() -> u64 {
    io_(b'U', 2)
}
fn ui_dev_setup() -> u64 {
    iow(b'U', 3, std::mem::size_of::<UinputSetup>())
}

#[repr(C)]
struct InputIdRaw {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputIdRaw,
    name: [u8; UINPUT_MAX_NAME_SIZE],
    ff_effects_max: u32,
}

/// Mirrors `struct input_event`, matching [`crate::evdev`]'s definition
/// (kept local so this module doesn't need to reach into `evdev`'s private
/// items).
#[repr(C)]
struct RawInputEvent {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

unsafe fn ioctl_ptr<T>(fd: RawFd, request: u64, arg: *mut T, call: &'static str) -> Result<i32> {
    let ret = libc::ioctl(fd, request as _, arg);
    if ret < 0 {
        Err(ioctl_error(call))
    } else {
        Ok(ret)
    }
}

unsafe fn ioctl_val(fd: RawFd, request: u64, call: &'static str) -> Result<i32> {
    let ret = libc::ioctl(fd, request as _, 0usize);
    if ret < 0 {
        Err(ioctl_error(call))
    } else {
        Ok(ret)
    }
}

fn write_event(file: &mut File, ev_type: u16, code: u16, value: i32) -> io::Result<()> {
    let ev = RawInputEvent {
        tv_sec: 0,
        tv_usec: 0,
        type_: ev_type,
        code,
        value,
    };
    // SAFETY: RawInputEvent is `#[repr(C)]` and plain-old-data; reading its
    // bytes for a write() is always valid.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            &ev as *const RawInputEvent as *const u8,
            std::mem::size_of::<RawInputEvent>(),
        )
    };
    file.write_all(bytes)
}

fn set_name(buf: &mut [u8; UINPUT_MAX_NAME_SIZE], name: &str) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(UINPUT_MAX_NAME_SIZE - 1);
    buf[..n].copy_from_slice(&bytes[..n]);
    buf[n] = 0;
}

fn open_uinput() -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(UINPUT_PATH)
        .map_err(|e| InputError::DeviceOpen {
            path: UINPUT_PATH.to_string(),
            source: e,
        })
}

fn create_device(mut file: File, name: &str, ev_bits: &[u16], key_bits: &[u16]) -> Result<File> {
    let fd = file.as_raw_fd();
    unsafe {
        for &ev in ev_bits {
            let mut v = ev as i32;
            ioctl_ptr(fd, ui_set_evbit(), &mut v as *mut i32, "UI_SET_EVBIT")?;
        }
        for &key in key_bits {
            let mut v = key as i32;
            ioctl_ptr(fd, ui_set_keybit(), &mut v as *mut i32, "UI_SET_KEYBIT")?;
        }
        if ev_bits.contains(&EV_REL) {
            for rel_code in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
                let mut v = rel_code as i32;
                ioctl_ptr(fd, ui_set_relbit(), &mut v as *mut i32, "UI_SET_RELBIT")?;
            }
        }

        let mut setup = UinputSetup {
            id: InputIdRaw {
                bustype: 0x06, // BUS_VIRTUAL
                vendor: 0x4d49,   // "MI" (MITOS), arbitrary but stable
                product: 0x544f,  // "TO"
                version: 1,
            },
            name: [0u8; UINPUT_MAX_NAME_SIZE],
            ff_effects_max: 0,
        };
        set_name(&mut setup.name, name);
        ioctl_ptr(fd, ui_dev_setup(), &mut setup as *mut UinputSetup, "UI_DEV_SETUP")?;
        ioctl_val(fd, ui_dev_create(), "UI_DEV_CREATE")?;
    }
    // The kernel finishes registering the new /dev/input/eventN node for
    // this device asynchronously; give it a brief moment so code elsewhere
    // that immediately re-scans devices (see `crate::evdev`) is more likely
    // to see it show up right away. Not required for writing events below.
    std::thread::sleep(std::time::Duration::from_millis(10));
    Ok(file)
}

/// A synthetic keyboard, indistinguishable to the rest of the system from a
/// real one once created.
pub struct VirtualKeyboard {
    file: File,
}

impl VirtualKeyboard {
    /// Create the virtual device, registering every named [`KeyCode`]
    /// constant plus the full extended range so arbitrary raw codes can
    /// still be injected later.
    pub fn create(name: &str) -> Result<Self> {
        let file = open_uinput()?;
        let key_bits: Vec<u16> = (0..=KEY_MAX).collect();
        let file = create_device(file, name, &[EV_KEY, EV_SYN], &key_bits)?;
        Ok(VirtualKeyboard { file })
    }

    pub fn key_raw(&mut self, code: u16, pressed: bool) -> Result<()> {
        write_event(&mut self.file, EV_KEY, code, pressed as i32).map_err(InputError::from)?;
        write_event(&mut self.file, EV_SYN, SYN_REPORT, 0).map_err(InputError::from)
    }

    pub fn key(&mut self, code: KeyCode, pressed: bool) -> Result<()> {
        self.key_raw(code.0, pressed)
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        unsafe {
            let _ = ioctl_val(self.file.as_raw_fd(), ui_dev_destroy(), "UI_DEV_DESTROY");
        }
    }
}

/// A synthetic relative-motion mouse with the standard three buttons plus
/// side/extra.
pub struct VirtualMouse {
    file: File,
}

impl VirtualMouse {
    pub fn create(name: &str) -> Result<Self> {
        let file = open_uinput()?;
        let buttons = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA];
        let file = create_device(file, name, &[EV_KEY, EV_REL, EV_SYN], &buttons)?;
        Ok(VirtualMouse { file })
    }

    pub fn motion(&mut self, dx: i32, dy: i32) -> Result<()> {
        if dx != 0 {
            write_event(&mut self.file, EV_REL, REL_X, dx).map_err(InputError::from)?;
        }
        if dy != 0 {
            write_event(&mut self.file, EV_REL, REL_Y, dy).map_err(InputError::from)?;
        }
        write_event(&mut self.file, EV_SYN, SYN_REPORT, 0).map_err(InputError::from)
    }

    pub fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        let code = match button {
            Button::Left => BTN_LEFT,
            Button::Right => BTN_RIGHT,
            Button::Middle => BTN_MIDDLE,
            Button::Side => BTN_SIDE,
            Button::Extra => BTN_EXTRA,
            Button::Forward => BTN_FORWARD,
            Button::Back => BTN_BACK,
            Button::Other(code) => code,
        };
        write_event(&mut self.file, EV_KEY, code, pressed as i32).map_err(InputError::from)?;
        write_event(&mut self.file, EV_SYN, SYN_REPORT, 0).map_err(InputError::from)
    }

    pub fn scroll(&mut self, vertical: i32, horizontal: i32) -> Result<()> {
        if vertical != 0 {
            write_event(&mut self.file, EV_REL, REL_WHEEL, vertical).map_err(InputError::from)?;
        }
        if horizontal != 0 {
            write_event(&mut self.file, EV_REL, REL_HWHEEL, horizontal).map_err(InputError::from)?;
        }
        write_event(&mut self.file, EV_SYN, SYN_REPORT, 0).map_err(InputError::from)
    }
}

impl Drop for VirtualMouse {
    fn drop(&mut self) {
        unsafe {
            let _ = ioctl_val(self.file.as_raw_fd(), ui_dev_destroy(), "UI_DEV_DESTROY");
        }
    }
}

/// Lazily creates and owns the virtual devices used to satisfy IPC
/// injection requests (`INJECT-KEY`, `INJECT-MOTION`, ...), so a client
/// that never asks for injection never causes a `/dev/uinput` open.
#[derive(Default)]
pub struct VirtualDeviceManager {
    keyboard: Option<VirtualKeyboard>,
    mouse: Option<VirtualMouse>,
}

impl VirtualDeviceManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn keyboard(&mut self) -> Result<&mut VirtualKeyboard> {
        if self.keyboard.is_none() {
            self.keyboard = Some(VirtualKeyboard::create("mitos-input virtual keyboard")?);
        }
        Ok(self.keyboard.as_mut().unwrap())
    }

    fn mouse(&mut self) -> Result<&mut VirtualMouse> {
        if self.mouse.is_none() {
            self.mouse = Some(VirtualMouse::create("mitos-input virtual mouse")?);
        }
        Ok(self.mouse.as_mut().unwrap())
    }

    pub fn inject_key(&mut self, code: u16, pressed: bool) -> Result<()> {
        self.keyboard()?.key_raw(code, pressed)
    }

    pub fn inject_motion(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.mouse()?.motion(dx, dy)
    }

    pub fn inject_button(&mut self, button: Button, pressed: bool) -> Result<()> {
        self.mouse()?.button(button, pressed)
    }

    pub fn inject_scroll(&mut self, vertical: i32, horizontal: i32) -> Result<()> {
        self.mouse()?.scroll(vertical, horizontal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_dev_create_matches_known_kernel_constant() {
        // Widely documented as 0x5501 for UI_DEV_CREATE.
        assert_eq!(ui_dev_create(), 0x5501);
    }

    #[test]
    fn ui_set_evbit_matches_known_kernel_constant() {
        assert_eq!(ui_set_evbit(), 0x4004_5564);
    }
}
