/* The top-level prompt: slash commands are handled locally, everything else
 * goes to the agent. */
#include <stdio.h>
#include <string.h>
#include "../kernel/tomos.h"

static void cmd_help(void) {
    console_puts(
        "Type what you want in plain language; the agent answers or builds an app for it.\n"
        "Commands:\n"
        "  /apps          list files (apps are saved as apps/<name>.lua)\n"
        "  /run <name>    start an app (Ctrl-C stops it)\n"
        "  /cat <path>    show a file\n"
        "  /rm <path>     delete a file\n"
        "  /lua <code>    run one line of Lua\n"
        "  /clear         forget the conversation\n"
        "  /info          memory, network and LLM settings\n"
        "  /reboot, /halt\n");
}

static void cmd_info(void) {
    char ip[16], llm[64] = "10.0.2.2:8080", model[96] = "default";
    net_fmt_ip(net_ip(), ip);
    cmdline_get("llm", llm, sizeof llm);
    cmdline_get("model", model, sizeof model);
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
    kprintf("llm    : http://%s  model=%s\n", llm, model);
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
        if (!*arg) console_puts("usage: /run <name>\n");
        else if (app_run(arg, &err) != 0) kprintf("[error] %s\n", sb_cstr(&err));
        sb_free(&err);
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
        int n = console_readline(line, sizeof line);
        if (n <= 0) continue;
        if (line[0] == '/') command(line);
        else agent_turn(line);
    }
}
