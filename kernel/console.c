/* Terminals and console I/O.
 *
 * Output: each thread writes to its tty, which feeds a terminal (the VGA text
 * screen or a window's cell buffer) and optionally COM1.
 * Input: COM1 always types into the shell's tty; the PS/2 keyboard types into
 * the shell (text mode) or the focused window (graphics mode). */
#include <stdlib.h>
#include <string.h>
#include "tomos.h"

#define COM1 0x3F8

/* ---------------------------------------------------------------- serial */
static void serial_init(void) {
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x80);
    outb(COM1 + 0, 0x01); /* 115200 baud */
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x03); /* 8N1 */
    outb(COM1 + 2, 0xC7);
    outb(COM1 + 4, 0x0B); /* DTR, RTS, OUT2 */
    outb(COM1 + 1, 0x01); /* RX interrupt: wakes HLT */
}

static void serial_putc(char c) {
    for (int i = 0; i < 100000 && !(inb(COM1 + 5) & 0x20); i++) {}
    outb(COM1, (u8)c);
}

/* -------------------------------------------------------------- terminal */
static void term_touch(term *t) {
    if (t->cy >= 0 && t->cy < 32) t->dirty |= 1u << t->cy;
}

static void term_clear_range(term *t, int from, int to) {
    for (int i = from; i < to; i++) t->cells[i] = (u16)(t->attr << 8) | ' ';
    t->dirty = ~0u;
}

static void term_newline(term *t) {
    t->cx = 0;
    if (++t->cy < t->rows) return;
    int n = (t->rows - 1) * t->cols;
    for (int i = 0; i < n; i++) t->cells[i] = t->cells[i + t->cols];
    term_clear_range(t, n, t->rows * t->cols);
    t->cy = t->rows - 1;
}

static void term_sgr(term *t, int code) {
    static const u8 map[8] = {0, 4, 2, 6, 1, 5, 3, 7};
    u8 a = t->attr;
    if (code == 0) a = 0x07;
    else if (code == 1) a |= 0x08;
    else if (code == 22) a &= (u8)~0x08;
    else if (code == 7) a = (u8)((a >> 4) | (a << 4));
    else if (code >= 30 && code <= 37) a = (u8)((a & 0xF8) | map[code - 30]);
    else if (code == 39) a = (u8)((a & 0xF8) | 7);
    else if (code >= 40 && code <= 47) a = (u8)((a & 0x8F) | (map[code - 40] << 4));
    else if (code == 49) a &= 0x0F;
    else if (code >= 90 && code <= 97) a = (u8)((a & 0xF0) | map[code - 90] | 8);
    t->attr = a;
}

static int clampi(int v, int lo, int hi) { return v < lo ? lo : v > hi ? hi : v; }

static void term_csi(term *t, char final) {
    int a = t->esc_args[0], b = t->esc_args[1];
    int W = t->cols, H = t->rows;
    if (t->esc_private) return;
    switch (final) {
    case 'H': case 'f':
        t->cy = clampi((a ? a : 1) - 1, 0, H - 1);
        t->cx = clampi((b ? b : 1) - 1, 0, W - 1);
        break;
    case 'A': t->cy = clampi(t->cy - (a ? a : 1), 0, H - 1); break;
    case 'B': t->cy = clampi(t->cy + (a ? a : 1), 0, H - 1); break;
    case 'C': t->cx = clampi(t->cx + (a ? a : 1), 0, W - 1); break;
    case 'D': t->cx = clampi(t->cx - (a ? a : 1), 0, W - 1); break;
    case 'J':
        if (a == 2 || a == 3) term_clear_range(t, 0, W * H);
        else if (a == 0) term_clear_range(t, t->cy * W + t->cx, W * H);
        else if (a == 1) term_clear_range(t, 0, t->cy * W + t->cx + 1);
        break;
    case 'K':
        if (a == 0) term_clear_range(t, t->cy * W + t->cx, (t->cy + 1) * W);
        else if (a == 1) term_clear_range(t, t->cy * W, t->cy * W + t->cx + 1);
        else term_clear_range(t, t->cy * W, (t->cy + 1) * W);
        break;
    case 'm':
        for (int i = 0; i <= t->esc_n && i < 4; i++) term_sgr(t, t->esc_args[i]);
        break;
    }
}

static void term_putc(term *t, u8 c) {
    if (t->esc_state == 1) {
        if (c == '[') {
            t->esc_state = 2;
            t->esc_n = 0;
            t->esc_private = 0;
            memset(t->esc_args, 0, sizeof t->esc_args);
        } else {
            t->esc_state = 0;
        }
        return;
    }
    if (t->esc_state == 2) {
        if (c >= '0' && c <= '9') {
            if (t->esc_n < 4) t->esc_args[t->esc_n] = t->esc_args[t->esc_n] * 10 + (c - '0');
        } else if (c == ';') {
            t->esc_n++;
        } else if (c == '?') {
            t->esc_private = 1;
        } else {
            term_touch(t);
            term_csi(t, (char)c);
            term_touch(t);
            t->esc_state = 0;
        }
        return;
    }
    term_touch(t);
    switch (c) {
    case 0x1B: t->esc_state = 1; return;
    case '\n': term_newline(t); term_touch(t); return;
    case '\r': t->cx = 0; return;
    case '\b': if (t->cx > 0) t->cx--; return;
    case '\t': do term_putc(t, ' '); while (t->cx % 8); return;
    case 7: return;
    }
    if (c >= 0x80 && c < 0xC0) return; /* UTF-8 continuation byte */
    if (c >= 0x80) c = '?';            /* only ASCII glyphs are available */
    if (c < 0x20) return;
    t->cells[t->cy * t->cols + t->cx] = (u16)(t->attr << 8) | c;
    if (++t->cx >= t->cols) term_newline(t);
    term_touch(t);
}

static void hw_cursor(const term *t) {
    u16 pos = (u16)(t->cy * t->cols + t->cx);
    outb(0x3D4, 0x0F);
    outb(0x3D5, (u8)pos);
    outb(0x3D4, 0x0E);
    outb(0x3D5, (u8)(pos >> 8));
}

/* ------------------------------------------------------------------- tty */
static term hwterm = {.cols = 80, .rows = 25, .cells = (u16 *)0xB8000, .attr = 0x07, .hw = 1};
static tty tty0 = {.term = &hwterm, .serial = 1};

tty *console_tty0(void) { return &tty0; }

tty *tty_create(int cols, int rows) {
    tty *t = calloc(1, sizeof *t);
    term *tm = calloc(1, sizeof *tm);
    u16 *cells = malloc((size_t)cols * (size_t)rows * sizeof *cells);
    if (!t || !tm || !cells) {
        free(t);
        free(tm);
        free(cells);
        return NULL;
    }
    tm->cols = cols;
    tm->rows = rows;
    tm->cells = cells;
    tm->attr = 0x07;
    term_clear_range(tm, 0, cols * rows);
    t->term = tm;
    return t;
}

void tty_destroy(tty *t) {
    if (!t || t == &tty0) return;
    if (t->term && !t->term->hw) {
        free(t->term->cells);
        free(t->term);
    }
    free(t);
}

void tty_input(tty *t, const char *bytes) {
    for (; *bytes; bytes++) {
        if (*bytes == 3) t->brk = 1;
        else if (t->head - t->tail < sizeof t->inq) t->inq[t->head++ % sizeof t->inq] = (u8)*bytes;
    }
}

void console_capture(strbuf *b, size_t limit) {
    tty *t = thread_tty();
    t->cap = b;
    t->cap_limit = limit;
}

void console_write(const char *s, size_t n) {
    tty *t = thread_tty();
    if (t->cap) {
        if (t->cap->len < t->cap_limit) {
            size_t room = t->cap_limit - t->cap->len;
            sb_put(t->cap, s, n < room ? n : room);
        }
        return;
    }
    for (size_t i = 0; i < n; i++) {
        if (t->serial) {
            if (s[i] == '\n') serial_putc('\r');
            serial_putc(s[i]);
        }
        if (t->term) term_putc(t->term, (u8)s[i]);
    }
    if (t->term && t->term->hw) hw_cursor(t->term);
}

void console_puts(const char *s) { console_write(s, strlen(s)); }

/* ---------------------------------------------------------------- PS/2 */
static const char kmap[128] = {
    0, 27, '1', '2', '3', '4', '5', '6', '7', '8', '9', '0', '-', '=', '\b', '\t',
    'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p', '[', ']', '\n', 0, 'a', 's',
    'd', 'f', 'g', 'h', 'j', 'k', 'l', ';', '\'', '`', 0, '\\', 'z', 'x', 'c', 'v',
    'b', 'n', 'm', ',', '.', '/', 0, '*', 0, ' '};
static const char kmap_shift[128] = {
    0, 27, '!', '@', '#', '$', '%', '^', '&', '*', '(', ')', '_', '+', '\b', '\t',
    'Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P', '{', '}', '\n', 0, 'A', 'S',
    'D', 'F', 'G', 'H', 'J', 'K', 'L', ':', '"', '~', 0, '|', 'Z', 'X', 'C', 'V',
    'B', 'N', 'M', '<', '>', '?', 0, '*', 0, ' '};

static int kbd_alt;

/* code: scancode (set 1), +0x80 for E0-prefixed keys. chars: what the key types. */
static void key_route(int code, int down, const char *chars) {
    if (wm_active()) wm_key(code, down, kbd_alt, chars);
    else if (down && chars) tty_input(&tty0, chars);
}

static void ps2_scancode(u8 sc) {
    static int shift, ctrl, ext;
    if (sc == 0xE0) {
        ext = 1;
        return;
    }
    int down = !(sc & 0x80);
    u8 code = sc & 0x7F;
    if (ext) {
        ext = 0;
        const char *chars = NULL;
        if (code == 0x1D) ctrl = down;
        if (code == 0x38) kbd_alt = down;
        switch (code) {
        case 0x48: chars = "\x1b[A"; break;
        case 0x50: chars = "\x1b[B"; break;
        case 0x4D: chars = "\x1b[C"; break;
        case 0x4B: chars = "\x1b[D"; break;
        case 0x1C: chars = "\n"; break;
        }
        key_route(code | 0x80, down, chars);
        return;
    }
    if (code == 0x2A || code == 0x36) shift = down;
    if (code == 0x1D) ctrl = down;
    if (code == 0x38) kbd_alt = down;
    char buf[2] = {shift ? kmap_shift[code] : kmap[code], 0};
    if (ctrl && buf[0] >= '@' && buf[0] < 127) buf[0] &= 0x1F;
    key_route(code, down, buf[0] ? buf : NULL);
}

static void mouse_byte(u8 b) {
    static u8 pkt[3];
    static int n;
    if (n == 0 && !(b & 0x08)) return; /* resynchronise on the header byte */
    pkt[n++] = b;
    if (n < 3) return;
    n = 0;
    int dx = pkt[1] - ((pkt[0] & 0x10) ? 256 : 0);
    int dy = pkt[2] - ((pkt[0] & 0x20) ? 256 : 0);
    if (pkt[0] & 0xC0) return; /* overflow */
    if (wm_active()) wm_mouse(dx, -dy, pkt[0] & 7);
}

static void ps2_wait_write(void) {
    for (int i = 0; i < 100000 && (inb(0x64) & 2); i++) {}
}

static int ps2_read(void) {
    for (int i = 0; i < 100000; i++)
        if (inb(0x64) & 1) return inb(0x60);
    return -1;
}

static void mouse_cmd(u8 cmd) {
    ps2_wait_write();
    outb(0x64, 0xD4);
    ps2_wait_write();
    outb(0x60, cmd);
    ps2_read(); /* ACK */
}

void mouse_init(void) {
    ps2_wait_write();
    outb(0x64, 0xA8); /* enable the auxiliary port */
    ps2_wait_write();
    outb(0x64, 0x20);
    int cfg = ps2_read();
    if (cfg < 0) return;
    ps2_wait_write();
    outb(0x64, 0x60);
    ps2_wait_write();
    outb(0x60, (u8)(cfg & ~0x20)); /* enable the mouse clock */
    mouse_cmd(0xF6);               /* defaults */
    mouse_cmd(0xF4);               /* start reporting */
}

void console_poll_input(void) {
    char buf[2] = {0, 0};
    /* When the queue is nearly full, leave bytes in the UART so the sender is held back. */
    while (tty0.head - tty0.tail < sizeof tty0.inq - 8 && (inb(COM1 + 5) & 1)) {
        u8 c = inb(COM1);
        if (c == '\r') c = '\n';
        if (c == 127) c = '\b';
        buf[0] = (char)c;
        if (c) tty_input(&tty0, buf);
    }
    for (int i = 0; i < 64; i++) {
        u8 st = inb(0x64);
        if (!(st & 1)) break;
        u8 b = inb(0x60);
        if (st & 0x20) mouse_byte(b);
        else ps2_scancode(b);
    }
}

/* ----------------------------------------------------------------- input */
int console_break_pending(void) {
    tty *t = thread_tty();
    if (!t->brk) return 0;
    t->brk = 0;
    return 1;
}

void console_clear_break(void) { thread_tty()->brk = 0; }

int console_getc(int timeout_ms) {
    tty *t = thread_tty();
    u64 deadline = time_ms() + (u64)(timeout_ms < 0 ? 0 : timeout_ms);
    for (;;) {
        if (t->brk) {
            t->brk = 0;
            return KEY_BREAK;
        }
        if (thread_killed()) return KEY_BREAK;
        if (t->head != t->tail) return t->inq[t->tail++ % sizeof t->inq];
        if (timeout_ms >= 0 && time_ms() >= deadline) return KEY_TIMEOUT;
        cpu_idle();
    }
}

/* Columns a UTF-8 string occupies on a terminal (wide for 3+ byte characters). */
static int display_width(const char *s, size_t n) {
    int w = 0;
    for (size_t i = 0; i < n; i++) {
        u8 c = (u8)s[i];
        if ((c & 0xC0) == 0x80) continue;
        w += c >= 0xE0 ? 2 : 1;
    }
    return w;
}

#define HISTORY 32
static char *history[HISTORY];
static int history_n;

static void history_add(const char *line) {
    if (!*line || (history_n && strcmp(history[history_n - 1], line) == 0)) return;
    char *copy = strdup(line);
    if (!copy) return;
    if (history_n == HISTORY) {
        free(history[0]);
        memmove(history, history + 1, (HISTORY - 1) * sizeof *history);
        history_n--;
    }
    history[history_n++] = copy;
}

int console_readline(char *buf, size_t max, int use_history) {
    size_t n = 0;
    int hpos = history_n;
    for (;;) {
        int c = console_getc(-1);
        if (c == KEY_BREAK) {
            console_puts("^C\n");
            return KEY_BREAK;
        }
        if (c == '\n') {
            console_puts("\n");
            buf[n] = 0;
            if (use_history) history_add(buf);
            return (int)n;
        }
        if (c == '\b') {
            if (!n) continue;
            /* remove one UTF-8 code point */
            size_t start = n - 1;
            while (start > 0 && ((u8)buf[start] & 0xC0) == 0x80) start--;
            int wide = (n - start) >= 3; /* CJK etc. occupy two columns */
            n = start;
            console_puts(wide ? "\b\b  \b\b" : "\b \b");
            continue;
        }
        if (c == 0x1B) { /* escape sequence: arrows walk the history */
            if (console_getc(20) != '[') continue;
            int dir = console_getc(20);
            if (!use_history || (dir != 'A' && dir != 'B')) continue;
            int npos = hpos + (dir == 'A' ? -1 : 1);
            if (npos < 0 || npos > history_n) continue;
            hpos = npos;
            for (int w = display_width(buf, n); w > 0; w--) console_puts("\b");
            console_puts("\x1b[K");
            const char *line = hpos < history_n ? history[hpos] : "";
            n = strlen(line);
            if (n > max - 1) n = max - 1;
            memcpy(buf, line, n);
            console_write(buf, n);
            continue;
        }
        if (c < 0x20 && c != '\t') continue;
        if (n + 1 < max) {
            buf[n++] = (char)c;
            char ch = (char)c;
            console_write(&ch, 1);
        }
    }
}

void console_init(void) {
    thread_set_tty(&tty0);
    serial_init();
    term_clear_range(&hwterm, 0, hwterm.cols * hwterm.rows);
    hw_cursor(&hwterm);
}
