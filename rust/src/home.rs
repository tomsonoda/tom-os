//! The home screen in full-screen layout: the prompt as a friendly chat.
//!
//! The shell still writes plain text to its terminal (and the serial line);
//! a copy of that stream comes here. Private escape sequences, which
//! terminals ignore, say what the text is (see `rt::ui`): the agent's
//! reply, a note such as a tool step, an error, the waiting status, or text
//! to leave out (the prompt and line editing). Text without a marker is
//! shown as a note. The line being typed is shown in an input box at the
//! bottom.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::ffi::{c_char, c_void};

use crate::gfx::{Surface, font_wide, utf8_decode};
use crate::rt::cstr;
use crate::sys;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Note,
    User,
    Reply,
    Error,
}

const MAX_ENTRIES: usize = 200;
const LINE_H: i32 = 20;
const GAP: i32 = 14;
const MAX_COL: i32 = 760;

// colours
const C_BG: u32 = 0xF2F6F5;
const C_TEXT: u32 = 0x1E2B2E;
const C_MUTED: u32 = 0x7A8B8E;
const C_ACCENT: u32 = 0x0E9F8E;
const C_USER: u32 = 0xD6EFEA;
const C_NOTE_BG: u32 = 0xE6EDEC;
const C_ERROR: u32 = 0xC2413B;
const C_ERROR_BG: u32 = 0xF5E1DC;
const C_BOX: u32 = 0xFFFFFF;
const C_BOX_LINE: u32 = 0xCFDAD8;
const C_COMP: u32 = 0xFFE9A8;
const C_CONV: u32 = 0xBFE8E0;
/// the dot in the logo
const C_SPARK: u32 = 0xFFC53D;
const C_SEND_OFF: u32 = 0xCBD6D4;
const WHITE: u32 = 0xFFFFFF;

const SUGGESTIONS: [&str; 4] =
    ["電卓を作って", "ブロック崩しを作って", "Make a drawing app I can use with the mouse", "今日は何曜日？"];

struct Entry {
    kind: Kind,
    text: Vec<u8>,
}

struct Chip {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    text: &'static str,
}

pub struct HomeView {
    /// the shell's tty: clicks type into it
    tty: *mut c_void,
    entries: VecDeque<Entry>,
    /// the newest entry is still receiving text
    open: bool,
    /// between HIDE and END
    hidden: bool,
    /// between STATUS and END
    status_on: bool,
    status: Vec<u8>,
    /// an escape sequence is being read into `esc_buf`
    in_esc: bool,
    esc_buf: Vec<u8>,
    input: Vec<u8>,
    /// the input method's composition, shown at the cursor
    comp: Vec<u8>,
    /// cursor in `input`
    cursor: usize,
    converted: bool,
    /// pixels scrolled back from the newest text
    scroll: i32,
    max_scroll: i32,
    dirty: bool,
    chips: Vec<Chip>,
    send: (i32, i32, i32), // x, y, radius of the send button
}

fn char_w(cp: u32) -> i32 {
    if font_wide(cp).is_some() { 16 } else { 8 }
}

fn text_w(mut s: &[u8]) -> i32 {
    let mut w = 0;
    while !s.is_empty() {
        let (cp, used) = utf8_decode(s);
        w += char_w(cp);
        s = &s[used..];
    }
    w
}

/// The next line of `s` from `*pos` that fits in `maxw` pixels, breaking at
/// spaces if possible, as (start, length).
fn next_line(s: &[u8], pos: &mut usize, maxw: i32) -> Option<(usize, usize)> {
    if *pos >= s.len() {
        return None;
    }
    let start = *pos;
    let (mut i, mut w) = (start, 0);
    let mut space = None;
    while i < s.len() {
        if s[i] == b'\n' {
            *pos = i + 1;
            return Some((start, i - start));
        }
        let (cp, used) = utf8_decode(&s[i..]);
        let cw = char_w(cp);
        if w + cw > maxw && i > start {
            let end = match space {
                Some(k) => {
                    *pos = k + 1;
                    k
                }
                None => {
                    *pos = i;
                    i
                }
            };
            return Some((start, end - start));
        }
        if cp == ' ' as u32 {
            space = Some(i);
        }
        w += cw;
        i += used;
    }
    *pos = i;
    Some((start, i - start))
}

fn count_lines(s: &[u8], maxw: i32) -> i32 {
    let (mut pos, mut lines) = (0, 0);
    while next_line(s, &mut pos, maxw).is_some() {
        lines += 1;
    }
    lines.max(1)
}

/// Text without leading and trailing blank lines and spaces.
fn trimmed(mut t: &[u8]) -> &[u8] {
    while let [b'\n' | b' ', rest @ ..] = t {
        t = rest;
    }
    while let [rest @ .., b'\n' | b' '] = t {
        t = rest;
    }
    t
}

fn draw_lines(s: &mut Surface, t: &[u8], x: i32, mut y: i32, maxw: i32, color: u32) {
    let mut pos = 0;
    while let Some((start, len)) = next_line(t, &mut pos, maxw) {
        s.text(x, y + 2, &t[start..start + len], color, None, 1);
        y += LINE_H;
    }
}

/// The tom-os mark: a teal rounded square with a "t" and a yellow dot.
fn draw_logo(s: &mut Surface, x: i32, y: i32, size: i32) {
    let big = size >= 32;
    s.round(x, y, size, size, size * 5 / 16, C_ACCENT);
    let (gx, gy) = if big { (9, 17) } else { (5, 9) };
    s.glyph(x + size / 2 - gx, y + size / 2 - gy, b't', WHITE, None, if big { 2 } else { 1 });
    let (r, pad) = if big { (5, 5) } else { (3, 2) };
    s.circle(x + size - r - pad, y + r + pad, r, C_SPARK, true);
}

/// The height of one entry; draws it when given a surface.
fn entry_block(mut s: Option<&mut Surface>, e: &Entry, col_x: i32, col_w: i32, y: i32) -> i32 {
    let t = trimmed(&e.text);
    if t.is_empty() {
        return -GAP; // nothing to show
    }
    match e.kind {
        Kind::User => {
            let maxw = col_w * 3 / 4 - 28;
            let lines = count_lines(t, maxw);
            let bw = (if lines == 1 { text_w(t) } else { maxw }) + 28;
            let bh = lines * LINE_H + 16;
            if let Some(s) = s.as_deref_mut() {
                s.round(col_x + col_w - bw, y, bw, bh, 14, C_USER);
                draw_lines(s, t, col_x + col_w - bw + 14, y + 8, maxw, C_TEXT);
            }
            bh
        }
        Kind::Reply => {
            let maxw = col_w - 40;
            if let Some(s) = s.as_deref_mut() {
                draw_logo(s, col_x, y, 24);
                draw_lines(s, t, col_x + 40, y + 2, maxw, C_TEXT);
            }
            (count_lines(t, maxw) * LINE_H + 4).max(26)
        }
        Kind::Note | Kind::Error => {
            // quieter, in a soft box
            let error = e.kind == Kind::Error;
            let maxw = col_w - 64;
            let lines = count_lines(t, maxw);
            let bh = lines * LINE_H + 10;
            if let Some(s) = s.as_deref_mut() {
                s.round(col_x + 40, y, col_w - 40, bh, 8, if error { C_ERROR_BG } else { C_NOTE_BG });
                // "[tool] details": the tool name in the accent colour
                let close = if t.len() > 2 && t[0] == b'[' { t[..t.len().min(40)].iter().position(|&b| b == b']') } else { None };
                match close {
                    Some(close) if !error && lines == 1 => {
                        let name = &t[1..close];
                        s.text(col_x + 52, y + 7, name, C_ACCENT, None, 1);
                        let mut rest = &t[close + 1..];
                        while let [b' ', r @ ..] = rest {
                            rest = r;
                        }
                        s.text(col_x + 52 + text_w(name) + 12, y + 7, rest, C_MUTED, None, 1);
                    }
                    _ => draw_lines(s, t, col_x + 52, y + 5, maxw, if error { C_ERROR } else { C_MUTED }),
                }
            }
            bh
        }
    }
}

impl HomeView {
    fn new(tty: *mut c_void) -> HomeView {
        HomeView {
            tty,
            entries: VecDeque::new(),
            open: false,
            hidden: false,
            status_on: false,
            status: Vec::new(),
            in_esc: false,
            esc_buf: Vec::new(),
            input: Vec::new(),
            comp: Vec::new(),
            cursor: 0,
            converted: false,
            scroll: 0,
            max_scroll: 0,
            dirty: true,
            chips: Vec::new(),
            send: (0, 0, 0),
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Something outside changed what the view shows (the input method).
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    fn new_entry(&mut self, kind: Kind) -> &mut Vec<u8> {
        if self.entries.len() == MAX_ENTRIES {
            self.entries.pop_front(); // forget the oldest
        }
        self.entries.push_back(Entry { kind, text: Vec::new() });
        self.open = true;
        self.scroll = 0; // new text: back to the bottom
        &mut self.entries.back_mut().unwrap().text
    }

    fn marker(&mut self, code: u32, set: bool) {
        if !set {
            // END closes whatever was open
            if self.hidden {
                self.hidden = false;
            } else if self.status_on {
                self.status_on = false;
            } else {
                self.open = false;
            }
            return;
        }
        match code {
            7101 => _ = self.new_entry(Kind::Reply),
            7102 => _ = self.new_entry(Kind::Note),
            7103 => _ = self.new_entry(Kind::Error),
            7104 => {
                self.status_on = true;
                self.status.clear();
            }
            7105 => self.hidden = true,
            _ => {}
        }
    }

    /// Takes a piece of the shell's output.
    fn feed(&mut self, bytes: &[u8]) {
        for &c in bytes {
            if self.in_esc {
                if self.esc_buf.len() < 23 {
                    self.esc_buf.push(c);
                }
                let n = self.esc_buf.len();
                if ((b'@'..=b'~').contains(&c) && n > 1) || (n == 1 && c != b'[') {
                    if let [b'[', b'?', digits @ .., b'h' | b'l'] = self.esc_buf.as_slice() {
                        let code = core::str::from_utf8(digits).ok().and_then(|d| d.parse().ok()).unwrap_or(0);
                        self.marker(code, c == b'h');
                    }
                    self.in_esc = false;
                }
                continue;
            }
            if c == 0x1B {
                self.in_esc = true;
                self.esc_buf.clear();
                continue;
            }
            if matches!(c, b'\r' | 8 | 7) || self.hidden {
                continue;
            }
            if self.status_on {
                self.status.push(c);
            } else if self.open {
                self.entries.back_mut().unwrap().text.push(c);
            } else if c != b'\n' {
                self.new_entry(Kind::Note).push(c); // no empty notes
            }
        }
        self.dirty = true;
    }

    fn add_user(&mut self, line: &[u8]) {
        self.new_entry(Kind::User).extend_from_slice(line);
        self.open = false;
        self.dirty = true;
    }

    fn set_input(&mut self, line: &[u8], cursor: usize, comp: &[u8], converted: bool) {
        let cursor = cursor.min(line.len());
        if self.input == line && self.comp == comp && self.converted == converted && self.cursor == cursor {
            return;
        }
        self.input = line.to_vec();
        self.comp = comp.to_vec();
        self.converted = converted;
        self.cursor = cursor;
        self.dirty = true;
    }

    pub fn scroll_by(&mut self, dy: i32) {
        self.scroll = self.scroll.saturating_add(dy).clamp(0, self.max_scroll);
        self.dirty = true;
    }

    fn type_text(&self, text: &[u8]) {
        let t = cstr(text);
        unsafe { sys::tty_input(self.tty, t.as_ptr() as *const c_char) };
    }

    pub fn click(&mut self, x: i32, y: i32) {
        if let Some(c) = self.chips.iter().find(|c| x >= c.x && x < c.x + c.w && y >= c.y && y < c.y + c.h) {
            self.type_text(c.text.as_bytes());
            self.type_text(b"\n");
            return;
        }
        let (sx, sy, r) = self.send;
        let (dx, dy) = (x - sx, y - sy);
        if !self.input.is_empty() && dx * dx + dy * dy <= r * r {
            self.type_text(b"\n");
        }
    }

    fn draw_welcome(&mut self, s: &mut Surface, col_x: i32, col_w: i32, bottom: i32) {
        let hello = "こんにちは".as_bytes();
        let sub = b"What would you like to make? Ask in any language.";
        let y = (bottom / 2 - 70).max(20);
        draw_logo(s, s.w / 2 - 20, y - 52, 40);
        s.text((s.w - text_w(hello) * 2) / 2, y, hello, C_TEXT, None, 2);
        s.text((s.w - text_w(sub)) / 2, y + 44, sub, C_MUTED, None, 1);
        // suggestion chips, centred, in rows
        let widths = SUGGESTIONS.map(|t| text_w(t.as_bytes()) + 28);
        let (mut row_y, mut x, mut row_start) = (y + 84, 0, 0);
        for i in 0..=SUGGESTIONS.len() {
            if i == SUGGESTIONS.len() || (x + widths[i] > col_w && i > row_start) {
                let mut cx = col_x + (col_w - (x - 10)) / 2;
                for j in row_start..i {
                    let c = Chip { x: cx, y: row_y, w: widths[j], h: 32, text: SUGGESTIONS[j] };
                    s.round(c.x, c.y, c.w, c.h, 16, C_BOX_LINE);
                    s.round(c.x + 1, c.y + 1, c.w - 2, c.h - 2, 15, C_BOX);
                    s.text(c.x + 14, c.y + 8, c.text.as_bytes(), C_TEXT, None, 1);
                    cx += c.w + 10;
                    self.chips.push(c);
                }
                row_y += 42;
                x = 0;
                row_start = i;
            }
            if i < SUGGESTIONS.len() {
                x += widths[i] + 10;
            }
        }
    }

    fn draw_input(&mut self, s: &mut Surface, col_x: i32, col_w: i32, box_y: i32, box_h: i32, inner_w: i32) {
        s.round(col_x, box_y, col_w, box_h, 16, C_BOX_LINE);
        s.round(col_x + 1, box_y + 1, col_w - 2, box_h - 2, 15, C_BOX);
        let (tx, ty) = (col_x + 18, box_y + 12);
        if self.input.is_empty() && self.comp.is_empty() {
            let placeholder = "Ask tom-os anything...   (Option/Alt+Space: 日本語)";
            s.text(tx, ty + 2, placeholder.as_bytes(), C_MUTED, None, 1);
            s.fill(tx, ty, 2, 18, C_ACCENT);
        } else {
            // the line with the composition (highlighted) and the caret at the cursor
            let mut all = self.input[..self.cursor].to_vec();
            let comp_from = all.len();
            all.extend_from_slice(&self.comp);
            let caret = all.len();
            all.extend_from_slice(&self.input[self.cursor..]);
            let (mut pos, mut y) = (0, ty);
            let (mut cx, mut cy) = (tx, ty);
            while let Some((start, len)) = next_line(&all, &mut pos, inner_w) {
                let end = start + len;
                let (mut x, mut i) = (tx, start);
                while i < end {
                    let (cp, used) = utf8_decode(&all[i..end]);
                    let w = char_w(cp);
                    if i == caret {
                        (cx, cy) = (x, y);
                    }
                    if i >= comp_from && i < caret {
                        s.fill(x, y, w, 20, if self.converted { C_CONV } else { C_COMP });
                    }
                    s.text(x, y + 2, &all[i..i + used], C_TEXT, None, 1);
                    x += w;
                    i += used;
                }
                if caret >= end {
                    (cx, cy) = (x, y); // at or after the end of this line
                }
                y += LINE_H;
            }
            s.fill(cx, cy, 2, 18, C_ACCENT);
        }
        // language and send buttons
        let r = 14;
        let (sx, sy) = (col_x + col_w - 12 - r, box_y + box_h - 8 - r);
        self.send = (sx, sy, r);
        s.circle(sx, sy, r, if self.input.is_empty() { C_SEND_OFF } else { C_ACCENT }, true);
        s.text(sx - 8, sy - 8, "↑".as_bytes(), WHITE, None, 1);
        if unsafe { sys::ime_enabled() } != 0 {
            s.text(sx - r - 26, sy - 8, "あ".as_bytes(), C_ACCENT, None, 1);
        } else {
            s.text(sx - r - 22, sy - 8, b"A", C_MUTED, None, 1);
        }
    }

    pub fn render(&mut self, s: &mut Surface) {
        self.dirty = false;
        s.fill(0, 0, s.w, s.h, C_BG);
        let col_w = (s.w - 64).min(MAX_COL);
        let col_x = (s.w - col_w) / 2;

        // input box: grows with the text, up to four lines
        let inner_w = col_w - 36 - 60;
        let mut all = self.input.clone();
        all.extend_from_slice(&self.comp);
        let lines = if all.is_empty() { 1 } else { count_lines(&all, inner_w).min(4) };
        let box_h = lines * LINE_H + 24;
        let box_y = s.h - 18 - box_h;
        let bottom = box_y - 14;

        // conversation
        self.chips.clear();
        let shown = self.entries.iter().any(|e| !trimmed(&e.text).is_empty());
        let status = !self.status.is_empty();
        if !shown && !status {
            self.draw_welcome(s, col_x, col_w, bottom);
            self.max_scroll = 0;
        } else {
            let mut total: i32 = self.entries.iter().map(|e| entry_block(None, e, col_x, col_w, 0) + GAP).sum();
            if status {
                total += LINE_H + GAP;
            }
            self.max_scroll = (total - bottom + 16).max(0);
            self.scroll = self.scroll.min(self.max_scroll);
            // few messages: start at the top
            let mut y = (bottom - total + self.scroll).min(16);
            let clip = s.cy1;
            s.cy1 = s.cy1.min(bottom);
            for e in &self.entries {
                let height = entry_block(None, e, col_x, col_w, y);
                if y + height >= 0 && y < bottom {
                    entry_block(Some(s), e, col_x, col_w, y);
                }
                y += height + GAP;
            }
            if status {
                // the agent is working: three dots and the status text
                for d in 0..3 {
                    s.circle(col_x + 6 + d * 9, y + 10, 3, C_ACCENT, true);
                }
                let mut t = self.status.as_slice();
                while let [b'(' | b' ', rest @ ..] = t {
                    t = rest;
                }
                while let [rest @ .., b')' | b' '] = t {
                    t = rest;
                }
                s.text(col_x + 40, y + 2, t, C_MUTED, None, 1);
            }
            s.cy1 = clip;
            if self.scroll != 0 {
                // there is newer text below
                let more = b"v newer messages (scroll down / PageDown)";
                s.round((s.w - 360) / 2, bottom - 30, 360, 26, 13, C_TEXT);
                s.text((s.w - text_w(more)) / 2, bottom - 25, more, WHITE, None, 1);
            }
        }
        self.draw_input(s, col_x, col_w, box_y, box_h, inner_w);
    }
}

// ------------------------------------------------------------ C interface
// For the console and start-up code, which are still C. `h` is always
// a pointer from `home_create`; views are never freed.

#[unsafe(no_mangle)]
pub extern "C" fn home_create(tty: *mut c_void) -> *mut HomeView {
    Box::into_raw(Box::new(HomeView::new(tty)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn home_feed(h: *mut HomeView, s: *const u8, n: usize) {
    unsafe { &mut *h }.feed(unsafe { core::slice::from_raw_parts(s, n) })
}

/// The line being typed: text, cursor (byte offset), and the composition
/// at the cursor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn home_set_input(
    h: *mut HomeView, line: *const u8, n: usize, cursor: usize, comp: *const u8, comp_len: usize, converted: i32,
) {
    let bytes = |p: *const u8, n: usize| if n == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(p, n) } };
    unsafe { &mut *h }.set_input(bytes(line, n), cursor, bytes(comp, comp_len), converted != 0)
}

/// Shows a line the user sent, on the calling thread's home view if it has
/// one.
pub fn add_user_line(line: &[u8]) {
    let h = unsafe { sys::thread_home() } as *mut HomeView;
    if !h.is_null() {
        // SAFETY: a view from home_create, used by one thread at a time
        unsafe { &mut *h }.add_user(line);
    }
}
