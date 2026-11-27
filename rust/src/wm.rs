//! Window manager and compositor.
//!
//! Two layouts:
//! - Full screen (default), like a phone: the home screen is the shell (the
//!   prompt). Apps open on top of it, one at a time, filling the screen.
//!   Click or drag up the bar at the bottom to go home; drag it sideways to
//!   switch between running apps; the card view shows all running apps.
//! - Windows: overlapping windows with title bars, dragged with the mouse.
//!
//! The compositor draws into a shadow buffer at 1/scale of the display size
//! and redraws only what changed.
//!
//! Windows are named by an id that is never reused. Code outside (the
//! scheduler, apps) only holds ids, so a window that has been closed is
//! simply not found any more.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::console::{self, TERM_CONT, TtyId};
use crate::gfx::{self, Surface, font_wide};
use crate::rt::{KCell, lossy};
use crate::sched::{self, ThreadId};
use crate::{cpu, home};

const MAX_WINDOWS: usize = 12;
const CELL_W: i32 = 8;
const CELL_H: i32 = 16;

// windows layout
const TITLE_H: i32 = 20;
const BORDER: i32 = 1;
// full-screen layout
const STATUS_H: i32 = 24;
const BAR_H: i32 = 28;
const ARROW_W: i32 = 40;

const COL_DESKTOP: u32 = 0x1F4E5F;
const COL_HINT: u32 = 0x9FC3CF;
const COL_BORDER: u32 = 0x10181C;
const COL_TITLE_ON: u32 = 0x2F6DB5;
const COL_TITLE_OFF: u32 = 0x5A6268;
const COL_TITLE_TEXT: u32 = 0xFFFFFF;
const COL_CLOSE: u32 = 0xC8473E;
// full-screen layout: soft, light bars matching the home screen
const COL_BAR: u32 = 0xF2F6F5;
const COL_BAR_LINE: u32 = 0xDCE5E3;
const COL_BAR_TEXT: u32 = 0x1E2B2E;
const COL_BAR_DIM: u32 = 0x8A9A9C;
const COL_PILL: u32 = 0x24383B;
const COL_OVERVIEW: u32 = 0xDDE8E6;
const COL_ACCENT: u32 = 0x0E9F8E;
const COL_APP_BG: u32 = 0x000000;

const PALETTE: [u32; 16] = [
    0x000000, 0x0000AA, 0x00AA00, 0x00AAAA, 0xAA0000, 0xAA00AA, 0xAA5500, 0xC0C0C0, 0x555555, 0x5555FF, 0x55FF55,
    0x55FFFF, 0xFF5555, 0xFF55FF, 0xFFFF55, 0xFFFFFF,
];

const CURSOR: [&[u8]; 17] = [
    b"X           ",
    b"XX          ",
    b"X.X         ",
    b"X..X        ",
    b"X...X       ",
    b"X....X      ",
    b"X.....X     ",
    b"X......X    ",
    b"X.......X   ",
    b"X........X  ",
    b"X.....XXXXX ",
    b"X..X..X     ",
    b"X.X X..X    ",
    b"XX  X..X    ",
    b"X    X..X   ",
    b"     X..X   ",
    b"      XX    ",
];
const CURSOR_W: i32 = 12;
const CURSOR_H: i32 = 17;

const MIN_W: i32 = 800;
const MIN_H: i32 = 600;

pub const LAYOUT_FULL: i32 = 0;
pub const LAYOUT_WINDOWS: i32 = 1;

// scancodes (extended keys have 0x80 added)
const KEY_ESC: i32 = 0x01;
const KEY_TAB: i32 = 0x0F;
const KEY_Q: i32 = 0x10;
const KEY_ENTER: i32 = 0x1C;
const KEY_H: i32 = 0x23;
const KEY_KP_ENTER: i32 = 0x9C;
const KEY_UP: i32 = 0xC8;
const KEY_PGUP: i32 = 0xC9;
const KEY_LEFT: i32 = 0xCB;
const KEY_RIGHT: i32 = 0xCD;
const KEY_DOWN: i32 = 0xD0;
const KEY_PGDN: i32 = 0xD1;

/// Names a window; never 0, never reused.
pub type Id = usize;

fn redraw_term(tty: TtyId) {
    console::with_term(tty, |t| t.dirty = !0);
}

/// Does this tty's output go to the home screen's chat view?
fn has_view(tty: TtyId) -> bool {
    console::has_home(tty)
}

// ----------------------------------------------------------------- state

struct Window {
    id: Id,
    /// content rectangle on screen (windows layout)
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    title: String,
    /// what the compositor shows
    front: Vec<u32>,
    /// the canvas the app draws on (graphics windows)
    back: Vec<u32>,
    /// false: shows the tty's terminal, true: shows the canvas
    canvas: bool,
    tty: TtyId,
    /// the thread stopped when the window is closed; 0 = not closable
    owner: ThreadId,
    /// keys currently held (scancode, +0x80 for extended)
    keys: [bool; 256],
    /// mouse position inside the content, buttons
    mx: i32,
    my: i32,
    mb: i32,
    /// buttons pressed since the app last looked (short clicks)
    mb_latch: i32,
    last_flip: u64,
}

struct Wm {
    active: bool,
    layout: i32,
    /// composed image at 1/scale size, copied to the framebuffer
    shadow: Vec<u32>,
    sw: i32,
    sh: i32,
    scale: i32,
    /// scratch: one scaled-up row
    line: Vec<u32>,
    windows: Vec<Window>,
    /// bottom .. top; the top window has the focus
    zorder: Vec<Id>,
    /// creation order: the left-to-right order of apps
    order: Vec<Id>,
    next_id: Id,
    cascade: i32,
    mouse_x: i32,
    mouse_y: i32,
    mouse_b: i32,
    /// pending damage: x0, y0, x1, y1 (empty if x1 <= x0)
    damage: (i32, i32, i32, i32),
    last_compose: u64,
    last_minute: i64,
    last_home_render: u64,

    // windows layout: dragging a window by its title bar
    drag: Option<Id>,
    drag_dx: i32,
    drag_dy: i32,

    // full-screen layout
    /// the shell; apps open on top of it
    home: Option<Id>,
    /// the home bar is being dragged
    gesture: bool,
    /// where the left button went down
    press_x: i32,
    press_y: i32,
    /// horizontal offset while switching apps
    slide_off: i32,
    anim: bool,
    anim_target: i32,
    /// focused once the animation reaches its target
    anim_next: Option<Id>,
    /// vertical: an app sliding up (opening) or down (to home)
    vanim: bool,
    voff: i32,
    vtarget: i32,
    vtop: Option<Id>,
    vbehind: Option<Id>,
    vnext: Option<Id>,

    // full-screen layout: the card view of running apps
    overview: bool,
    /// index (among apps) of the card in the middle
    ov_sel: i32,
    /// extra horizontal offset while dragging the row
    ov_scroll: i32,
    /// how far the pressed card has been dragged up
    ov_lift: i32,
    /// card under the pointer when the button went down; -2: the home bar
    ov_press_card: i32,
    ov_dragging: bool,
    /// where Esc goes back to
    ov_return: Option<Id>,
    /// the button went down inside the card view
    ov_pressed: bool,
}

/// No function here waits or yields while it holds the state.
static WM: KCell<Wm> = KCell::new(Wm {
    active: false,
    layout: LAYOUT_FULL,
    shadow: Vec::new(),
    sw: 0,
    sh: 0,
    scale: 1,
    line: Vec::new(),
    windows: Vec::new(),
    zorder: Vec::new(),
    order: Vec::new(),
    next_id: 1,
    cascade: 0,
    mouse_x: 0,
    mouse_y: 0,
    mouse_b: 0,
    damage: (0, 0, 0, 0),
    last_compose: 0,
    last_minute: -1,
    last_home_render: 0,
    drag: None,
    drag_dx: 0,
    drag_dy: 0,
    home: None,
    gesture: false,
    press_x: 0,
    press_y: 0,
    slide_off: 0,
    anim: false,
    anim_target: 0,
    anim_next: None,
    vanim: false,
    voff: 0,
    vtarget: 0,
    vtop: None,
    vbehind: None,
    vnext: None,
    overview: false,
    ov_sel: 0,
    ov_scroll: 0,
    ov_lift: 0,
    ov_press_card: -1,
    ov_dragging: false,
    ov_return: None,
    ov_pressed: false,
});

fn now() -> u64 {
    cpu::time_ms()
}

fn kill(owner: ThreadId) {
    if owner != 0 {
        sched::kill(owner);
    }
}

/// Zero-filled pixels, or None if there is not enough memory.
fn alloc_pixels(w: i32, h: i32) -> Option<Vec<u32>> {
    let n = (w.max(0) as usize).checked_mul(h.max(0) as usize)?;
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    v.resize(n, 0);
    Some(v)
}

/// Large screens get everything drawn twice as big, so text stays readable.
pub fn default_scale(w: i32, h: i32) -> i32 {
    if w >= 2048 && h >= 1200 { 2 } else { 1 }
}

fn text_at(s: &mut Surface, x: i32, y: i32, text: &str, color: u32) {
    s.text(x, y, text.as_bytes(), color, None, 1);
}

/// Moves `v` towards `target`; true once it has arrived.
fn approach(v: &mut i32, target: i32) -> bool {
    let diff = target - *v;
    let mut step = diff / 4;
    if step > -12 && step < 12 {
        step = if diff > 0 { 12 } else { -12 };
    }
    if diff == 0 || (diff > 0 && step >= diff) || (diff < 0 && step <= diff) {
        *v = target;
        return true;
    }
    *v += step;
    false
}

/// A window's title cut to what fits in `width` pixels.
fn title_fitting(title: &str, width: i32) -> &[u8] {
    let max = (width.max(0) / CELL_W) as usize;
    let mut end = title.len().min(max);
    while !title.is_char_boundary(end) {
        end -= 1;
    }
    &title.as_bytes()[..end]
}

impl Wm {
    fn win(&self, id: Id) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    fn win_mut(&mut self, id: Id) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    fn focused(&self) -> Option<Id> {
        self.zorder.last().copied()
    }

    fn area_y(&self) -> i32 {
        STATUS_H
    }

    fn area_h(&self) -> i32 {
        self.sh - STATUS_H - BAR_H
    }

    /// The largest canvas an app can use.
    fn canvas_max(&self) -> (i32, i32) {
        if self.layout == LAYOUT_FULL { (self.sw, self.area_h()) } else { (self.sw - 4, self.sh - TITLE_H - 8) }
    }

    fn damage(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if w <= 0 || h <= 0 {
            return;
        }
        let d = &mut self.damage;
        if d.2 <= d.0 {
            *d = (x, y, x + w, y + h);
        } else {
            *d = (d.0.min(x), d.1.min(y), d.2.max(x + w), d.3.max(y + h));
        }
    }

    fn damage_all(&mut self) {
        self.damage(0, 0, self.sw, self.sh);
    }

    /// Running apps (everything but the home screen), in the order they started.
    fn apps(&self) -> Vec<Id> {
        self.order.iter().copied().filter(|&id| Some(id) != self.home).collect()
    }

    fn app_index(&self, id: Id) -> Option<usize> {
        self.apps().iter().position(|&a| a == id)
    }

    fn app_at(&self, i: i32) -> Option<Id> {
        if i < 0 { None } else { self.apps().get(i as usize).copied() }
    }

    fn app_count(&self) -> i32 {
        self.apps().len() as i32
    }

    fn neighbour(&self, id: Id, dir: i32) -> Option<Id> {
        if Some(id) == self.home {
            return None;
        }
        self.app_at(self.app_index(id)? as i32 + dir)
    }

    /// Where a window's content appears on screen, and how much it is
    /// enlarged: (x, y, width, height, factor).
    fn view_rect(&self, w: &Window) -> (i32, i32, i32, i32, i32) {
        if self.layout == LAYOUT_WINDOWS {
            return (w.x, w.y, w.w, w.h, 1);
        }
        let k = (self.sw / w.w.max(1)).min(self.area_h() / w.h.max(1)).max(1);
        let (vw, vh) = (w.w * k, w.h * k);
        ((self.sw - vw) / 2, self.area_y() + (self.area_h() - vh) / 2, vw, vh, k)
    }

    /// The home screen in full-screen layout is the chat view, not the terminal.
    fn home_is_chat(&self, id: Id) -> bool {
        Some(id) == self.home
            && self.layout == LAYOUT_FULL
            && self.win(id).is_some_and(|w| has_view(w.tty))
    }

    fn sliding(&self) -> bool {
        self.gesture || self.anim || self.slide_off != 0 || self.vanim
    }

    /// A part of a window's content changed (window coordinates).
    fn damage_content(&mut self, id: Id, x: i32, y: i32, ww: i32, hh: i32) {
        let Some(w) = self.win(id) else { return };
        if self.layout == LAYOUT_WINDOWS {
            let (wx, wy) = (w.x, w.y);
            self.damage(wx + x, wy + y, ww, hh);
        } else if self.overview || self.sliding() {
            self.damage(0, self.area_y(), self.sw, self.area_h()); // thumbnails or moving pictures
        } else if self.focused() == Some(id) {
            let (vx, vy, _, _, k) = self.view_rect(w);
            self.damage(vx + x * k, vy + y * k, ww * k, hh * k);
        }
    }

    fn damage_window(&mut self, id: Id) {
        if self.layout == LAYOUT_FULL {
            self.damage_all();
        } else if let Some(w) = self.win(id) {
            let (x, y, ww, hh) = (w.x, w.y, w.w, w.h);
            self.damage(x - BORDER, y - TITLE_H - BORDER, ww + 2 * BORDER, hh + TITLE_H + 2 * BORDER);
        }
    }

    /// The window's text (if it shows a terminal) must be drawn again.
    fn redraw_text(&self, id: Id) {
        if let Some(w) = self.win(id).filter(|w| !w.canvas) {
            redraw_term(w.tty);
        }
    }

    fn lose_focus(&mut self, id: Id) {
        if let Some(w) = self.win_mut(id) {
            w.keys = [false; 256]; // keys do not stay held across a focus change
            w.mb = 0;
        }
        self.damage_window(id);
        self.redraw_text(id);
    }

    fn raise(&mut self, id: Id) {
        let Some(i) = self.zorder.iter().position(|&z| z == id) else { return };
        let old = self.focused();
        self.zorder.remove(i);
        self.zorder.push(id);
        if let Some(old) = old.filter(|&o| o != id) {
            self.lose_focus(old);
        }
        self.damage_window(id);
        self.redraw_text(id);
    }

    /// Starts moving the current app sideways from `from` to `to`; at the
    /// end, `next` (if any) becomes the current app.
    fn start_anim(&mut self, from: i32, to: i32, next: Option<Id>) {
        self.slide_off = from;
        self.anim_target = to;
        self.anim_next = next;
        self.anim = true;
        self.damage_all();
    }

    /// Slides `top` vertically from `from` to `to` over `behind`; `next`
    /// gets the focus at the end.
    fn start_vanim(&mut self, top: Id, behind: Id, from: i32, to: i32, next: Option<Id>) {
        self.vtop = Some(top);
        self.vbehind = Some(behind);
        self.voff = from;
        self.vtarget = to;
        self.vnext = next;
        self.vanim = true;
        self.damage_all();
    }

    /// A new action finishes any animation still running, so it starts
    /// from the state the user is about to see.
    fn finish_anims(&mut self) {
        if self.anim {
            self.anim = false;
            self.slide_off = 0;
            if let Some(n) = self.anim_next.take() {
                self.raise(n);
            }
        }
        if self.vanim {
            self.vanim = false;
            if let Some(n) = self.vnext.take() {
                self.raise(n);
            }
        }
    }

    fn stop_sliding(&mut self) {
        self.gesture = false;
        self.anim = false;
        self.slide_off = 0;
    }

    /// Full-screen layout: show the app to the left (-1) or right (+1).
    fn switch_app(&mut self, dir: i32) {
        self.finish_anims();
        let Some(cur) = self.focused().filter(|&c| Some(c) != self.home) else { return };
        match self.neighbour(cur, dir) {
            Some(n) => self.start_anim(self.slide_off, -dir * self.sw, Some(n)),
            None => self.start_anim(-dir * 40, 0, None), // nothing there: a small bounce
        }
    }

    /// Full-screen layout: the current app slides down, revealing the home screen.
    fn go_home(&mut self) {
        self.finish_anims();
        self.stop_sliding();
        let (Some(f), Some(home)) = (self.focused(), self.home) else { return };
        if f != home {
            self.start_vanim(f, home, 0, self.area_h(), Some(home));
        }
    }

    fn create(&mut self, title: &[u8], tty: TtyId, owner: ThreadId) -> Option<Id> {
        if !self.active || self.windows.len() >= MAX_WINDOWS {
            return None;
        }
        let (w, h) = console::with_term(tty, |t| (t.cols * CELL_W, t.rows * CELL_H))?;
        let front = alloc_pixels(w, h)?;
        redraw_term(tty);
        let id = self.next_id;
        self.next_id += 1;
        let step = self.cascade % 8;
        self.cascade += 1;
        let old = self.focused();
        self.windows.push(Window {
            id,
            x: 24 + step * 36,
            y: TITLE_H + 12 + step * 30,
            w,
            h,
            title: lossy(&title[..title.len().min(47)]),
            front,
            back: Vec::new(),
            canvas: false,
            tty,
            owner,
            keys: [false; 256],
            mx: 0,
            my: 0,
            mb: 0,
            mb_latch: 0,
            last_flip: 0,
        });
        self.zorder.push(id);
        self.order.push(id);
        if let Some(old) = old {
            self.lose_focus(old);
        }
        self.damage_window(id);
        if let Some(old) = old.filter(|_| self.layout == LAYOUT_FULL) {
            self.overview = false;
            self.stop_sliding();
            self.start_vanim(id, old, self.area_h(), 0, None); // opens over what was shown
        }
        Some(id)
    }

    fn destroy(&mut self, id: Id) {
        if self.win(id).is_none() {
            return;
        }
        let was_focused = self.focused() == Some(id);
        self.damage_window(id);
        self.zorder.retain(|&z| z != id);
        self.order.retain(|&z| z != id);
        self.windows.retain(|w| w.id != id);
        let gone = |slot: &mut Option<Id>| {
            if *slot == Some(id) {
                *slot = None;
            }
        };
        gone(&mut self.drag);
        gone(&mut self.anim_next);
        gone(&mut self.ov_return);
        gone(&mut self.home);
        if [self.vtop, self.vbehind, self.vnext].contains(&Some(id)) {
            self.vanim = false;
            (self.vtop, self.vbehind, self.vnext) = (None, None, None);
        }
        if self.layout == LAYOUT_FULL {
            self.stop_sliding();
            let n = self.app_count();
            self.ov_sel = self.ov_sel.min(n - 1);
            if n == 0 {
                self.overview = false;
            }
            if let Some(home) = self.home.filter(|_| was_focused) {
                self.raise(home); // a closed app returns to home
            }
            self.damage_all();
        }
        if let Some(f) = self.focused() {
            self.damage_window(f);
            self.redraw_text(f);
        }
    }

    /// Gives a window new buffers of this size; false if memory is short.
    fn resize(&mut self, id: Id, width: i32, height: i32, canvas: bool) -> bool {
        let Some(front) = alloc_pixels(width, height) else { return false };
        let back = if canvas {
            let Some(b) = alloc_pixels(width, height) else { return false };
            b
        } else {
            Vec::new()
        };
        self.damage_window(id);
        let (sw, sh) = (self.sw, self.sh);
        let Some(w) = self.win_mut(id) else { return false };
        w.front = front;
        w.back = back;
        w.w = width;
        w.h = height;
        w.canvas = canvas;
        // keep the title bar reachable
        if w.x + width > sw {
            w.x = (sw - width).max(0);
        }
        if w.y + height > sh {
            w.y = if sh - height > TITLE_H { sh - height } else { TITLE_H + 1 };
        }
        self.damage_window(id);
        true
    }

    /// Back to showing the terminal.
    fn set_text(&mut self, id: Id) {
        let Some(tty) = self.win(id).map(|w| w.tty) else { return };
        let Some((cols, rows)) = console::with_term(tty, |t| (t.cols, t.rows)) else { return };
        if self.resize(id, cols * CELL_W, rows * CELL_H, false) {
            redraw_term(tty);
        }
    }

    fn set_title(&mut self, id: Id, title: &[u8]) {
        let Some(w) = self.win_mut(id) else { return };
        w.title = lossy(&title[..title.len().min(47)]);
        if self.layout == LAYOUT_FULL {
            self.damage(0, 0, self.sw, STATUS_H);
        } else {
            self.damage_window(id);
        }
    }

    /// Shows what the app has drawn.
    fn present(&mut self, id: Id) {
        let Some(w) = self.win_mut(id).filter(|w| w.canvas) else { return };
        w.front.copy_from_slice(&w.back);
        let (ww, hh) = (w.w, w.h);
        self.damage_content(id, 0, 0, ww, hh);
    }

    /// Redraws the rows of a terminal window that changed.
    fn render_term(&mut self, id: Id) {
        let show_cursor = self.focused() == Some(id);
        let Some(w) = self.win_mut(id) else { return };
        let (ww, hh) = (w.w, w.h);
        let front = &mut w.front;
        let drawn = console::with_term(w.tty, |t| {
            let dirty = core::mem::take(&mut t.dirty);
            let (cols, rows) = (t.cols as usize, t.rows as usize);
            if dirty == 0 || t.cells.len() < cols * rows {
                return (0, 0);
            }
            gfx::draw_on(front, ww, hh, |s| {
                for row in (0..rows.min(64)).filter(|row| dirty & (1 << row) != 0) {
                    let y = row as i32 * CELL_H;
                    for col in 0..cols {
                        let cell = t.cells[row * cols + col];
                        let uc = t.wide.get(row * cols + col).copied().unwrap_or(0);
                        if uc == TERM_CONT {
                            continue; // drawn with the left half
                        }
                        let attr = (cell >> 8) as u8;
                        let fg = PALETTE[(attr & 15) as usize];
                        let bg = Some(PALETTE[((attr >> 4) & 7) as usize]);
                        let x = col as i32 * CELL_W;
                        match font_wide(uc).filter(|_| uc != 0) {
                            Some(g) => s.glyph16(x, y, g, fg, bg, 1),
                            None => s.glyph(x, y, cell as u8, fg, bg, 1),
                        }
                    }
                    if show_cursor && row as i32 == t.cy {
                        s.fill(t.cx * CELL_W, y + CELL_H - 3, CELL_W, 2, 0xE0E0E0);
                    }
                }
            });
            (dirty, rows)
        });
        let Some((dirty, rows)) = drawn else { return };
        for row in (0..rows.min(64)).filter(|row| dirty & (1 << row) != 0) {
            self.damage_content(id, 0, row as i32 * CELL_H, ww, CELL_H);
        }
    }

    fn render_home(&mut self, id: Id) {
        if now() - self.last_home_render < 30 {
            return;
        }
        let Some(w) = self.win_mut(id) else { return };
        let (ww, hh) = (w.w, w.h);
        let front = &mut w.front;
        let drawn = home::with(|view| {
            let dirty = view.is_dirty();
            if dirty {
                gfx::draw_on(front, ww, hh, |s| view.render(s));
            }
            dirty
        });
        if drawn == Some(true) {
            self.last_home_render = now();
            self.damage_content(id, 0, 0, ww, hh);
        }
    }

    // ------------------------------------------------- windows layout

    fn close_box_x(w: &Window) -> i32 {
        w.x + w.w - 18
    }

    fn draw_window(&self, s: &mut Surface, w: &Window) {
        let (ox, oy) = (w.x - BORDER, w.y - TITLE_H - BORDER);
        let (ow, oh) = (w.w + 2 * BORDER, w.h + TITLE_H + 2 * BORDER);
        // skip windows entirely outside the damaged area
        if ox >= s.cx1 || oy >= s.cy1 || ox + ow <= s.cx0 || oy + oh <= s.cy0 {
            return;
        }
        s.fill(ox, oy, ow, TITLE_H + BORDER, COL_BORDER);
        s.fill(ox, oy, BORDER, oh, COL_BORDER);
        s.fill(ox + ow - BORDER, oy, BORDER, oh, COL_BORDER);
        s.fill(ox, oy + oh - BORDER, ow, BORDER, COL_BORDER);
        let bar = if self.focused() == Some(w.id) { COL_TITLE_ON } else { COL_TITLE_OFF };
        s.fill(w.x, w.y - TITLE_H, w.w, TITLE_H - 1, bar);
        s.text(w.x + 6, w.y - TITLE_H + 2, title_fitting(&w.title, w.w - 30), COL_TITLE_TEXT, None, 1);
        if w.owner != 0 {
            s.fill(Self::close_box_x(w), w.y - TITLE_H + 2, 15, 15, COL_CLOSE);
            s.glyph(Self::close_box_x(w) + 4, w.y - TITLE_H + 1, b'x', COL_TITLE_TEXT, None, 1);
        }
        s.blit(w.x, w.y, &w.front, w.w);
    }

    fn compose_windows(&self, s: &mut Surface) {
        s.fill(0, 0, self.sw, self.sh, COL_DESKTOP);
        text_at(
            s,
            8,
            self.sh - CELL_H - 4,
            "tom-os   Alt+Tab: next window   Alt+Q: close   Alt+arrows: move   /layout full: one app at a time",
            COL_HINT,
        );
        for w in self.zorder.iter().filter_map(|&id| self.win(id)) {
            self.draw_window(s, w);
        }
        self.draw_ime(s, self.sw - 32, self.sh - CELL_H - 6);
    }

    // ---------------------------------------------- full-screen layout

    /// Draws an app filling the content area, moved by (sx, sy).
    fn draw_app(&self, s: &mut Surface, id: Id, sx: i32, sy: i32) {
        let Some(w) = self.win(id) else { return };
        let (vx, vy, vw, vh, k) = self.view_rect(w);
        // never paint over the status bar or the home bar
        let (cy0, cy1) = (s.cy0, s.cy1);
        s.cy0 = s.cy0.max(self.area_y());
        s.cy1 = s.cy1.min(self.area_y() + self.area_h());
        let bg = if self.home_is_chat(id) { COL_BAR } else { COL_APP_BG };
        s.fill(sx, self.area_y() + sy, self.sw, self.area_h(), bg);
        if k == 1 {
            s.blit(vx + sx, vy + sy, &w.front, w.w);
        } else {
            s.blit_resize(vx + sx, vy + sy, vw, vh, &w.front, w.w);
        }
        (s.cy0, s.cy1) = (cy0, cy1);
    }

    fn draw_status_bar(&self, s: &mut Surface) {
        let napps = self.app_count();
        s.fill(0, 0, self.sw, STATUS_H, COL_BAR);
        s.fill(0, STATUS_H - 1, self.sw, 1, COL_BAR_LINE);
        let Some(f) = self.focused().and_then(|id| self.win(id)) else { return };
        let at_home = Some(f.id) == self.home;
        let title = if self.overview {
            format!("Running apps ({napps})")
        } else if at_home {
            String::from("tom-os")
        } else {
            format!("{}   {}/{napps}", f.title, self.app_index(f.id).map_or(0, |i| i + 1))
        };
        // as wide as its bytes: titles are mostly ASCII
        text_at(s, (self.sw - title.len() as i32 * CELL_W) / 2, 4, &title, COL_BAR_TEXT);
        if !self.overview && !at_home {
            let color = |there: Option<Id>| if there.is_some() { COL_BAR_TEXT } else { COL_BAR_DIM };
            text_at(s, 14, 4, "<", color(self.neighbour(f.id, -1)));
            text_at(s, self.sw - 22, 4, ">", color(self.neighbour(f.id, 1)));
        } else if !self.overview && napps > 0 {
            text_at(s, 8, 4, &format!("{napps} app{} running", if napps == 1 { "" } else { "s" }), COL_BAR_DIM);
        }
        let t = cpu::unix_time();
        text_at(s, self.sw - ARROW_W - 48, 4, &format!("{:02}:{:02}", t / 3600 % 24, t / 60 % 60), COL_BAR_DIM);
        self.draw_ime(s, self.sw - ARROW_W - 84, 3);
    }

    /// The input method indicator: あ when typing becomes kana, A otherwise.
    fn draw_ime(&self, s: &mut Surface, x: i32, y: i32) {
        let on = console::ime_enabled();
        let text_color = if self.layout == LAYOUT_FULL {
            s.round(x, y, 24, 18, 6, if on { COL_ACCENT } else { COL_BAR_LINE });
            if on { 0xFFFFFF } else { COL_BAR_TEXT }
        } else {
            s.fill(x, y, 24, 18, if on { COL_TITLE_ON } else { COL_BAR });
            COL_TITLE_TEXT
        };
        if on {
            text_at(s, x + 4, y + 1, "あ", text_color);
        } else {
            text_at(s, x + 8, y + 1, "A", text_color);
        }
    }

    fn ime_changed(&mut self) {
        home::with(|view| view.invalidate());
        if self.layout == LAYOUT_FULL {
            self.damage(0, 0, self.sw, STATUS_H);
        } else {
            self.damage(0, self.sh - CELL_H - 8, self.sw, CELL_H + 8);
        }
    }

    fn draw_home_bar(&self, s: &mut Surface) {
        let y = self.sh - BAR_H;
        let f = self.focused();
        let apps = self.apps();
        let n = apps.len() as i32;
        s.fill(0, y, self.sw, BAR_H, COL_BAR);
        s.fill(0, y, self.sw, 1, COL_BAR_LINE);
        // one dot per running app, the current one bright
        let dots_x = self.sw / 2 - n * 6;
        for (i, &a) in apps.iter().enumerate() {
            s.circle(dots_x + i as i32 * 12 + 6, y + 8, 3, if Some(a) == f { COL_PILL } else { COL_BAR_DIM }, true);
        }
        s.fill(self.sw / 2 - 60, y + 17, 120, 5, COL_PILL);
        let hint = if self.overview {
            "click an app to open it - drag it up or click x to close it - Esc: back"
        } else if f != self.home {
            "click or drag up: home - drag sideways: other apps"
        } else if n > 0 {
            "click or drag up: running apps"
        } else {
            ""
        };
        if self.sw >= 1000 {
            text_at(s, 8, y + 6, hint, COL_BAR_DIM);
        }
    }

    /// Cards in the card view: (width, height, gap, top).
    fn card_geometry(&self) -> (i32, i32, i32, i32) {
        let cw = self.sw * 5 / 12;
        let ch = cw * self.area_h() / self.sw.max(1);
        (cw, ch, 28, self.area_y() + (self.area_h() - ch) / 2 + 8)
    }

    fn card_x(&self, i: i32) -> i32 {
        let (cw, _, gap, _) = self.card_geometry();
        self.sw / 2 - cw / 2 + (i - self.ov_sel) * (cw + gap) + self.ov_scroll
    }

    /// The card at a point, or -1.
    fn card_at(&self, x: i32, y: i32) -> i32 {
        let (cw, ch, _, cy) = self.card_geometry();
        (0..self.app_count())
            .find(|&i| {
                let cx = self.card_x(i);
                x >= cx && x < cx + cw && y >= cy - 22 && y < cy + ch
            })
            .unwrap_or(-1)
    }

    fn on_close_box(&self, i: i32, x: i32, y: i32) -> bool {
        let (cw, _, _, cy) = self.card_geometry();
        let cx = self.card_x(i);
        let closable = self.app_at(i).and_then(|id| self.win(id)).is_some_and(|w| w.owner != 0);
        closable && x >= cx + cw - 20 && x < cx + cw && y >= cy - 22 && y < cy - 2
    }

    fn draw_overview(&self, s: &mut Surface) {
        let (cw, ch, _, cy) = self.card_geometry();
        s.fill(0, self.area_y(), self.sw, self.area_h(), COL_OVERVIEW);
        for (i, w) in self.apps().iter().filter_map(|&id| self.win(id)).enumerate() {
            let i = i as i32;
            let cx = self.card_x(i);
            let y = cy + if i == self.ov_press_card && self.ov_lift < 0 { self.ov_lift } else { 0 };
            if cx + cw < 0 || cx > self.sw {
                continue;
            }
            s.text(cx, y - 20, title_fitting(&w.title, cw - 24), COL_BAR_TEXT, None, 1);
            if w.owner != 0 {
                s.fill(cx + cw - 18, y - 20, 16, 16, COL_CLOSE);
                s.glyph(cx + cw - 14, y - 20, b'x', COL_TITLE_TEXT, None, 1);
            }
            let frame = if Some(w.id) == self.ov_return { COL_PILL } else { COL_BAR_DIM };
            s.fill(cx - 2, y - 2, cw + 4, ch + 4, frame);
            s.fill(cx, y, cw, ch, COL_APP_BG);
            // the app as it looks full screen, shrunk into the card
            let (vx, vy, vw, vh, _) = self.view_rect(w);
            let (sw, ah) = (self.sw.max(1), self.area_h().max(1));
            s.blit_resize(cx + vx * cw / sw, y + (vy - self.area_y()) * ch / ah, vw * cw / sw, vh * ch / ah, &w.front, w.w);
        }
    }

    fn compose_full(&self, s: &mut Surface) {
        self.draw_status_bar(s);
        if self.overview {
            self.draw_overview(s);
        } else if self.vanim {
            if let Some(b) = self.vbehind {
                self.draw_app(s, b, 0, 0);
            }
            if let Some(t) = self.vtop {
                self.draw_app(s, t, 0, self.voff);
            }
        } else if let Some(f) = self.focused() {
            let off = self.slide_off;
            self.draw_app(s, f, off, 0);
            let next = match off {
                0 => None,
                o if o < 0 => self.neighbour(f, 1),
                _ => self.neighbour(f, -1),
            };
            if let Some(n) = next {
                self.draw_app(s, n, if off < 0 { off + self.sw } else { off - self.sw }, 0);
            } else if off != 0 {
                s.fill(if off < 0 { self.sw + off } else { 0 }, self.area_y(), off.abs(), self.area_h(), COL_OVERVIEW);
            }
        }
        self.draw_home_bar(s);
    }

    /// Redraws the damaged part of the screen.
    fn compose(&mut self) {
        let (x0, y0) = (self.damage.0.max(0), self.damage.1.max(0));
        let (x1, y1) = (self.damage.2.min(self.sw), self.damage.3.min(self.sh));
        self.damage = (0, 0, 0, 0);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        // the shadow buffer leaves the state while the rest is read to draw on it
        let mut shadow = core::mem::take(&mut self.shadow);
        gfx::draw_on(&mut shadow, self.sw, self.sh, |s| {
            (s.cx0, s.cy0, s.cx1, s.cy1) = (x0, y0, x1, y1);
            if self.layout == LAYOUT_FULL {
                self.compose_full(s);
            } else {
                self.compose_windows(s);
            }
            for (row, line) in CURSOR.iter().enumerate() {
                for (col, &c) in line.iter().enumerate() {
                    if c != b' ' {
                        let color = if c == b'X' { 0x000000 } else { 0xFFFFFF };
                        s.pixel(self.mouse_x + col as i32, self.mouse_y + row as i32, color);
                    }
                }
            }
        });
        let rect = (x0 as usize, y0 as usize, x1 as usize, y1 as usize);
        gfx::show(&shadow, self.sw as usize, rect, self.scale as usize, &mut self.line);
        self.shadow = shadow;
    }

    fn step_anim(&mut self) {
        if !self.anim && !self.vanim {
            return;
        }
        if self.anim && approach(&mut self.slide_off, self.anim_target) {
            self.anim = false;
            self.slide_off = 0;
            if let Some(n) = self.anim_next.take() {
                self.raise(n);
            }
        }
        if self.vanim && approach(&mut self.voff, self.vtarget) {
            self.vanim = false;
            if let Some(n) = self.vnext.take() {
                self.raise(n);
            }
        }
        self.damage_all();
    }

    /// Called from the scheduler's tick: redraws what changed.
    fn tick(&mut self) {
        for id in self.zorder.clone() {
            if self.win(id).is_none_or(|w| w.canvas) {
                continue;
            }
            if self.home_is_chat(id) {
                self.render_home(id);
            } else {
                self.render_term(id);
            }
        }
        let t = now();
        if t - self.last_compose < 15 {
            return;
        }
        if self.layout == LAYOUT_FULL {
            self.step_anim();
            let minute = (cpu::unix_time() / 60) as i64;
            if minute != self.last_minute {
                self.last_minute = minute;
                self.damage(0, 0, self.sw, STATUS_H);
            }
        }
        if self.damage.2 > self.damage.0 {
            self.last_compose = t;
            self.compose();
        }
    }

    fn move_window(&mut self, id: Id, nx: i32, ny: i32) {
        let Some(w) = self.win(id) else { return };
        let nx = nx.max(8 - w.w).min(self.sw - 24);
        let ny = ny.max(TITLE_H + BORDER).min(self.sh - 4);
        if (nx, ny) == (w.x, w.y) {
            return;
        }
        self.damage_window(id);
        if let Some(w) = self.win_mut(id) {
            (w.x, w.y) = (nx, ny);
        }
        self.damage_window(id);
    }

    fn enter_overview(&mut self) {
        self.finish_anims();
        let n = self.app_count();
        if n == 0 {
            return; // nothing is running
        }
        let f = self.focused();
        self.overview = true;
        self.stop_sliding();
        self.vanim = false;
        (self.anim_next, self.vnext) = (None, None);
        self.ov_return = f;
        self.ov_sel = match f {
            Some(f) if Some(f) != self.home => self.app_index(f).map_or(0, |i| i as i32),
            _ => n - 1,
        };
        (self.ov_scroll, self.ov_lift) = (0, 0);
        self.ov_press_card = -1;
        self.ov_dragging = false;
        self.ov_pressed = false; // the release of the click that opened the view is not a click in it
        self.damage_all();
    }

    fn leave_overview(&mut self, open: Option<Id>) {
        self.overview = false;
        (self.ov_scroll, self.ov_lift) = (0, 0);
        self.ov_press_card = -1;
        if let Some(target) = open.or(self.ov_return).or(self.home) {
            self.raise(target);
        }
        self.damage_all();
    }

    /// The home screen's terminal fills the screen in full-screen layout.
    fn fit_home(&mut self) {
        let Some((id, tty, w, h)) = self.home.and_then(|h| self.win(h)).map(|w| (w.id, w.tty, w.w, w.h)) else { return };
        let Some(size) = console::with_term(tty, |t| (t.cols, t.rows)) else { return };
        let full = self.layout == LAYOUT_FULL;
        let (cols, rows) = if full {
            (self.sw / CELL_W, self.area_h() / CELL_H)
        } else {
            ((80 + (self.sw - 1024) / 16).max(80), (30 + (self.sh - 768) / 32).max(30))
        };
        let (cols, rows) = (cols.min(200), rows.min(64));
        let chat = full && has_view(tty);
        let (want_w, want_h) = if chat { (self.sw, self.area_h()) } else { (cols * CELL_W, rows * CELL_H) };
        if (cols, rows, w, h) == (size.0, size.1, want_w, want_h) {
            return;
        }
        console::term_resize(tty, cols, rows);
        if chat {
            self.resize(id, want_w, want_h, false);
        } else {
            self.set_text(id);
        }
        home::with(|view| view.invalidate());
    }

    fn set_layout(&mut self, layout: i32) {
        if !self.active || layout == self.layout {
            return;
        }
        self.layout = layout;
        self.overview = false;
        self.vanim = false;
        self.stop_sliding();
        self.drag = None;
        self.fit_home();
        if let Some(home) = self.home.filter(|_| layout == LAYOUT_WINDOWS).and_then(|h| self.win_mut(h)) {
            (home.x, home.y) = (24, TITLE_H + 12); // full screen moved it to the edge
        }
        for id in self.zorder.clone() {
            self.redraw_text(id);
            if layout == LAYOUT_WINDOWS {
                self.keep_on_screen(id);
            }
        }
        self.damage_all();
    }

    fn keep_on_screen(&mut self, id: Id) {
        if let Some((x, y)) = self.win(id).map(|w| (w.x, w.y)) {
            self.move_window(id, x, y);
        }
    }

    // ------------------------------------------------------------ input

    fn key(&mut self, code: i32, down: bool, alt: bool, chars: Option<&[u8]>) {
        let Some(f) = self.focused() else { return };
        let owner = |wm: &Wm, id: Option<Id>| id.and_then(|id| wm.win(id)).map_or(0, |w| w.owner);
        if self.layout == LAYOUT_FULL && self.overview {
            if !down {
                return;
            }
            let n = self.app_count();
            if code == KEY_ESC || (alt && (code == KEY_UP || code == KEY_DOWN)) {
                self.leave_overview(None);
            } else if alt && code == KEY_H {
                self.leave_overview(self.home);
            } else if code == KEY_LEFT && self.ov_sel > 0 {
                self.ov_sel -= 1;
            } else if code == KEY_RIGHT && self.ov_sel < n - 1 {
                self.ov_sel += 1;
            } else if code == KEY_ENTER || code == KEY_KP_ENTER {
                self.leave_overview(self.app_at(self.ov_sel));
            } else if alt && code == KEY_Q {
                kill(owner(self, self.app_at(self.ov_sel)));
            }
            self.damage_all();
            return;
        }
        if down && self.home_is_chat(f) && (code == KEY_PGUP || code == KEY_PGDN) {
            home::with(|view| view.scroll_by(if code == KEY_PGUP { 300 } else { -300 }));
            return;
        }
        if alt && down && self.layout == LAYOUT_FULL {
            match code {
                KEY_TAB => {
                    // next app, round
                    let first = self.app_at(0);
                    if Some(f) == self.home {
                        if let Some(a) = first {
                            self.raise(a);
                        }
                    } else if self.neighbour(f, 1).is_some() {
                        self.switch_app(1);
                    } else if let Some(a) = first.filter(|_| self.app_count() > 1) {
                        self.raise(a);
                    }
                }
                KEY_Q => kill(owner(self, Some(f))),
                KEY_H | KEY_DOWN => self.go_home(),
                KEY_UP => self.enter_overview(),
                KEY_LEFT => self.switch_app(-1),
                KEY_RIGHT => self.switch_app(1),
                _ => return self.key_to_app(f, code, down, alt, chars),
            }
            return;
        }
        if alt && down && self.layout == LAYOUT_WINDOWS {
            let (x, y) = self.win(f).map_or((0, 0), |w| (w.x, w.y));
            match code {
                KEY_TAB => {
                    // next window
                    if let Some(&bottom) = self.zorder.first().filter(|_| self.zorder.len() > 1) {
                        self.raise(bottom);
                    }
                }
                KEY_Q => kill(owner(self, Some(f))),
                KEY_LEFT => self.move_window(f, x - 24, y),
                KEY_RIGHT => self.move_window(f, x + 24, y),
                KEY_UP => self.move_window(f, x, y - 24),
                KEY_DOWN => self.move_window(f, x, y + 24),
                _ => return self.key_to_app(f, code, down, alt, chars),
            }
            return;
        }
        self.key_to_app(f, code, down, alt, chars);
    }

    fn key_to_app(&mut self, id: Id, code: i32, down: bool, alt: bool, chars: Option<&[u8]>) {
        let Some(w) = self.win_mut(id) else { return };
        w.keys[(code & 0xFF) as usize] = down;
        if let Some(chars) = chars.filter(|_| down && !alt) {
            console::tty_input(w.tty, chars);
        }
    }

    /// Gives the focused app the pointer position in its own coordinates.
    fn feed_app(&mut self, buttons: i32, pressed: i32) {
        let Some(f) = self.focused().and_then(|id| self.win(id)) else { return };
        let (vx, vy, _, _, k) = self.view_rect(f);
        let (id, mx, my) = (f.id, (self.mouse_x - vx) / k, (self.mouse_y - vy) / k);
        if let Some(w) = self.win_mut(id) {
            (w.mx, w.my, w.mb) = (mx, my, buttons);
            w.mb_latch |= pressed;
        }
    }

    fn pointer_windows(&mut self, buttons: i32, pressed: i32) {
        if buttons & 1 == 0 {
            self.drag = None;
        }
        if let Some(d) = self.drag {
            self.move_window(d, self.mouse_x - self.drag_dx, self.mouse_y - self.drag_dy);
        }
        if pressed & 1 != 0 {
            let (mx, my) = (self.mouse_x, self.mouse_y);
            // topmost window under the pointer
            let hit = self.zorder.iter().rev().filter_map(|&id| self.win(id)).find(|w| {
                mx >= w.x - BORDER && mx < w.x + w.w + BORDER && my >= w.y - TITLE_H - BORDER && my < w.y + w.h + BORDER
            });
            if let Some((id, x, y, close_x, owner)) = hit.map(|w| (w.id, w.x, w.y, Self::close_box_x(w), w.owner)) {
                self.raise(id);
                if my < y {
                    if owner != 0 && mx >= close_x {
                        kill(owner);
                    } else {
                        self.drag = Some(id);
                        (self.drag_dx, self.drag_dy) = (mx - x, my - y);
                    }
                    return;
                }
            }
        }
        self.feed_app(buttons, pressed);
    }

    fn pointer_overview(&mut self, buttons: i32, pressed: i32, released: i32) {
        let (cw, ch, gap, _) = self.card_geometry();
        if pressed & 1 != 0 {
            self.ov_pressed = true;
            (self.press_x, self.press_y) = (self.mouse_x, self.mouse_y);
            self.ov_press_card = self.card_at(self.mouse_x, self.mouse_y);
            self.ov_dragging = false;
            self.ov_lift = 0;
            if self.mouse_y >= self.sh - BAR_H {
                self.ov_press_card = -2; // the home bar: go home
            }
            return;
        }
        if buttons & 1 != 0 {
            let (dx, dy) = (self.mouse_x - self.press_x, self.mouse_y - self.press_y);
            if !self.ov_dragging && (dx.abs() > 6 || dy.abs() > 6) {
                self.ov_dragging = true;
            }
            if self.ov_dragging && self.ov_press_card != -2 {
                if self.ov_press_card >= 0 && dy < 0 && -dy > dx.abs() {
                    self.ov_lift = dy;
                } else {
                    self.ov_scroll = dx;
                }
                self.damage(0, self.area_y(), self.sw, self.area_h());
            }
            return;
        }
        if released & 1 == 0 || !self.ov_pressed {
            return;
        }
        self.ov_pressed = false;
        let card = core::mem::replace(&mut self.ov_press_card, -1);
        let app = self.app_at(card);
        let owner = app.and_then(|id| self.win(id)).map_or(0, |w| w.owner);
        if card == -2 {
            self.leave_overview(self.home);
        } else if self.ov_lift < -ch / 3 && owner != 0 {
            kill(owner); // flicked up: close it
        } else if self.ov_dragging {
            let half = (cw + gap) / 2;
            let moved = -(self.ov_scroll + if self.ov_scroll > 0 { half } else { -half }) / (cw + gap);
            self.ov_sel = (self.ov_sel + moved).min(self.app_count() - 1).max(0);
        } else if app.is_some() && self.on_close_box(card, self.mouse_x, self.mouse_y) {
            kill(owner);
        } else {
            self.leave_overview(app);
        }
        (self.ov_scroll, self.ov_lift) = (0, 0);
        self.ov_dragging = false;
        self.damage_all();
    }

    fn pointer_full(&mut self, buttons: i32, pressed: i32, released: i32) {
        if self.overview {
            return self.pointer_overview(buttons, pressed, released);
        }
        let click = pressed & 1 != 0;
        if click {
            self.finish_anims();
        }
        let f = self.focused();
        let at_home = f == self.home;
        let (mx, my) = (self.mouse_x, self.mouse_y);
        if click {
            (self.press_x, self.press_y) = (mx, my);
            if my >= self.sh - BAR_H {
                // home bar
                self.gesture = true;
                self.anim = false;
                return;
            }
            if my < STATUS_H {
                if at_home {
                    self.enter_overview();
                } else if mx < ARROW_W {
                    self.switch_app(-1);
                } else if mx >= self.sw - ARROW_W {
                    self.switch_app(1);
                } else {
                    self.enter_overview();
                }
                return;
            }
        }
        if self.gesture {
            let (dx, up) = (mx - self.press_x, self.press_y - my);
            let towards = |wm: &Wm, dir: i32| f.and_then(|f| wm.neighbour(f, dir));
            if buttons & 1 != 0 {
                if up > 40 && up > dx.abs() {
                    // dragged up
                    self.gesture = false;
                    self.slide_off = 0;
                    if at_home { self.enter_overview() } else { self.go_home() }
                    return;
                }
                if !at_home {
                    // follow the pointer; resist where there is no app
                    let there = towards(self, if dx < 0 { 1 } else { -1 }).is_some();
                    self.slide_off = if there { dx } else { dx / 6 };
                    self.damage_all();
                }
                return;
            }
            self.gesture = false;
            if dx > -5 && dx < 5 && up < 5 {
                // a click
                self.slide_off = 0;
                if at_home { self.enter_overview() } else { self.go_home() }
            } else if at_home {
                self.slide_off = 0;
            } else if (dx < -self.sw / 5 && towards(self, 1).is_some()) || (dx > self.sw / 5 && towards(self, -1).is_some()) {
                let next = towards(self, if dx < 0 { 1 } else { -1 });
                self.start_anim(self.slide_off, if dx < 0 { -self.sw } else { self.sw }, next);
            } else {
                self.start_anim(self.slide_off, 0, None);
            }
            return;
        }
        let in_area = my >= self.area_y() && my < self.area_y() + self.area_h();
        if let Some(tty) = f.filter(|&f| self.home_is_chat(f)).and_then(|f| self.win(f)).map(|w| w.tty) {
            if click && in_area {
                if let Some(typed) = home::with(|view| view.click(mx, my - self.area_y())).flatten() {
                    console::tty_input(tty, &typed);
                }
            }
        } else if in_area {
            self.feed_app(buttons, pressed);
        } else {
            self.feed_app(buttons & !1, 0);
        }
    }

    fn pointer(&mut self, nx: i32, ny: i32, buttons: i32) {
        let (ox, oy) = (self.mouse_x, self.mouse_y);
        self.mouse_x = nx.min(self.sw - 1).max(0);
        self.mouse_y = ny.min(self.sh - 1).max(0);
        if (self.mouse_x, self.mouse_y) != (ox, oy) {
            self.damage(ox, oy, CURSOR_W, CURSOR_H);
            self.damage(self.mouse_x, self.mouse_y, CURSOR_W, CURSOR_H);
        }
        let (pressed, released) = (buttons & !self.mouse_b, self.mouse_b & !buttons);
        self.mouse_b = buttons;
        if self.layout == LAYOUT_FULL {
            self.pointer_full(buttons, pressed, released);
        } else {
            self.pointer_windows(buttons, pressed);
        }
    }

    // ------------------------------------------------------------ modes

    /// Allocates the buffers for a display mode and switches to it.
    fn set_mode(&mut self, w: i32, h: i32, scale: i32) -> Result<(), String> {
        let scale = if scale <= 0 { default_scale(w, h) } else { scale };
        if scale > 4 || w > 8192 || h > 8192 || w <= 0 || h <= 0 {
            return Err(String::from("unsupported size or scale (scale must be 1 to 4)"));
        }
        let (lw, lh) = (w / scale, h / scale);
        if lw < MIN_W || lh < MIN_H {
            return Err(format!("{w}x{h} at scale {scale} leaves only {lw}x{lh}; at least {MIN_W}x{MIN_H} is needed"));
        }
        let mut line = Vec::new();
        let buffers = alloc_pixels(lw, lh).filter(|_| line.try_reserve_exact(w as usize).is_ok());
        let (old_w, old_h) = gfx::screen_size();
        let r = if buffers.is_some() { gfx::fb_init(w, h) } else { -3 };
        let Some(shadow) = buffers.filter(|_| r == 0) else {
            if self.active && old_w != 0 {
                gfx::fb_init(old_w, old_h); // stay on the previous mode
            }
            return Err(match r {
                -2 => format!(
                    "{w}x{h} needs {} MiB of video memory; the display adapter has {} MiB \
                     (start QEMU with -vga none -device VGA,vgamem_mb=64)",
                    (w as usize * h as usize * 4 + (1 << 20) - 1) >> 20,
                    gfx::fb_vram() >> 20
                ),
                -3 => String::from("out of memory"),
                _ => format!("the display adapter does not support {w}x{h}"),
            });
        };
        self.shadow = shadow;
        self.line = line;
        (self.sw, self.sh, self.scale) = (lw, lh, scale);
        Ok(())
    }

    fn set_display(&mut self, w: i32, h: i32, scale: i32) -> Result<(), String> {
        if !self.active {
            return Err(String::from("only available in graphical mode"));
        }
        self.set_mode(w, h, scale)?;
        for id in self.zorder.clone() {
            self.keep_on_screen(id);
        }
        self.mouse_x = self.mouse_x.min(self.sw - 1);
        self.mouse_y = self.mouse_y.min(self.sh - 1);
        self.drag = None;
        self.overview = false;
        self.vanim = false;
        self.stop_sliding();
        self.fit_home();
        self.damage = (0, 0, 0, 0);
        self.damage_all();
        Ok(())
    }
}

// --------------------------------------------------------- for the kernel

pub fn active() -> bool {
    WM.with(|wm| wm.active)
}

pub fn layout() -> i32 {
    WM.with(|wm| wm.layout)
}

pub fn set_layout(layout: i32) {
    WM.with(|wm| wm.set_layout(layout))
}

/// The display mode: width and height in device pixels, and the scale.
pub fn display() -> (i32, i32, i32) {
    let (w, h) = gfx::screen_size();
    (w, h, WM.with(|wm| wm.scale))
}

/// Changes the display mode at run time.
pub fn set_display(w: i32, h: i32, scale: i32) -> Result<(), String> {
    WM.with(|wm| wm.set_display(w, h, scale))
}

/// A window showing the terminal of `tty`. `owner` is the thread that is
/// stopped when the window is closed (0: the window cannot be closed).
pub fn create(title: &[u8], tty: TtyId, owner: ThreadId) -> Option<Id> {
    WM.with(|wm| wm.create(title, tty, owner))
}

pub fn destroy(id: Id) {
    WM.with(|wm| wm.destroy(id))
}

pub fn set_owner(id: Id, owner: ThreadId) {
    WM.with(|wm| {
        if let Some(w) = wm.win_mut(id) {
            w.owner = owner;
        }
    })
}

/// Does the window show a canvas (rather than its terminal)?
pub fn is_canvas(id: Id) -> bool {
    WM.with(|wm| wm.win(id).is_some_and(|w| w.canvas))
}

/// Makes the window show its terminal again.
pub fn set_text(id: Id) {
    WM.with(|wm| wm.set_text(id))
}

// ------------------------------------------------------------ for apps
// What the Lua `gfx` library does to the calling thread's window.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GfxError {
    /// no window: text mode, or not started as an app
    Unavailable,
    /// gfx.mode has not been called
    NoCanvas,
    OutOfMemory,
}

/// The calling thread's window, if it is an app's.
fn app_window(wm: &mut Wm) -> Result<&mut Window, GfxError> {
    let id = sched::window();
    wm.win_mut(id).filter(|w| w.owner != 0).ok_or(GfxError::Unavailable)
}

/// Gives the app a canvas (the whole usable area if `size` is None) and
/// returns its size.
pub fn app_mode(size: Option<(i32, i32)>, title: Option<&[u8]>) -> Result<(i32, i32), GfxError> {
    WM.with(|wm| {
        let id = app_window(wm)?.id;
        let (max_w, max_h) = wm.canvas_max();
        let (w, h) = size.unwrap_or((max_w, max_h));
        let (w, h) = (w.max(64).min(max_w), h.max(32).min(max_h));
        if !wm.resize(id, w, h, true) {
            return Err(GfxError::OutOfMemory);
        }
        if let Some(t) = title {
            wm.set_title(id, t);
        }
        Ok((w, h))
    })
}

/// Draws on the app's canvas.
pub fn app_draw(f: impl FnOnce(&mut Surface)) -> Result<(), GfxError> {
    WM.with(|wm| {
        let w = app_window(wm)?;
        if !w.canvas {
            return Err(GfxError::NoCanvas);
        }
        let (ww, hh) = (w.w, w.h);
        gfx::draw_on(&mut w.back, ww, hh, f);
        Ok(())
    })
}

/// Shows the frame and waits for the next one. `Ok(true)` if the run was
/// stopped while waiting.
pub fn app_flip(fps: f64) -> Result<bool, GfxError> {
    let frame = if fps >= 1.0 { (1000.0 / fps) as u64 } else { 1000 };
    let id = WM.with(|wm| {
        let w = app_window(wm)?;
        if !w.canvas {
            return Err(GfxError::NoCanvas);
        }
        let id = w.id;
        wm.present(id);
        Ok(id)
    })?;
    let last_flip = || WM.with(|wm| wm.win(id).map_or(0, |w| w.last_flip));
    loop {
        if crate::run::stopped().is_some() {
            return Ok(true);
        }
        sched::idle();
        if now() - last_flip() >= frame {
            break;
        }
    }
    WM.with(|wm| {
        if let Some(w) = wm.win_mut(id) {
            w.last_flip = now();
        }
    });
    Ok(false)
}

const KEY_NAMES: [(&str, usize); 48] = [
    ("left", 0xCB), ("right", 0xCD), ("up", 0xC8), ("down", 0xD0), ("space", 0x39), ("enter", 0x1C),
    ("escape", 0x01), ("esc", 0x01), ("tab", 0x0F), ("backspace", 0x0E), ("shift", 0x2A), ("ctrl", 0x1D),
    ("a", 0x1E), ("b", 0x30), ("c", 0x2E), ("d", 0x20), ("e", 0x12), ("f", 0x21), ("g", 0x22), ("h", 0x23),
    ("i", 0x17), ("j", 0x24), ("k", 0x25), ("l", 0x26), ("m", 0x32), ("n", 0x31), ("o", 0x18), ("p", 0x19),
    ("q", 0x10), ("r", 0x13), ("s", 0x1F), ("t", 0x14), ("u", 0x16), ("v", 0x2F), ("w", 0x11), ("x", 0x2D),
    ("y", 0x15), ("z", 0x2C), ("1", 0x02), ("2", 0x03), ("3", 0x04), ("4", 0x05), ("5", 0x06), ("6", 0x07),
    ("7", 0x08), ("8", 0x09), ("9", 0x0A), ("0", 0x0B),
];

/// Is the named key held? `Ok(None)` for an unknown name.
pub fn app_key(name: &[u8]) -> Result<Option<bool>, GfxError> {
    WM.with(|wm| {
        let w = app_window(wm)?;
        let Some(&(_, code)) = KEY_NAMES.iter().find(|k| k.0.as_bytes() == name) else { return Ok(None) };
        let also = match code {
            0x2A => 0x36, // either shift
            0x1D => 0x9D, // either ctrl
            0x1C => 0x9C, // keypad enter
            _ => code,
        };
        Ok(Some(w.keys[code] || w.keys[also]))
    })
}

/// The pointer: x, y, left button, right button. A button counts as down
/// if it is held now or was pressed since the last call, so short clicks
/// are not missed.
pub fn app_mouse() -> Result<(i32, i32, bool, bool), GfxError> {
    WM.with(|wm| {
        let w = app_window(wm)?;
        let b = w.mb | core::mem::take(&mut w.mb_latch);
        Ok((w.mx, w.my, b & 1 != 0, b & 2 != 0))
    })
}

pub fn app_size() -> Result<(i32, i32), GfxError> {
    WM.with(|wm| app_window(wm).map(|w| (w.w, w.h)))
}

// ------------------------------------------------- start-up and input

/// Starts graphics; false if they are not available in this mode.
pub fn init(w: i32, h: i32, scale: i32) -> bool {
    let ok = WM.with(|wm| {
        let ok = wm.set_mode(w, h, scale).is_ok();
        (wm.mouse_x, wm.mouse_y) = (wm.sw / 2, wm.sh / 2);
        ok
    });
    if ok {
        crate::arch::input::init_pointer();
        WM.with(|wm| {
            wm.active = true;
            wm.damage_all();
        });
    }
    ok
}

/// The shell's window: the home screen apps open on top of.
pub fn set_home(id: Id) {
    WM.with(|wm| {
        wm.home = wm.win(id).map(|w| w.id);
        wm.fit_home();
    })
}

/// A key went down or up; `chars` is what it types.
pub fn key(code: i32, down: bool, alt: bool, chars: Option<&[u8]>) {
    WM.with(|wm| wm.key(code, down, alt, chars))
}

/// Relative pointer movement (PS/2 mouse).
pub fn mouse(dx: i32, dy: i32, buttons: i32) {
    WM.with(|wm| wm.pointer(wm.mouse_x.saturating_add(dx), wm.mouse_y.saturating_add(dy), buttons))
}

/// Absolute pointer position, 0..65535 on both axes, and the wheel.
pub fn mouse_abs(x: i32, y: i32, buttons: i32, wheel: i32) {
    WM.with(|wm| {
        wm.pointer((x as i64 * wm.sw as i64 / 65536) as i32, (y as i64 * wm.sh as i64 / 65536) as i32, buttons);
        if wheel != 0 && !wm.overview && wm.focused().is_some_and(|f| wm.home_is_chat(f)) {
            home::with(|view| view.scroll_by(-wheel * 60));
        }
    })
}

/// The input method was switched: redraw its indicator.
pub fn ime_changed() {
    WM.with(Wm::ime_changed)
}

/// Called from the scheduler's tick: redraws what changed.
pub fn tick() {
    if active() {
        WM.try_with(Wm::tick);
    }
}
