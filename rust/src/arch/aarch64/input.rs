//! Keyboard and pointer: virtio input devices (a keyboard, and a tablet
//! that reports where the host's pointer is). They send the same events
//! as Linux input devices: (type, code, value).

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::rt::KCell;
use crate::virtio::{self, Kind, Queue, Transport};
use crate::{console, wm};

const EVENT_SIZE: usize = 8;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_TOUCH: u16 = 0x14A;
const REL_WHEEL: u16 = 8;

struct Device {
    dev: Box<dyn Transport>,
    events: Queue,
}

struct Input {
    devices: Vec<Device>,
    /// the pointer: position (0..32767 on both axes), buttons, wheel steps
    x: i32,
    y: i32,
    buttons: i32,
    wheel: i32,
    moved: bool,
}

static INPUT: KCell<Input> = KCell::new(Input { devices: Vec::new(), x: 0, y: 0, buttons: 0, wheel: 0, moved: false });

/// The PC scancode (set 1) of a Linux key code, and whether it is one of
/// the "extended" keys; see `console::key`.
fn scancode(key: u16) -> Option<(u8, bool)> {
    Some(match key {
        1..=88 => (key as u8, false), // the main block has the same numbers
        96 => (0x1C, true),           // keypad Enter
        97 => (0x1D, true),           // right Ctrl
        100 => (0x38, true),          // right Alt
        102 => (0x47, true),          // Home
        103 => (0x48, true),          // Up
        104 => (0x49, true),          // Page Up
        105 => (0x4B, true),          // Left
        106 => (0x4D, true),          // Right
        107 => (0x4F, true),          // End
        108 => (0x50, true),          // Down
        109 => (0x51, true),          // Page Down
        111 => (0x53, true),          // Delete
        92 => (0x79, false),          // henkan
        93 => (0x70, false),          // katakana/hiragana
        94 => (0x7B, false),          // muhenkan
        122 => (0x72, false),         // LANG1 (かな)
        123 => (0x71, false),         // LANG2 (英数)
        _ => return None,
    })
}

/// The host pointer is followed exactly.
pub fn pointer_absolute() -> bool {
    INPUT.with(|i| !i.devices.is_empty())
}

/// Starts the keyboard and the pointer (graphical mode).
pub fn init_pointer() {
    let mut devices = Vec::new();
    for mut dev in virtio::find(Kind::Input) {
        dev.set_status(0);
        dev.set_status(1); // acknowledge
        dev.set_status(1 | 2); // driver
        dev.set_driver_features(0);
        let Some(mut events) = Queue::new(dev.as_mut(), 0, 64, EVENT_SIZE) else { continue };
        dev.set_status(1 | 2 | 4); // driver ok
        events.fill(dev.as_mut());
        devices.push(Device { dev, events });
    }
    INPUT.with(|i| i.devices = devices);
}

enum Event {
    Key(u8, bool, bool),
    Pointer(i32, i32, i32, i32),
}

/// Drains the keyboard and the pointer.
pub fn poll() {
    // collect first: delivering an event may come back for the state
    let events = INPUT.with(|input| {
        let mut out = Vec::new();
        let Input { devices, x, y, buttons, wheel, moved } = input;
        for d in devices.iter_mut() {
            let mut returned = false;
            while let Some((slot, len)) = d.events.pop_used() {
                if slot >= d.events.slots as u32 {
                    continue;
                }
                let slot = slot as u16;
                let b = d.events.buf(slot);
                let (kind, code) = (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]));
                let value = i32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                d.events.push_avail(slot);
                returned = true;
                if len as usize != EVENT_SIZE {
                    continue;
                }
                let mut button = |bit: i32| {
                    *buttons = if value != 0 { *buttons | bit } else { *buttons & !bit };
                    *moved = true;
                };
                match (kind, code) {
                    (EV_KEY, BTN_LEFT | BTN_TOUCH) => button(1),
                    (EV_KEY, BTN_RIGHT) => button(2),
                    (EV_KEY, BTN_MIDDLE) => button(4),
                    (EV_KEY, key) => {
                        // value: 0 released, 1 pressed, 2 repeated
                        if let Some((code, extended)) = scancode(key) {
                            out.push(Event::Key(code, extended, value != 0));
                        }
                    }
                    (EV_ABS, 0) => (*x, *moved) = (value, true),
                    (EV_ABS, 1) => (*y, *moved) = (value, true),
                    (EV_REL, REL_WHEEL) => (*wheel, *moved) = (*wheel + value, true),
                    (EV_SYN, _) if *moved => {
                        // the window manager takes 0..65535
                        out.push(Event::Pointer(*x * 2, *y * 2, *buttons, core::mem::take(wheel)));
                        *moved = false;
                    }
                    _ => {}
                }
            }
            if returned {
                d.events.notify(d.dev.as_mut());
            }
        }
        out
    });
    for e in events {
        match e {
            Event::Key(code, extended, down) => console::key(code, extended, down),
            Event::Pointer(x, y, buttons, wheel) => {
                if wm::active() {
                    wm::mouse_abs(x, y, buttons, wheel);
                }
            }
        }
    }
}
