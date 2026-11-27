//! virtio devices with memory-mapped registers (virtio-mmio), in both
//! versions of the interface: 1 ("legacy") and 2. Devices on PCI are in
//! pci.rs.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::fdt;
use super::mmio::Registers;
use crate::virtio::{Kind, Transport};

const MAGIC: usize = 0x000;
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014;
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const GUEST_PAGE_SIZE: usize = 0x028; // version 1
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_ALIGN: usize = 0x03C; // version 1
const QUEUE_PFN: usize = 0x040; // version 1
const QUEUE_READY: usize = 0x044; // version 2
const QUEUE_NOTIFY: usize = 0x050;
const STATUS: usize = 0x070;
const QUEUE_DESC: usize = 0x080; // version 2: three 64-bit addresses
const QUEUE_AVAIL: usize = 0x090;
const QUEUE_USED: usize = 0x0A0;
const CONFIG: usize = 0x100;

const STATUS_FEATURES_OK: u8 = 8;

struct Device {
    regs: Registers,
    /// version 2 of the interface
    modern: bool,
}

impl Transport for Device {
    fn device_features(&mut self) -> u32 {
        self.regs.write32(DEVICE_FEATURES_SEL, 0);
        self.regs.read32(DEVICE_FEATURES)
    }

    fn set_driver_features(&mut self, features: u32) {
        self.regs.write32(DRIVER_FEATURES_SEL, 0);
        self.regs.write32(DRIVER_FEATURES, features);
        if self.modern {
            // feature bit 32: we speak version 1 of the virtio standard
            self.regs.write32(DRIVER_FEATURES_SEL, 1);
            self.regs.write32(DRIVER_FEATURES, 1);
            let status = self.regs.read32(STATUS);
            self.regs.write32(STATUS, status | STATUS_FEATURES_OK as u32);
        } else {
            self.regs.write32(GUEST_PAGE_SIZE, 4096);
        }
    }

    fn set_status(&mut self, status: u8) {
        // keep "features ok" once it is set, until a reset
        let keep = if status != 0 { self.regs.read32(STATUS) & STATUS_FEATURES_OK as u32 } else { 0 };
        self.regs.write32(STATUS, status as u32 | keep);
    }

    fn config(&mut self, offset: usize) -> u8 {
        self.regs.read8(CONFIG + offset)
    }

    fn modern(&self) -> bool {
        self.modern
    }

    fn queue_max(&mut self, index: u16) -> u16 {
        self.regs.write32(QUEUE_SEL, index as u32);
        self.regs.read32(QUEUE_NUM_MAX).min(32768) as u16
    }

    fn queue_size_fixed(&self) -> bool {
        false
    }

    fn queue_set(&mut self, index: u16, size: u16, addr: usize) {
        let n = size as usize;
        self.regs.write32(QUEUE_SEL, index as u32);
        self.regs.write32(QUEUE_NUM, size as u32);
        if self.modern {
            // the same layout as the legacy interface prescribes
            self.regs.write64(QUEUE_DESC, addr as u64);
            self.regs.write64(QUEUE_AVAIL, (addr + 16 * n) as u64);
            self.regs.write64(QUEUE_USED, (addr + ((16 * n + 6 + 2 * n + 4095) & !4095)) as u64);
            self.regs.write32(QUEUE_READY, 1);
        } else {
            self.regs.write32(QUEUE_ALIGN, 4096);
            self.regs.write32(QUEUE_PFN, (addr >> 12) as u32);
        }
    }

    fn notify(&mut self, index: u16) {
        self.regs.write32(QUEUE_NOTIFY, index as u32);
    }
}

pub fn find(kind: Kind) -> Vec<Box<dyn Transport>> {
    let mut found: Vec<Box<dyn Transport>> = Vec::new();
    let Some(tree) = fdt::tree() else { return found };
    tree.all_compatible("virtio,mmio", |base| {
        // SAFETY: a virtio-mmio device's registers, as the device tree says
        let regs = unsafe { Registers::new(base) };
        let version = regs.read32(VERSION);
        if regs.read32(MAGIC) == 0x7472_6976 && (version == 1 || version == 2) && regs.read32(DEVICE_ID) == kind as u32 {
            found.push(Box::new(Device { regs, modern: version == 2 }));
        }
    });
    found.extend(super::pci::find_virtio(kind));
    found
}
