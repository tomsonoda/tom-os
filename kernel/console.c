/* Console: COM1 serial + VGA text output, serial + PS/2 keyboard input. */
#include <string.h>
#include "tomos.h"

#define COM1 0x3F8
#define VGA_W 80
#define VGA_H 25

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

/* ------------------------------------------------------------------- VGA */
static volatile u16 *const vga = (volatile u16 *)0xB8000;
static int vx, vy;
static u8 vattr = 0x07;
static int esc_state; /* 0 normal, 1 after ESC, 2 inside CSI */
static int esc_args[4], esc_n, esc_private;

static void vga_cursor(void) {
    u16 pos = (u16)(vy * VGA_W + vx);
    outb(0x3D4, 0x0F);
    outb(0x3D5, (u8)pos);
    outb(0x3D4, 0x0E);
    outb(0x3D5, (u8)(pos >> 8));
}

static void vga_clear_range(int from, int to) {
    for (int i = from; i < to; i++) vga[i] = (u16)(vattr << 8) | ' ';
}

static void vga_newline(void) {
    vx = 0;
    if (++vy < VGA_H) return;
    for (int i = 0; i < (VGA_H - 1) * VGA_W; i++) vga[i] = vga[i + VGA_W];
    vga_clear_range((VGA_H - 1) * VGA_W, VGA_H * VGA_W);
    vy = VGA_H - 1;
}

static void vga_sgr(int code) {
    static const u8 map[8] = {0, 4, 2, 6, 1, 5, 3, 7};
    if (code == 0) vattr = 0x07;
    else if (code == 1) vattr |= 0x08;
    else if (code == 22) vattr &= (u8)~0x08;
    else if (code == 7) vattr = (u8)((vattr >> 4) | (vattr << 4));
    else if (code >= 30 && code <= 37) vattr = (u8)((vattr & 0xF8) | map[code - 30]);
    else if (code == 39) vattr = (u8)((vattr & 0xF8) | 7);
    else if (code >= 40 && code <= 47) vattr = (u8)((vattr & 0x8F) | (map[code - 40] << 4));
    else if (code == 49) vattr &= 0x0F;
    else if (code >= 90 && code <= 97) vattr = (u8)((vattr & 0xF0) | map[code - 90] | 8);
}

static int clampi(int v, int lo, int hi) { return v < lo ? lo : v > hi ? hi : v; }

static void vga_csi(char final) {
    int a = esc_args[0], b = esc_args[1];
    if (esc_private) return;
    switch (final) {
    case 'H': case 'f':
        vy = clampi((a ? a : 1) - 1, 0, VGA_H - 1);
        vx = clampi((b ? b : 1) - 1, 0, VGA_W - 1);
        break;
    case 'A': vy = clampi(vy - (a ? a : 1), 0, VGA_H - 1); break;
    case 'B': vy = clampi(vy + (a ? a : 1), 0, VGA_H - 1); break;
    case 'C': vx = clampi(vx + (a ? a : 1), 0, VGA_W - 1); break;
    case 'D': vx = clampi(vx - (a ? a : 1), 0, VGA_W - 1); break;
    case 'J':
        if (a == 2 || a == 3) vga_clear_range(0, VGA_W * VGA_H);
        else if (a == 0) vga_clear_range(vy * VGA_W + vx, VGA_W * VGA_H);
        else if (a == 1) vga_clear_range(0, vy * VGA_W + vx + 1);
        break;
    case 'K':
        if (a == 0) vga_clear_range(vy * VGA_W + vx, (vy + 1) * VGA_W);
        else if (a == 1) vga_clear_range(vy * VGA_W, vy * VGA_W + vx + 1);
        else vga_clear_range(vy * VGA_W, (vy + 1) * VGA_W);
        break;
    case 'm':
        for (int i = 0; i <= esc_n && i < 4; i++) vga_sgr(esc_args[i]);
        break;
    }
}

static void vga_putc(u8 c) {
    if (esc_state == 1) {
        if (c == '[') {
            esc_state = 2;
            esc_n = 0;
            esc_private = 0;
            memset(esc_args, 0, sizeof esc_args);
        } else {
            esc_state = 0;
        }
        return;
    }
    if (esc_state == 2) {
        if (c >= '0' && c <= '9') {
            if (esc_n < 4) esc_args[esc_n] = esc_args[esc_n] * 10 + (c - '0');
        } else if (c == ';') {
            esc_n++;
        } else if (c == '?') {
            esc_private = 1;
        } else {
            vga_csi((char)c);
            esc_state = 0;
        }
        return;
    }
    switch (c) {
    case 0x1B: esc_state = 1; return;
    case '\n': vga_newline(); return;
    case '\r': vx = 0; return;
    case '\b': if (vx > 0) vx--; return;
    case '\t': do vga_putc(' '); while (vx % 8); return;
    case 7: return;
    }
    if (c >= 0x80 && c < 0xC0) return; /* UTF-8 continuation byte */
    if (c >= 0x80) c = '?';            /* no glyphs beyond ASCII in text mode */
    if (c < 0x20) return;
    vga[vy * VGA_W + vx] = (u16)(vattr << 8) | c;
    if (++vx >= VGA_W) vga_newline();
}

/* ---------------------------------------------------------------- output */
static strbuf *cap;
static size_t cap_limit;

void console_capture(strbuf *b, size_t limit) {
    cap = b;
    cap_limit = limit;
}

int console_capturing(void) { return cap != NULL; }

void console_write(const char *s, size_t n) {
    if (cap) {
        if (cap->len < cap_limit) {
            size_t room = cap_limit - cap->len;
            sb_put(cap, s, n < room ? n : room);
        }
        return;
    }
    for (size_t i = 0; i < n; i++) {
        if (s[i] == '\n') serial_putc('\r');
        serial_putc(s[i]);
        vga_putc((u8)s[i]);
    }
    vga_cursor();
}

void console_puts(const char *s) { console_write(s, strlen(s)); }

/* ----------------------------------------------------------------- input */
static u8 inq[4096];
static unsigned inq_head, inq_tail;
static int break_flag;

static void inq_push(u8 c) {
    if (c == 3) {
        break_flag = 1;
        return;
    }
    if (inq_head - inq_tail < sizeof inq) inq[inq_head++ % sizeof inq] = c;
}

static void inq_push_str(const char *s) {
    while (*s) inq_push((u8)*s++);
}

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

static void ps2_scancode(u8 sc) {
    static int shift, ctrl, ext;
    if (sc == 0xE0) {
        ext = 1;
        return;
    }
    int release = sc & 0x80;
    u8 code = sc & 0x7F;
    if (ext) {
        ext = 0;
        if (code == 0x1D) ctrl = !release;
        if (release) return;
        switch (code) {
        case 0x48: inq_push_str("\x1b[A"); break;
        case 0x50: inq_push_str("\x1b[B"); break;
        case 0x4D: inq_push_str("\x1b[C"); break;
        case 0x4B: inq_push_str("\x1b[D"); break;
        case 0x1C: inq_push('\n'); break;
        }
        return;
    }
    if (code == 0x2A || code == 0x36) {
        shift = !release;
        return;
    }
    if (code == 0x1D) {
        ctrl = !release;
        return;
    }
    if (release) return;
    char ch = shift ? kmap_shift[code] : kmap[code];
    if (!ch) return;
    if (ctrl && ch >= '@' && ch < 127) ch &= 0x1F;
    inq_push((u8)ch);
}

static void input_poll(void) {
    /* When the queue is full, leave bytes in the UART so the sender is held back. */
    while (inq_head - inq_tail < sizeof inq - 8 && (inb(COM1 + 5) & 1)) {
        u8 c = inb(COM1);
        if (c == '\r') c = '\n';
        if (c == 127) c = '\b';
        inq_push(c);
    }
    for (int i = 0; i < 16; i++) {
        u8 st = inb(0x64);
        if (!(st & 1)) break;
        u8 sc = inb(0x60);
        if (!(st & 0x20)) ps2_scancode(sc); /* ignore mouse bytes */
    }
}

int console_break_pending(void) {
    input_poll();
    if (!break_flag) return 0;
    break_flag = 0;
    return 1;
}

void console_clear_break(void) {
    input_poll();
    break_flag = 0;
}

int console_getc(int timeout_ms) {
    u64 deadline = time_ms() + (u64)(timeout_ms < 0 ? 0 : timeout_ms);
    for (;;) {
        input_poll();
        if (break_flag) {
            break_flag = 0;
            return KEY_BREAK;
        }
        if (inq_head != inq_tail) return inq[inq_tail++ % sizeof inq];
        if (timeout_ms >= 0 && time_ms() >= deadline) return KEY_TIMEOUT;
        net_poll();
        cpu_idle();
    }
}

int console_readline(char *buf, size_t max) {
    size_t n = 0;
    for (;;) {
        int c = console_getc(-1);
        if (c == KEY_BREAK) {
            console_puts("^C\n");
            return KEY_BREAK;
        }
        if (c == '\n') {
            console_puts("\n");
            buf[n] = 0;
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
        if (c == 0x1B) { /* swallow escape sequences (arrow keys) */
            if (console_getc(20) == '[') console_getc(20);
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
    serial_init();
    vga_clear_range(0, VGA_W * VGA_H);
    vx = vy = 0;
    vga_cursor();
}
