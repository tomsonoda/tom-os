/* The top-level prompt: slash commands are handled locally, everything else
 * goes to the agent. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

static void cmd_help(void) {
    console_puts(
        "Type what you want in plain language; the agent answers or builds an app for it.\n"
        "Commands:\n"
        "  /apps          list files (apps are saved as apps/<name>.lua)\n"
        "  /run <name>    start an app (Ctrl-C stops it)\n"
        "  /ps, /kill <id>  list and stop running apps (graphical mode)\n"
        "  /display <w>x<h> [scale]  change the screen size (graphical mode)\n"
        "  /layout full|windows      one app at a time, or overlapping windows\n"
        "Japanese input: Option/Alt+Space switches romaji -> kana (あ) on and off.\n"
        "  Space: convert to kanji (again: another candidate), Enter: accept,\n"
        "  Ctrl+K: katakana, Backspace/Esc: undo. Lines starting with / stay in letters.\n"
        "  /cat <path>    show a file\n"
        "  /rm <path>     delete a file\n"
        "  /lua <code>    run one line of Lua\n"
        "  /clear         forget the conversation\n"
        "  /info          memory, network and LLM settings\n"
        "  /set llm <ip:port> | model <name> | path <url path>\n"
        "                 change which LLM server and model are used\n"
        "  /reboot, /halt\n");
}

static void cmd_info(void) {
    char ip[16];
    net_fmt_ip(net_ip(), ip);
    kprintf("tom-os %s\n", TOMOS_VERSION);
    kprintf("memory : %zu KiB used of %zu MiB\n", mem_used() / 1024, mem_total() >> 20);
    if (net_up()) {
        u8 mac[6];
        nic_mac(mac);
        kprintf("network: virtio-net %02x:%02x:%02x:%02x:%02x:%02x, ip %s\n", mac[0], mac[1],
                mac[2], mac[3], mac[4], mac[5], ip);
    } else {
        kprintf("network: no device\n");
    }
    if (wm_active())
        kprintf("display: graphical (%s), %dx%d, scale %d, %s pointer\n",
                wm_layout() == LAYOUT_FULL ? "full screen" : "windows", wm_display_w(), wm_display_h(),
                wm_scale(), mouse_absolute() ? "absolute" : "relative");
    else
        kprintf("display: text\n");
    kprintf("llm    : http://%s%s  model=%s\n", agent_setting("llm"), agent_setting("path"),
            agent_setting("model"));
}

static void cmd_apps(void) {
    void *it = NULL;
    size_t len;
    const char *name;
    int n = 0;
    while ((name = fs_next(&it, &len))) {
        kprintf("  %-40s %zu bytes\n", name, len);
        n++;
    }
    if (!n) console_puts("  (no files yet - ask the agent to make an app)\n");
}

static void run_lua_checked(const char *name, const char *code, size_t len) {
    strbuf err;
    sb_init(&err);
    if (lua_run(name, code, len, 1, 0, NULL, &err) != 0) kprintf("[error] %s\n", sb_cstr(&err));
    sb_free(&err);
}

static void command(char *line) {
    char *arg = strchr(line, ' ');
    if (arg) {
        *arg++ = 0;
        while (*arg == ' ') arg++;
    } else {
        arg = "";
    }

    if (!strcmp(line, "/help") || !strcmp(line, "/?")) {
        cmd_help();
    } else if (!strcmp(line, "/apps") || !strcmp(line, "/ls")) {
        cmd_apps();
    } else if (!strcmp(line, "/run")) {
        strbuf err;
        sb_init(&err);
        int r = *arg ? app_launch(arg, &err) : -2;
        if (r == -2) console_puts("usage: /run <name>\n");
        else if (r < 0) kprintf("[error] %s\n", sb_cstr(&err));
        else if (r == 1 && wm_layout() == LAYOUT_FULL)
            kprintf("%s is running full screen (drag the bottom bar to switch apps)\n", arg);
        else if (r == 1) kprintf("%s is running in its own window\n", arg);
        sb_free(&err);
    } else if (!strcmp(line, "/display")) {
        int w = 0, h = 0, sc = 0;
        char *x = strchr(arg, 'x');
        if (x) {
            w = atoi(arg);
            h = atoi(x + 1);
            char *sp = strchr(x, ' ');
            if (sp) sc = atoi(sp + 1);
        }
        if (!wm_active()) {
            console_puts("/display only works in graphical mode\n");
        } else if (!w || !h) {
            kprintf("display: %dx%d, scale %d\n", wm_display_w(), wm_display_h(), wm_scale());
            console_puts("usage: /display <width>x<height> [scale]\n"
                         "  e.g. /display 1920x1080      more room\n"
                         "       /display 2048x1536 2    the same layout, twice as big\n"
                         "       /display 1024x768       the default\n");
        } else {
            strbuf err;
            sb_init(&err);
            if (wm_set_display(w, h, sc, &err) != 0) kprintf("[error] %s\n", sb_cstr(&err));
            else kprintf("display: %dx%d, scale %d\n", wm_display_w(), wm_display_h(), wm_scale());
            sb_free(&err);
        }
    } else if (!strcmp(line, "/layout")) {
        if (!wm_active()) console_puts("/layout only works in graphical mode\n");
        else if (!strcmp(arg, "full")) wm_set_layout(LAYOUT_FULL);
        else if (!strcmp(arg, "windows")) wm_set_layout(LAYOUT_WINDOWS);
        else
            kprintf("layout: %s\nusage: /layout full      one app fills the screen (switch by dragging the bottom bar)\n"
                    "       /layout windows   overlapping windows\n",
                    wm_layout() == LAYOUT_FULL ? "full" : "windows");
    } else if (!strcmp(line, "/ps")) {
        app_list();
    } else if (!strcmp(line, "/kill")) {
        if (app_kill(atoi(arg)) != 0) console_puts("usage: /kill <id>   (ids are shown by /ps)\n");
    } else if (!strcmp(line, "/cat")) {
        size_t len;
        const char *d = fs_read(arg, &len);
        if (!d) kprintf("no such file: %s\n", arg);
        else {
            console_write(d, len);
            if (len && d[len - 1] != '\n') console_puts("\n");
        }
    } else if (!strcmp(line, "/rm")) {
        kprintf(fs_delete(arg) == 0 ? "deleted %s\n" : "no such file: %s\n", arg);
    } else if (!strcmp(line, "/lua")) {
        run_lua_checked("lua", arg, strlen(arg));
    } else if (!strcmp(line, "/clear")) {
        agent_reset();
        console_puts("conversation cleared\n");
    } else if (!strcmp(line, "/set")) {
        char *val = strchr(arg, ' ');
        if (val) {
            *val++ = 0;
            while (*val == ' ') val++;
        }
        if (!val || agent_set(arg, val) != 0)
            console_puts("usage: /set llm <ip:port> | /set model <name> | /set path <url path>\n");
        else
            kprintf("%s = %s\n", arg, agent_setting(arg));
    } else if (!strcmp(line, "/info")) {
        cmd_info();
    } else if (!strcmp(line, "/halt")) {
        console_puts("powering off\n");
        cpu_poweroff();
    } else if (!strcmp(line, "/reboot")) {
        cpu_reboot();
    } else {
        kprintf("unknown command %s (try /help)\n", line);
    }
}

void shell_run(void) {
    static char line[4096];
    for (;;) {
        console_puts("\x1b[1;32mtom-os>\x1b[0m ");
        int n = console_readline(line, sizeof line, 1);
        if (n <= 0) continue;
        if (line[0] == '/') command(line);
        else agent_turn(line);
    }
}
