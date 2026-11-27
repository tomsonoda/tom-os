//! The thread switch (the code itself is in arch/x86_64/boot.asm).

unsafe extern "C" {
    /// Saves the callee-saved registers and the stack pointer (into
    /// `*save_sp`) and continues on the stack `new_sp`.
    pub fn ctx_switch(save_sp: *mut u64, new_sp: u64);
}

/// What `ctx_switch` expects on the stack of a thread that has not run
/// yet (lowest address first): six saved registers, then the entry point
/// as the return address, then a zero as the entry's own caller.
pub fn first_frame(entry: extern "C" fn() -> !) -> [u64; 8] {
    [0, 0, 0, 0, 0, 0, entry as *const () as u64, 0]
}
