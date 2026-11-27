//! Memory-mapped device registers, and the address space.

use core::arch::asm;

/// A block of device registers.
#[derive(Clone, Copy)]
pub struct Registers(usize);

impl Registers {
    /// # Safety
    /// `base` must be the address of a device's registers (not of memory
    /// the kernel uses), and all offsets used with the result must be
    /// inside them.
    pub const unsafe fn new(base: usize) -> Registers {
        Registers(base)
    }
    pub fn at(self, offset: usize) -> Registers {
        Registers(self.0 + offset)
    }
    // SAFETY (all accesses below): see `new`
    pub fn read8(self, offset: usize) -> u8 {
        unsafe { ((self.0 + offset) as *const u8).read_volatile() }
    }
    pub fn read16(self, offset: usize) -> u16 {
        unsafe { ((self.0 + offset) as *const u16).read_volatile() }
    }
    pub fn read32(self, offset: usize) -> u32 {
        unsafe { ((self.0 + offset) as *const u32).read_volatile() }
    }
    pub fn write8(self, offset: usize, v: u8) {
        unsafe { ((self.0 + offset) as *mut u8).write_volatile(v) }
    }
    pub fn write16(self, offset: usize, v: u16) {
        unsafe { ((self.0 + offset) as *mut u16).write_volatile(v) }
    }
    pub fn write32(self, offset: usize, v: u32) {
        unsafe { ((self.0 + offset) as *mut u32).write_volatile(v) }
    }
    pub fn write64(self, offset: usize, v: u64) {
        unsafe { ((self.0 + offset) as *mut u64).write_volatile(v) }
    }
}

unsafe extern "C" {
    /// The translation table set up by the boot code: 512 entries of 1 GiB.
    static mut __page_table: [u64; 512];
}

/// The boot code maps everything as device registers except the kernel
/// itself; this turns the 1 GiB pieces that hold memory into ordinary,
/// cached memory.
///
/// # Safety
/// `base..base + size` must be memory, according to the boot loader.
pub unsafe fn map_memory(base: usize, size: usize) {
    const GIB: usize = 1 << 30;
    for i in (base / GIB..(base + size).div_ceil(GIB)).take_while(|&i| i < 512) {
        // a block of normal memory: see the boot code for the bits
        unsafe { (&raw mut __page_table).cast::<u64>().add(i).write_volatile((i * GIB) as u64 | 0x701) };
    }
    // SAFETY: forgets cached translations; the new ones are a superset
    unsafe { asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb", options(nostack, preserves_flags)) };
}
