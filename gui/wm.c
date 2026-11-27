/* Window manager and compositor.
 *
 * Two layouts:
 * - Full screen (default), like a phone: the home screen is the shell (the
 *   prompt). Apps open on top of it, one at a time, filling the screen.
 *   Click or drag up the bar at the bottom to go home; drag it sideways to
 *   switch between running apps; the card view shows all running apps.
 * - Windows: overlapping windows with title bars, dragged with the mouse.
 *
 * The compositor draws into a shadow buffer at 1/scale of the display size
 * and redraws only what changed. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

#define MAX_WINDOWS 12
#define CELL_W 8
#define CELL_H 16

/* windows layout */
#define TITLE_H 20
#define BORDER 1
/* full-screen layout */
#define STATUS_H 24
#define BAR_H 28
#define ARROW_W 40

#define COL_DESKTOP 0x1F4E5F
#define COL_HINT 0x9FC3CF
#define COL_BORDER 0x10181C
#define COL_TITLE_ON 0x2F6DB5
#define COL_TITLE_OFF 0x5A6268
#define COL_TITLE_TEXT 0xFFFFFF
#define COL_CLOSE 0xC8473E
/* full-screen layout: soft, light bars matching the home screen */
#define COL_BAR 0xF2F6F5
#define COL_BAR_LINE 0xDCE5E3
#define COL_BAR_TEXT 0x1E2B2E
#define COL_BAR_DIM 0x8A9A9C
#define COL_PILL 0x24383B
#define COL_OVERVIEW 0xDDE8E6
#define COL_ACCENT 0x0E9F8E
#define COL_APP_BG 0x000000

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
static int layout = LAYOUT_FULL;
static surface shadow; /* composed image at 1/scale size, copied to the framebuffer */
static int scale = 1;
static u32 *line_buf;  /* one scaled-up row */
static window windows[MAX_WINDOWS];
static window *zorder[MAX_WINDOWS]; /* bottom .. top; the top window has focus */
static window *order[MAX_WINDOWS];  /* creation order: the left-to-right order of apps */
static int nwin;
static int mouse_x, mouse_y, mouse_b;
static int dx0, dy0, dx1, dy1; /* pending damage */
static u64 last_compose;
static int last_minute = -1;

/* windows layout: dragging a window by its title bar */
static window *drag;
static int drag_dx, drag_dy;

/* full-screen layout */
static window *home;         /* the shell; apps open on top of it */
static int gesture;          /* home bar is being dragged */
static int press_x, press_y; /* where the left button went down */
static int slide_off;        /* horizontal offset while switching apps */
static int anim, anim_target;
static window *anim_next;    /* focused once the animation reaches its target */
static int vanim, voff, vtarget; /* vertical: an app sliding up (opening) or down (to home) */
static window *vtop, *vbehind, *vnext;

/* full-screen layout: the card view of running apps */
static int overview;
static int ov_sel;           /* index (among apps) of the card in the middle */
static int ov_scroll;        /* extra horizontal offset while dragging the row */
static int ov_lift;          /* how far the pressed card has been dragged up */
static int ov_press_card;    /* card under the pointer when the button went down */
static int ov_dragging;
static window *ov_return;    /* where Esc goes back to */
static int ov_pressed;       /* the button went down inside the card view */

int wm_active(void) { return active; }
int wm_screen_w(void) { return shadow.w; }
int wm_screen_h(void) { return shadow.h; }
int wm_display_w(void) { return fb_screen.w; }
int wm_display_h(void) { return fb_screen.h; }
int wm_scale(void) { return scale; }
int wm_layout(void) { return layout; }

/* Large screens get everything drawn twice as big, so text stays readable. */
int wm_default_scale(int w, int h) { return w >= 2048 && h >= 1200 ? 2 : 1; }

static int area_y(void) { return STATUS_H; }
static int area_h(void) { return shadow.h - STATUS_H - BAR_H; }

void wm_canvas_max(int *w, int *h) {
    if (layout == LAYOUT_FULL) {
        *w = shadow.w;
        *h = area_h();
    } else {
        *w = shadow.w - 4;
        *h = shadow.h - TITLE_H - 8;
    }
}

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

static void damage_all(void) { damage(0, 0, shadow.w, shadow.h); }

static window *focused(void) { return nwin ? zorder[nwin - 1] : NULL; }

/* Running apps (everything but the home screen), in the order they started. */
static int running_apps(window **out) {
    int n = 0;
    for (int i = 0; i < nwin; i++)
        if (order[i] != home) out[n++] = order[i];
    return n;
}

static int app_index(const window *w) {
    window *a[MAX_WINDOWS];
    int n = running_apps(a);
    for (int i = 0; i < n; i++)
        if (a[i] == w) return i;
    return -1;
}

static window *app_at(int i) {
    window *a[MAX_WINDOWS];
    int n = running_apps(a);
    return i >= 0 && i < n ? a[i] : NULL;
}

static int app_count(void) {
    window *a[MAX_WINDOWS];
    return running_apps(a);
}

static window *neighbour(const window *w, int dir) {
    if (w == home) return NULL;
    return app_at(app_index(w) + dir);
}

/* Where a window's content appears on screen, and how much it is enlarged. */
static void view_rect(const window *w, int *x, int *y, int *vw, int *vh, int *k) {
    if (layout == LAYOUT_WINDOWS) {
        *x = w->x;
        *y = w->y;
        *vw = w->w;
        *vh = w->h;
        *k = 1;
        return;
    }
    int kx = shadow.w / w->w, ky = area_h() / w->h;
    *k = kx < ky ? kx : ky;
    if (*k < 1) *k = 1;
    *vw = w->w * *k;
    *vh = w->h * *k;
    *x = (shadow.w - *vw) / 2;
    *y = area_y() + (area_h() - *vh) / 2;
}

/* The home screen in full-screen layout is the chat view, not the terminal. */
static int home_is_chat(const window *w) { return w && w == home && layout == LAYOUT_FULL && w->tty->home; }

static int sliding(void) { return gesture || anim || slide_off || vanim; }

/* A part of a window's content changed (window coordinates). */
static void damage_content(const window *w, int x, int y, int ww, int hh) {
    if (layout == LAYOUT_WINDOWS) {
        damage(w->x + x, w->y + y, ww, hh);
        return;
    }
    if (overview || sliding()) {
        damage(0, area_y(), shadow.w, area_h()); /* thumbnails or moving pictures */
        return;
    }
    if (w != focused()) return;
    int vx, vy, vw, vh, k;
    view_rect(w, &vx, &vy, &vw, &vh, &k);
    damage(vx + x * k, vy + y * k, ww * k, hh * k);
}

static void damage_window(const window *w) {
    if (layout == LAYOUT_FULL) {
        damage_all();
        return;
    }
    damage(w->x - BORDER, w->y - TITLE_H - BORDER, w->w + 2 * BORDER, w->h + TITLE_H + 2 * BORDER);
}

static void lose_focus(window *w) {
    memset(w->keys, 0, sizeof w->keys); /* keys do not stay held across a focus change */
    w->mb = 0;
    damage_window(w);
    if (!w->canvas) w->tty->term->dirty = ~0ULL;
}

static void raise_window(window *w) {
    int i = 0;
    while (i < nwin && zorder[i] != w) i++;
    if (i == nwin) return;
    window *old = focused();
    for (; i < nwin - 1; i++) zorder[i] = zorder[i + 1];
    zorder[nwin - 1] = w;
    if (old && old != w) lose_focus(old);
    damage_window(w);
    if (!w->canvas) w->tty->term->dirty = ~0ULL;
}

static u32 *alloc_pixels(int w, int h) { return calloc((size_t)w * (size_t)h, 4); }

/* Starts moving the current app sideways from `from` to `to`; at the end,
 * `next` (if any) becomes the current app. */
static void start_anim(int from, int to, window *next) {
    slide_off = from;
    anim_target = to;
    anim_next = next;
    anim = 1;
    damage_all();
}

/* Slides `top` vertically from `from` to `to` over `behind`; `next` gets the
 * focus at the end. */
static void start_vanim(window *top, window *behind, int from, int to, window *next) {
    vtop = top;
    vbehind = behind;
    voff = from;
    vtarget = to;
    vnext = next;
    vanim = 1;
    damage_all();
}

/* A new action finishes any animation still running, so it starts from
 * the state the user is about to see. */
static void finish_anims(void) {
    if (anim) {
        anim = 0;
        slide_off = 0;
        if (anim_next) raise_window(anim_next);
        anim_next = NULL;
    }
    if (vanim) {
        vanim = 0;
        if (vnext) raise_window(vnext);
        vnext = NULL;
    }
}

/* Full-screen layout: show the app to the left (-1) or right (+1). */
static void switch_app(int dir) {
    finish_anims();
    window *cur = focused();
    if (!cur || cur == home) return;
    window *n = neighbour(cur, dir);
    if (n) start_anim(slide_off, -dir * shadow.w, n);
    else start_anim(-dir * 40, 0, NULL); /* nothing there: a small bounce */
}

/* Full-screen layout: the current app slides down, revealing the home screen. */
static void go_home(void) {
    finish_anims();
    window *f = focused();
    gesture = anim = slide_off = 0;
    if (!home || f == home || !f) return;
    start_vanim(f, home, 0, area_h(), home);
}

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
    t->term->dirty = ~0ULL;
    window *old = focused();
    zorder[nwin] = w;
    order[nwin] = w;
    nwin++;
    if (old) lose_focus(old);
    damage_window(w);
    if (layout == LAYOUT_FULL && old) {
        overview = 0;
        gesture = anim = slide_off = 0;
        start_vanim(w, old, area_h(), 0, NULL); /* opens over what was shown */
    }
    return w;
}

void wm_destroy(window *w) {
    if (!w || !w->used) return;
    int was_focused = w == focused();
    damage_window(w);
    int i = 0;
    while (i < nwin && zorder[i] != w) i++;
    for (; i < nwin - 1; i++) zorder[i] = zorder[i + 1];
    i = 0;
    while (i < nwin && order[i] != w) i++;
    for (; i < nwin - 1; i++) order[i] = order[i + 1];
    nwin--;
    if (drag == w) drag = NULL;
    if (anim_next == w) anim_next = NULL;
    if (vtop == w || vbehind == w || vnext == w) vanim = 0;
    if (ov_return == w) ov_return = NULL;
    if (home == w) home = NULL;
    free(w->front);
    free(w->back);
    w->used = 0;
    if (layout == LAYOUT_FULL) {
        gesture = anim = slide_off = 0;
        int n = app_count();
        if (ov_sel >= n) ov_sel = n - 1;
        if (overview && n == 0) overview = 0;
        if (was_focused && home) raise_window(home); /* a closed app returns to home */
        damage_all();
    }
    window *f = focused();
    if (f) {
        damage_window(f);
        if (!f->canvas) f->tty->term->dirty = ~0ULL;
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
    if (resize(w, t->cols * CELL_W, t->rows * CELL_H, 0) == 0) t->dirty = ~0ULL;
}

void wm_present(window *w) {
    if (!w->canvas) return;
    memcpy(w->front, w->back, (size_t)w->w * (size_t)w->h * 4);
    damage_content(w, 0, 0, w->w, w->h);
}

void wm_set_title(window *w, const char *title) {
    snprintf(w->title, sizeof w->title, "%s", title);
    if (layout == LAYOUT_FULL) damage(0, 0, shadow.w, STATUS_H);
    else damage_window(w);
}

/* Redraws the rows of a terminal window that changed. */
static void render_term(window *w) {
    term *t = w->tty->term;
    u64 dirty = t->dirty;
    if (!dirty) return;
    t->dirty = 0;
    surface s;
    surface_init(&s, w->front, w->w, w->h);
    int show_cursor = w == focused();
    for (int row = 0; row < t->rows && row < 64; row++) {
        if (!(dirty & (1ULL << row))) continue;
        for (int col = 0; col < t->cols; col++) {
            u16 cell = t->cells[row * t->cols + col];
            u32 uc = t->uc ? t->uc[row * t->cols + col] : 0;
            u8 attr = (u8)(cell >> 8);
            u32 fg = palette[attr & 15];
            long bg = (long)palette[(attr >> 4) & 7];
            const u8 *g = uc && uc != TERM_CONT ? font_wide(uc) : NULL;
            if (g) gfx_glyph16(&s, col * CELL_W, row * CELL_H, g, fg, bg, 1);
            else if (uc != TERM_CONT) gfx_glyph(&s, col * CELL_W, row * CELL_H, (u8)cell, fg, bg, 1);
        }
        if (show_cursor && row == t->cy)
            gfx_fill(&s, t->cx * CELL_W, row * CELL_H + CELL_H - 3, CELL_W, 2, 0xE0E0E0);
        damage_content(w, 0, row * CELL_H, w->w, CELL_H);
    }
}

static void draw_ime(int x, int y);

static void text_at(int x, int y, const char *s, u32 color) {
    gfx_text(&shadow, x, y, s, strlen(s), color, -1, 1);
}

/* ------------------------------------------------------- windows layout */
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

static void compose_windows(void) {
    gfx_fill(&shadow, 0, 0, shadow.w, shadow.h, COL_DESKTOP);
    text_at(8, shadow.h - CELL_H - 4,
            "tom-os   Alt+Tab: next window   Alt+Q: close   Alt+arrows: move   /layout full: one app at a time",
            COL_HINT);
    for (int i = 0; i < nwin; i++) draw_window(zorder[i]);
    draw_ime(shadow.w - 32, shadow.h - CELL_H - 6);
}

/* --------------------------------------------------- full-screen layout */
/* Draws an app filling the content area, moved by (sx, sy). */
static void draw_app(const window *w, int sx, int sy) {
    int vx, vy, vw, vh, k;
    view_rect(w, &vx, &vy, &vw, &vh, &k);
    /* never paint over the status bar or the home bar */
    int cy0 = shadow.cy0, cy1 = shadow.cy1;
    if (shadow.cy0 < area_y()) shadow.cy0 = area_y();
    if (shadow.cy1 > area_y() + area_h()) shadow.cy1 = area_y() + area_h();
    gfx_fill(&shadow, sx, area_y() + sy, shadow.w, area_h(), home_is_chat(w) ? COL_BAR : COL_APP_BG);
    if (k == 1) gfx_blit(&shadow, vx + sx, vy + sy, w->front, w->w, w->h);
    else gfx_blit_resize(&shadow, vx + sx, vy + sy, vw, vh, w->front, w->w, w->h);
    shadow.cy0 = cy0;
    shadow.cy1 = cy1;
}

static void draw_status_bar(void) {
    window *f = focused();
    char buf[96];
    int napps = app_count();
    gfx_fill(&shadow, 0, 0, shadow.w, STATUS_H, COL_BAR);
    gfx_fill(&shadow, 0, STATUS_H - 1, shadow.w, 1, COL_BAR_LINE);
    if (!f) return;
    if (overview) snprintf(buf, sizeof buf, "Running apps (%d)", napps);
    else if (f == home) snprintf(buf, sizeof buf, "tom-os");
    else snprintf(buf, sizeof buf, "%s   %d/%d", f->title, app_index(f) + 1, napps);
    text_at((shadow.w - (int)strlen(buf) * CELL_W) / 2, 4, buf, COL_BAR_TEXT);
    if (!overview && f != home) {
        text_at(14, 4, "<", neighbour(f, -1) ? COL_BAR_TEXT : COL_BAR_DIM);
        text_at(shadow.w - 22, 4, ">", neighbour(f, +1) ? COL_BAR_TEXT : COL_BAR_DIM);
    } else if (!overview && napps) {
        snprintf(buf, sizeof buf, "%d app%s running", napps, napps == 1 ? "" : "s");
        text_at(8, 4, buf, COL_BAR_DIM);
    }
    u64 t = rtc_unix_time();
    snprintf(buf, sizeof buf, "%02d:%02d", (int)(t / 3600 % 24), (int)(t / 60 % 60));
    text_at(shadow.w - ARROW_W - 48, 4, buf, COL_BAR_DIM);
    draw_ime(shadow.w - ARROW_W - 84, 3);
}

/* The input method indicator: あ when typing becomes kana, A otherwise. */
static void draw_ime(int x, int y) {
    int on = ime_enabled();
    if (layout == LAYOUT_FULL) {
        gfx_round(&shadow, x, y, 24, 18, 6, on ? COL_ACCENT : COL_BAR_LINE);
        const char *l = on ? "あ" : "A";
        gfx_text(&shadow, x + (on ? 4 : 8), y + 1, l, strlen(l), on ? 0xFFFFFF : COL_BAR_TEXT, -1, 1);
        return;
    }
    gfx_fill(&shadow, x, y, 24, 18, on ? COL_TITLE_ON : COL_BAR);
    const char *label = on ? "あ" : "A";
    gfx_text(&shadow, x + (on ? 4 : 8), y + 1, label, strlen(label), COL_TITLE_TEXT, -1, 1);
}

void wm_ime_changed(void) {
    if (home && home->tty->home) home_invalidate(home->tty->home);
    if (layout == LAYOUT_FULL) damage(0, 0, shadow.w, STATUS_H);
    else damage(0, shadow.h - CELL_H - 8, shadow.w, CELL_H + 8);
}

static void draw_home_bar(void) {
    int y = shadow.h - BAR_H;
    window *f = focused();
    window *a[MAX_WINDOWS];
    int n = running_apps(a);
    gfx_fill(&shadow, 0, y, shadow.w, BAR_H, COL_BAR);
    gfx_fill(&shadow, 0, y, shadow.w, 1, COL_BAR_LINE);
    /* one dot per running app, the current one bright */
    int dots_x = shadow.w / 2 - n * 6;
    for (int i = 0; i < n; i++)
        gfx_circle(&shadow, dots_x + i * 12 + 6, y + 8, 3, a[i] == f ? COL_PILL : COL_BAR_DIM, 1);
    gfx_fill(&shadow, shadow.w / 2 - 60, y + 17, 120, 5, COL_PILL);
    const char *hint;
    if (overview) hint = "click an app to open it - drag it up or click x to close it - Esc: back";
    else if (f != home) hint = "click or drag up: home - drag sideways: other apps";
    else if (n) hint = "click or drag up: running apps";
    else hint = "";
    text_at(8, y + 6, shadow.w >= 1000 ? hint : "", COL_BAR_DIM);
}

static void card_geometry(int *cw, int *ch, int *gap, int *cy) {
    *cw = shadow.w * 5 / 12;
    *ch = *cw * area_h() / shadow.w;
    *gap = 28;
    *cy = area_y() + (area_h() - *ch) / 2 + 8;
}

static int card_x(int i) {
    int cw, ch, gap, cy;
    card_geometry(&cw, &ch, &gap, &cy);
    return shadow.w / 2 - cw / 2 + (i - ov_sel) * (cw + gap) + ov_scroll;
}

static int card_at(int x, int y) {
    int cw, ch, gap, cy;
    card_geometry(&cw, &ch, &gap, &cy);
    int n = app_count();
    for (int i = 0; i < n; i++) {
        int cx = card_x(i);
        if (x >= cx && x < cx + cw && y >= cy - 22 && y < cy + ch) return i;
    }
    return -1;
}

static int on_close_box(int i, int x, int y) {
    int cw, ch, gap, cy;
    card_geometry(&cw, &ch, &gap, &cy);
    int cx = card_x(i);
    window *w = app_at(i);
    return w && w->owner && x >= cx + cw - 20 && x < cx + cw && y >= cy - 22 && y < cy - 2;
}

static void draw_overview(void) {
    int cw, ch, gap, cy;
    card_geometry(&cw, &ch, &gap, &cy);
    window *a[MAX_WINDOWS];
    int n = running_apps(a);
    gfx_fill(&shadow, 0, area_y(), shadow.w, area_h(), COL_OVERVIEW);
    for (int i = 0; i < n; i++) {
        window *w = a[i];
        int cx = card_x(i), y = cy + (i == ov_press_card && ov_lift < 0 ? ov_lift : 0);
        if (cx + cw < 0 || cx > shadow.w) continue;
        size_t max_chars = (size_t)(cw - 24) / CELL_W, len = strlen(w->title);
        gfx_text(&shadow, cx, y - 20, w->title, len < max_chars ? len : max_chars, COL_BAR_TEXT, -1, 1);
        if (w->owner) {
            gfx_fill(&shadow, cx + cw - 18, y - 20, 16, 16, COL_CLOSE);
            gfx_glyph(&shadow, cx + cw - 14, y - 20, 'x', COL_TITLE_TEXT, -1, 1);
        }
        gfx_fill(&shadow, cx - 2, y - 2, cw + 4, ch + 4, w == ov_return ? COL_PILL : COL_BAR_DIM);
        gfx_fill(&shadow, cx, y, cw, ch, COL_APP_BG);
        /* the app as it looks full screen, shrunk into the card */
        int vx, vy, vw, vh, k;
        view_rect(w, &vx, &vy, &vw, &vh, &k);
        gfx_blit_resize(&shadow, cx + vx * cw / shadow.w, y + (vy - area_y()) * ch / area_h(),
                        vw * cw / shadow.w, vh * ch / area_h(), w->front, w->w, w->h);
    }
}

static void compose_full(void) {
    draw_status_bar();
    window *f = focused();
    if (overview) {
        draw_overview();
    } else if (vanim) {
        if (vbehind) draw_app(vbehind, 0, 0);
        if (vtop) draw_app(vtop, 0, voff);
    } else if (f) {
        int off = slide_off;
        draw_app(f, off, 0);
        window *n = off < 0 ? neighbour(f, +1) : off > 0 ? neighbour(f, -1) : NULL;
        if (n) draw_app(n, off < 0 ? off + shadow.w : off - shadow.w, 0);
        else if (off) gfx_fill(&shadow, off < 0 ? shadow.w + off : 0, area_y(), off < 0 ? -off : off,
                               area_h(), COL_OVERVIEW);
    }
    draw_home_bar();
}

static void compose(void) {
    int x0 = dx0 < 0 ? 0 : dx0, y0 = dy0 < 0 ? 0 : dy0;
    int x1 = dx1 > shadow.w ? shadow.w : dx1, y1 = dy1 > shadow.h ? shadow.h : dy1;
    dx0 = dy0 = dx1 = dy1 = 0;
    if (x1 <= x0 || y1 <= y0) return;
    shadow.cx0 = x0; shadow.cy0 = y0; shadow.cx1 = x1; shadow.cy1 = y1;

    if (layout == LAYOUT_FULL) compose_full();
    else compose_windows();
    for (int row = 0; row < CURSOR_H; row++) {
        for (int col = 0; col < CURSOR_W; col++) {
            char c = cursor_shape[row][col];
            if (c != ' ') gfx_pixel(&shadow, mouse_x + col, mouse_y + row, c == 'X' ? 0x000000 : 0xFFFFFF);
        }
    }
    if (scale == 1) {
        for (int y = y0; y < y1; y++)
            memcpy(fb_screen.px + (size_t)y * fb_screen.w + x0,
                   shadow.px + (size_t)y * shadow.w + x0, (size_t)(x1 - x0) * 4);
        return;
    }
    /* enlarge: every pixel becomes a scale x scale block */
    size_t n = (size_t)(x1 - x0) * (size_t)scale;
    for (int y = y0; y < y1; y++) {
        const u32 *src = shadow.px + (size_t)y * shadow.w;
        u32 *out = line_buf;
        for (int x = x0; x < x1; x++)
            for (int k = 0; k < scale; k++) *out++ = src[x];
        for (int k = 0; k < scale; k++)
            memcpy(fb_screen.px + (size_t)(y * scale + k) * fb_screen.w + (size_t)x0 * scale,
                   line_buf, n * 4);
    }
}

static int approach(int *v, int target) {
    int diff = target - *v, step = diff / 4;
    if (step > -12 && step < 12) step = diff > 0 ? 12 : -12;
    if (diff == 0 || (diff > 0 && step >= diff) || (diff < 0 && step <= diff)) {
        *v = target;
        return 1; /* arrived */
    }
    *v += step;
    return 0;
}

static void step_anim(void) {
    if (!anim && !vanim) return;
    if (anim && approach(&slide_off, anim_target)) {
        anim = 0;
        slide_off = 0;
        if (anim_next) {
            window *n = anim_next;
            anim_next = NULL;
            raise_window(n);
        }
    }
    if (vanim && approach(&voff, vtarget)) {
        vanim = 0;
        if (vnext) {
            window *n = vnext;
            vnext = NULL;
            raise_window(n);
        }
    }
    damage_all();
}


static void render_home(window *w) {
    static u64 last;
    if (!home_dirty(w->tty->home) || time_ms() - last < 30) return;
    last = time_ms();
    surface s;
    surface_init(&s, w->front, w->w, w->h);
    home_render(w->tty->home, &s);
    damage_content(w, 0, 0, w->w, w->h);
}

void wm_tick(void) {
    for (int i = 0; i < nwin; i++) {
        if (zorder[i]->canvas) continue;
        if (home_is_chat(zorder[i])) render_home(zorder[i]);
        else render_term(zorder[i]);
    }
    u64 now = time_ms();
    if (now - last_compose < 15) return;
    if (layout == LAYOUT_FULL) {
        step_anim();
        int minute = (int)(rtc_unix_time() / 60);
        if (minute != last_minute) {
            last_minute = minute;
            damage(0, 0, shadow.w, STATUS_H);
        }
    }
    if (dx1 > dx0) {
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

static void enter_overview(void) {
    finish_anims();
    window *f = focused();
    int n = app_count();
    if (!n) return; /* nothing is running */
    overview = 1;
    gesture = anim = slide_off = vanim = 0;
    anim_next = vnext = NULL;
    ov_return = f;
    ov_sel = f == home ? n - 1 : app_index(f);
    ov_scroll = ov_lift = 0;
    ov_press_card = -1;
    ov_dragging = 0;
    ov_pressed = 0; /* the release of the click that opened the view is not a click in it */
    damage_all();
}

static void leave_overview(window *open) {
    overview = 0;
    ov_scroll = ov_lift = 0;
    ov_press_card = -1;
    window *target = open ? open : ov_return ? ov_return : home;
    if (target) raise_window(target);
    damage_all();
}

/* The home screen's terminal fills the screen in full-screen layout. */
static void fit_home(void) {
    if (!home) return;
    term *t = home->tty->term;
    int cols, rows;
    if (layout == LAYOUT_FULL) {
        cols = shadow.w / CELL_W;
        rows = area_h() / CELL_H;
    } else {
        cols = 80 + (shadow.w - 1024) / 16;
        rows = 30 + (shadow.h - 768) / 32;
        cols = cols < 80 ? 80 : cols;
        rows = rows < 30 ? 30 : rows;
    }
    if (cols > 200) cols = 200;
    if (rows > 64) rows = 64;
    int want_w = layout == LAYOUT_FULL && home->tty->home ? shadow.w : cols * CELL_W;
    int want_h = layout == LAYOUT_FULL && home->tty->home ? area_h() : rows * CELL_H;
    if (cols == t->cols && rows == t->rows && home->w == want_w && home->h == want_h) return;
    term_resize(t, cols, rows);
    if (want_w == cols * CELL_W && want_h == rows * CELL_H) wm_set_text(home);
    else resize(home, want_w, want_h, 0);
    if (home->tty->home) home_invalidate(home->tty->home);
}

void wm_set_home(window *w) {
    home = w;
    fit_home();
}

void wm_set_layout(int l) {
    if (!active || l == layout) return;
    layout = l;
    overview = gesture = anim = slide_off = vanim = 0;
    drag = NULL;
    fit_home();
    if (home && layout == LAYOUT_WINDOWS) { /* full screen moved it to the edge */
        home->x = 24;
        home->y = TITLE_H + 12;
    }
    for (int i = 0; i < nwin; i++) {
        if (!zorder[i]->canvas) zorder[i]->tty->term->dirty = ~0ULL;
        if (layout == LAYOUT_WINDOWS) move_window(zorder[i], zorder[i]->x, zorder[i]->y);
    }
    damage_all();
}

/* --------------------------------------------------------------- input */
void wm_key(int code, int down, int alt, const char *chars) {
    window *w = focused();
    if (!w) return;
    if (layout == LAYOUT_FULL && overview) {
        if (!down) return;
        int n = app_count();
        if (code == 0x01 || (alt && (code == 0xC8 || code == 0xD0))) leave_overview(NULL); /* Esc */
        else if (alt && code == 0x23) leave_overview(home);                                   /* Alt+H */
        else if (code == 0xCB && ov_sel > 0) ov_sel--;
        else if (code == 0xCD && ov_sel < n - 1) ov_sel++;
        else if (code == 0x1C || code == 0x9C) leave_overview(app_at(ov_sel)); /* Enter */
        else if (alt && code == 0x10 && app_at(ov_sel) && app_at(ov_sel)->owner)
            thread_kill(app_at(ov_sel)->owner);
        damage_all();
        return;
    }
    if (down && home_is_chat(w) && (code == 0xC9 || code == 0xD1)) { /* PageUp / PageDown */
        home_scroll(w->tty->home, code == 0xC9 ? 300 : -300);
        return;
    }
    if (alt && down && layout == LAYOUT_FULL) {
        switch (code) {
        case 0x0F: /* Tab: next app, round */
            if (w == home) { if (app_count()) raise_window(app_at(0)); }
            else if (neighbour(w, +1)) switch_app(+1);
            else if (app_count() > 1) raise_window(app_at(0));
            return;
        case 0x10: if (w->owner) thread_kill(w->owner); return;  /* Q: close */
        case 0x23: go_home(); return;                            /* H: home */
        case 0xD0: go_home(); return;                            /* Down: home */
        case 0xC8: enter_overview(); return;                     /* Up: running apps */
        case 0xCB: switch_app(-1); return;
        case 0xCD: switch_app(+1); return;
        }
    }
    if (alt && down && layout == LAYOUT_WINDOWS) {
        switch (code) {
        case 0x0F: if (nwin > 1) raise_window(zorder[0]); return; /* Tab: next window */
        case 0x10: if (w->owner) thread_kill(w->owner); return;
        case 0xCB: move_window(w, w->x - 24, w->y); return;
        case 0xCD: move_window(w, w->x + 24, w->y); return;
        case 0xC8: move_window(w, w->x, w->y - 24); return;
        case 0xD0: move_window(w, w->x, w->y + 24); return;
        }
    }
    w->keys[code & 0xFF] = (u8)down;
    if (down && chars && !alt) tty_input(w->tty, chars);
}

/* Gives the focused app the pointer position in its own coordinates. */
static void feed_app(int buttons, int pressed) {
    window *f = focused();
    if (!f) return;
    int vx, vy, vw, vh, k;
    view_rect(f, &vx, &vy, &vw, &vh, &k);
    f->mx = (mouse_x - vx) / k;
    f->my = (mouse_y - vy) / k;
    f->mb = buttons;
    f->mb_latch |= pressed;
}

static void pointer_windows(int buttons, int pressed) {
    if (!(buttons & 1)) drag = NULL;
    if (drag) move_window(drag, mouse_x - drag_dx, mouse_y - drag_dy);
    if (pressed & 1) {
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
                return;
            }
            break;
        }
    }
    feed_app(buttons, pressed);
}

static void pointer_overview(int buttons, int pressed, int released) {
    int cw, ch, gap, cy;
    card_geometry(&cw, &ch, &gap, &cy);
    if (pressed & 1) {
        ov_pressed = 1;
        press_x = mouse_x;
        press_y = mouse_y;
        ov_press_card = card_at(mouse_x, mouse_y);
        ov_dragging = 0;
        ov_lift = 0;
        if (mouse_y >= shadow.h - BAR_H) ov_press_card = -2; /* the home bar: go home */
        return;
    }
    if (buttons & 1) {
        int dx = mouse_x - press_x, dy = mouse_y - press_y;
        if (!ov_dragging && (dx > 6 || dx < -6 || dy > 6 || dy < -6)) ov_dragging = 1;
        if (ov_dragging && ov_press_card != -2) {
            if (ov_press_card >= 0 && dy < 0 && -dy > (dx < 0 ? -dx : dx)) ov_lift = dy;
            else ov_scroll = dx;
            damage(0, area_y(), shadow.w, area_h());
        }
        return;
    }
    if (!(released & 1) || !ov_pressed) return;
    ov_pressed = 0;
    int card = ov_press_card;
    ov_press_card = -1;
    window *w = card >= 0 ? app_at(card) : NULL;
    if (card == -2) {
        leave_overview(home);
    } else if (ov_lift < -ch / 3 && w && w->owner) {
        thread_kill(w->owner); /* flicked up: close it */
    } else if (ov_dragging) {
        int moved = -(ov_scroll + (ov_scroll > 0 ? (cw + gap) / 2 : -(cw + gap) / 2)) / (cw + gap);
        int n = app_count();
        ov_sel += moved;
        if (ov_sel > n - 1) ov_sel = n - 1;
        if (ov_sel < 0) ov_sel = 0;
    } else if (w && on_close_box(card, mouse_x, mouse_y)) {
        thread_kill(w->owner);
    } else if (w) {
        leave_overview(w);
    } else {
        leave_overview(NULL);
    }
    ov_scroll = ov_lift = 0;
    ov_dragging = 0;
    damage_all();
}

static void pointer_full(int buttons, int pressed, int released) {
    if (overview) {
        pointer_overview(buttons, pressed, released);
        return;
    }
    if (pressed & 1) finish_anims();
    window *f = focused();
    if (pressed & 1) {
        press_x = mouse_x;
        press_y = mouse_y;
        if (mouse_y >= shadow.h - BAR_H) { /* home bar */
            gesture = 1;
            anim = 0;
            return;
        }
        if (mouse_y < STATUS_H) {
            if (f == home) enter_overview();
            else if (mouse_x < ARROW_W) switch_app(-1);
            else if (mouse_x >= shadow.w - ARROW_W) switch_app(+1);
            else enter_overview();
            return;
        }
    }
    if (gesture) {
        int dx = mouse_x - press_x, up = press_y - mouse_y;
        int at_home = f == home;
        if (buttons & 1) {
            if (up > 40 && up > (dx < 0 ? -dx : dx)) { /* dragged up */
                gesture = 0;
                slide_off = 0;
                if (at_home) enter_overview();
                else go_home();
                return;
            }
            if (!at_home) { /* follow the pointer; resist where there is no app */
                slide_off = (dx < 0 ? neighbour(f, +1) : neighbour(f, -1)) ? dx : dx / 6;
                damage_all();
            }
            return;
        }
        gesture = 0;
        if (dx > -5 && dx < 5 && up < 5) { /* a click */
            slide_off = 0;
            if (at_home) enter_overview();
            else go_home();
        } else if (at_home) {
            slide_off = 0;
        } else if ((dx < -shadow.w / 5 && neighbour(f, +1)) || (dx > shadow.w / 5 && neighbour(f, -1))) {
            start_anim(slide_off, dx < 0 ? -shadow.w : shadow.w, neighbour(f, dx < 0 ? +1 : -1));
        } else {
            start_anim(slide_off, 0, NULL);
        }
        return;
    }
    if (f && home_is_chat(f)) {
        if ((pressed & 1) && mouse_y >= area_y() && mouse_y < area_y() + area_h())
            home_click(f->tty->home, mouse_x, mouse_y - area_y());
        return;
    }
    if (mouse_y >= area_y() && mouse_y < area_y() + area_h()) feed_app(buttons, pressed);
    else feed_app(buttons & ~1, 0);
}

static void pointer(int nx, int ny, int buttons) {
    int ox = mouse_x, oy = mouse_y;
    mouse_x = nx < 0 ? 0 : nx >= shadow.w ? shadow.w - 1 : nx;
    mouse_y = ny < 0 ? 0 : ny >= shadow.h ? shadow.h - 1 : ny;
    if (mouse_x != ox || mouse_y != oy) {
        damage(ox, oy, CURSOR_W, CURSOR_H);
        damage(mouse_x, mouse_y, CURSOR_W, CURSOR_H);
    }
    int pressed = buttons & ~mouse_b, released = mouse_b & ~buttons;
    mouse_b = buttons;
    if (layout == LAYOUT_FULL) pointer_full(buttons, pressed, released);
    else pointer_windows(buttons, pressed);
}

void wm_mouse(int dx, int dy, int buttons) { pointer(mouse_x + dx, mouse_y + dy, buttons); }

void wm_mouse_abs(int x, int y, int buttons, int wheel) {
    pointer((int)((long)x * shadow.w / 65536), (int)((long)y * shadow.h / 65536), buttons);
    window *f = focused();
    if (wheel && f && home_is_chat(f) && !overview) home_scroll(f->tty->home, -wheel * 3 * 20);
}

/* --------------------------------------------------------------- modes */
#define MIN_W 800
#define MIN_H 600

/* Allocates the buffers for a display mode and switches to it. */
static int set_mode(int w, int h, int sc, strbuf *err) {
    if (sc <= 0) sc = wm_default_scale(w, h);
    if (sc > 4 || w > 8192 || h > 8192 || w <= 0 || h <= 0) {
        if (err) sb_puts(err, "unsupported size or scale (scale must be 1 to 4)");
        return -1;
    }
    int lw = w / sc, lh = h / sc;
    if (lw < MIN_W || lh < MIN_H) {
        if (err)
            sb_printf(err, "%dx%d at scale %d leaves only %dx%d; at least %dx%d is needed",
                      w, h, sc, lw, lh, MIN_W, MIN_H);
        return -1;
    }
    u32 *px = malloc((size_t)lw * lh * 4), *line = malloc((size_t)w * 4);
    int old_w = fb_screen.w, old_h = fb_screen.h;
    int r = px && line ? fb_init(w, h) : -3;
    if (r != 0) {
        free(px);
        free(line);
        if (err) {
            if (r == -2)
                sb_printf(err, "%dx%d needs %zu MiB of video memory; the display adapter has %zu MiB "
                          "(start QEMU with -vga none -device VGA,vgamem_mb=64)",
                          w, h, ((size_t)w * h * 4 + (1 << 20) - 1) >> 20, fb_vram() >> 20);
            else if (r == -3) sb_puts(err, "out of memory");
            else sb_printf(err, "the display adapter does not support %dx%d", w, h);
        }
        if (active && old_w) fb_init(old_w, old_h); /* stay on the previous mode */
        return -1;
    }
    free(shadow.px);
    free(line_buf);
    surface_init(&shadow, px, lw, lh);
    line_buf = line;
    scale = sc;
    return 0;
}

int wm_set_display(int w, int h, int sc, strbuf *err) {
    if (!active) {
        sb_puts(err, "only available in graphical mode");
        return -1;
    }
    if (set_mode(w, h, sc, err) != 0) return -1;
    for (int i = 0; i < nwin; i++) move_window(zorder[i], zorder[i]->x, zorder[i]->y);
    if (mouse_x >= shadow.w) mouse_x = shadow.w - 1;
    if (mouse_y >= shadow.h) mouse_y = shadow.h - 1;
    drag = NULL;
    overview = gesture = anim = slide_off = vanim = 0;
    fit_home();
    dx0 = dy0 = dx1 = dy1 = 0;
    damage_all();
    return 0;
}

int wm_init(int w, int h, int sc) {
    if (set_mode(w, h, sc, NULL) != 0) return -1;
    mouse_x = shadow.w / 2;
    mouse_y = shadow.h / 2;
    mouse_init();
    active = 1;
    damage_all();
    return 0;
}
