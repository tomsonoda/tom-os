//! The serial console: COM1.

use super::pci::{inb, outb};

const COM1: u16 = 0x3F8;

// SAFETY (all port I/O in this file): the ports of the PC's first serial
// port, which belongs to the console.

pub fn init() {
    unsafe {
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x80);
        outb(COM1, 0x01); // 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8N1
        outb(COM1 + 2, 0xC7);
        outb(COM1 + 4, 0x0B); // DTR, RTS, OUT2
        outb(COM1 + 1, 0x01); // RX interrupt: wakes HLT
    }
}

pub fn put(c: u8) {
    unsafe {
        let mut spins = 0;
        while spins < 100_000 && inb(COM1 + 5) & 0x20 == 0 {
            spins += 1;
        }
        outb(COM1, c);
    }
}

/// A received byte, if there is one.
pub fn get() -> Option<u8> {
    unsafe { (inb(COM1 + 5) & 1 != 0).then(|| inb(COM1)) }
}
