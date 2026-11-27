/* The home screen in full-screen layout: the prompt as a friendly chat.
 *
 * The shell still writes plain text to its terminal (and the serial line);
 * a copy of that stream comes here. Private escape sequences, which
 * terminals ignore, say what the text is (see UI_* in tomos.h): the agent's
 * reply, a note such as a tool step, an error, the waiting status, or text
 * to leave out (the prompt and line editing). Text without a marker is shown
 * as a note. The line being typed is shown in an input box at the bottom. */
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

enum { H_NOTE, H_USER, H_REPLY, H_ERROR };

#define MAX_ENTRIES 200
#define LINE_H 20
#define GAP 14
#define MAX_COL 760

/* colours */
#define C_BG 0xF6F3EE
#define C_TEXT 0x2B2926
#define C_MUTED 0x8C877F
#define C_ACCENT 0xD0794F
#define C_USER 0xE9E2D6
#define C_NOTE_BG 0xEEE9E1
#define C_ERROR 0xB8473B
#define C_BOX 0xFFFFFF
#define C_BOX_LINE 0xDCD4C8
#define C_COMP 0xF6DCCB
#define C_CONV 0xCFE6E1
#define C_SEND_OFF 0xDAD3C7

typedef struct {
    int kind;
    strbuf text;
} entry;

typedef struct {
    int x, y, w, h;
    const char *text;
} chip;

struct homeview {
    tty *tty;
    entry e[MAX_ENTRIES];
    int n;
    int open;      /* entry receiving text, or -1 */
    int hidden;    /* between UI_HIDE and UI_END */
    int status_on; /* between UI_STATUS and UI_END */
    strbuf status;
    int esc, escn;
    char escbuf[24];
    strbuf input, comp;
    int converted;
    int scroll; /* pixels scrolled back from the newest text */
    int max_scroll;
    int dirty;
    chip chips[4];
    int nchips;
    int send_x, send_y, send_r;
};

static const char *const suggestions[] = {
    "電卓を作って",
    "ブロック崩しを作って",
    "Make a drawing app I can use with the mouse",
    "今日は何曜日？",
};

homeview *home_create(tty *t) {
    homeview *h = calloc(1, sizeof *h);
    if (!h) return NULL;
    h->tty = t;
    h->open = -1;
    h->dirty = 1;
    return h;
}

int home_dirty(const homeview *h) { return h->dirty; }
void home_invalidate(homeview *h) { h->dirty = 1; }

static entry *new_entry(homeview *h, int kind) {
    if (h->n == MAX_ENTRIES) { /* forget the oldest */
        sb_free(&h->e[0].text);
        memmove(h->e, h->e + 1, (MAX_ENTRIES - 1) * sizeof *h->e);
        h->n--;
    }
    entry *e = &h->e[h->n];
    e->kind = kind;
    sb_init(&e->text);
    h->open = h->n++;
    h->scroll = 0; /* new text: back to the bottom */
    return e;
}

static void marker(homeview *h, int code, char mode) {
    if (mode == 'l') { /* UI_END closes whatever was open */
        if (h->hidden) h->hidden = 0;
        else if (h->status_on) h->status_on = 0;
        else h->open = -1;
        return;
    }
    switch (code) {
    case 7101: new_entry(h, H_REPLY); break;
    case 7102: new_entry(h, H_NOTE); break;
    case 7103: new_entry(h, H_ERROR); break;
    case 7104:
        h->status_on = 1;
        h->status.len = 0;
        if (h->status.p) h->status.p[0] = 0;
        break;
    case 7105: h->hidden = 1; break;
    }
}

void home_feed(homeview *h, const char *s, size_t n) {
    for (size_t i = 0; i < n; i++) {
        char c = s[i];
        if (h->esc) {
            if (h->escn < (int)sizeof h->escbuf - 1) h->escbuf[h->escn++] = c;
            if ((c >= '@' && c <= '~' && h->escn > 1) || (h->escn == 1 && c != '[')) {
                h->escbuf[h->escn] = 0;
                if (h->escbuf[0] == '[' && h->escbuf[1] == '?' && (c == 'h' || c == 'l'))
                    marker(h, atoi(h->escbuf + 2), c);
                h->esc = 0;
            }
            continue;
        }
        if (c == 0x1B) {
            h->esc = 1;
            h->escn = 0;
            continue;
        }
        if (c == '\r' || c == '\b' || c == 7 || h->hidden) continue;
        if (h->status_on) {
            sb_putc(&h->status, c);
        } else {
            if (h->open < 0) {
                if (c == '\n') continue; /* no empty notes */
                new_entry(h, H_NOTE);
            }
            sb_putc(&h->e[h->open].text, c);
        }
        h->dirty = 1;
    }
    h->dirty = 1;
}

void home_add_user(homeview *h, const char *line) {
    entry *e = new_entry(h, H_USER);
    sb_puts(&e->text, line);
    h->open = -1;
    h->dirty = 1;
}

void home_set_input(homeview *h, const char *line, size_t n, const char *comp, size_t cn, int converted) {
    if (h->input.len == n && h->comp.len == cn && h->converted == converted &&
        (!n || !memcmp(h->input.p, line, n)) && (!cn || !memcmp(h->comp.p, comp, cn)))
        return;
    h->input.len = 0;
    sb_put(&h->input, line, n);
    if (!n && h->input.p) h->input.p[0] = 0;
    h->comp.len = 0;
    sb_put(&h->comp, comp, cn);
    if (!cn && h->comp.p) h->comp.p[0] = 0;
    h->converted = converted;
    h->dirty = 1;
}

void home_scroll(homeview *h, int dy) {
    h->scroll += dy;
    if (h->scroll < 0) h->scroll = 0;
    if (h->scroll > h->max_scroll) h->scroll = h->max_scroll;
    h->dirty = 1;
}

void home_click(homeview *h, int x, int y) {
    for (int i = 0; i < h->nchips; i++) {
        chip *c = &h->chips[i];
        if (x >= c->x && x < c->x + c->w && y >= c->y && y < c->y + c->h) {
            tty_input(h->tty, c->text);
            tty_input(h->tty, "\n");
            return;
        }
    }
    int dx = x - h->send_x, dy = y - h->send_y;
    if (h->input.len && dx * dx + dy * dy <= h->send_r * h->send_r) tty_input(h->tty, "\n");
}

/* ------------------------------------------------------------- drawing */
static int char_w(u32 cp) { return font_wide(cp) ? 16 : 8; }

static int text_w(const char *s, size_t n) {
    int w = 0;
    for (size_t i = 0; i < n;) {
        u32 cp;
        i += (size_t)utf8_decode(s + i, n - i, &cp);
        w += char_w(cp);
    }
    return w;
}

/* Next line of `s` that fits in maxw pixels, breaking at spaces if possible. */
static int next_line(const char *s, size_t n, size_t *pos, int maxw, size_t *start, size_t *len) {
    if (*pos >= n) return 0;
    size_t i = *pos, brk = 0;
    int w = 0, have_brk = 0;
    *start = i;
    while (i < n) {
        if (s[i] == '\n') {
            *len = i - *start;
            *pos = i + 1;
            return 1;
        }
        u32 cp;
        int b = utf8_decode(s + i, n - i, &cp);
        int cw = char_w(cp);
        if (w + cw > maxw && i > *start) {
            if (have_brk) {
                *len = brk - *start;
                *pos = brk + 1;
            } else {
                *len = i - *start;
                *pos = i;
            }
            return 1;
        }
        if (cp == ' ') {
            brk = i;
            have_brk = 1;
        }
        w += cw;
        i += (size_t)b;
    }
    *len = i - *start;
    *pos = i;
    return 1;
}

static int count_lines(const char *s, size_t n, int maxw) {
    size_t pos = 0, st, len;
    int lines = 0;
    while (next_line(s, n, &pos, maxw, &st, &len)) lines++;
    return lines ? lines : 1;
}

/* Text of an entry without leading/trailing blank lines and indentation. */
static void trimmed(const entry *e, const char **s, size_t *n) {
    const char *p = sb_cstr((strbuf *)&e->text);
    size_t len = e->text.len;
    while (len && (*p == '\n' || *p == ' ')) p++, len--;
    while (len && (p[len - 1] == '\n' || p[len - 1] == ' ')) len--;
    *s = p;
    *n = len;
}

static void draw_lines(surface *s, const char *t, size_t n, int x, int y, int maxw, u32 color) {
    size_t pos = 0, st, len;
    while (next_line(t, n, &pos, maxw, &st, &len)) {
        gfx_text(s, x, y + 2, t + st, len, color, -1, 1);
        y += LINE_H;
    }
}

/* Height and drawing of one entry; draw only when s is not NULL. */
static int entry_block(surface *s, const entry *e, int col_x, int col_w, int y) {
    const char *t;
    size_t n;
    trimmed(e, &t, &n);
    if (!n) return -GAP; /* nothing to show */
    switch (e->kind) {
    case H_USER: {
        int maxw = col_w * 3 / 4 - 28, tw = text_w(t, n);
        int lines = count_lines(t, n, maxw);
        int bw = (lines == 1 ? tw : maxw) + 28, bh = lines * LINE_H + 16;
        if (s) {
            gfx_round(s, col_x + col_w - bw, y, bw, bh, 14, C_USER);
            draw_lines(s, t, n, col_x + col_w - bw + 14, y + 8, maxw, C_TEXT);
        }
        return bh;
    }
    case H_REPLY: {
        int maxw = col_w - 40;
        int lines = count_lines(t, n, maxw);
        if (s) {
            gfx_circle(s, col_x + 12, y + 12, 12, C_ACCENT, 1);
            gfx_glyph(s, col_x + 8, y + 4, 't', 0xFFFFFF, -1, 1);
            draw_lines(s, t, n, col_x + 40, y + 2, maxw, C_TEXT);
        }
        int h = lines * LINE_H + 4;
        return h < 26 ? 26 : h;
    }
    default: { /* notes and errors: quieter, in a soft box */
        int maxw = col_w - 64;
        int lines = count_lines(t, n, maxw);
        int bh = lines * LINE_H + 10;
        if (s) {
            gfx_round(s, col_x + 40, y, col_w - 40, bh, 8, e->kind == H_ERROR ? 0xF5E1DC : C_NOTE_BG);
            /* "[tool] details": the tool name in the accent colour */
            const char *close = n > 2 && t[0] == '[' ? memchr(t, ']', n < 40 ? n : 40) : NULL;
            if (close && e->kind != H_ERROR && lines == 1) {
                size_t name = (size_t)(close - t - 1);
                gfx_text(s, col_x + 52, y + 7, t + 1, name, C_ACCENT, -1, 1);
                size_t rest = n - (size_t)(close + 1 - t);
                const char *r = close + 1;
                while (rest && *r == ' ') r++, rest--;
                gfx_text(s, col_x + 52 + text_w(t + 1, name) + 12, y + 7, r, rest, C_MUTED, -1, 1);
            } else {
                draw_lines(s, t, n, col_x + 52, y + 5, maxw, e->kind == H_ERROR ? C_ERROR : C_MUTED);
            }
        }
        return bh;
    }
    }
}

static void draw_welcome(homeview *h, surface *s, int col_x, int col_w, int bottom) {
    static const char hello[] = "こんにちは";
    static const char sub[] = "What would you like to make? Ask in any language.";
    int y = bottom / 2 - 70;
    if (y < 20) y = 20;
    gfx_circle(s, s->w / 2, y - 30, 18, C_ACCENT, 1);
    gfx_glyph(s, s->w / 2 - 8, y - 46, 't', 0xFFFFFF, -1, 2);
    gfx_text(s, (s->w - text_w(hello, sizeof hello - 1) * 2) / 2, y, hello, sizeof hello - 1, C_TEXT, -1, 2);
    gfx_text(s, (s->w - text_w(sub, sizeof sub - 1)) / 2, y + 44, sub, sizeof sub - 1, C_MUTED, -1, 1);
    /* suggestion chips, centred, in rows */
    h->nchips = 0;
    int row_y = y + 84, x = 0, row_start = 0;
    int widths[4];
    for (int i = 0; i < 4; i++) widths[i] = text_w(suggestions[i], strlen(suggestions[i])) + 28;
    for (int i = 0; i <= 4; i++) {
        if (i == 4 || (x + widths[i] > col_w && i > row_start)) {
            int cx = col_x + (col_w - (x - 10)) / 2;
            for (int j = row_start; j < i; j++) {
                chip *c = &h->chips[h->nchips++];
                c->x = cx;
                c->y = row_y;
                c->w = widths[j];
                c->h = 32;
                c->text = suggestions[j];
                gfx_round(s, c->x, c->y, c->w, c->h, 16, C_BOX_LINE);
                gfx_round(s, c->x + 1, c->y + 1, c->w - 2, c->h - 2, 15, C_BOX);
                gfx_text(s, c->x + 14, c->y + 8, c->text, strlen(c->text), C_TEXT, -1, 1);
                cx += widths[j] + 10;
            }
            row_y += 42;
            x = 0;
            row_start = i;
        }
        if (i < 4) x += widths[i] + 10;
    }
}

static void draw_input(homeview *h, surface *s, int col_x, int col_w, int box_y, int box_h, int inner_w) {
    gfx_round(s, col_x, box_y, col_w, box_h, 16, C_BOX_LINE);
    gfx_round(s, col_x + 1, box_y + 1, col_w - 2, box_h - 2, 15, C_BOX);
    int tx = col_x + 18, ty = box_y + 12;
    if (!h->input.len && !h->comp.len) {
        static const char ph[] = "Ask tom-os anything...   (Option/Alt+Space: 日本語)";
        gfx_text(s, tx, ty + 2, ph, sizeof ph - 1, C_MUTED, -1, 1);
        gfx_fill(s, tx, ty, 2, 18, C_ACCENT);
    } else {
        /* the line, then the composition highlighted, then the caret */
        strbuf all;
        sb_init(&all);
        sb_put(&all, sb_cstr(&h->input), h->input.len);
        size_t comp_from = all.len;
        sb_put(&all, sb_cstr(&h->comp), h->comp.len);
        size_t pos = 0, st, len;
        int y = ty, cx = tx, cy = ty;
        while (next_line(all.p, all.len, &pos, inner_w, &st, &len)) {
            int x = tx;
            for (size_t i = st; i < st + len;) {
                u32 cp;
                int b = utf8_decode(all.p + i, st + len - i, &cp);
                int w = char_w(cp);
                if (i >= comp_from) gfx_fill(s, x, y, w, 20, h->converted ? C_CONV : C_COMP);
                gfx_text(s, x, y + 2, all.p + i, (size_t)b, C_TEXT, -1, 1);
                x += w;
                i += (size_t)b;
            }
            cx = x;
            cy = y;
            y += LINE_H;
        }
        if (all.len && all.p[all.len - 1] == '\n') {
            cx = tx;
            cy = y;
        }
        gfx_fill(s, cx, cy, 2, 18, C_ACCENT);
        sb_free(&all);
    }
    /* language and send buttons */
    int r = 14, sx = col_x + col_w - 12 - r, sy = box_y + box_h - 8 - r;
    h->send_x = sx;
    h->send_y = sy;
    h->send_r = r;
    gfx_circle(s, sx, sy, r, h->input.len ? C_ACCENT : C_SEND_OFF, 1);
    gfx_text(s, sx - 8, sy - 8, "↑", 3, 0xFFFFFF, -1, 1);
    const char *ime = ime_enabled() ? "あ" : "A";
    gfx_text(s, sx - r - 26 + (ime_enabled() ? 0 : 4), sy - 8, ime, strlen(ime),
             ime_enabled() ? C_ACCENT : C_MUTED, -1, 1);
}

void home_render(homeview *h, surface *s) {
    h->dirty = 0;
    gfx_fill(s, 0, 0, s->w, s->h, C_BG);
    int col_w = s->w - 64 > MAX_COL ? MAX_COL : s->w - 64;
    int col_x = (s->w - col_w) / 2;

    /* input box: grows with the text, up to four lines */
    int inner_w = col_w - 36 - 60;
    strbuf all;
    sb_init(&all);
    sb_put(&all, sb_cstr(&h->input), h->input.len);
    sb_put(&all, sb_cstr(&h->comp), h->comp.len);
    int lines = all.len ? count_lines(all.p, all.len, inner_w) : 1;
    sb_free(&all);
    if (lines > 4) lines = 4;
    int box_h = lines * LINE_H + 24, box_y = s->h - 18 - box_h;
    int bottom = box_y - 14;

    /* conversation */
    int shown = 0;
    for (int i = 0; i < h->n; i++) {
        const char *t;
        size_t n;
        trimmed(&h->e[i], &t, &n);
        if (n) shown++;
    }
    int status = h->status.len > 0;
    if (!shown && !status) {
        draw_welcome(h, s, col_x, col_w, bottom);
        h->max_scroll = 0;
    } else {
        h->nchips = 0;
        int total = 0;
        for (int i = 0; i < h->n; i++) total += entry_block(NULL, &h->e[i], col_x, col_w, 0) + GAP;
        if (status) total += LINE_H + GAP;
        h->max_scroll = total - bottom + 16 > 0 ? total - bottom + 16 : 0;
        if (h->scroll > h->max_scroll) h->scroll = h->max_scroll;
        int y = bottom - total + h->scroll;
        if (y > 16) y = 16; /* few messages: start at the top */
        int cy1 = s->cy1;
        if (s->cy1 > bottom) s->cy1 = bottom;
        for (int i = 0; i < h->n; i++) {
            int hgt = entry_block(NULL, &h->e[i], col_x, col_w, y);
            if (y + hgt >= 0 && y < bottom) entry_block(s, &h->e[i], col_x, col_w, y);
            y += hgt + GAP;
        }
        if (status) { /* the agent is working: three dots and the status text */
            for (int d = 0; d < 3; d++) gfx_circle(s, col_x + 6 + d * 9, y + 10, 3, C_ACCENT, 1);
            const char *t = sb_cstr(&h->status);
            size_t n = h->status.len;
            while (n && (*t == '(' || *t == ' ')) t++, n--;
            while (n && (t[n - 1] == ')' || t[n - 1] == ' ')) n--;
            gfx_text(s, col_x + 40, y + 2, t, n, C_MUTED, -1, 1);
        }
        s->cy1 = cy1;
        if (h->scroll) { /* there is newer text below */
            static const char more[] = "v newer messages (scroll down / PageDown)";
            gfx_round(s, (s->w - 360) / 2, bottom - 30, 360, 26, 13, C_TEXT);
            gfx_text(s, (s->w - text_w(more, sizeof more - 1)) / 2, bottom - 25, more, sizeof more - 1,
                     0xFFFFFF, -1, 1);
        }
    }
    draw_input(h, s, col_x, col_w, box_y, box_h, inner_w);
}
