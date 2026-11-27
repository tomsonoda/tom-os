//! The PC (x86-64): started by a Multiboot loader, with the classic PC
//! devices and PCI.

pub mod boot;
pub mod cpu;
pub mod display;
pub mod input;
pub mod llm;
mod pci;
pub mod serial;
pub mod sound;
pub mod thread;
pub mod virtio;
