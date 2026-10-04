# mitos-input

The input device management subsystem for the MITOS desktop environment:
keyboards, pointers, touch, tablets, and gamepads; the Linux `evdev`/`uinput`
backends; a Bluetooth HID bridge; global hotkeys and multi-finger gestures;
seat/session gating; and the IPC server other MITOS components (the
compositor, accessibility tools, an on-screen keyboard, ...) use to subscribe
to events and inject synthetic input.

```
Bluetooth Keyboard  ──▶ mitos-bluetooth ──▶ HID events ──▶┐
USB Keyboard        ──▶ Linux kernel ─────────────────────┤
Laptop Keyboard     ──▶ Linux kernel ─────────────────────┤
                                                           ▼
                                                     mitos-input
                                                           │
                                                      KeyEvent (IPC)
                                                           ▼
                                                 mitos-gui / compositor
                                                           │
                                                           ▼
                                                      Application
```

## Why so few dependencies

The whole crate has exactly **one** external dependency: [`libc`](https://crates.io/crates/libc),
needed for raw `ioctl`/`poll`/`inotify` syscalls in `evdev.rs` and `uinput.rs`.
Everything else — error handling, bit flags, the IPC wire format, the event
bus — is hand-rolled on top of `std`. For a privileged, always-running input
daemon that can observe every keystroke on the system, a small, auditable
dependency tree is a feature, not a shortcut.

## Building

```sh
cargo build --release
cargo test
```

`cargo test` runs entirely in-process (no real `/dev/input` or `/dev/uinput`
access needed — see [Testing](#testing)). Actually running the daemon
(`cargo run`) needs read access to `/dev/input/event*` and, for synthetic
input via `INJECT-*` IPC commands, write access to `/dev/uinput`.

```sh
mitos-input [--socket PATH] [--bluetooth] [--help]
```

| Flag             | Meaning                                                        |
|------------------|-----------------------------------------------------------------|
| `--socket PATH`  | IPC socket path (default `/run/mitos/input.sock`)               |
| `--bluetooth`    | also start the Bluetooth HID bridge (socket `/run/mitos/bluetooth-bridge.sock`) |
| `--help`, `-h`   | print usage and exit                                             |

The process shuts down cleanly on `SIGINT`/`SIGTERM`, stopping every source
and removing its socket files.

## Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│                              InputManager                             │
│                              (input.rs)                               │
│                                                                        │
│   ┌─────────────┐    ┌──────────────────┐     device registry        │
│   │ EvdevSource │───▶│                  │───▶ seat assignment         │
│   │ (evdev.rs)  │    │  EventSink/mpsc  │                             │
│   └─────────────┘    │  channel + the   │     hotkey matching         │
│   ┌─────────────┐    │  dispatch loop   │───▶ (hotkeys.rs)            │
│   │ Bluetooth   │───▶│                  │                             │
│   │ Bridge      │    └──────────────────┘            │                │
│   │(bluetooth.rs)│                                   ▼                │
│   └─────────────┘                              ┌───────────┐          │
│                                                 │ IpcServer │──▶ subscribers
│   ┌──────────────────────┐                     │ (ipc.rs)  │          │
│   │ VirtualDeviceManager │◀────────────────────│  INJECT-* │          │
│   │     (uinput.rs)      │                     └───────────┘          │
│   └──────────────────────┘                                            │
└──────────────────────────────────────────────────────────────────────┘
```

Each physical or virtual device is represented by one of five stateful
"device-type" structs, each implementing the `InputDevice` trait
(`handle_event(type, code, value, time) -> Vec<Event>`) so a backend can
drive them polymorphically:

| Device type | Module         | Owns                                              |
|-------------|----------------|----------------------------------------------------|
| Keyboard    | `keyboard.rs`  | held keys, modifier mask, keymap translation       |
| Mouse       | `pointer.rs`   | position, button mask, scroll, accel profile       |
| Touch       | `touch.rs`     | multitouch slots, tap detection, **owns a `GestureEngine`** |
| Tablet      | `tablet.rs`    | pen position/pressure/tilt, proximity, tool state  |
| Gamepad     | `gamepad.rs`   | button/axis state, deadzone, hat-switch-as-dpad    |

Two backends (`InputSource` trait: `start(sink)`, `stop()`) discover devices
and feed them raw protocol codes:

- **`evdev.rs`** — talks directly to `/dev/input/eventN` via raw `ioctl`s (no
  `libinput`/`libevdev` dependency). Classifies each node by its reported
  capabilities (gamepad → tablet → touch → mouse → keyboard, most specific
  first), watches `/dev/input` with `inotify` for hotplug (plus a 2s periodic
  rescan as a safety net), and multiplexes every open device with a single
  `poll()` loop.
- **`bluetooth.rs`** — *receives* already-paired HID boot-protocol reports
  from a separate `mitos-bluetooth` process over a Unix socket (pairing and
  the BlueZ/D-Bus HID-Host interaction live there, not in this crate) and
  diffs them into the same `Keyboard`/`Mouse` state machines `evdev.rs` uses,
  so the rest of the system can't tell a Bluetooth key press from a USB one.

Cross-cutting systems:

- **`hotkeys.rs`** — exact key+modifier chord matching, with the default
  MITOS shortcuts (`Super+D` show desktop, `Super+Tab` switch window,
  `Super+1..9` switch workspace, `Super+L` lock, `PrintScreen` screenshot,
  ...) pre-registered.
- **`gestures.rs`** — multi-finger swipe/pinch/hold recognition from
  per-frame touch snapshots (centroid + average pairwise spacing, compared
  frame-to-frame), plus the mapping to desktop actions (3-finger up →
  overview, 4-finger left/right → switch workspace). Single-finger taps are
  detected inline in `touch.rs` instead, since they're a property of one
  contact's lifecycle rather than a cross-finger pattern.
- **`seat.rs`** — groups devices into seats and gates delivery while a
  seat's session is inactive (VT-switched away). Deliberately doesn't talk to
  `logind`/`libseat` itself — the embedding compositor already gets those
  enable/disable notifications and just calls `set_session_active` here,
  keeping D-Bus out of this crate's dependency tree.
- **`uinput.rs`** — creates virtual keyboard/mouse devices via `/dev/uinput`
  for the IPC `INJECT-*` commands (on-screen keyboards, accessibility tools,
  automated testing).
- **`ipc.rs`** — the Unix-socket server described below.

## IPC protocol

Newline-delimited text on a Unix socket (default `/run/mitos/input.sock`),
in the spirit of `libinput debug-events`. One command per line in; replies
and (once subscribed) event lines share the same stream, so **clients must
dispatch on the first token of each line** rather than assuming replies and
events alternate predictably.

**The socket is created owner-only (`0600`) by default.** Anyone who can
connect can observe every keystroke and inject arbitrary input — widen the
mode deliberately (`IpcServer::with_socket_mode`) only with a trusted,
dedicated group.

### Commands

| Command | Reply | Notes |
|---|---|---|
| `PING` | `PONG` | |
| `LIST-DEVICES` | `DEVICE ...` lines, then `END` | |
| `SUBSCRIBE [all \| kind[,kind...]]` | `OK` | kinds: `key pointer touch tablet gamepad device gesture hotkey`; subscription is active by the time `OK` arrives |
| `UNSUBSCRIBE` | `OK` | |
| `INJECT-KEY <code> <press\|release>` | `OK` / `ERR ...` | `code` is a raw Linux keycode |
| `INJECT-MOTION <dx> <dy>` | `OK` / `ERR ...` | |
| `INJECT-BUTTON <left\|right\|middle\|side\|extra\|forward\|back> <press\|release>` | `OK` / `ERR ...` | |
| `INJECT-SCROLL <vertical> <horizontal>` | `OK` / `ERR ...` | |
| `QUIT` | `OK`, then closes | |

Free-text values (device names, custom hotkey action names) are emitted
double-quoted with quotes/backslashes/control characters escaped, so a
hostile device name can never forge a fake protocol line.

### Example session

```
$ socat - UNIX-CONNECT:/run/mitos/input.sock
SUBSCRIBE key,gesture
OK
KEY dev=3 t=12.345678 code=30 key=A state=pressed mods=0x00 repeat=0
GESTURE-SWIPE dev=7 t=12.9 fingers=3 dir=up dx=0.01 dy=-0.08
```

## Testing

- **Unit tests** (`cargo test --lib`) live alongside each module and cover
  pure logic: modifier tracking, keymap translation, pointer acceleration,
  multitouch slot bookkeeping, gesture recognition, gamepad deadzone/axis
  normalization, hotkey matching, seat gating, and the IPC wire protocol
  (including a loopback client/server test over a real — but temp-path —
  Unix socket). None of this needs root or real hardware.
- **Integration tests** (`tests/keyboard.rs`, `tests/pointer.rs`,
  `tests/touch.rs`, `tests/gamepad.rs`) exercise the same logic strictly
  through the crate's public API, the way an external consumer would.
- `evdev.rs` and `uinput.rs` additionally carry regression tests that check
  the hand-rolled `ioctl` number encoding against independently-documented
  kernel constants (e.g. `EVIOCGID == 0x80084502`, `UI_DEV_CREATE == 0x5501`)
  as a sanity check on the whole `_IOC` bit-layout implementation.

Device I/O itself (opening real `/dev/input`/`/dev/uinput` nodes) isn't unit
tested, since that needs root/hardware/CI support this crate doesn't assume;
exercise it with `cargo run` on real hardware.

## Known limitations / extension points

- **Seat backend**: `seat.rs` is a self-contained local implementation, not
  wired to `logind` or `libseat`. A full desktop session would have the
  compositor forward libseat's enable/disable (or logind's `PauseDevice`/
  `ResumeDevice`) signals into `SeatManager::set_session_active`.
- **HID usage table**: `bluetooth.rs`'s USB-HID-usage-to-`KeyCode` table
  covers the common alphanumeric/navigation/function-key range, not the full
  0–255 usage space. Extend `hid_usage_to_keycode` following the same
  pattern (USB HID Usage Tables, Keyboard/Keypad Page) for less common keys.
- **Keymap**: `keyboard::Keymap` ships a single static US-QWERTY layout, not
  a full layout engine (no dead keys, compose sequences, or non-Latin
  layouts). Swap in a different `Keymap` per device as needed.
- **IPC transport**: broadcasting holds each subscriber's write lock (with a
  200ms write timeout) in turn; a handful of simultaneously-stalled
  subscribers can delay delivery to healthy ones by up to that long each.
  Acceptable for the expected handful of local clients; a design with
  per-subscriber channels would remove the bound entirely if that ever
  matters.

## License

MIT OR Apache-2.0
