//! Runtime support shared by the Rust parts of the kernel: the allocator,
//! the panic handler, console output and small helpers around the C side.

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ffi::c_char;

use crate::sys;

/// Rust allocations come from the kernel heap.
struct KernelHeap;

unsafe impl core::alloc::GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        // the kernel's malloc aligns to 16 bytes
        if layout.align() <= 16 {
            return unsafe { sys::malloc(layout.size().max(1)) };
        }
        // larger alignment: over-allocate and remember the real start just below
        let raw = unsafe { sys::malloc(layout.size() + layout.align() + 8) };
        if raw.is_null() {
            return raw;
        }
        let aligned = (raw as usize + 8 + layout.align() - 1) & !(layout.align() - 1);
        unsafe { *((aligned - 8) as *mut usize) = raw as usize };
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        if layout.align() <= 16 {
            unsafe { sys::free(ptr) }
        } else {
            unsafe { sys::free(*((ptr as usize - 8) as *const usize) as *mut u8) }
        }
    }
}

#[global_allocator]
static HEAP: KernelHeap = KernelHeap;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    struct Buf([u8; 256], usize);
    impl core::fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let n = s.len().min(self.0.len() - self.1);
            self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
            self.1 += n;
            Ok(())
        }
    }
    let mut b = Buf([0; 256], 0);
    let _ = core::fmt::write(&mut b, format_args!("{info}"));
    halt("Rust panic", &b.0[..b.1])
}

/// Same as TOMOS_VERSION in kernel/tomos.h.
pub const VERSION: &str = "0.1";

/// Reports a fatal error and stops the machine.
pub fn halt(what: &str, details: &[u8]) -> ! {
    crate::console::console_capture(core::ptr::null_mut(), 0);
    console(alloc::format!("\n*** {what}: ").as_bytes());
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

/// A NUL-terminated copy for C (cut at the first NUL inside).
pub fn cstr(bytes: &[u8]) -> Vec<u8> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let mut v = Vec::with_capacity(end + 1);
    v.extend_from_slice(&bytes[..end]);
    v.push(0);
    v
}

/// Text from C: the bytes of a NUL-terminated string (empty for NULL).
///
/// # Safety
/// `p` must be NULL or point to a NUL-terminated string that outlives `'a`.
pub unsafe fn from_c<'a>(p: *const c_char) -> &'a [u8] {
    if p.is_null() { &[] } else { unsafe { core::ffi::CStr::from_ptr(p) }.to_bytes() }
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
