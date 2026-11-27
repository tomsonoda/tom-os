/* Kernel side of the Lua runtime. The interpreter and the library bindings
 * are Rust (rust/src/lua.rs, third_party/remedy-lua); this file gives them
 * flat C functions for the console, windows, keys and the run state, and
 * provides lua_run() to the rest of the kernel. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

enum { RUN_OK, ABORT_BREAK, ABORT_TIMEOUT, ABORT_EXIT };

/* State of one run. Several apps can run at once, each on its own thread,
 * so this hangs off the thread. Its address also names the app's sounds. */
typedef struct {
    int abort;       /* sticky */
    int interactive; /* console input allowed */
    u64 deadline;    /* 0 = none */
} runctx;
#define CTX ((runctx *)thread_data())

/* rust/src/lua.rs */
int rl_run(const char *name, size_t name_len, const char *code, size_t len, size_t mem_limit, strbuf *err);

/* Checks whether the run must stop; the result is sticky. */
int host_abort(void) {
    runctx *c = CTX;
    if (!c) return ABORT_BREAK;
    if (!c->abort) {
        if (thread_killed() || console_break_pending()) c->abort = ABORT_BREAK;
        else if (c->deadline && time_ms() > c->deadline) c->abort = ABORT_TIMEOUT;
    }
    return c->abort;
}

/* Called by the interpreter every few thousand steps. */
int host_poll(void) {
    int a = host_abort();
    if (!a) sched_yield(); /* share the CPU with other apps and the shell */
    return a;
}

void host_exit(void) {
    if (CTX) CTX->abort = ABORT_EXIT;
}

int host_interactive(void) { return CTX && CTX->interactive; }
void *host_owner(void) { return thread_data(); }

void host_panic(const char *msg, size_t len) {
    console_capture(NULL, 0);
    kprintf("\n*** Rust panic: %.*s\n*** system halted\n", (int)len, msg);
    for (;;) __asm__ volatile("cli; hlt");
}

int lua_run(const char *name, const char *code, size_t len, int interactive, int timeout_ms,
            strbuf *out, strbuf *err) {
    if (thread_data()) {
        sb_puts(err, "a Lua program is already running on this thread");
        return -1;
    }
    runctx ctx = {0};
    ctx.interactive = interactive;
    ctx.deadline = timeout_ms > 0 ? time_ms() + (u64)timeout_ms : 0;
    thread_set_data(&ctx);
    console_clear_break();
    if (!interactive) console_capture(out, 16384);

    size_t avail = mem_total() - mem_used();
    int rc = rl_run(name, strlen(name), code, len, avail / 2, err);
    if (rc != 0 && ctx.abort == ABORT_EXIT) { /* os.exit() */
        err->len = 0;
        if (err->p) err->p[0] = 0;
        rc = 0;
    } else if (rc != 0 && ctx.abort) {
        err->len = 0;
        sb_puts(err, ctx.abort == ABORT_TIMEOUT ? "time limit exceeded" : "interrupted (Ctrl-C)");
    }
    /* Sounds may outlive a program that ends normally (a short jingle);
     * a stopped one is silenced. */
    if (ctx.abort == ABORT_BREAK || ctx.abort == ABORT_TIMEOUT || thread_killed()) sound_stop(&ctx);
    if (!interactive) console_capture(NULL, 0);
    thread_set_data(NULL);
    return rc;
}

/* ----------------------------------------------------------------- tomos */
/* Returns the abort code if the run was stopped while waiting. */
int host_sleep(double ms) {
    u64 end = time_ms() + (u64)(ms > 0 ? ms : 0);
    while (time_ms() < end) {
        if (host_abort()) return host_abort();
        cpu_idle();
    }
    return 0;
}

/* One key as a name or character in out (NUL-terminated, at most 15 bytes).
 * Returns its length, 0 on timeout (or when there is no keyboard), -1 when
 * the run was stopped. timeout_ms < 0 waits. */
int host_getkey(int timeout_ms, char *out) {
    if (!host_interactive()) return 0;
    int c;
    u64 end = timeout_ms < 0 ? ~0ULL : time_ms() + (u64)timeout_ms;
    for (;;) { /* in slices, so a time limit or a closed window still applies */
        int slice = timeout_ms < 0 || end - time_ms() > 50 ? 50 : (int)(end - time_ms());
        c = console_getc(slice);
        if (c != KEY_TIMEOUT) break;
        if (host_abort()) return -1;
        if (time_ms() >= end) return 0;
    }
    if (c == KEY_BREAK) {
        if (CTX) CTX->abort = ABORT_BREAK;
        return -1;
    }
    const char *name = NULL;
    if (c == 0x1B) {
        int c2 = console_getc(30);
        name = "esc";
        if (c2 == '[' || c2 == 'O') {
            int c3 = console_getc(30);
            name = c3 == 'A' ? "up" : c3 == 'B' ? "down" : c3 == 'C' ? "right" : c3 == 'D' ? "left" : "esc";
        }
    } else if (c == '\n') name = "enter";
    else if (c == '\b') name = "backspace";
    else if (c == '\t') name = "tab";
    if (name) {
        strcpy(out, name);
        return (int)strlen(name);
    }
    int n = 0, extra = c >= 0xF0 ? 3 : c >= 0xE0 ? 2 : c >= 0xC0 ? 1 : 0;
    out[n++] = (char)c;
    while (extra-- > 0) {
        int cc = console_getc(30);
        if (cc < 0) break;
        out[n++] = (char)cc;
    }
    out[n] = 0;
    return n;
}

/* A line of input. Returns its length, -1 at end of input (no keyboard),
 * -2 when the run was stopped. */
int host_readline(char *buf, size_t max) {
    if (!host_interactive()) return -1;
    int n = console_readline(buf, max, 0);
    if (n == KEY_BREAK) {
        if (CTX) CTX->abort = ABORT_BREAK;
        return -2;
    }
    return n;
}

/* ----------------------------------------------------------------- sound */
/* Returns 1 if the run was stopped while waiting. */
int host_sound_wait(void) {
    while (sound_busy(thread_data())) {
        if (host_abort()) return 1;
        cpu_idle();
    }
    return 0;
}
