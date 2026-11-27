/* Kernel side of the Lua runtime. The interpreter and the library bindings
 * are Rust (rust/src/lib.rs, third_party/remedy-lua); this file gives them
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

/* rust/src/lib.rs */
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

/* ------------------------------------------------------------------- gfx */
/* Results: 0 ok, -1 graphics unavailable here, -2 gfx.mode not called yet,
 * -3 out of memory. */
static window *app_window(void) {
    window *w = thread_window();
    return w && w->owner ? w : NULL;
}

static int canvas(surface *s) {
    window *w = app_window();
    if (!w) return -1;
    if (!w->canvas) return -2;
    surface_init(s, w->back, w->w, w->h);
    return 0;
}

int host_gfx_mode(int whole, int width, int height, const char *title, int *out_w, int *out_h) {
    window *w = app_window();
    if (!w) return -1;
    int max_w, max_h;
    wm_canvas_max(&max_w, &max_h);
    if (whole) width = max_w, height = max_h;
    if (width < 64) width = 64;
    if (height < 32) height = 32;
    if (width > max_w) width = max_w;
    if (height > max_h) height = max_h;
    if (wm_set_canvas(w, width, height) != 0) return -3;
    if (title) wm_set_title(w, title);
    *out_w = width;
    *out_h = height;
    return 0;
}

int host_gfx_clear(u32 color) {
    surface s;
    int r = canvas(&s);
    if (r == 0) gfx_fill(&s, 0, 0, s.w, s.h, color & 0xFFFFFF);
    return r;
}

int host_gfx_rect(int x, int y, int w, int h, u32 color, int outline) {
    surface s;
    int r = canvas(&s);
    if (r) return r;
    color &= 0xFFFFFF;
    if (!outline) {
        gfx_fill(&s, x, y, w, h, color);
    } else {
        gfx_fill(&s, x, y, w, 1, color);
        gfx_fill(&s, x, y + h - 1, w, 1, color);
        gfx_fill(&s, x, y, 1, h, color);
        gfx_fill(&s, x + w - 1, y, 1, h, color);
    }
    return 0;
}

int host_gfx_pixel(int x, int y, u32 color) {
    surface s;
    int r = canvas(&s);
    if (r == 0) gfx_pixel(&s, x, y, color & 0xFFFFFF);
    return r;
}

int host_gfx_line(int x0, int y0, int x1, int y1, u32 color) {
    surface s;
    int r = canvas(&s);
    if (r == 0) gfx_line(&s, x0, y0, x1, y1, color & 0xFFFFFF);
    return r;
}

int host_gfx_circle(int x, int y, int radius, u32 color, int fill) {
    surface s;
    int r = canvas(&s);
    if (r == 0) gfx_circle(&s, x, y, radius, color & 0xFFFFFF, fill);
    return r;
}

int host_gfx_text(int x, int y, const char *str, size_t len, u32 color, int scale) {
    surface s;
    int r = canvas(&s);
    if (r == 0) gfx_text(&s, x, y, str, len, color & 0xFFFFFF, -1, scale);
    return r;
}

/* Shows the frame and waits for the next one. Returns 1 if the run was stopped. */
int host_gfx_flip(double fps) {
    window *w = app_window();
    if (!w) return -1;
    if (!w->canvas) return -2;
    u64 frame = fps >= 1 ? (u64)(1000.0 / fps) : 1000;
    wm_present(w);
    do {
        if (host_abort()) return 1;
        cpu_idle();
    } while (time_ms() - w->last_flip < frame);
    w->last_flip = time_ms();
    return 0;
}

static const struct {
    const char *name;
    int code;
} key_names[] = {
    {"left", 0xCB}, {"right", 0xCD}, {"up", 0xC8}, {"down", 0xD0}, {"space", 0x39},
    {"enter", 0x1C}, {"escape", 0x01}, {"esc", 0x01}, {"tab", 0x0F}, {"backspace", 0x0E},
    {"shift", 0x2A}, {"ctrl", 0x1D}, {"a", 0x1E}, {"b", 0x30}, {"c", 0x2E}, {"d", 0x20},
    {"e", 0x12}, {"f", 0x21}, {"g", 0x22}, {"h", 0x23}, {"i", 0x17}, {"j", 0x24}, {"k", 0x25},
    {"l", 0x26}, {"m", 0x32}, {"n", 0x31}, {"o", 0x18}, {"p", 0x19}, {"q", 0x10}, {"r", 0x13},
    {"s", 0x1F}, {"t", 0x14}, {"u", 0x16}, {"v", 0x2F}, {"w", 0x11}, {"x", 0x2D}, {"y", 0x15},
    {"z", 0x2C}, {"1", 0x02}, {"2", 0x03}, {"3", 0x04}, {"4", 0x05}, {"5", 0x06}, {"6", 0x07},
    {"7", 0x08}, {"8", 0x09}, {"9", 0x0A}, {"0", 0x0B}};

/* 1/0: the key is held or not; -1: graphics unavailable; -4: unknown key name. */
int host_gfx_key(const char *name) {
    window *w = app_window();
    if (!w) return -1;
    for (size_t i = 0; i < sizeof key_names / sizeof *key_names; i++) {
        if (strcmp(key_names[i].name, name) != 0) continue;
        int code = key_names[i].code;
        int held = w->keys[code];
        if (code == 0x2A) held |= w->keys[0x36]; /* either shift */
        if (code == 0x1D) held |= w->keys[0x9D]; /* either ctrl */
        if (code == 0x1C) held |= w->keys[0x9C]; /* keypad enter */
        return held != 0;
    }
    return -4;
}

/* out: x, y, left, right. A button counts as down if it is held now or was
 * pressed since the last call, so short clicks are not missed. */
int host_gfx_mouse(int out[4]) {
    window *w = app_window();
    if (!w) return -1;
    int b = w->mb | w->mb_latch;
    w->mb_latch = 0;
    out[0] = w->mx;
    out[1] = w->my;
    out[2] = b & 1;
    out[3] = (b >> 1) & 1;
    return 0;
}

int host_gfx_size(int out[2]) {
    window *w = app_window();
    if (!w) return -1;
    out[0] = w->w;
    out[1] = w->h;
    return 0;
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
