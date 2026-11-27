/* tom-os on the ARM "virt" machine: a Linux-style kernel image, loaded
 * 0x80000 above the start of memory. */
ENTRY(_start)

SECTIONS
{
    . = 0x40080000;

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }
    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    }
    .data : ALIGN(4096) {
        *(.data .data.*)
    }
    .bss : ALIGN(4096) {
        __bss_start = .;
        *(.bss .bss.* COMMON)
        . = ALIGN(4096);
        __page_table = .;
        . += 4096;
        . += 0x200000;          /* the boot thread's stack */
        __stack_top = .;
        __bss_end = .;
    }
    __kernel_end = .;
    __image_size = __kernel_end - 0x40080000;

    /DISCARD/ : {
        *(.comment) *(.note*) *(.eh_frame*)
    }
}
