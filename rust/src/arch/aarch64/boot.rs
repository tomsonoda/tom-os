//! Start-up: the first instructions, and what the boot loader passed (a
//! device tree).

use core::arch::global_asm;

use super::{fdt, mmio};

// The first instructions. The boot loader enters here with the address of
// the device tree in x0, the MMU off and interrupts masked.
global_asm!(
    r#"
    .section .text.boot.a, "ax"
    .global _start
_start:
    // The header of a Linux arm64 kernel image: boot loaders that can
    // start Linux (QEMU's -kernel among them) then load this file at an
    // address of their choice and enter it with the device tree in x0.
    b 0f                            // the first instruction: skip the header
    .long 0
    .quad 0x80000                   // offset from a 2 MiB boundary
    // (here the linker script puts the memory the image needs)
    .section .text.boot.b, "ax"
    .quad 0xa                       // flags: little-endian, 4 KiB pages, anywhere in memory
    .quad 0, 0, 0
    .ascii "ARM\x64"
    .long 0
0:
    mov x19, x0

    // The image may be loaded anywhere. It is linked as if at address 0,
    // so the addresses stored in its data are off by where it really is:
    // add that to each of them (the linker lists where they are).
    adr x9, _start
    adrp x10, __rela_start
    add x10, x10, :lo12:__rela_start
    adrp x11, __rela_end
    add x11, x11, :lo12:__rela_end
8:  cmp x10, x11
    b.hs 9f
    ldp x12, x13, [x10]             // where, kind
    ldr x14, [x10, #16]             // value
    add x10, x10, #24
    cmp w13, #1027                  // "relative": value + load address
    b.ne 8b
    add x14, x14, x9
    str x14, [x9, x12]
    b 8b
9:

    // If we are started in EL2 (a hypervisor's level), drop to EL1.
    mrs x0, CurrentEL
    lsr x0, x0, #2
    cmp x0, #2
    b.ne 1f
    mov x0, #(1 << 31)              // EL1 is 64-bit
    msr hcr_el2, x0
    mov x0, #3                      // EL1 may use the timer
    msr cnthctl_el2, x0
    msr cntvoff_el2, xzr
    mov x0, #0x3c5                  // EL1h, interrupts masked
    msr spsr_el2, x0
    adr x0, 1f
    msr elr_el2, x0
    eret
1:
    mov x0, #(3 << 20)              // floating point and SIMD allowed
    msr cpacr_el1, x0
    isb

    adrp x0, __stack_top
    add x0, x0, :lo12:__stack_top
    mov sp, x0

    // zero .bss (the page table and the stack live there)
    adrp x0, __bss_start
    add x0, x0, :lo12:__bss_start
    adrp x1, __bss_end
    add x1, x1, :lo12:__bss_end
2:  cmp x0, x1
    b.hs 3f
    str xzr, [x0], #8
    b 2b
3:
    // Identity map of 512 GiB in 1 GiB blocks. Everything is mapped as
    // device registers, except the GiB the kernel is in (and the next
    // one), which are memory; the rest of the memory follows once the
    // device tree has been read (mmio::map_memory).
    adrp x0, __page_table
    add x0, x0, :lo12:__page_table
    ldr x1, =0x0060000000000405     // device registers, not executable
    mov x2, #0
    mov x3, #0x40000000
4:  str x1, [x0, x2, lsl #3]
    add x1, x1, x3
    add x2, x2, #1
    cmp x2, #512
    b.lo 4b
    adr x2, _start
    lsr x2, x2, #30
    lsl x1, x2, #30
    orr x1, x1, #0x700              // memory: cached, shared
    orr x1, x1, #0x1
    str x1, [x0, x2, lsl #3]
    add x2, x2, #1
    add x1, x1, x3
    cmp x2, #512
    b.hs 10f
    str x1, [x0, x2, lsl #3]
10:
    msr ttbr0_el1, x0
    mov x1, #0x00ff                 // attribute 0: normal memory, 1: device
    msr mair_el1, x1
    ldr x1, =0x0000000200803519     // 39-bit addresses, 4 KiB pages, cached walks
    msr tcr_el1, x1
    isb
    adr x1, vectors
    msr vbar_el1, x1
    mrs x1, sctlr_el1
    mov x2, #0x1005                 // MMU, data cache, instruction cache
    orr x1, x1, x2
    bic x1, x1, #2                  // unaligned access is allowed
    msr sctlr_el1, x1
    isb

    mov x0, x19
    bl kmain
5:  wfi
    b 5b

    // Exception vectors. Interrupts stay masked, so anything that arrives
    // here is a fault: report it and stop.
    .balign 0x800
vectors:
    .rept 16
    .balign 0x80
    b 6f
    .endr
6:  mrs x0, esr_el1
    mrs x1, elr_el1
    mrs x2, far_el1
    bl exception_handler
7:  wfi
    b 7b
    .ltorg
"#
);

unsafe extern "C" {
    /// The start and the end of the kernel image (linker.ld).
    static _start: u8;
    static __kernel_end: u8;
}

/// Memory for the heap and the kernel command line.
pub struct Info {
    pub heap_base: usize,
    pub heap_len: usize,
    pub command_line: &'static [u8],
}

/// Reads the device tree; the heap is the memory after the kernel.
///
/// # Safety
/// `device_tree` must be what the boot loader gave to the kernel.
pub unsafe fn info(device_tree: usize) -> Info {
    unsafe { fdt::set(device_tree) };
    let tree = fdt::tree();
    let kernel_start = &raw const _start as usize;
    let kernel_end = (&raw const __kernel_end as usize + 0xFFFF) & !0xFFFF;
    let memory = tree.as_ref().and_then(|t| t.named("memory")).and_then(|n| n.reg(0));
    // no tree: assume 128 MiB around the kernel
    let (base, size) = memory.unwrap_or((kernel_start & !0x7FF_FFFF, 128 << 20));
    let top = (base + size).min(512 << 30); // what the translation table covers
    // SAFETY: memory, according to the boot loader
    unsafe { mmio::map_memory(base, top - base) };
    // The heap is the memory after the kernel. If the boot loader has put
    // the device tree in there, move the tree to the very end first.
    let mut heap_end = top;
    if let Some((at, len)) = tree.as_ref().map(|t| t.extent()).filter(|&(at, len)| at >= kernel_end && at + len <= top) {
        heap_end = (top - len) & !0xFFF;
        // SAFETY: both ranges are memory nothing else uses yet
        unsafe {
            core::ptr::copy(at as *const u8, heap_end as *mut u8, len);
            fdt::set(heap_end);
        }
    }
    let heap_base = kernel_end;
    let command_line = fdt::tree()
        .as_ref()
        .and_then(|t| t.named("chosen"))
        .and_then(|n| n.property("bootargs"))
        .map_or(&[][..], |args| args.split(|&b| b == 0).next().unwrap_or(&[]));
    Info { heap_base, heap_len: heap_end.saturating_sub(heap_base), command_line }
}

/// Called by the first instructions above.
#[unsafe(no_mangle)]
extern "C" fn kmain(device_tree: usize) -> ! {
    crate::start::main(device_tree)
}
