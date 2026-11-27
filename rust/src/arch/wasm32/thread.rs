//! Threads in WebAssembly. Each thread has a control block that the page
//! reads and writes (web/tomos.js):
//!
//!   word 0   has the thread started?
//!   word 1   its stack pointer (for data on the stack in memory)
//!   word 2   is it the boot thread? (which function to enter it by)
//!   word 3,4 asyncify's record: where the saved call stack ends and may end
//!   ...      the saved call stack
//!
//! To switch, the page lets asyncify unwind the running thread's call
//! stack into its block and rewinds the other one's (or calls its entry
//! function if it has not started).

use core::sync::atomic::{AtomicUsize, Ordering};

use super::js;
use crate::rt::KCell;

/// Bytes of a block, including the room for the saved call stack.
const BLOCK: usize = 384 * 1024;

#[repr(C, align(16))]
struct BootBlock([u8; BLOCK]);

/// The boot thread's block; the others have theirs at the bottom of
/// their stacks.
static mut BOOT_BLOCK: BootBlock = BootBlock([0; BLOCK]);

/// The running thread's block; 0 means the boot thread's.
static CURRENT: AtomicUsize = AtomicUsize::new(0);

/// The running thread's block, for calls to the page that may unwind.
pub fn current() -> u32 {
    match CURRENT.load(Ordering::Relaxed) {
        0 => {
            let block = &raw mut BOOT_BLOCK as *mut u32;
            // SAFETY: the block's header; only the boot thread gets here
            // before the first switch, and the page reads it afterwards
            unsafe {
                block.add(2).write(1);
                block.add(4).write(block as u32 + BLOCK as u32);
            }
            CURRENT.store(block as usize, Ordering::Relaxed);
            block as u32
        }
        block => block as u32,
    }
}

/// Leaves the running thread, storing its block in `*save_sp`, and
/// continues the thread whose block is `new_sp`.
///
/// # Safety
/// `new_sp` must come from `first_frame` or an earlier switch.
pub unsafe fn ctx_switch(save_sp: *mut u64, new_sp: u64) {
    let from = current();
    unsafe { save_sp.write(from as u64) };
    CURRENT.store(new_sp as usize, Ordering::Relaxed);
    js::js_switch(from, new_sp as u32);
}

static ENTRY: KCell<Option<extern "C" fn() -> !>> = KCell::new(None);

/// Prepares the stack of a thread that has not run yet: its block goes at
/// the bottom, the rest is its stack. Returns the block's address.
pub fn first_frame(stack: &mut [u8], entry: extern "C" fn() -> !) -> u64 {
    ENTRY.with(|e| *e = Some(entry));
    let block = (stack.as_ptr() as usize + 15) & !15;
    let top = (stack.as_ptr() as usize + stack.len()) & !15;
    assert!(block + BLOCK + 65536 <= top);
    let header = [0, top as u32, 0, 0, (block + BLOCK) as u32];
    for (i, word) in header.iter().enumerate() {
        let at = block - stack.as_ptr() as usize + 4 * i;
        stack[at..at + 4].copy_from_slice(&word.to_le_bytes());
    }
    block as u64
}

/// Called by the page to enter a thread that is not the boot thread.
#[unsafe(no_mangle)]
extern "C" fn fiber_start() {
    if let Some(entry) = ENTRY.with(|e| *e) {
        entry()
    }
}
