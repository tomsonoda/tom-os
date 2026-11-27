//! The tom-os kernel. Everything is Rust except the boot code and the
//! thread switch (arch/x86_64/boot.asm).
#![no_std]

extern crate alloc;

pub mod rt;

mod arch;
pub(crate) use arch::{cpu, sound};

mod agent;
mod apps;
mod console;
mod fs;
mod gfx;
mod home;
mod http;
mod json;
mod lua;
mod mem;
mod net;
mod run;
mod sched;
mod shell;
mod start;
mod tcp;
mod virtio;
mod virtio_net;
mod wm;
