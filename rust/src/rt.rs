//! Runtime support shared by the Rust parts of the kernel: the allocator,
//! the panic handler, console output and small helpers around the C side.

use alloc::string::String;
use core::cell::RefCell;


/// Text formatted without the heap (for fatal errors); what does not fit
/// is dropped.
pub struct StackText {
    bytes: [u8; 256],
    len: usize,
}

impl StackText {
    pub fn new() -> StackText {
        StackText { bytes: [0; 256], len: 0 }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl core::fmt::Write for StackText {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let mut text = StackText::new();
    let _ = core::fmt::write(&mut text, format_args!("{info}"));
    halt("Rust panic", text.as_bytes())
}

pub const VERSION: &str = "0.1";

/// Reports a fatal error and stops the machine. Does not use the heap.
pub fn halt(what: &str, details: &[u8]) -> ! {
    crate::console::capture_off();
    console(b"\n*** ");
    console(what.as_bytes());
    console(b": ");
    console(details);
    console(b"\n*** system halted\n");
    loop {
        // SAFETY: stops the CPU for good; nothing runs after a fatal error
        unsafe { core::arch::asm!("cli", "hlt") };
    }
}

pub fn console(bytes: &[u8]) {
    crate::console::write(bytes)
}

pub fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `print!` for the kernel console (the calling thread's terminal).
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::rt::console(alloc::format!($($arg)*).as_bytes()) };
}

/// Markers in the shell's output for the home screen's chat view (see
/// UI_* in kernel/tomos.h).
pub mod ui {
    pub const REPLY: &str = "\x1b[?7101h";
    pub const NOTE: &str = "\x1b[?7102h";
    pub const ERROR: &str = "\x1b[?7103h";
    pub const STATUS: &str = "\x1b[?7104h";
    pub const HIDE: &str = "\x1b[?7105h";
    pub const END: &str = "\x1b[?7100l";
}

/// A global for kernel state. The kernel runs on one CPU and its threads
/// switch only at explicit yields, so a `RefCell` is enough; do not hold a
/// borrow across anything that can yield (network waits, running Lua).
pub struct KCell<T>(RefCell<T>);

// SAFETY: one CPU, cooperative threads: no two borrows are ever live at once
// unless one is held across a yield, which RefCell turns into a panic.
unsafe impl<T> Sync for KCell<T> {}

impl<T> KCell<T> {
    pub const fn new(v: T) -> KCell<T> {
        KCell(RefCell::new(v))
    }
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
    /// Like `with`, but gives None, without calling `f`, if the value is in use.
    pub fn try_with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        self.0.try_borrow_mut().ok().map(|mut v| f(&mut v))
    }
}
