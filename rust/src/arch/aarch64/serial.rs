//! The serial console: a PL011 UART (QEMU) or a 16550 (crosvm), whichever
//! the device tree names.

use super::fdt;
use super::mmio::Registers;
use crate::rt::KCell;

#[derive(Clone, Copy)]
enum Uart {
    None,
    Pl011(Registers),
    Ns16550(Registers),
}

static UART: KCell<Uart> = KCell::new(Uart::None);

pub fn init() {
    let Some(tree) = fdt::tree() else { return };
    let found = |device: &str| tree.compatible(device).and_then(|n| n.reg(0));
    // SAFETY: device registers, at the address the device tree gives
    let uart = unsafe {
        if let Some((base, _)) = found("arm,pl011") {
            Uart::Pl011(Registers::new(base))
        } else if let Some((base, _)) = found("ns16550a") {
            Uart::Ns16550(Registers::new(base))
        } else {
            Uart::None
        }
    };
    UART.with(|u| *u = uart);
}

fn uart() -> Uart {
    UART.try_with(|u| *u).unwrap_or(Uart::None)
}

pub fn put(c: u8) {
    let wait = |busy: &dyn Fn() -> bool| {
        let mut spins = 0;
        while spins < 100_000 && busy() {
            spins += 1;
        }
    };
    match uart() {
        Uart::Pl011(r) => {
            wait(&|| r.read32(0x18) & (1 << 5) != 0); // transmit queue full
            r.write32(0x00, c as u32);
        }
        Uart::Ns16550(r) => {
            wait(&|| r.read8(5) & 0x20 == 0); // transmit register not empty
            r.write8(0, c);
        }
        Uart::None => {}
    }
}

/// A received byte, if there is one.
pub fn get() -> Option<u8> {
    match uart() {
        Uart::Pl011(r) => (r.read32(0x18) & (1 << 4) == 0).then(|| r.read32(0x00) as u8),
        Uart::Ns16550(r) => (r.read8(5) & 1 != 0).then(|| r.read8(0)),
        Uart::None => None,
    }
}
