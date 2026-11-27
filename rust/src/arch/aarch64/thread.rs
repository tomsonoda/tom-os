//! The thread switch.

use core::arch::global_asm;

global_asm!(
    r#"
    .section .text
    .global ctx_switch
    // void ctx_switch(u64 *save_sp, u64 new_sp)
    // Cooperative switch: only callee-saved registers need to survive.
ctx_switch:
    sub sp, sp, #160
    stp x19, x20, [sp, #0]
    stp x21, x22, [sp, #16]
    stp x23, x24, [sp, #32]
    stp x25, x26, [sp, #48]
    stp x27, x28, [sp, #64]
    stp x29, x30, [sp, #80]
    stp d8, d9, [sp, #96]
    stp d10, d11, [sp, #112]
    stp d12, d13, [sp, #128]
    stp d14, d15, [sp, #144]
    mov x2, sp
    str x2, [x0]
    mov sp, x1
    ldp x19, x20, [sp, #0]
    ldp x21, x22, [sp, #16]
    ldp x23, x24, [sp, #32]
    ldp x25, x26, [sp, #48]
    ldp x27, x28, [sp, #64]
    ldp x29, x30, [sp, #80]
    ldp d8, d9, [sp, #96]
    ldp d10, d11, [sp, #112]
    ldp d12, d13, [sp, #128]
    ldp d14, d15, [sp, #144]
    add sp, sp, #160
    ret
"#
);

unsafe extern "C" {
    /// Saves the callee-saved registers and the stack pointer (into
    /// `*save_sp`) and continues on the stack `new_sp`.
    pub fn ctx_switch(save_sp: *mut u64, new_sp: u64);
}

/// What `ctx_switch` expects on the stack of a thread that has not run
/// yet (lowest address first): x19..x29, then x30, where the switch
/// returns to, then d8..d15.
pub fn first_frame(entry: extern "C" fn() -> !) -> [u64; 20] {
    let mut frame = [0; 20];
    frame[11] = entry as *const () as u64;
    frame
}
