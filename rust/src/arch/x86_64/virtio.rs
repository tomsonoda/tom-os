//! virtio devices on PCI: the legacy interface through I/O ports.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::pci::{self, inb, inl, inw, outb, outl, outw};
use crate::virtio::{Kind, Transport};

const VIRTIO_VENDOR: u16 = 0x1AF4;

const REG_DEVICE_FEATURES: u16 = 0x00;
const REG_DRIVER_FEATURES: u16 = 0x04;
const REG_QUEUE_PFN: u16 = 0x08;
const REG_QUEUE_SIZE: u16 = 0x0C;
const REG_QUEUE_SELECT: u16 = 0x0E;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_STATUS: u16 = 0x12;
const REG_CONFIG: u16 = 0x14;

/// The device's block of I/O ports.
struct Ports(u16);

// SAFETY (all port I/O below): the device's own register block, as given
// by its PCI configuration.
impl Transport for Ports {
    fn device_features(&mut self) -> u32 {
        unsafe { inl(self.0 + REG_DEVICE_FEATURES) }
    }
    fn set_driver_features(&mut self, features: u32) {
        unsafe { outl(self.0 + REG_DRIVER_FEATURES, features) }
    }
    fn set_status(&mut self, status: u8) {
        unsafe { outb(self.0 + REG_STATUS, status) }
    }
    fn config(&mut self, offset: usize) -> u8 {
        unsafe { inb(self.0 + REG_CONFIG + offset as u16) }
    }
    fn queue_max(&mut self, index: u16) -> u16 {
        unsafe {
            outw(self.0 + REG_QUEUE_SELECT, index);
            inw(self.0 + REG_QUEUE_SIZE)
        }
    }
    fn queue_size_fixed(&self) -> bool {
        true
    }
    fn queue_set(&mut self, index: u16, _size: u16, addr: usize) {
        unsafe {
            outw(self.0 + REG_QUEUE_SELECT, index);
            outl(self.0 + REG_QUEUE_PFN, (addr >> 12) as u32);
        }
    }
    fn notify(&mut self, index: u16) {
        unsafe { outw(self.0 + REG_QUEUE_NOTIFY, index) }
    }
}

pub fn find(kind: Kind) -> Vec<Box<dyn Transport>> {
    // the ids of "transitional" devices, which have the legacy interface
    let device = match kind {
        Kind::Net => 0x1000,
        Kind::Input | Kind::Gpu => return Vec::new(),
    };
    let Some((bus, dev)) = pci::find(VIRTIO_VENDOR, device) else { return Vec::new() };
    let bar0 = pci::pci_read(bus, dev, 0, 0x10);
    if bar0 & 1 == 0 {
        return Vec::new(); // need the I/O-port BAR
    }
    pci::pci_write(bus, dev, 0, 0x04, pci::pci_read(bus, dev, 0, 0x04) | 0x05); // I/O space + bus master
    alloc::vec![Box::new(Ports((bar0 & !3) as u16)) as Box<dyn Transport>]
}
