//! `mitos-input`: thin binary wrapper around [`mitos_input::InputManager`].
//!
//! Usage:
//! ```text
//! mitos-input [--socket PATH] [--bluetooth] [--help]
//! ```
//! - `--socket PATH`   IPC socket path (default: see `mitos_input::ipc::DEFAULT_SOCKET_PATH`)
//! - `--bluetooth`     also start the Bluetooth HID bridge source
//! - `--help`, `-h`    print this usage and exit

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use mitos_input::{InputManager, Result};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Signal handler body: async-signal-safe by construction (a single atomic
/// store, nothing else). Actually stopping the manager happens on a normal
/// thread that polls this flag -- see `spawn_signal_watcher`.
extern "C" fn on_signal(_signum: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as libc::sighandler_t);
    }
}

fn spawn_signal_watcher(stop_handle: mitos_input::input::StopHandle) {
    thread::spawn(move || loop {
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            stop_handle.stop();
            return;
        }
        thread::sleep(Duration::from_millis(100));
    });
}

struct Args {
    socket_path: Option<String>,
    bluetooth: bool,
}

fn parse_args() -> std::result::Result<Args, String> {
    let mut socket_path = None;
    let mut bluetooth = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                socket_path = Some(args.next().ok_or("--socket requires a path argument")?);
            }
            "--bluetooth" => bluetooth = true,
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(Args { socket_path, bluetooth })
}

fn print_usage() {
    println!("mitos-input [--socket PATH] [--bluetooth] [--help]");
    println!();
    println!("  --socket PATH   IPC socket path (default: {})", mitos_input::ipc::DEFAULT_SOCKET_PATH);
    println!("  --bluetooth     also start the Bluetooth HID bridge source");
    println!("  --help, -h      print this usage and exit");
}

fn main() -> Result<()> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("mitos-input: {msg}");
            print_usage();
            std::process::exit(2);
        }
    };

    install_signal_handlers();

    let mut manager = InputManager::new();
    if args.bluetooth {
        manager = manager.with_bluetooth_bridge();
    }
    if let Some(path) = args.socket_path {
        manager = manager.with_ipc_socket_path(path);
    }

    spawn_signal_watcher(manager.stop_handle());

    eprintln!("mitos-input: starting (bluetooth bridge: {})", args.bluetooth);
    let result = manager.run();
    eprintln!("mitos-input: stopped");
    result
}
