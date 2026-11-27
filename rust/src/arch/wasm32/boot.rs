//! Start-up: the page calls `kmain`.

use core::arch::wasm32::{memory_grow, memory_size};

use super::js;

/// Memory for the heap and the kernel command line.
pub struct Info {
    pub heap_base: usize,
    pub heap_len: usize,
    pub command_line: &'static [u8],
}

const PAGE: usize = 65536;
const HEAP_PAGES: usize = 2048; // 128 MiB

/// The heap is memory added to the module's own; the command line comes
/// from the page.
///
/// # Safety
/// Must be called once.
pub unsafe fn info(_boot_arg: usize) -> Info {
    let mut pages = HEAP_PAGES;
    let mut base = memory_grow(0, pages);
    while base == usize::MAX && pages > 256 {
        pages /= 2; // the browser does not give that much: take less
        base = memory_grow(0, pages);
    }
    if base == usize::MAX {
        (base, pages) = (memory_size(0), 0);
    }
    let heap_base = base * PAGE;
    // the command line goes at the start of the new memory
    let room = (pages * PAGE).min(1024);
    // SAFETY: memory just obtained, used for nothing else
    let len = if room > 0 { unsafe { js::js_cmdline(heap_base as *mut u8, room) }.min(room) } else { 0 };
    let command_line = unsafe { core::slice::from_raw_parts(heap_base as *const u8, len) };
    Info { heap_base: heap_base + room, heap_len: pages * PAGE - room, command_line }
}

/// Called by the page to start the kernel; it "returns" only by unwinding
/// (see thread.rs).
#[unsafe(no_mangle)]
extern "C" fn kmain() {
    crate::start::main(0)
}
