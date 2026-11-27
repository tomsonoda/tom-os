//! Keyboard and pointer: the PS/2 keyboard and mouse, and the absolute
//! pointer that QEMU provides ("vmmouse").

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use super::pci::{inb, outb};
use crate::rt::KCell;
use crate::{console, wm};

// SAFETY (all port I/O in this file): the PC's keyboard controller.

struct Ps2 {
    /// an E0 prefix was received
    extended: bool,
    mouse_packet: [u8; 3],
    mouse_n: usize,
}

static PS2: KCell<Ps2> = KCell::new(Ps2 { extended: false, mouse_packet: [0; 3], mouse_n: 0 });
static VMMOUSE: AtomicBool = AtomicBool::new(false);

impl Ps2 {
    /// A byte from the keyboard: scancode set 1. Gives (code, extended, down).
    fn scancode(&mut self, sc: u8) -> Option<(u8, bool, bool)> {
        if sc == 0xE0 {
            self.extended = true;
            return None;
        }
        Some((sc & 0x7F, core::mem::take(&mut self.extended), sc & 0x80 == 0))
    }

    /// A byte from the mouse; a full packet gives (dx, dy, buttons).
    fn mouse_byte(&mut self, b: u8) -> Option<(i32, i32, i32)> {
        if self.mouse_n == 0 && b & 0x08 == 0 {
            return None; // resynchronise on the header byte
        }
        self.mouse_packet[self.mouse_n] = b;
        self.mouse_n += 1;
        if self.mouse_n < 3 {
            return None;
        }
        self.mouse_n = 0;
        let [head, x, y] = self.mouse_packet;
        if head & 0xC0 != 0 {
            return None; // overflow
        }
        let dx = x as i32 - if head & 0x10 != 0 { 256 } else { 0 };
        let dy = y as i32 - if head & 0x20 != 0 { 256 } else { 0 };
        Some((dx, -dy, (head & 7) as i32))
    }
}

fn wait_write() {
    let mut spins = 0;
    while spins < 100_000 && unsafe { inb(0x64) } & 2 != 0 {
        spins += 1;
    }
}

fn read() -> Option<u8> {
    (0..100_000).find_map(|_| unsafe { (inb(0x64) & 1 != 0).then(|| inb(0x60)) })
}

fn mouse_command(cmd: u8) {
    wait_write();
    unsafe { outb(0x64, 0xD4) };
    wait_write();
    unsafe { outb(0x60, cmd) };
    read(); // ACK
}

// VMware-compatible absolute pointer ("vmmouse"), provided by QEMU on PCs.
// It reports where the host cursor is, so the pointer needs no grabbing and
// never drifts from the host's cursor, also when QEMU zooms the picture.
const VMPORT_MAGIC: u32 = 0x564D_5868;
const VMPORT_PORT: u32 = 0x5658;
const VMMOUSE_DATA: u32 = 39;
const VMMOUSE_STATUS: u32 = 40;
const VMMOUSE_COMMAND: u32 = 41;

/// One call through the hypervisor's "backdoor" port; returns eax..edx.
fn vmport(cmd: u32, arg: u32) -> [u32; 4] {
    let (mut a, mut b, mut c, mut d) = (VMPORT_MAGIC, arg as u64, cmd, VMPORT_PORT);
    // SAFETY: on real hardware this is an ordinary read of an unused port;
    // rbx is swapped in and out because the compiler reserves it
    unsafe {
        asm!("xchg rbx, {b}", "in eax, dx", "xchg rbx, {b}",
             b = inout(reg) b, inout("eax") a, inout("ecx") c, inout("edx") d,
             options(nomem, nostack, preserves_flags));
    }
    [a, b as u32, c, d]
}

fn vmmouse_init() {
    if vmport(10, 0)[1] != VMPORT_MAGIC {
        return; // GETVERSION: no such hypervisor
    }
    vmport(VMMOUSE_COMMAND, 0x4541_4552); // READ_ID
    let status = vmport(VMMOUSE_STATUS, 0)[0];
    if status & 0xFFFF_0000 == 0xFFFF_0000 || status & 0xFFFF == 0 {
        return;
    }
    if vmport(VMMOUSE_DATA, 1)[0] != 0x3442_554A {
        return;
    }
    vmport(VMMOUSE_COMMAND, 0x5342_4152); // REQUEST_ABSOLUTE
    VMMOUSE.store(true, Ordering::Relaxed);
}

/// The host pointer is followed exactly.
pub fn pointer_absolute() -> bool {
    VMMOUSE.load(Ordering::Relaxed)
}

fn vmmouse_poll() {
    for _ in 0..64 {
        let status = vmport(VMMOUSE_STATUS, 0)[0];
        if status & 0xFFFF_0000 == 0xFFFF_0000 {
            // error: start over
            VMMOUSE.store(false, Ordering::Relaxed);
            vmmouse_init();
            return;
        }
        if status & 0xFFFF < 4 {
            return;
        }
        let r = vmport(VMMOUSE_DATA, 4);
        let bit = |mask: u32, button: i32| if r[0] & mask != 0 { button } else { 0 };
        let buttons = bit(0x20, 1) | bit(0x10, 2) | bit(0x08, 4);
        if wm::active() {
            // the position is 0..65535 on both axes
            wm::mouse_abs((r[1] & 0xFFFF) as i32, (r[2] & 0xFFFF) as i32, buttons, r[3] as i8 as i32);
        }
    }
}

/// Starts the pointer devices (graphical mode).
pub fn init_pointer() {
    wait_write();
    unsafe { outb(0x64, 0xA8) }; // enable the auxiliary port
    wait_write();
    unsafe { outb(0x64, 0x20) };
    let Some(config) = read() else { return };
    wait_write();
    unsafe { outb(0x64, 0x60) };
    wait_write();
    unsafe { outb(0x60, config & !0x20) }; // enable the mouse clock
    mouse_command(0xF6); // defaults
    mouse_command(0xF4); // start reporting
    vmmouse_init();
}

/// Drains the keyboard and the pointer.
pub fn poll() {
    for _ in 0..64 {
        let status = unsafe { inb(0x64) };
        if status & 1 == 0 {
            break;
        }
        let b = unsafe { inb(0x60) };
        if status & 0x20 == 0 {
            if let Some((code, extended, down)) = PS2.with(|p| p.scancode(b)) {
                console::key(code, extended, down);
            }
        } else if !pointer_absolute() {
            // (with vmmouse these are only wake-up packets)
            if let Some((dx, dy, buttons)) = PS2.with(|p| p.mouse_byte(b)) {
                if wm::active() {
                    wm::mouse(dx, dy, buttons);
                }
            }
        }
    }
    if pointer_absolute() {
        vmmouse_poll();
    }
}
