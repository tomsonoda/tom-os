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

    /* Graphical mode unless gui=0 was passed or no suitable display exists. */
    char v[8];
    int want_gui = !(cmdline_get("gui", v, sizeof v) && v[0] == '0');
    if (want_gui && wm_init() == 0) {
        tty *shell = tty_create(80, 30);
        if (shell) {
            shell->serial = 1;
            /* the shell moves from the VGA text screen into a window */
            *console_tty0() = *shell;
            wm_create("tom-os", console_tty0(), NULL);
        }
    }

    kprintf("\x1b[1mtom-os %s\x1b[0m - an operating system whose apps are written by an LLM\n",
            TOMOS_VERSION);
    kprintf("%zu MiB memory, network %s. Type /help for commands.\n\n", mem_total() >> 20,
            net_up() ? "up" : "not found");
    shell_run();
}
