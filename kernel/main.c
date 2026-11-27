/* Kernel entry point. */
#include <stdlib.h>
#include <string.h>
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
    int w = 1024, h = 768, sc = 0;
    char res[24];
    if (cmdline_get("res", res, sizeof res) && strchr(res, 'x')) {
        w = atoi(res);
        h = atoi(strchr(res, 'x') + 1);
    }
    if (cmdline_get("scale", v, sizeof v)) sc = atoi(v);
    int gui = want_gui && wm_init(w, h, sc) == 0;
    if (want_gui && !gui && (w != 1024 || h != 768 || sc)) {
        kprintf("[display] %dx%d is not possible (at least 800x600 per scale step), using 1024x768\n",
                w, h);
        gui = wm_init(1024, 768, 1) == 0;
    }
    if (gui && cmdline_get("layout", v, sizeof v) && v[0] == 'w') wm_set_layout(LAYOUT_WINDOWS);
    if (gui) {
        tty *shell = tty_create(80, 30); /* resized to fit the screen by wm_set_home */
        if (shell) {
            shell->serial = 1;
            /* the shell moves from the VGA text screen into the home screen */
            *console_tty0() = *shell;
            wm_set_home(wm_create("tom-os", console_tty0(), NULL));
        }
    }

    kprintf("\x1b[1mtom-os %s\x1b[0m - an operating system whose apps are written by an LLM\n",
            TOMOS_VERSION);
    kprintf("%zu MiB memory, network %s. Type /help for commands.\n\n", mem_total() >> 20,
            net_up() ? "up" : "not found");
    shell_run();
}
