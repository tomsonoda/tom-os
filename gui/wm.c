/* Window manager: overlapping windows, focus, keyboard and mouse handling,
 * and a compositor that redraws only the part of the screen that changed. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

#define MAX_WINDOWS 12
#define TITLE_H 20
#define BORDER 1
#define CELL_W 8
#define CELL_H 16

#define COL_DESKTOP 0x1F4E5F
#define COL_HINT 0x9FC3CF
#define COL_BORDER 0x10181C
#define COL_TITLE_ON 0x2F6DB5
#define COL_TITLE_OFF 0x5A6268
#define COL_TITLE_TEXT 0xFFFFFF
#define COL_CLOSE 0xC8473E

static const u32 palette[16] = {
    0x000000, 0x0000AA, 0x00AA00, 0x00AAAA, 0xAA0000, 0xAA00AA, 0xAA5500, 0xC0C0C0,
    0x555555, 0x5555FF, 0x55FF55, 0x55FFFF, 0xFF5555, 0xFF55FF, 0xFFFF55, 0xFFFFFF};

static const char *const cursor_shape[] = {
    "X           ", "XX          ", "X.X         ", "X..X        ", "X...X       ",
    "X....X      ", "X.....X     ", "X......X    ", "X.......X   ", "X........X  ",
    "X.....XXXXX ", "X..X..X     ", "X.X X..X    ", "XX  X..X    ", "X    X..X   ",
    "     X..X   ", "      XX    "};
#define CURSOR_W 12
#define CURSOR_H 17

static int active;
static surface shadow; /* composed image, copied to the framebuffer */
static window windows[MAX_WINDOWS];
static window *zorder[MAX_WINDOWS]; /* bottom .. top; the top window has focus */
static int nwin;
static int mouse_x, mouse_y, mouse_b;
static window *drag;
static int drag_dx, drag_dy;
static int dx0, dy0, dx1, dy1; /* pending damage */
static u64 last_compose;

int wm_active(void) { return active; }
int wm_screen_w(void) { return shadow.w; }
int wm_screen_h(void) { return shadow.h; }

static void damage(int x, int y, int w, int h) {
    if (w <= 0 || h <= 0) return;
    if (dx1 <= dx0) {
        dx0 = x; dy0 = y; dx1 = x + w; dy1 = y + h;
        return;
    }
    if (x < dx0) dx0 = x;
    if (y < dy0) dy0 = y;
    if (x + w > dx1) dx1 = x + w;
    if (y + h > dy1) dy1 = y + h;
}

static void damage_window(const window *w) {
    damage(w->x - BORDER, w->y - TITLE_H - BORDER, w->w + 2 * BORDER, w->h + TITLE_H + 2 * BORDER);
}

static window *focused(void) { return nwin ? zorder[nwin - 1] : NULL; }

static void raise_window(window *w) {
    int i = 0;
    while (i < nwin && zorder[i] != w) i++;
    if (i == nwin) return;
    window *old = focused();
    for (; i < nwin - 1; i++) zorder[i] = zorder[i + 1];
    zorder[nwin - 1] = w;
    if (old && old != w) {
        memset(old->keys, 0, sizeof old->keys); /* keys do not stay held across a focus change */
        damage_window(old);
        if (!old->canvas) old->tty->term->dirty = ~0u;
    }
    damage_window(w);
    if (!w->canvas) w->tty->term->dirty = ~0u;
}

static u32 *alloc_pixels(int w, int h) { return calloc((size_t)w * (size_t)h, 4); }

window *wm_create(const char *title, tty *t, thread *owner) {
    if (!active || nwin == MAX_WINDOWS) return NULL;
    window *w = NULL;
    for (int i = 0; i < MAX_WINDOWS && !w; i++)
        if (!windows[i].used) w = &windows[i];
    if (!w) return NULL;
    memset(w, 0, sizeof *w);
    w->w = t->term->cols * CELL_W;
    w->h = t->term->rows * CELL_H;
    if (!(w->front = alloc_pixels(w->w, w->h))) return NULL;
    static int cascade;
    w->x = 24 + (cascade % 8) * 36;
    w->y = TITLE_H + 12 + (cascade % 8) * 30;
    cascade++;
    w->used = 1;
    w->tty = t;
    w->owner = owner;
    snprintf(w->title, sizeof w->title, "%s", title);
    t->term->dirty = ~0u;
    window *old = focused();
    zorder[nwin++] = w;
    if (old) { /* the previous top window loses focus */
        memset(old->keys, 0, sizeof old->keys);
        damage_window(old);
        if (!old->canvas) old->tty->term->dirty = ~0u;
    }
    damage_window(w);
    return w;
}

void wm_destroy(window *w) {
    if (!w || !w->used) return;
    damage_window(w);
    int i = 0;
    while (i < nwin && zorder[i] != w) i++;
    for (; i < nwin - 1; i++) zorder[i] = zorder[i + 1];
    nwin--;
    if (drag == w) drag = NULL;
    free(w->front);
    free(w->back);
    w->used = 0;
    window *f = focused();
    if (f) {
        damage_window(f);
        if (!f->canvas) f->tty->term->dirty = ~0u;
    }
}

static int resize(window *w, int width, int height, int canvas) {
    u32 *front = alloc_pixels(width, height);
    u32 *back = canvas ? alloc_pixels(width, height) : NULL;
    if (!front || (canvas && !back)) {
        free(front);
        free(back);
        return -1;
    }
    damage_window(w);
    free(w->front);
    free(w->back);
    w->front = front;
    w->back = back;
    w->w = width;
    w->h = height;
    w->canvas = canvas;
    /* keep the title bar reachable */
    if (w->x + width > shadow.w) w->x = shadow.w - width > 0 ? shadow.w - width : 0;
    if (w->y + height > shadow.h) w->y = shadow.h - height > TITLE_H ? shadow.h - height : TITLE_H + 1;
    damage_window(w);
    return 0;
}

int wm_set_canvas(window *w, int width, int height) { return resize(w, width, height, 1); }

void wm_set_text(window *w) {
    term *t = w->tty->term;
    if (resize(w, t->cols * CELL_W, t->rows * CELL_H, 0) == 0) t->dirty = ~0u;
}

void wm_present(window *w) {
    if (!w->canvas) return;
    memcpy(w->front, w->back, (size_t)w->w * (size_t)w->h * 4);
    damage(w->x, w->y, w->w, w->h);
}

void wm_set_title(window *w, const char *title) {
    snprintf(w->title, sizeof w->title, "%s", title);
    damage_window(w);
}

/* Redraws the rows of a terminal window that changed. */
static void render_term(window *w) {
    term *t = w->tty->term;
    u32 dirty = t->dirty;
    if (!dirty) return;
    t->dirty = 0;
    surface s;
    surface_init(&s, w->front, w->w, w->h);
    int show_cursor = w == focused();
    for (int row = 0; row < t->rows && row < 32; row++) {
        if (!(dirty & (1u << row))) continue;
        for (int col = 0; col < t->cols; col++) {
            u16 cell = t->cells[row * t->cols + col];
            u8 attr = (u8)(cell >> 8);
            gfx_glyph(&s, col * CELL_W, row * CELL_H, (u8)cell, palette[attr & 15],
                      (long)palette[(attr >> 4) & 7], 1);
        }
        if (show_cursor && row == t->cy)
            gfx_fill(&s, t->cx * CELL_W, row * CELL_H + CELL_H - 3, CELL_W, 2, 0xE0E0E0);
        damage(w->x, w->y + row * CELL_H, w->w, CELL_H);
    }
}

static int close_box_x(const window *w) { return w->x + w->w - 18; }

static void draw_window(const window *w) {
    int ox = w->x - BORDER, oy = w->y - TITLE_H - BORDER;
    int ow = w->w + 2 * BORDER, oh = w->h + TITLE_H + 2 * BORDER;
    /* skip windows entirely outside the damaged area */
    if (ox >= shadow.cx1 || oy >= shadow.cy1 || ox + ow <= shadow.cx0 || oy + oh <= shadow.cy0) return;
    gfx_fill(&shadow, ox, oy, ow, TITLE_H + BORDER, COL_BORDER);
    gfx_fill(&shadow, ox, oy, BORDER, oh, COL_BORDER);
    gfx_fill(&shadow, ox + ow - BORDER, oy, BORDER, oh, COL_BORDER);
    gfx_fill(&shadow, ox, oy + oh - BORDER, ow, BORDER, COL_BORDER);
    gfx_fill(&shadow, w->x, w->y - TITLE_H, w->w, TITLE_H - 1,
             w == focused() ? COL_TITLE_ON : COL_TITLE_OFF);
    size_t max_chars = (size_t)(w->w - 30) / CELL_W, len = strlen(w->title);
    gfx_text(&shadow, w->x + 6, w->y - TITLE_H + 2, w->title, len < max_chars ? len : max_chars,
             COL_TITLE_TEXT, -1, 1);
    if (w->owner) {
        gfx_fill(&shadow, close_box_x(w), w->y - TITLE_H + 2, 15, 15, COL_CLOSE);
        gfx_glyph(&shadow, close_box_x(w) + 4, w->y - TITLE_H + 1, 'x', COL_TITLE_TEXT, -1, 1);
    }
    gfx_blit(&shadow, w->x, w->y, w->front, w->w, w->h);
}

static void compose(void) {
    int x0 = dx0 < 0 ? 0 : dx0, y0 = dy0 < 0 ? 0 : dy0;
    int x1 = dx1 > shadow.w ? shadow.w : dx1, y1 = dy1 > shadow.h ? shadow.h : dy1;
    dx0 = dy0 = dx1 = dy1 = 0;
    if (x1 <= x0 || y1 <= y0) return;
    shadow.cx0 = x0; shadow.cy0 = y0; shadow.cx1 = x1; shadow.cy1 = y1;

    gfx_fill(&shadow, 0, 0, shadow.w, shadow.h, COL_DESKTOP);
    static const char hint[] = "tom-os   Alt+Tab: next window   Alt+Q: close   Alt+arrows: move";
    gfx_text(&shadow, 8, shadow.h - CELL_H - 4, hint, sizeof hint - 1, COL_HINT, -1, 1);
    for (int i = 0; i < nwin; i++) draw_window(zorder[i]);
    for (int row = 0; row < CURSOR_H; row++) {
        for (int col = 0; col < CURSOR_W; col++) {
            char c = cursor_shape[row][col];
            if (c != ' ') gfx_pixel(&shadow, mouse_x + col, mouse_y + row, c == 'X' ? 0x000000 : 0xFFFFFF);
        }
    }
    for (int y = y0; y < y1; y++)
        memcpy(fb_screen.px + (size_t)y * shadow.w + x0, shadow.px + (size_t)y * shadow.w + x0,
               (size_t)(x1 - x0) * 4);
}

void wm_tick(void) {
    for (int i = 0; i < nwin; i++)
        if (!zorder[i]->canvas) render_term(zorder[i]);
    u64 now = time_ms();
    if (dx1 > dx0 && now - last_compose >= 15) {
        last_compose = now;
        compose();
    }
}

static void move_window(window *w, int nx, int ny) {
    if (nx < 8 - w->w) nx = 8 - w->w;
    if (nx > shadow.w - 24) nx = shadow.w - 24;
    if (ny < TITLE_H + BORDER) ny = TITLE_H + BORDER;
    if (ny > shadow.h - 4) ny = shadow.h - 4;
    if (nx == w->x && ny == w->y) return;
    damage_window(w);
    w->x = nx;
    w->y = ny;
    damage_window(w);
}

void wm_key(int code, int down, int alt, const char *chars) {
    window *w = focused();
    if (!w) return;
    if (alt && down) {
        switch (code) {
        case 0x0F: /* Tab: bring the bottom window to the top */
            if (nwin > 1) raise_window(zorder[0]);
            return;
        case 0x10: /* Q */
            if (w->owner) thread_kill(w->owner);
            return;
        case 0xCB: move_window(w, w->x - 24, w->y); return;
        case 0xCD: move_window(w, w->x + 24, w->y); return;
        case 0xC8: move_window(w, w->x, w->y - 24); return;
        case 0xD0: move_window(w, w->x, w->y + 24); return;
        }
    }
    w->keys[code & 0xFF] = (u8)down;
    if (down && chars && !alt) tty_input(w->tty, chars);
}

void wm_mouse(int dx, int dy, int buttons) {
    int ox = mouse_x, oy = mouse_y;
    mouse_x += dx;
    mouse_y += dy;
    if (mouse_x < 0) mouse_x = 0;
    if (mouse_y < 0) mouse_y = 0;
    if (mouse_x >= shadow.w) mouse_x = shadow.w - 1;
    if (mouse_y >= shadow.h) mouse_y = shadow.h - 1;
    if (mouse_x != ox || mouse_y != oy) {
        damage(ox, oy, CURSOR_W, CURSOR_H);
        damage(mouse_x, mouse_y, CURSOR_W, CURSOR_H);
    }

    int pressed = (buttons & 1) && !(mouse_b & 1);
    mouse_b = buttons;
    if (!(buttons & 1)) drag = NULL;
    if (drag) move_window(drag, mouse_x - drag_dx, mouse_y - drag_dy);

    if (pressed) {
        for (int i = nwin - 1; i >= 0; i--) { /* topmost window under the pointer */
            window *w = zorder[i];
            if (mouse_x < w->x - BORDER || mouse_x >= w->x + w->w + BORDER ||
                mouse_y < w->y - TITLE_H - BORDER || mouse_y >= w->y + w->h + BORDER)
                continue;
            raise_window(w);
            if (mouse_y < w->y) {
                if (w->owner && mouse_x >= close_box_x(w)) {
                    thread_kill(w->owner);
                } else {
                    drag = w;
                    drag_dx = mouse_x - w->x;
                    drag_dy = mouse_y - w->y;
                }
            }
            break;
        }
    }
    window *f = focused();
    if (f) {
        f->mx = mouse_x - f->x;
        f->my = mouse_y - f->y;
        f->mb = buttons;
    }
}

int wm_init(void) {
    const int W = 1024, H = 768;
    u32 *px = malloc((size_t)W * H * 4);
    if (!px) return -1;
    if (fb_init(W, H) != 0) {
        free(px);
        return -1;
    }
    surface_init(&shadow, px, W, H);
    mouse_x = W / 2;
    mouse_y = H / 2;
    mouse_init();
    active = 1;
    damage(0, 0, W, H);
    return 0;
}
