//! PCI on machines described by a device tree ("pci-host-ecam-generic"):
//! configuration space is memory-mapped, and nobody has given the devices
//! addresses before us, so we do.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::fdt;
use super::mmio::Registers;
use crate::rt::KCell;
use crate::virtio::{Kind, Transport};

struct Host {
    /// configuration space: 4 KiB per function
    config: Registers,
    /// the next free address for device registers, and the end
    window: (usize, usize),
}

/// None = not looked for yet.
static HOST: KCell<Option<Option<Host>>> = KCell::new(None);

fn host() -> Option<Host> {
    let tree = fdt::tree()?;
    let node = tree.compatible("pci-host-ecam-generic")?;
    let (config, _) = node.reg(0)?;
    // "ranges": (kind, PCI address (2 cells), CPU address (2), size (2));
    // take the window for 32-bit memory addresses
    let ranges = node.property("ranges")?;
    let window = ranges.chunks_exact(28).find_map(|r| {
        let word = |i: usize| u32::from_be_bytes([r[4 * i], r[4 * i + 1], r[4 * i + 2], r[4 * i + 3]]) as usize;
        (word(0) >> 24 & 3 == 2).then(|| (word(3) << 32 | word(4), (word(3) << 32 | word(4)) + (word(5) << 32 | word(6))))
    })?;
    // SAFETY: the host bridge's configuration space, as the device tree says
    Some(Host { config: unsafe { Registers::new(config) }, window })
}

/// The configuration space of device `dev` on bus 0.
fn function(config: Registers, dev: usize) -> Registers {
    config.at(dev << 15)
}

/// Gives the device's memory registers ("BARs") addresses and switches the
/// device on; returns the address of each BAR.
fn assign(h: &mut Host, cfg: Registers) -> [usize; 6] {
    let mut bars = [0usize; 6];
    let mut i = 0;
    while i < 6 {
        let reg = 0x10 + 4 * i;
        let old = cfg.read32(reg);
        cfg.write32(reg, !0);
        let probe = cfg.read32(reg);
        let wide = probe & 0x7 == 0x4; // a 64-bit memory BAR takes two slots
        if probe != 0 && probe & 1 == 0 {
            let size = (!(probe & !0xF)).wrapping_add(1) as usize;
            let at = (h.window.0 + size - 1) & !(size - 1);
            if size != 0 && at + size <= h.window.1 {
                h.window.0 = at + size;
                cfg.write32(reg, at as u32);
                if wide {
                    cfg.write32(reg + 4, 0);
                }
                bars[i] = at;
            }
        } else {
            cfg.write32(reg, old);
        }
        i += if wide { 2 } else { 1 };
    }
    cfg.write16(0x04, cfg.read16(0x04) | 0x6); // memory space + bus master
    bars
}

// The registers of a virtio device on PCI (virtio 1.0, "modern"): several
// structures, found through the device's capability list.
const COMMON_DEVICE_FEATURE_SELECT: usize = 0x00;
const COMMON_DEVICE_FEATURE: usize = 0x04;
const COMMON_DRIVER_FEATURE_SELECT: usize = 0x08;
const COMMON_DRIVER_FEATURE: usize = 0x0C;
const COMMON_STATUS: usize = 0x14;
const COMMON_QUEUE_SELECT: usize = 0x16;
const COMMON_QUEUE_SIZE: usize = 0x18;
const COMMON_QUEUE_ENABLE: usize = 0x1C;
const COMMON_QUEUE_NOTIFY_OFF: usize = 0x1E;
const COMMON_QUEUE_DESC: usize = 0x20;
const COMMON_QUEUE_AVAIL: usize = 0x28;
const COMMON_QUEUE_USED: usize = 0x30;

struct Device {
    common: Registers,
    notify: Registers,
    notify_multiplier: usize,
    config: Registers,
    /// per queue: where to write to notify the device
    notify_at: Vec<usize>,
}

impl Transport for Device {
    fn device_features(&mut self) -> u32 {
        self.common.write32(COMMON_DEVICE_FEATURE_SELECT, 0);
        self.common.read32(COMMON_DEVICE_FEATURE)
    }

    fn set_driver_features(&mut self, features: u32) {
        self.common.write32(COMMON_DRIVER_FEATURE_SELECT, 0);
        self.common.write32(COMMON_DRIVER_FEATURE, features);
        // feature bit 32: we speak version 1 of the virtio standard
        self.common.write32(COMMON_DRIVER_FEATURE_SELECT, 1);
        self.common.write32(COMMON_DRIVER_FEATURE, 1);
        let status = self.common.read8(COMMON_STATUS);
        self.common.write8(COMMON_STATUS, status | 8); // features ok
    }

    fn set_status(&mut self, status: u8) {
        // keep "features ok" once it is set, until a reset
        let keep = if status != 0 { self.common.read8(COMMON_STATUS) & 8 } else { 0 };
        self.common.write8(COMMON_STATUS, status | keep);
    }

    fn config(&mut self, offset: usize) -> u8 {
        self.config.read8(offset)
    }

    fn modern(&self) -> bool {
        true
    }

    fn queue_max(&mut self, index: u16) -> u16 {
        self.common.write16(COMMON_QUEUE_SELECT, index);
        self.common.read16(COMMON_QUEUE_SIZE)
    }

    fn queue_size_fixed(&self) -> bool {
        false
    }

    fn queue_set(&mut self, index: u16, size: u16, addr: usize) {
        let n = size as usize;
        self.common.write16(COMMON_QUEUE_SELECT, index);
        self.common.write16(COMMON_QUEUE_SIZE, size);
        // the same layout as the legacy interface prescribes
        self.common.write64(COMMON_QUEUE_DESC, addr as u64);
        self.common.write64(COMMON_QUEUE_AVAIL, (addr + 16 * n) as u64);
        self.common.write64(COMMON_QUEUE_USED, (addr + ((16 * n + 6 + 2 * n + 4095) & !4095)) as u64);
        let at = self.common.read16(COMMON_QUEUE_NOTIFY_OFF) as usize * self.notify_multiplier;
        if self.notify_at.len() <= index as usize {
            self.notify_at.resize(index as usize + 1, 0);
        }
        self.notify_at[index as usize] = at;
        self.common.write16(COMMON_QUEUE_ENABLE, 1);
    }

    fn notify(&mut self, index: u16) {
        if let Some(&at) = self.notify_at.get(index as usize) {
            self.notify.write16(at, index);
        }
    }
}

/// The virtio devices of one kind on the PCI bus.
pub fn find_virtio(kind: Kind) -> Vec<Box<dyn Transport>> {
    let mut found: Vec<Box<dyn Transport>> = Vec::new();
    HOST.with(|slot| {
        let Some(h) = slot.get_or_insert_with(host) else { return };
        for dev in 0..32 {
            let cfg = function(h.config, dev);
            // vendor 0x1AF4, device 0x1040 + the virtio device id; a network
            // card that also has the legacy interface calls itself 0x1000
            let id = cfg.read32(0);
            let transitional = kind == Kind::Net && id == 0x1AF4 | 0x1000 << 16;
            if id != 0x1AF4 | (0x1040 + kind as u32) << 16 && !transitional {
                continue;
            }
            let bars = assign(h, cfg);
            // SAFETY (the register blocks below): inside the BARs just assigned
            let block = |cap: usize| {
                let (bar, offset) = (cfg.read8(cap + 4) as usize, cfg.read32(cap + 8) as usize);
                bars.get(bar).filter(|&&b| b != 0).map(|&b| unsafe { Registers::new(b + offset) })
            };
            let (mut common, mut notify, mut config, mut multiplier) = (None, None, None, 0);
            let mut cap = (cfg.read8(0x34) & !3) as usize;
            while cap != 0 {
                if cfg.read8(cap) == 0x09 {
                    // a vendor capability: 1 common, 2 notify, 4 device configuration
                    match cfg.read8(cap + 3) {
                        1 => common = common.or(block(cap)),
                        2 => {
                            notify = notify.or(block(cap));
                            multiplier = cfg.read32(cap + 16) as usize;
                        }
                        4 => config = config.or(block(cap)),
                        _ => {}
                    }
                }
                cap = (cfg.read8(cap + 1) & !3) as usize;
            }
            if let (Some(common), Some(notify)) = (common, notify) {
                let config = config.unwrap_or(common);
                found.push(Box::new(Device { common, notify, notify_multiplier: multiplier, config, notify_at: Vec::new() }));
            }
        }
    });
    found
}
