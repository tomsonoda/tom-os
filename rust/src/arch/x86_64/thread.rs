//! The thread switch (the code itself is in arch/x86_64/boot.asm).

unsafe extern "C" {
    /// Saves the callee-saved registers and the stack pointer (into
    /// `*save_sp`) and continues on the stack `new_sp`.
    pub fn ctx_switch(save_sp: *mut u64, new_sp: u64);
}

/// What `ctx_switch` expects on the stack of a thread that has not run
/// yet (lowest address first): six saved registers, then the entry point
/// as the return address, then a zero as the entry's own caller.
#[allow(non_snake_case)]
fn FRAME(entry: extern "C" fn() -> !) -> [u64; 8] {
    [0, 0, 0, 0, 0, 0, entry as *const () as u64, 0]
}

/// Prepares the stack of a thread that has not run yet, so that switching
/// to it enters `entry`; returns the stack pointer to switch to.
pub fn first_frame(stack: &mut [u8], entry: extern "C" fn() -> !) -> u64 {
    let frame = FRAME(entry);
    let top = (stack.as_ptr() as usize + stack.len()) & !15;
    let at = top - size_of_val(&frame) - stack.as_ptr() as usize;
    for (i, word) in frame.iter().enumerate() {
        stack[at + 8 * i..at + 8 * i + 8].copy_from_slice(&word.to_le_bytes());
    }
    (top - size_of_val(&frame)) as u64
}
