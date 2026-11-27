//! The serial console: a PL011 UART.

use super::mmio::UART;

const DATA: usize = 0x00;
const FLAGS: usize = 0x18;
const TX_FULL: u32 = 1 << 5;
const RX_EMPTY: u32 = 1 << 4;

pub fn init() {
    // the virtual machine's UART works as it is
}

pub fn put(c: u8) {
    let mut spins = 0;
    while spins < 100_000 && UART.read32(FLAGS) & TX_FULL != 0 {
        spins += 1;
    }
    UART.write32(DATA, c as u32);
}

/// A received byte, if there is one.
pub fn get() -> Option<u8> {
    (UART.read32(FLAGS) & RX_EMPTY == 0).then(|| UART.read32(DATA) as u8)
}
