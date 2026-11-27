//! Start-up: the first instructions, and what the boot loader passed (a
//! device tree).

use core::arch::global_asm;

use super::mmio::RAM_BASE;

// The first instructions. The boot loader enters here with the address of
// the device tree in x0, the MMU off and interrupts masked.
global_asm!(
    r#"
    .section .text.boot, "ax"
    .global _start
_start:
    // The header of a Linux arm64 kernel image: boot loaders that can
    // start Linux (QEMU's -kernel among them) then load this file to
    // "memory start + offset" and enter it with the device tree in x0.
    b 0f                            // the first instruction: skip the header
    .long 0
    .quad 0x80000                   // offset of the image from the start of memory
    .quad __image_size              // memory the image needs, .bss included
    .quad 0x2                       // flags: little-endian, 4 KiB pages
    .quad 0, 0, 0
    .ascii "ARM\x64"
    .long 0
0:
    mov x19, x0

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
    // Identity map with 1 GiB blocks: the first GiB is devices, the next
    // 255 are memory.
    adrp x0, __page_table
    add x0, x0, :lo12:__page_table
    ldr x1, =0x0060000000000405     // device memory, not executable
    str x1, [x0]
    ldr x1, =0x0000000040000701     // normal memory at 1 GiB
    mov x2, #1
    mov x3, #0x40000000
4:  str x1, [x0, x2, lsl #3]
    add x1, x1, x3
    add x2, x2, #1
    cmp x2, #256
    b.lo 4b
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
    /// The end of the kernel image (linker.ld).
    static __kernel_end: u8;
}

/// Memory for the heap and the kernel command line.
pub struct Info {
    pub heap_base: usize,
    pub heap_len: usize,
    pub command_line: &'static [u8],
}

/// A flattened device tree: just enough to find two properties.
struct DeviceTree {
    structure: &'static [u8],
    strings: &'static [u8],
}

fn be32(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
}

impl DeviceTree {
    /// # Safety
    /// `addr` must be the address of memory that holds a device tree (or
    /// at least 8 readable bytes) and is never changed.
    unsafe fn at(addr: usize) -> Option<DeviceTree> {
        let header = unsafe { core::slice::from_raw_parts(addr as *const u8, 40) };
        if be32(header, 0)? != 0xD00D_FEED {
            return None;
        }
        let total = be32(header, 4)?.min(16 << 20);
        let all = unsafe { core::slice::from_raw_parts(addr as *const u8, total) };
        Some(DeviceTree { structure: all.get(be32(header, 8)?..)?, strings: all.get(be32(header, 12)?..)? })
    }

    /// The value of property `name` in the first node below the root whose
    /// name starts with `node`.
    fn property(&self, node: &str, name: &str) -> Option<&'static [u8]> {
        let s = self.structure;
        let (mut at, mut depth, mut inside) = (0, 0, false);
        loop {
            let token = be32(s, at)?;
            at += 4;
            match token {
                1 => {
                    // a node begins: its name, NUL-terminated and padded
                    let len = s.get(at..)?.iter().position(|&b| b == 0)?;
                    depth += 1;
                    if depth == 2 {
                        inside = s[at..at + len].starts_with(node.as_bytes());
                    }
                    at = (at + len + 4) & !3;
                }
                2 => {
                    if depth == 2 {
                        inside = false;
                    }
                    depth -= 1;
                }
                3 => {
                    let (len, name_at) = (be32(s, at)?, be32(s, at + 4)?);
                    let value = s.get(at + 8..at + 8 + len)?;
                    if inside && depth == 2 && self.strings.get(name_at..)?.starts_with(name.as_bytes()) && self.strings.get(name_at + name.len()) == Some(&0) {
                        return Some(value);
                    }
                    at = (at + 8 + len + 3) & !3;
                }
                4 => {}
                _ => return None,
            }
        }
    }
}

/// Reads the device tree; the heap is the memory after the kernel.
///
/// # Safety
/// `device_tree` must be what the boot loader gave to the kernel.
pub unsafe fn info(device_tree: usize) -> Info {
    let tree = if device_tree != 0 { unsafe { DeviceTree::at(device_tree) } } else { None };
    let kernel_end = (&raw const __kernel_end as usize + 0xFFFF) & !0xFFFF;
    // memory: (base, size), 64 bits each
    let memory = tree.as_ref().and_then(|t| t.property("memory", "reg")).and_then(|reg| {
        let base = u64::from_be_bytes(reg.get(0..8)?.try_into().ok()?) as usize;
        let size = u64::from_be_bytes(reg.get(8..16)?.try_into().ok()?) as usize;
        Some((base, size))
    });
    let (base, size) = memory.unwrap_or((RAM_BASE, 128 << 20)); // no tree: assume 128 MiB
    // the boot code maps 255 GiB from 1 GiB on
    let top = (base + size).min(256 << 30);
    let command_line = tree
        .as_ref()
        .and_then(|t| t.property("chosen", "bootargs"))
        .map_or(&[][..], |args| args.split(|&b| b == 0).next().unwrap_or(&[]));
    Info { heap_base: kernel_end, heap_len: top.saturating_sub(kernel_end), command_line }
}

/// Called by the first instructions above.
#[unsafe(no_mangle)]
extern "C" fn kmain(device_tree: usize) -> ! {
    crate::start::main(device_tree)
}
