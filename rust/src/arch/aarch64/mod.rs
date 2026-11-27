//! 64-bit ARM (aarch64), on the "virt" machine that QEMU and other virtual
//! machine monitors provide: a device tree, a PL011 serial port, the ARM
//! generic timer and interrupt controller, and virtio devices with
//! memory-mapped registers.

pub mod boot;
pub mod cpu;
pub mod display;
pub mod input;
pub mod llm;
mod fdt;
mod mmio;
mod pci;
pub mod serial;
pub mod sound;
pub mod thread;
pub mod virtio;
