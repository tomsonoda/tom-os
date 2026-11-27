//! The display: the VGA text screen, and the linear framebuffer of QEMU's
//! standard display adapter (the Bochs "BGA" interface).

use super::pci::{self, inw, outb, outw};
use crate::rt::KCell;

/// The text screen the machine starts with: columns and rows.
pub const TEXT_SCREEN: Option<(i32, i32)> = Some((80, 25));

/// Copies the rows marked in `dirty` to the text screen and moves its
/// cursor to (column, row).
pub fn text_flush(cells: &[u16], cols: usize, dirty: u64, cursor: (i32, i32)) {
    let screen = 0xB8000 as *mut u16;
    for row in (0..25).filter(|row| dirty & (1 << row) != 0) {
        for (col, &cell) in cells.iter().skip(row * cols).take(cols.min(80)).enumerate() {
            // SAFETY: inside the 80x25 text screen, which the kernel maps
            unsafe { screen.add(row * 80 + col).write_volatile(cell) };
        }
    }
    let pos = (cursor.1 * 80 + cursor.0) as u16;
    // SAFETY: the VGA adapter's cursor registers
    unsafe {
        outb(0x3D4, 0x0F);
        outb(0x3D5, pos as u8);
        outb(0x3D4, 0x0E);
        outb(0x3D5, (pos >> 8) as u8);
    }
}

const BGA_INDEX: u16 = 0x1CE;
const BGA_DATA: u16 = 0x1CF;

// SAFETY (both): the Bochs display interface's own ports
fn bga_write(index: u16, value: u16) {
    unsafe {
        outw(BGA_INDEX, index);
        outw(BGA_DATA, value);
    }
}

fn bga_read(index: u16) -> u16 {
    unsafe {
        outw(BGA_INDEX, index);
        inw(BGA_DATA)
    }
}

/// The framebuffer's address: None = not looked for yet, Some(0) = no
/// usable display adapter.
static FRAMEBUFFER: KCell<Option<u64>> = KCell::new(None);

fn framebuffer() -> Option<u64> {
    let addr = FRAMEBUFFER.with(|a| {
        *a.get_or_insert_with(|| {
            if bga_read(0) & 0xFFF0 != 0xB0C0 {
                return 0;
            }
            // The framebuffer is BAR0 of the display adapter.
            for bus in 0..=255u8 {
                for dev in 0..32u8 {
                    if pci::pci_read(bus, dev, 0, 0) & 0xFFFF == 0xFFFF || pci::pci_read(bus, dev, 0, 0x08) >> 24 != 0x03 {
                        continue;
                    }
                    let bar0 = pci::pci_read(bus, dev, 0, 0x10);
                    if bar0 & 1 == 0 && bar0 & !0xF != 0 {
                        return (bar0 & !0xF) as u64;
                    }
                }
            }
            0
        })
    });
    (addr != 0).then_some(addr)
}

/// Video memory in bytes; 0 without a usable adapter.
pub fn vram() -> usize {
    if framebuffer().is_none() {
        return 0;
    }
    match bga_read(0x0A) as usize * 65536 {
        0 => 16 << 20,
        v => v,
    }
}

/// Switches to a mode of `w` x `h` pixels of 32 bits (the caller has
/// checked that it fits in the video memory) and returns where the pixels
/// are, or None if the adapter does not do it.
pub fn set_mode(w: i32, h: i32) -> Option<*mut u32> {
    let addr = framebuffer()?;
    if addr + w as u64 * h as u64 * 4 > 0x1_0000_0000 {
        return None; // only 4 GiB are mapped
    }
    bga_write(4, 0); // disable while changing the mode
    bga_write(1, w as u16);
    bga_write(2, h as u16);
    bga_write(3, 32);
    bga_write(4, 0x41); // enabled + linear framebuffer
    (bga_read(1) as i32 == w && bga_read(2) as i32 == h).then_some(addr as *mut u32)
}
