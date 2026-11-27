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
    if (t->cy >= 0 && t->cy < 64) t->dirty |= 1ULL << t->cy;
}

static void term_clear_range(term *t, int from, int to) {
    for (int i = from; i < to; i++) t->cells[i] = (u16)(t->attr << 8) | ' ';
    if (t->uc)
        for (int i = from; i < to; i++) t->uc[i] = 0;
    t->dirty = ~0ULL;
}

static void term_newline(term *t) {
    t->cx = 0;
    if (++t->cy < t->rows) return;
    int n = (t->rows - 1) * t->cols;
    for (int i = 0; i < n; i++) t->cells[i] = t->cells[i + t->cols];
    if (t->uc) memmove(t->uc, t->uc + t->cols, (size_t)n * sizeof *t->uc);
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

/* Writes one cell, keeping wide characters consistent: overwriting either
 * half of one clears the other half. */
static void term_set(term *t, int i, u16 cell, u32 uc) {
    if (t->uc) {
        if (t->uc[i] == TERM_CONT && i > 0 && t->uc[i - 1] != TERM_CONT) {
            t->uc[i - 1] = 0;
            t->cells[i - 1] = (u16)((t->cells[i - 1] & 0xFF00) | ' ');
        }
        if (t->uc[i] && t->uc[i] != TERM_CONT && i + 1 < t->cols * t->rows && t->uc[i + 1] == TERM_CONT) {
            t->uc[i + 1] = 0;
            t->cells[i + 1] = (u16)((t->cells[i + 1] & 0xFF00) | ' ');
        }
        t->uc[i] = uc;
    }
    t->cells[i] = cell;
}

static void term_put_cp(term *t, u32 cp) {
    int wide = t->uc && cp >= 0x80 && font_wide(cp);
    term_touch(t);
    if (cp >= 0x80 && !wide) cp = '?'; /* no glyph */
    if (wide && t->cx + 2 > t->cols) {
        term_newline(t);
        term_touch(t);
    }
    int i = t->cy * t->cols + t->cx;
    u16 attr = (u16)(t->attr << 8);
    if (wide) {
        term_set(t, i, attr | ' ', cp);
        term_set(t, i + 1, attr | ' ', TERM_CONT);
        t->cx += 2;
    } else {
        term_set(t, i, attr | (u16)cp, 0);
        t->cx++;
    }
    if (t->cx >= t->cols) term_newline(t);
    term_touch(t);
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
    if (c >= 0x80) { /* UTF-8 */
        if (c < 0xC0) {
            if (t->utf_need && --t->utf_need >= 0) {
                t->utf_cp = (t->utf_cp << 6) | (c & 0x3F);
                if (!t->utf_need) term_put_cp(t, t->utf_cp);
            }
        } else {
            t->utf_need = c >= 0xF0 ? 3 : c >= 0xE0 ? 2 : 1;
            t->utf_cp = c & (0x3F >> t->utf_need);
        }
        return;
    }
    t->utf_need = 0;
    term_touch(t);
    switch (c) {
    case 0x1B: t->esc_state = 1; return;
    case '\n': term_newline(t); term_touch(t); return;
    case '\r': t->cx = 0; return;
    case '\b': if (t->cx > 0) t->cx--; return;
    case '\t': do term_putc(t, ' '); while (t->cx % 8); return;
    case 7: return;
    }
    if (c < 0x20) return;
    term_put_cp(t, c);
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
    u32 *uc = calloc((size_t)cols * (size_t)rows, sizeof *uc);
    if (!t || !tm || !cells || !uc) {
        free(t);
        free(tm);
        free(cells);
        free(uc);
        return NULL;
    }
    tm->cols = cols;
    tm->rows = rows;
    tm->cells = cells;
    tm->uc = uc;
    tm->attr = 0x07;
    term_clear_range(tm, 0, cols * rows);
    t->term = tm;
    return t;
}

void term_resize(term *t, int cols, int rows) {
    if (t->hw || cols < 1 || rows < 1 || (cols == t->cols && rows == t->rows)) return;
    u16 *cells = malloc((size_t)cols * (size_t)rows * sizeof *cells);
    u32 *uc = calloc((size_t)cols * (size_t)rows, sizeof *uc);
    if (!cells || !uc) {
        free(cells);
        free(uc);
        return;
    }
    for (int i = 0; i < cols * rows; i++) cells[i] = (u16)(0x07 << 8) | ' ';
    /* keep the bottom part if the cursor would fall off */
    int shift = t->cy + 1 > rows ? t->cy + 1 - rows : 0;
    for (int r = 0; r < rows && r + shift < t->rows; r++) {
        for (int c = 0; c < cols && c < t->cols; c++) {
            cells[r * cols + c] = t->cells[(r + shift) * t->cols + c];
            if (t->uc) uc[r * cols + c] = t->uc[(r + shift) * t->cols + c];
        }
        /* a wide character cut in half at the new right edge */
        if (cols < t->cols && uc[r * cols + cols - 1] && uc[r * cols + cols - 1] != TERM_CONT &&
            t->uc && t->uc[(r + shift) * t->cols + cols] == TERM_CONT) {
            uc[r * cols + cols - 1] = 0;
            cells[r * cols + cols - 1] = (u16)((cells[r * cols + cols - 1] & 0xFF00) | ' ');
        }
    }
    free(t->cells);
    free(t->uc);
    t->cells = cells;
    t->uc = uc;
    t->cols = cols;
    t->rows = rows;
    t->cy -= shift;
    if (t->cx >= cols) t->cx = cols - 1;
    t->dirty = ~0ULL;
}

void tty_destroy(tty *t) {
    if (!t || t == &tty0) return;
    if (t->term && !t->term->hw) {
        free(t->term->uc);
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
    if (t->home && !t->home_mute) home_feed(t->home, s, n);
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

/* Delivers characters for a key regardless of Alt/Ctrl being held. */
static void key_route_plain(int code, const char *chars) {
    if (wm_active()) wm_key(code, 1, 0, chars);
    else tty_input(&tty0, chars);
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
    /* input method: Alt+Space or Ctrl+Space toggle; the Japanese keyboard keys */
    if (code == 0x39 && (ctrl || kbd_alt)) {
        if (down) key_route_plain(code, "\x1d");
        return;
    }
    if (code == 0x72 || code == 0x71) { /* the Mac JIS かな / 英数 keys (LANG1 / LANG2) */
        if (down) key_route_plain(code, code == 0x72 ? "\x1e" : "\x1f");
        return;
    }
    if (code == 0x70 || code == 0x77 || code == 0x79) { /* katakana/hiragana, hiragana, henkan: on */
        if (down) key_route_plain(code, "\x1e");
        return;
    }
    if (code == 0x7B) { /* muhenkan: off */
        if (down) key_route_plain(code, "\x1f");
        return;
    }
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

/* VMware-compatible absolute pointer ("vmmouse"), provided by QEMU on PCs.
 * It reports where the host cursor is, so the pointer needs no grabbing and
 * never drifts from the host's cursor, also when QEMU zooms the picture. */
#define VMPORT_MAGIC 0x564D5868u
#define VMPORT_PORT 0x5658
#define VMMOUSE_DATA 39
#define VMMOUSE_STATUS 40
#define VMMOUSE_COMMAND 41
static int vmmouse_on;

static void vmport(u32 cmd, u32 arg, u32 out[4]) {
    u32 a = VMPORT_MAGIC, b = arg, c = cmd, d = VMPORT_PORT;
    __asm__ volatile("inl %%dx, %%eax" : "+a"(a), "+b"(b), "+c"(c), "+d"(d) : : "memory");
    out[0] = a;
    out[1] = b;
    out[2] = c;
    out[3] = d;
}

static void vmmouse_init(void) {
    u32 r[4];
    vmport(10, 0, r); /* GETVERSION */
    if (r[1] != VMPORT_MAGIC) return;
    vmport(VMMOUSE_COMMAND, 0x45414552, r); /* READ_ID */
    vmport(VMMOUSE_STATUS, 0, r);
    if ((r[0] & 0xFFFF0000) == 0xFFFF0000 || (r[0] & 0xFFFF) == 0) return;
    vmport(VMMOUSE_DATA, 1, r);
    if (r[0] != 0x3442554a) return;
    vmport(VMMOUSE_COMMAND, 0x53424152, r); /* REQUEST_ABSOLUTE */
    vmmouse_on = 1;
}

int mouse_absolute(void) { return vmmouse_on; }

static void vmmouse_poll(void) {
    u32 r[4];
    for (int guard = 0; guard < 64; guard++) {
        vmport(VMMOUSE_STATUS, 0, r);
        if ((r[0] & 0xFFFF0000) == 0xFFFF0000) { /* error: start over */
            vmmouse_on = 0;
            vmmouse_init();
            return;
        }
        if ((r[0] & 0xFFFF) < 4) return;
        vmport(VMMOUSE_DATA, 4, r);
        int buttons = ((r[0] & 0x20) ? 1 : 0) | ((r[0] & 0x10) ? 2 : 0) | ((r[0] & 0x08) ? 4 : 0);
        if (wm_active()) wm_mouse_abs((int)(r[1] & 0xFFFF), (int)(r[2] & 0xFFFF), buttons, (int)(signed char)r[3]);
    }
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
    vmmouse_init();
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
        if (st & 0x20) {
            if (!vmmouse_on) mouse_byte(b); /* with vmmouse these are only wake-up packets */
        } else
            ps2_scancode(b);
    }
    if (vmmouse_on) vmmouse_poll();
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

/* Columns a UTF-8 string occupies on a terminal. */
static int display_width(const char *s, size_t n) {
    int w = 0;
    for (size_t i = 0; i < n;) {
        u32 cp;
        i += (size_t)utf8_decode(s + i, n - i, &cp);
        w += cp < 0x80 ? 1 : font_cells(cp);
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

/* ------------------------------------------------- Japanese input method
 * Romaji typed while the input method is on becomes hiragana in a
 * "composition" shown highlighted after the cursor. Space asks the LLM to
 * convert it to kanji (again for another candidate), Enter accepts,
 * Ctrl+K turns it into katakana, Backspace / Esc go back. */
static int ime_on;

int ime_enabled(void) { return ime_on; }

static void ime_set(int on) {
    if (on == ime_on) return;
    ime_on = on;
    if (wm_active()) wm_ime_changed();
}

static const char *const romaji[][2] = {
    {"a", "あ"}, {"i", "い"}, {"u", "う"}, {"e", "え"}, {"o", "お"},
    {"ka", "か"}, {"ki", "き"}, {"ku", "く"}, {"ke", "け"}, {"ko", "こ"},
    {"ga", "が"}, {"gi", "ぎ"}, {"gu", "ぐ"}, {"ge", "げ"}, {"go", "ご"},
    {"sa", "さ"}, {"si", "し"}, {"shi", "し"}, {"su", "す"}, {"se", "せ"}, {"so", "そ"},
    {"za", "ざ"}, {"zi", "じ"}, {"ji", "じ"}, {"zu", "ず"}, {"ze", "ぜ"}, {"zo", "ぞ"},
    {"ta", "た"}, {"ti", "ち"}, {"chi", "ち"}, {"tu", "つ"}, {"tsu", "つ"}, {"te", "て"}, {"to", "と"},
    {"da", "だ"}, {"di", "ぢ"}, {"du", "づ"}, {"de", "で"}, {"do", "ど"},
    {"na", "な"}, {"ni", "に"}, {"nu", "ぬ"}, {"ne", "ね"}, {"no", "の"},
    {"ha", "は"}, {"hi", "ひ"}, {"hu", "ふ"}, {"fu", "ふ"}, {"he", "へ"}, {"ho", "ほ"},
    {"ba", "ば"}, {"bi", "び"}, {"bu", "ぶ"}, {"be", "べ"}, {"bo", "ぼ"},
    {"pa", "ぱ"}, {"pi", "ぴ"}, {"pu", "ぷ"}, {"pe", "ぺ"}, {"po", "ぽ"},
    {"ma", "ま"}, {"mi", "み"}, {"mu", "む"}, {"me", "め"}, {"mo", "も"},
    {"ya", "や"}, {"yu", "ゆ"}, {"ye", "いぇ"}, {"yo", "よ"},
    {"ra", "ら"}, {"ri", "り"}, {"ru", "る"}, {"re", "れ"}, {"ro", "ろ"},
    {"la", "ぁ"}, {"li", "ぃ"}, {"lu", "ぅ"}, {"le", "ぇ"}, {"lo", "ぉ"},
    {"wa", "わ"}, {"wi", "うぃ"}, {"we", "うぇ"}, {"wo", "を"}, {"nn", "ん"}, {"n'", "ん"},
    {"va", "ゔぁ"}, {"vi", "ゔぃ"}, {"vu", "ゔ"}, {"ve", "ゔぇ"}, {"vo", "ゔぉ"},
    {"kya", "きゃ"}, {"kyi", "きぃ"}, {"kyu", "きゅ"}, {"kye", "きぇ"}, {"kyo", "きょ"},
    {"gya", "ぎゃ"}, {"gyu", "ぎゅ"}, {"gyo", "ぎょ"},
    {"sya", "しゃ"}, {"syu", "しゅ"}, {"sye", "しぇ"}, {"syo", "しょ"},
    {"sha", "しゃ"}, {"shu", "しゅ"}, {"she", "しぇ"}, {"sho", "しょ"},
    {"ja", "じゃ"}, {"ju", "じゅ"}, {"je", "じぇ"}, {"jo", "じょ"},
    {"jya", "じゃ"}, {"jyu", "じゅ"}, {"jye", "じぇ"}, {"jyo", "じょ"},
    {"zya", "じゃ"}, {"zyu", "じゅ"}, {"zye", "じぇ"}, {"zyo", "じょ"},
    {"tya", "ちゃ"}, {"tyu", "ちゅ"}, {"tye", "ちぇ"}, {"tyo", "ちょ"},
    {"cha", "ちゃ"}, {"chu", "ちゅ"}, {"che", "ちぇ"}, {"cho", "ちょ"},
    {"cya", "ちゃ"}, {"cyu", "ちゅ"}, {"cye", "ちぇ"}, {"cyo", "ちょ"},
    {"dya", "ぢゃ"}, {"dyu", "ぢゅ"}, {"dyo", "ぢょ"},
    {"thi", "てぃ"}, {"thu", "てゅ"}, {"dhi", "でぃ"}, {"dhu", "でゅ"},
    {"twu", "とぅ"}, {"dwu", "どぅ"}, {"tsa", "つぁ"}, {"tsi", "つぃ"}, {"tse", "つぇ"}, {"tso", "つぉ"},
    {"nya", "にゃ"}, {"nyu", "にゅ"}, {"nyo", "にょ"},
    {"hya", "ひゃ"}, {"hyu", "ひゅ"}, {"hyo", "ひょ"},
    {"fa", "ふぁ"}, {"fi", "ふぃ"}, {"fe", "ふぇ"}, {"fo", "ふぉ"}, {"fyu", "ふゅ"},
    {"bya", "びゃ"}, {"byu", "びゅ"}, {"byo", "びょ"},
    {"pya", "ぴゃ"}, {"pyu", "ぴゅ"}, {"pyo", "ぴょ"},
    {"mya", "みゃ"}, {"myu", "みゅ"}, {"myo", "みょ"},
    {"rya", "りゃ"}, {"ryu", "りゅ"}, {"ryo", "りょ"},
    {"xa", "ぁ"}, {"xi", "ぃ"}, {"xu", "ぅ"}, {"xe", "ぇ"}, {"xo", "ぉ"},
    {"xya", "ゃ"}, {"xyu", "ゅ"}, {"xyo", "ょ"}, {"lya", "ゃ"}, {"lyu", "ゅ"}, {"lyo", "ょ"},
    {"xtu", "っ"}, {"ltu", "っ"}, {"xtsu", "っ"}, {"ltsu", "っ"}, {"xwa", "ゎ"}, {"lwa", "ゎ"},
    {"-", "ー"}, {",", "、"}, {".", "。"}, {"[", "「"}, {"]", "」"}, {"~", "〜"},
    {"?", "？"}, {"!", "！"}, {"/", "・"},
};
#define NROMAJI (sizeof romaji / sizeof *romaji)

static int is_romaji_char(int c) { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || strchr("-,.[]~?!/'", c); }

typedef struct {
    strbuf pre;     /* kana typed so far (UTF-8) */
    char rom[8];    /* romaji not yet turned into kana */
    int nrom;
    strbuf cand;    /* conversion shown instead of `pre` */
    int converted;
    int shown;      /* columns the composition occupies on screen */
} compose;

static int composing(const compose *k) { return k->pre.len || k->nrom || k->converted; }

static void rom_flush(compose *k) { /* end of input: a lone n is ん, other letters stay */
    if (k->nrom == 1 && k->rom[0] == 'n') sb_puts(&k->pre, "ん");
    else sb_put(&k->pre, k->rom, (size_t)k->nrom);
    k->nrom = 0;
}

static void rom_feed(compose *k, char c) {
    if (c >= 'A' && c <= 'Z') c = (char)(c - 'A' + 'a');
    if (k->nrom >= (int)sizeof k->rom - 1) rom_flush(k);
    k->rom[k->nrom++] = c;
    for (;;) {
        k->rom[k->nrom] = 0;
        int prefix = 0;
        for (size_t i = 0; i < NROMAJI; i++) {
            if (strcmp(romaji[i][0], k->rom) == 0) {
                sb_puts(&k->pre, romaji[i][1]);
                k->nrom = 0;
                return;
            }
            if (strncmp(romaji[i][0], k->rom, (size_t)k->nrom) == 0) prefix = 1;
        }
        if (prefix || !k->nrom) return;
        char r0 = k->rom[0], r1 = k->nrom > 1 ? k->rom[1] : 0;
        if (r0 == 'n' && r1 && !strchr("aiueoyn'", r1)) {
            sb_puts(&k->pre, "ん"); /* n before a consonant */
        } else if (r1 && r0 == r1 && !strchr("aiueon", r0) && r0 >= 'a' && r0 <= 'z') {
            sb_puts(&k->pre, "っ"); /* doubled consonant */
        } else {
            sb_putc(&k->pre, r0); /* not romaji: keep the letter */
        }
        memmove(k->rom, k->rom + 1, (size_t)--k->nrom);
    }
}

/* Removes the last character of a UTF-8 string buffer. */
static void sb_pop_char(strbuf *b) {
    if (!b->len) return;
    size_t i = b->len - 1;
    while (i > 0 && ((u8)b->p[i] & 0xC0) == 0x80) i--;
    b->len = i;
    b->p[i] = 0;
}

static void compose_erase(compose *k) {
    for (int i = 0; i < k->shown; i++) console_puts("\b");
    if (k->shown) console_puts("\x1b[K");
    k->shown = 0;
}

static void compose_draw(compose *k, const char *note) {
    compose_erase(k);
    strbuf v;
    sb_init(&v);
    if (k->converted) sb_puts(&v, sb_cstr(&k->cand));
    else {
        sb_puts(&v, sb_cstr(&k->pre));
        sb_put(&v, k->rom, (size_t)k->nrom);
    }
    if (note) sb_puts(&v, note);
    if (v.len) {
        console_puts(k->converted ? "\x1b[30;46m" : "\x1b[30;43m");
        console_write(v.p, v.len);
        console_puts("\x1b[0m");
        k->shown = display_width(v.p, v.len);
    }
    sb_free(&v);
}

/* Moves the composition into the line being edited. */
static size_t compose_commit(compose *k, char *buf, size_t n, size_t max) {
    rom_flush(k);
    const strbuf *src = k->converted ? &k->cand : &k->pre;
    compose_erase(k);
    size_t len = src->len;
    if (n + len > max - 1) len = max - 1 - n;
    while (len && len < src->len && ((u8)src->p[len] & 0xC0) == 0x80) len--; /* whole characters */
    memcpy(buf + n, sb_cstr((strbuf *)src), len);
    console_write(buf + n, len);
    k->pre.len = k->cand.len = 0;
    k->converted = 0;
    return n + len;
}

static void compose_convert(compose *k) {
    rom_flush(k);
    if (!k->pre.len) return;
    strbuf out, err;
    sb_init(&out);
    sb_init(&err);
    char *avoid = k->converted ? strdup(sb_cstr(&k->cand)) : NULL;
    k->converted = 0;
    compose_draw(k, " …");
    if (agent_convert_kana(sb_cstr(&k->pre), avoid, &out, &err) == 0 && out.len) {
        k->cand.len = 0;
        sb_puts(&k->cand, out.p);
        k->converted = 1;
        compose_draw(k, NULL);
    } else {
        compose_draw(k, NULL);
        if (wm_active()) { /* show why, briefly, without disturbing the line */
            compose_erase(k);
            strbuf msg;
            sb_init(&msg);
            sb_printf(&msg, "\x1b[31m(%s)\x1b[0m", err.len ? err.p : "no conversion");
            console_write(msg.p, msg.len);
            int w = display_width(msg.p, msg.len) - 9; /* minus the colour codes */
            sleep_ms(1200);
            for (int i = 0; i < w; i++) console_puts("\b");
            console_puts("\x1b[K");
            compose_draw(k, NULL);
        }
    }
    free(avoid);
    sb_free(&out);
    sb_free(&err);
}

static void compose_katakana(compose *k) {
    rom_flush(k);
    for (size_t i = 0; i + 2 < k->pre.len + 1 && i < k->pre.len;) {
        u32 cp;
        int n = utf8_decode(k->pre.p + i, k->pre.len - i, &cp);
        if (n == 3 && cp >= 0x3041 && cp <= 0x3096) { /* hiragana -> katakana: +0x60 */
            cp += 0x60;
            k->pre.p[i] = (char)(0xE0 | (cp >> 12));
            k->pre.p[i + 1] = (char)(0x80 | ((cp >> 6) & 0x3F));
            k->pre.p[i + 2] = (char)(0x80 | (cp & 0x3F));
        }
        i += (size_t)n;
    }
}

/* The home screen shows the line being edited in its input box. */
static void show_input(const char *buf, size_t n, const compose *k) {
    tty *t = thread_tty();
    if (!t->home) return;
    strbuf c;
    sb_init(&c);
    if (k->converted) sb_put(&c, sb_cstr((strbuf *)&k->cand), k->cand.len);
    else {
        sb_put(&c, sb_cstr((strbuf *)&k->pre), k->pre.len);
        sb_put(&c, k->rom, (size_t)k->nrom);
    }
    home_set_input(t->home, buf, n, sb_cstr(&c), c.len, k->converted);
    sb_free(&c);
}

int console_readline(char *buf, size_t max, int use_history) {
    size_t n = 0;
    int hpos = history_n;
    compose k;
    memset(&k, 0, sizeof k);
    sb_init(&k.pre);
    sb_init(&k.cand);
    int result;
    tty *self = thread_tty();
    int muted = self->home_mute;
    self->home_mute = 1;
    for (;;) {
        show_input(buf, n, &k);
        int c = console_getc(-1);
        if (c == KEY_BREAK) {
            compose_erase(&k);
            console_puts("^C\n");
            result = KEY_BREAK;
            break;
        }
        if (c == 0x1D || c == 0x1E || c == 0x1F) { /* input method toggle / on / off */
            int on = c == 0x1D ? !ime_on : c == 0x1E;
            if (!on && composing(&k)) n = compose_commit(&k, buf, n, max);
            ime_set(on);
            continue;
        }
        if (composing(&k)) {
            if (c == 0x1B) { /* Esc: back to kana, or drop the composition; ignore arrows */
                int c2 = console_getc(20);
                if (c2 == '[') {
                    console_getc(20);
                    continue;
                }
                if (k.converted) k.converted = 0;
                else {
                    k.pre.len = 0;
                    k.nrom = 0;
                }
                compose_draw(&k, NULL);
                if (c2 < 0 || c2 == 0x1B) continue;
                c = c2;
            }
            if (k.converted) {
                if (c == '\n') {
                    n = compose_commit(&k, buf, n, max);
                    continue;
                }
                if (c == '\b') {
                    k.converted = 0;
                    compose_draw(&k, NULL);
                    continue;
                }
                if (c == ' ') {
                    compose_convert(&k); /* another candidate */
                    continue;
                }
                n = compose_commit(&k, buf, n, max); /* typing on accepts the conversion */
            } else if (composing(&k)) {
                if (c == '\n') {
                    n = compose_commit(&k, buf, n, max);
                    continue;
                }
                if (c == '\b') {
                    if (k.nrom) k.nrom--;
                    else sb_pop_char(&k.pre);
                    compose_draw(&k, NULL);
                    continue;
                }
                if (c == ' ') {
                    compose_convert(&k);
                    continue;
                }
                if (c == 0x0B) { /* Ctrl+K: katakana */
                    compose_katakana(&k);
                    compose_draw(&k, NULL);
                    continue;
                }
                if (!(ime_on && is_romaji_char(c))) n = compose_commit(&k, buf, n, max);
            }
        }
        /* lines starting with / are commands: typed as plain letters */
        int command = (n == 0 && c == '/' && !composing(&k)) || (n > 0 && buf[0] == '/');
        if (ime_on && is_romaji_char(c) && !command) {
            rom_feed(&k, (char)c);
            compose_draw(&k, NULL);
            continue;
        }
        if (c == '\n') {
            console_puts("\n");
            buf[n] = 0;
            if (use_history) history_add(buf);
            result = (int)n;
            break;
        }
        if (c == '\b') {
            if (!n) continue;
            /* remove one character */
            size_t start = n - 1;
            while (start > 0 && ((u8)buf[start] & 0xC0) == 0x80) start--;
            int w = display_width(buf + start, n - start);
            n = start;
            console_puts(w == 2 ? "\b\b  \b\b" : "\b \b");
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
    sb_free(&k.pre);
    sb_free(&k.cand);
    self->home_mute = muted;
    if (self->home) home_set_input(self->home, "", 0, "", 0, 0);
    return result;
}

void console_init(void) {
    thread_set_tty(&tty0);
    serial_init();
    term_clear_range(&hwterm, 0, hwterm.cols * hwterm.rows);
    hw_cursor(&hwterm);
}
