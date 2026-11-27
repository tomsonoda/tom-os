//! What the boot loader passed: the Multiboot information structure.

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

/// Reads the boot information; the heap goes into the largest free piece
/// of memory.
///
/// # Safety
/// `multiboot_info` must be the address the boot loader gave to the kernel.
pub unsafe fn info(multiboot_info: usize) -> Info {
    let field = |i: usize| unsafe { ((multiboot_info + 4 * i) as *const u32).read_unaligned() };
    let flags = field(0);
    let kernel_end = (&raw const __kernel_end as usize + 0xFFFF) & !0xFFFF;
    const LIMIT: u64 = 0x1_0000_0000; // only 4 GiB are mapped

    let (mut base, mut len) = (0u64, 0u64);
    if flags & (1 << 6) != 0 {
        // memory map: entries of (size, base, length, type)
        let (mut p, end) = (field(12) as usize, field(12) as usize + field(11) as usize);
        while p < end {
            let (size, start, l, kind) = unsafe {
                (
                    (p as *const u32).read_unaligned(),
                    ((p + 4) as *const u64).read_unaligned(),
                    ((p + 12) as *const u64).read_unaligned(),
                    ((p + 20) as *const u32).read_unaligned(),
                )
            };
            if kind == 1 && start < LIMIT {
                let top = start.saturating_add(l).min(LIMIT);
                let lo = start.max(kernel_end as u64);
                if top > lo && top - lo > len {
                    (base, len) = (lo, top - lo);
                }
            }
            p += size as usize + 4;
        }
    }
    if len == 0 {
        // no memory map: assume 64 MiB
        (base, len) = (kernel_end as u64, 0x400_0000 - kernel_end as u64);
    }
    let command_line = if flags & (1 << 2) != 0 {
        // SAFETY: a NUL-terminated string from the boot loader, never freed
        unsafe { core::ffi::CStr::from_ptr(field(4) as usize as *const core::ffi::c_char) }.to_bytes()
    } else {
        &[]
    };
    Info { heap_base: base as usize, heap_len: len as usize, command_line }
}

/// Called by the assembly boot code (arch/x86_64/boot.asm) once the CPU is
/// in 64-bit mode, with what the boot loader passed.
#[unsafe(no_mangle)]
extern "C" fn kmain(multiboot_info: u32, magic: u32) {
    if magic != 0x2BAD_B002 {
        crate::console::write(b"tom-os: not started by a Multiboot loader\n");
        return;
    }
    crate::start::main(multiboot_info as usize)
}
