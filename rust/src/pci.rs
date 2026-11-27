//! I/O ports and PCI configuration space.

use core::arch::asm;

/// # Safety
/// Port I/O talks to hardware directly: the caller must own the device
/// behind `port`. The same holds for the other functions here.
pub unsafe fn outb(port: u16, v: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags)) }
}
pub unsafe fn outw(port: u16, v: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack, preserves_flags)) }
}
pub unsafe fn outl(port: u16, v: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack, preserves_flags)) }
}
pub unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack, preserves_flags)) };
    v
}
pub unsafe fn inw(port: u16) -> u16 {
    let v: u16;
    unsafe { asm!("in ax, dx", in("dx") port, out("ax") v, options(nomem, nostack, preserves_flags)) };
    v
}
pub unsafe fn inl(port: u16) -> u32 {
    let v: u32;
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") v, options(nomem, nostack, preserves_flags)) };
    v
}

fn address(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    0x8000_0000 | (bus as u32) << 16 | (dev as u32) << 11 | (func as u32) << 8 | (off & 0xFC) as u32
}

/// Reads a 32-bit register of a PCI device's configuration space.
pub fn pci_read(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    // SAFETY: 0xCF8/0xCFC are the PCI configuration ports; reading is harmless
    unsafe {
        outl(0xCF8, address(bus, dev, func, off));
        inl(0xCFC)
    }
}

/// Writes a configuration register: drivers use it to switch on their
/// own device.
pub fn pci_write(bus: u8, dev: u8, func: u8, off: u8, v: u32) {
    unsafe {
        outl(0xCF8, address(bus, dev, func, off));
        outl(0xCFC, v);
    }
}

/// The first device with this vendor and device id, as (bus, device).
pub fn find(vendor: u16, device: u16) -> Option<(u8, u8)> {
    for bus in 0..=255u8 {
        for dev in 0..32u8 {
            let id = pci_read(bus, dev, 0, 0);
            if id & 0xFFFF == vendor as u32 && id >> 16 == device as u32 {
                return Some((bus, dev));
            }
        }
    }
    None
}
