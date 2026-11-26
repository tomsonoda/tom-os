/* Kernel entry point. */
#include "tomos.h"

void kmain(u32 mb_info, u32 magic) {
    console_init();
    if (magic != 0x2BADB002) {
        console_puts("tom-os: not started by a Multiboot loader\n");
        return;
    }
    mem_init(mb_info);
    cpu_init();
    net_init();

    kprintf("\x1b[1mtom-os %s\x1b[0m - an operating system whose apps are written by an LLM\n",
            TOMOS_VERSION);
    kprintf("%zu MiB memory, network %s. Type /help for commands.\n\n", mem_total() >> 20,
            net_up() ? "up" : "not found");
    shell_run();
}
