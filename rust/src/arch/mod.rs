//! What differs between machines: start-up, interrupts and clocks, the
//! serial line, keyboard and pointer, the display, the thread switch and
//! where devices are found. Each machine provides the same modules:
//!
//! - `boot`: what the boot loader passed (memory, command line)
//! - `cpu`: interrupts, clocks, power
//! - `thread`: the thread switch
//! - `serial`: the serial console
//! - `input`: keyboard and pointer
//! - `display`: the framebuffer and, if there is one, the text screen
//! - `virtio`: where virtio devices are
//! - `sound`: the sound card

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;
