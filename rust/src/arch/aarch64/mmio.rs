//! Memory-mapped device registers.

/// A block of device registers.
#[derive(Clone, Copy)]
pub struct Registers(usize);

impl Registers {
    /// # Safety
    /// `base` must be the address of a device's registers, mapped as device
    /// memory, and all offsets used with the result must be inside them.
    pub const unsafe fn new(base: usize) -> Registers {
        Registers(base)
    }
    pub fn read32(self, offset: usize) -> u32 {
        // SAFETY: see `new`
        unsafe { ((self.0 + offset) as *const u32).read_volatile() }
    }
    pub fn write32(self, offset: usize, v: u32) {
        unsafe { ((self.0 + offset) as *mut u32).write_volatile(v) }
    }
    pub fn read8(self, offset: usize) -> u8 {
        unsafe { ((self.0 + offset) as *const u8).read_volatile() }
    }
    pub fn write16(self, offset: usize, v: u16) {
        unsafe { ((self.0 + offset) as *mut u16).write_volatile(v) }
    }
    pub fn write64(self, offset: usize, v: u64) {
        unsafe { ((self.0 + offset) as *mut u64).write_volatile(v) }
    }
}

// Where the "virt" machine has its devices. These addresses are the same
// in every version of QEMU's machine (they are part of its interface).
// SAFETY: fixed device addresses of the machine, in the device-memory part
// of the address space set up by the boot code.
pub const UART: Registers = unsafe { Registers::new(0x0900_0000) };
pub const RTC: Registers = unsafe { Registers::new(0x0901_0000) };
pub const FW_CFG: Registers = unsafe { Registers::new(0x0902_0000) };
pub const GIC_DISTRIBUTOR: Registers = unsafe { Registers::new(0x0800_0000) };
pub const GIC_CPU: Registers = unsafe { Registers::new(0x0801_0000) };
pub const VIRTIO_BASE: usize = 0x0A00_0000;
pub const VIRTIO_SLOTS: usize = 32;
pub const VIRTIO_STEP: usize = 0x200;
/// Where memory starts.
pub const RAM_BASE: usize = 0x4000_0000;
