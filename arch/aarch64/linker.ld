/* tom-os for 64-bit ARM: a Linux-style kernel image that can be loaded
 * anywhere. It is linked as if at address 0; the first instructions add
 * the real address to the addresses listed in .rela.dyn. */
ENTRY(_start)

SECTIONS
{
    . = 0;

    .text : {
        KEEP(*(.text.boot.a))
        QUAD(__kernel_end)      /* image size, in the image header */
        KEEP(*(.text.boot.b))
        *(.text .text.*)
    }
    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    }
    .data : ALIGN(4096) {
        *(.data .data.*)
        *(.got .got.*)
    }
    .rela.dyn : ALIGN(8) {
        __rela_start = .;
        *(.rela .rela.*)
        __rela_end = .;
    }
    /* what a position-independent link also produces; not used */
    .dynamic : { *(.dynamic) }
    .dynsym : { *(.dynsym) }
    .dynstr : { *(.dynstr) }
    .gnu.hash : { *(.gnu.hash) }
    .hash : { *(.hash) }
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

    /DISCARD/ : {
        *(.comment) *(.note*) *(.eh_frame*)
    }
}
