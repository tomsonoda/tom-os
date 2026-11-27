//! Terminals and console I/O.
//!
//! Output: each thread writes to its tty, which feeds a terminal (the VGA
//! text screen or a window's cell buffer) and optionally COM1.
//! Input: COM1 always types into the shell's tty; the PS/2 keyboard types
//! into the shell (text mode) or the focused window (graphics mode).
//!
//! Ttys are named by ids that are never reused; the scheduler keeps the id
//! of each thread's tty.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::asm;
use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::gfx::{font_wide, utf8_decode};
use crate::pci::{inb, outb};
use crate::rt::KCell;
use crate::{home, sys, wm};

const COM1: u16 = 0x3F8;

pub const KEY_TIMEOUT: i32 = -1;
pub const KEY_BREAK: i32 = -2;

/// The right half of a wide character in `Term::wide`.
pub const TERM_CONT: u32 = 0xFFFF_FFFF;

/// Names a tty; never 0, never reused.
pub type TtyId = usize;

/// The shell's tty: the serial line and the first screen.
pub const SHELL_TTY: TtyId = 1;

// ---------------------------------------------------------------- serial

// SAFETY (all port I/O in this file): the ports of the PC's serial port,
// keyboard controller and VGA adapter, which belong to the console.

fn serial_init() {
    unsafe {
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x80);
        outb(COM1, 0x01); // 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8N1
        outb(COM1 + 2, 0xC7);
        outb(COM1 + 4, 0x0B); // DTR, RTS, OUT2
        outb(COM1 + 1, 0x01); // RX interrupt: wakes HLT
    }
}

fn serial_putc(c: u8) {
    unsafe {
        let mut spins = 0;
        while spins < 100_000 && inb(COM1 + 5) & 0x20 == 0 {
            spins += 1;
        }
        outb(COM1, c);
    }
}

fn serial_write(s: &[u8]) {
    for &c in s {
        if c == b'\n' {
            serial_putc(b'\r');
        }
        serial_putc(c);
    }
}

// -------------------------------------------------------------- terminal

/// A character-cell terminal. Cells use the VGA text layout
/// (attribute << 8 | character), so the hardware text screen and terminal
/// windows share one implementation.
pub struct Term {
    pub cols: i32,
    pub rows: i32,
    pub cells: Vec<u16>,
    /// per cell: the code point if it is not ASCII, TERM_CONT for the right
    /// half of a wide character, 0 otherwise. Empty on the VGA text screen,
    /// which cannot show such characters.
    pub wide: Vec<u32>,
    pub cx: i32,
    pub cy: i32,
    attr: u8,
    esc_state: u8,
    esc_args: [i32; 4],
    esc_n: usize,
    esc_private: bool,
    /// one bit per row that needs redrawing (up to 64 rows)
    pub dirty: u64,
    utf_cp: u32,
    /// UTF-8 continuation bytes still expected
    utf_need: u8,
    /// backed by the VGA text screen
    hw: bool,
}

impl Term {
    fn new(cols: i32, rows: i32, hw: bool) -> Option<Term> {
        let n = (cols.max(1) as usize).checked_mul(rows.max(1) as usize)?;
        let mut cells = Vec::new();
        let mut wide = Vec::new();
        cells.try_reserve_exact(n).ok()?;
        cells.resize(n, 0x0700 | b' ' as u16);
        if !hw {
            wide.try_reserve_exact(n).ok()?;
            wide.resize(n, 0);
        }
        Some(Term {
            cols: cols.max(1),
            rows: rows.max(1),
            cells,
            wide,
            cx: 0,
            cy: 0,
            attr: 0x07,
            esc_state: 0,
            esc_args: [0; 4],
            esc_n: 0,
            esc_private: false,
            dirty: !0,
            utf_cp: 0,
            utf_need: 0,
            hw,
        })
    }

    fn touch(&mut self) {
        if (0..64).contains(&self.cy) {
            self.dirty |= 1 << self.cy;
        }
    }

    fn clear_range(&mut self, from: i32, to: i32) {
        let n = self.cells.len();
        let (from, to) = ((from.max(0) as usize).min(n), (to.max(0) as usize).min(n));
        if from < to {
            self.cells[from..to].fill((self.attr as u16) << 8 | b' ' as u16);
            if !self.wide.is_empty() {
                self.wide[from..to].fill(0);
            }
        }
        self.dirty = !0;
    }

    fn newline(&mut self) {
        self.cx = 0;
        self.cy += 1;
        if self.cy < self.rows {
            return;
        }
        let cols = self.cols as usize;
        self.cells.copy_within(cols.., 0);
        if !self.wide.is_empty() {
            self.wide.copy_within(cols.., 0);
        }
        self.clear_range((self.rows - 1) * self.cols, self.rows * self.cols);
        self.cy = self.rows - 1;
    }

    fn sgr(&mut self, code: i32) {
        const MAP: [u8; 8] = [0, 4, 2, 6, 1, 5, 3, 7];
        let a = self.attr;
        self.attr = match code {
            0 => 0x07,
            1 => a | 0x08,
            22 => a & !0x08,
            7 => a >> 4 | a << 4,
            30..=37 => (a & 0xF8) | MAP[(code - 30) as usize],
            39 => (a & 0xF8) | 7,
            40..=47 => (a & 0x8F) | MAP[(code - 40) as usize] << 4,
            49 => a & 0x0F,
            90..=97 => (a & 0xF0) | MAP[(code - 90) as usize] | 8,
            _ => a,
        };
    }

    fn csi(&mut self, last: u8) {
        if self.esc_private {
            return;
        }
        let (a, b) = (self.esc_args[0], self.esc_args[1]);
        let (w, h) = (self.cols, self.rows);
        let n = a.clamp(1, 10_000);
        let at = self.cy * w + self.cx;
        match last {
            b'H' | b'f' => {
                self.cy = (n - 1).clamp(0, h - 1);
                self.cx = (b.clamp(1, 10_000) - 1).clamp(0, w - 1);
            }
            b'A' => self.cy = (self.cy - n).clamp(0, h - 1),
            b'B' => self.cy = (self.cy + n).clamp(0, h - 1),
            b'C' => self.cx = (self.cx + n).clamp(0, w - 1),
            b'D' => self.cx = (self.cx - n).clamp(0, w - 1),
            b'J' => match a {
                2 | 3 => self.clear_range(0, w * h),
                0 => self.clear_range(at, w * h),
                1 => self.clear_range(0, at + 1),
                _ => {}
            },
            b'K' => match a {
                0 => self.clear_range(at, (self.cy + 1) * w),
                1 => self.clear_range(self.cy * w, at + 1),
                _ => self.clear_range(self.cy * w, (self.cy + 1) * w),
            },
            b'm' => {
                for i in 0..=self.esc_n.min(3) {
                    self.sgr(self.esc_args[i]);
                }
            }
            _ => {}
        }
    }

    /// Writes one cell, keeping wide characters consistent: overwriting
    /// either half of one clears the other half.
    fn set(&mut self, i: usize, cell: u16, uc: u32) {
        if i >= self.cells.len() {
            return;
        }
        if !self.wide.is_empty() {
            if self.wide[i] == TERM_CONT && i > 0 && self.wide[i - 1] != TERM_CONT {
                self.wide[i - 1] = 0;
                self.cells[i - 1] = (self.cells[i - 1] & 0xFF00) | b' ' as u16;
            }
            if self.wide[i] != 0 && self.wide[i] != TERM_CONT && self.wide.get(i + 1) == Some(&TERM_CONT) {
                self.wide[i + 1] = 0;
                self.cells[i + 1] = (self.cells[i + 1] & 0xFF00) | b' ' as u16;
            }
            self.wide[i] = uc;
        }
        self.cells[i] = cell;
    }

    fn put_cp(&mut self, cp: u32) {
        let wide = !self.wide.is_empty() && cp >= 0x80 && font_wide(cp).is_some();
        self.touch();
        let cp = if cp >= 0x80 && !wide { b'?' as u32 } else { cp }; // no glyph
        if wide && self.cx + 2 > self.cols {
            self.newline();
            self.touch();
        }
        let i = (self.cy * self.cols + self.cx) as usize;
        let attr = (self.attr as u16) << 8;
        if wide {
            self.set(i, attr | b' ' as u16, cp);
            self.set(i + 1, attr | b' ' as u16, TERM_CONT);
            self.cx += 2;
        } else {
            self.set(i, attr | cp as u16, 0);
            self.cx += 1;
        }
        if self.cx >= self.cols {
            self.newline();
        }
        self.touch();
    }

    fn putc(&mut self, c: u8) {
        if self.esc_state == 1 {
            if c == b'[' {
                self.esc_state = 2;
                self.esc_n = 0;
                self.esc_private = false;
                self.esc_args = [0; 4];
            } else {
                self.esc_state = 0;
            }
            return;
        }
        if self.esc_state == 2 {
            match c {
                b'0'..=b'9' => {
                    if let Some(arg) = self.esc_args.get_mut(self.esc_n) {
                        *arg = arg.saturating_mul(10).saturating_add((c - b'0') as i32);
                    }
                }
                b';' => self.esc_n += 1,
                b'?' => self.esc_private = true,
                _ => {
                    self.touch();
                    self.csi(c);
                    self.touch();
                    self.esc_state = 0;
                }
            }
            return;
        }
        if c >= 0x80 {
            // UTF-8
            if c < 0xC0 {
                if self.utf_need > 0 {
                    self.utf_need -= 1;
                    self.utf_cp = self.utf_cp << 6 | (c & 0x3F) as u32;
                    if self.utf_need == 0 {
                        self.put_cp(self.utf_cp);
                    }
                }
            } else {
                self.utf_need = if c >= 0xF0 { 3 } else if c >= 0xE0 { 2 } else { 1 };
                self.utf_cp = (c & (0x3F >> self.utf_need)) as u32;
            }
            return;
        }
        self.utf_need = 0;
        self.touch();
        match c {
            0x1B => self.esc_state = 1,
            b'\n' => {
                self.newline();
                self.touch();
            }
            b'\r' => self.cx = 0,
            8 => self.cx = (self.cx - 1).max(0),
            b'\t' => loop {
                self.put_cp(b' ' as u32);
                if self.cx % 8 == 0 {
                    break;
                }
            },
            0x20.. => self.put_cp(c as u32),
            _ => {}
        }
    }

    /// Gives the terminal a new size, keeping the lines around the cursor.
    fn resize(&mut self, cols: i32, rows: i32) {
        if self.hw || cols < 1 || rows < 1 || (cols == self.cols && rows == self.rows) {
            return;
        }
        let Some(mut new) = Term::new(cols, rows, false) else { return };
        // keep the bottom part if the cursor would fall off
        let shift = (self.cy + 1 - rows).max(0);
        for r in 0..rows.min(self.rows - shift) {
            let (from, to) = (((r + shift) * self.cols) as usize, (r * cols) as usize);
            let n = cols.min(self.cols) as usize;
            new.cells[to..to + n].copy_from_slice(&self.cells[from..from + n]);
            new.wide[to..to + n].copy_from_slice(&self.wide[from..from + n]);
            // a wide character cut in half at the new right edge
            let last = to + n - 1;
            if cols < self.cols && new.wide[last] != 0 && new.wide[last] != TERM_CONT && self.wide[from + n] == TERM_CONT {
                new.wide[last] = 0;
                new.cells[last] = (new.cells[last] & 0xFF00) | b' ' as u16;
            }
        }
        new.cy = self.cy - shift;
        new.cx = self.cx.min(cols - 1);
        new.attr = self.attr;
        *self = new;
    }

    /// Copies what changed to the VGA text screen and moves its cursor.
    fn flush_hw(&mut self) {
        if !self.hw {
            return;
        }
        let dirty = core::mem::take(&mut self.dirty);
        let cols = self.cols as usize;
        let screen = 0xB8000 as *mut u16;
        for row in (0..self.rows as usize).filter(|row| dirty & (1 << row) != 0) {
            for col in 0..cols {
                // SAFETY: the 80x25 text screen, which the kernel maps
                unsafe { screen.add(row * cols + col).write_volatile(self.cells[row * cols + col]) };
            }
        }
        let pos = (self.cy * self.cols + self.cx) as u16;
        unsafe {
            outb(0x3D4, 0x0F);
            outb(0x3D5, pos as u8);
            outb(0x3D4, 0x0E);
            outb(0x3D5, (pos >> 8) as u8);
        }
    }
}

// ------------------------------------------------------------------- tty

/// Where a thread's console I/O goes.
struct Tty {
    id: TtyId,
    term: Option<Term>,
    /// when set, output is collected here (up to the limit) instead
    capture: Option<(Vec<u8>, usize)>,
    /// mirror output to COM1
    serial: bool,
    input: VecDeque<u8>,
    /// Ctrl-C seen
    brk: bool,
    /// also show the output in the home screen's chat view
    home: bool,
    /// line editing: echo goes to the terminal only
    home_mute: bool,
}

const INPUT_QUEUE: usize = 1024;

struct Console {
    ttys: Vec<Tty>,
    next_id: TtyId,
}

impl Console {
    fn tty(&mut self, id: TtyId) -> Option<&mut Tty> {
        self.ttys.iter_mut().find(|t| t.id == id)
    }
}

/// Nothing waits or calls into the window manager while it holds this.
static CONSOLE: KCell<Console> = KCell::new(Console { ttys: Vec::new(), next_id: SHELL_TTY });

fn current() -> TtyId {
    unsafe { sys::thread_tty() }
}

fn new_tty(c: &mut Console, term: Option<Term>, serial: bool) -> TtyId {
    let id = c.next_id;
    c.next_id += 1;
    c.ttys.push(Tty { id, term, capture: None, serial, input: VecDeque::new(), brk: false, home: false, home_mute: false });
    id
}

/// Sets up the shell's tty on the VGA text screen and the serial line.
/// Needs the heap.
pub fn init() {
    serial_init();
    CONSOLE.with(|c| {
        let mut term = Term::new(80, 25, true);
        if let Some(t) = &mut term {
            t.flush_hw();
        }
        new_tty(c, term, true);
    });
    unsafe { sys::thread_set_tty(SHELL_TTY) };
}

/// A tty with its own terminal buffer, for a window.
pub fn tty_create(cols: i32, rows: i32) -> Option<TtyId> {
    let term = Term::new(cols, rows, false)?;
    Some(CONSOLE.with(|c| new_tty(c, Some(term), false)))
}

pub fn tty_destroy(id: TtyId) {
    if id != SHELL_TTY {
        CONSOLE.with(|c| c.ttys.retain(|t| t.id != id));
    }
}

/// Graphical mode: the shell moves from the VGA text screen into a
/// terminal buffer (shown in a window) and gets the home screen's chat
/// view. False if memory is short.
pub fn shell_to_window(cols: i32, rows: i32) -> bool {
    let Some(term) = Term::new(cols, rows, false) else { return false };
    CONSOLE.with(|c| {
        if let Some(t) = c.tty(SHELL_TTY) {
            t.term = Some(term);
            t.home = true;
        }
    });
    true
}

/// Delivers typed characters to a tty.
pub fn tty_input(id: TtyId, bytes: &[u8]) {
    CONSOLE.with(|c| {
        let Some(t) = c.tty(id) else { return };
        for &b in bytes {
            if b == 3 {
                t.brk = true;
            } else if t.input.len() < INPUT_QUEUE {
                t.input.push_back(b);
            }
        }
    })
}

/// Lets the window manager look at (and mark) a tty's terminal.
pub fn with_term<R>(id: TtyId, f: impl FnOnce(&mut Term) -> R) -> Option<R> {
    CONSOLE.with(|c| c.tty(id).and_then(|t| t.term.as_mut()).map(f))
}

pub fn term_resize(id: TtyId, cols: i32, rows: i32) {
    with_term(id, |t| t.resize(cols, rows));
}

/// Is the tty's output also shown in the home screen's chat view?
pub fn has_home(id: TtyId) -> bool {
    CONSOLE.with(|c| c.tty(id).is_some_and(|t| t.home))
}

/// Starts collecting the calling thread's output (up to `limit` bytes)
/// instead of showing it.
pub fn capture_start(limit: usize) {
    CONSOLE.with(|c| {
        if let Some(t) = c.tty(current()) {
            t.capture = Some((Vec::new(), limit));
        }
    })
}

/// Stops collecting and returns what was written.
pub fn capture_end() -> Vec<u8> {
    CONSOLE.with(|c| c.tty(current()).and_then(|t| t.capture.take()).map_or(Vec::new(), |(out, _)| out))
}

/// Writes to the calling thread's tty.
pub fn write(s: &[u8]) {
    let id = current();
    let (found, to_home) = CONSOLE.try_with(|c| {
        let Some(t) = c.tty(id) else { return (false, false) };
        if let Some((out, limit)) = &mut t.capture {
            let room = limit.saturating_sub(out.len());
            out.extend_from_slice(&s[..s.len().min(room)]);
            return (true, false);
        }
        if t.serial {
            serial_write(s);
        }
        if let Some(term) = &mut t.term {
            for &b in s {
                term.putc(b);
            }
            term.flush_hw();
        }
        (true, t.home && !t.home_mute)
    })
    .unwrap_or((false, false));
    if !found {
        serial_write(s); // before start-up has finished, or while the console itself is at work
    }
    if to_home {
        home::feed(s);
    }
}

// ------------------------------------------------------- keyboard, mouse

const KMAP: &[u8; 58] = b"\x00\x1b1234567890-=\x08\tqwertyuiop[]\n\x00asdfghjkl;'`\x00\\zxcvbnm,./\x00*\x00 ";
const KMAP_SHIFT: &[u8; 58] = b"\x00\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\x00ASDFGHJKL:\"~\x00|ZXCVBNM<>?\x00*\x00 ";

/// The input method keys arrive as these bytes: toggle, on, off.
const IME_TOGGLE: u8 = 0x1D;
const IME_ON: u8 = 0x1E;
const IME_OFF: u8 = 0x1F;

struct Devices {
    shift: bool,
    ctrl: bool,
    alt: bool,
    /// an E0 prefix was received
    extended: bool,
    mouse_packet: [u8; 3],
    mouse_n: usize,
}

static DEVICES: KCell<Devices> =
    KCell::new(Devices { shift: false, ctrl: false, alt: false, extended: false, mouse_packet: [0; 3], mouse_n: 0 });
static VMMOUSE: AtomicBool = AtomicBool::new(false);

/// A key press or release: scancode (set 1, +0x80 for E0-prefixed keys),
/// and what the key types.
struct KeyEvent {
    code: i32,
    down: bool,
    alt: bool,
    chars: Option<&'static [u8]>,
    /// one typed character that is not in a table
    typed: u8,
}

impl Devices {
    fn scancode(&mut self, sc: u8) -> Option<KeyEvent> {
        if sc == 0xE0 {
            self.extended = true;
            return None;
        }
        let down = sc & 0x80 == 0;
        let code = sc & 0x7F;
        let event = |d: &Devices, code: i32, chars: Option<&'static [u8]>| {
            Some(KeyEvent { code, down, alt: d.alt, chars, typed: 0 })
        };
        // delivered whatever modifiers are held, on press only
        let plain = |code: u8, byte: &'static [u8]| {
            down.then_some(KeyEvent { code: code as i32, down: true, alt: false, chars: Some(byte), typed: 0 })
        };
        if core::mem::take(&mut self.extended) {
            match code {
                0x1D => self.ctrl = down,
                0x38 => self.alt = down,
                _ => {}
            }
            let chars: Option<&'static [u8]> = match code {
                0x48 => Some(b"\x1b[A"),
                0x50 => Some(b"\x1b[B"),
                0x4D => Some(b"\x1b[C"),
                0x4B => Some(b"\x1b[D"),
                0x1C => Some(b"\n"),
                0x47 => Some(b"\x1b[H"),  // Home
                0x4F => Some(b"\x1b[F"),  // End
                0x53 => Some(b"\x1b[3~"), // Delete
                _ => None,
            };
            return event(self, code as i32 | 0x80, chars);
        }
        match code {
            0x2A | 0x36 => self.shift = down,
            // input method: Alt+Space or Ctrl+Space toggle; the Japanese keyboard keys
            0x39 if self.ctrl || self.alt => return plain(code, &[IME_TOGGLE]),
            0x72 => return plain(code, &[IME_ON]),  // the Mac JIS かな key (LANG1)
            0x71 => return plain(code, &[IME_OFF]), // the Mac JIS 英数 key (LANG2)
            0x70 | 0x77 | 0x79 => return plain(code, &[IME_ON]), // katakana/hiragana, hiragana, henkan
            0x7B => return plain(code, &[IME_OFF]), // muhenkan
            0x1D => self.ctrl = down,
            0x38 => self.alt = down,
            _ => {}
        }
        let table = if self.shift { KMAP_SHIFT } else { KMAP };
        let mut ch = table.get(code as usize).copied().unwrap_or(0);
        if self.ctrl && (b'@'..127).contains(&ch) {
            ch &= 0x1F;
        }
        Some(KeyEvent { code: code as i32, down, alt: self.alt, chars: None, typed: ch })
    }

    /// A byte from the PS/2 mouse; a full packet gives (dx, dy, buttons).
    fn mouse_byte(&mut self, b: u8) -> Option<(i32, i32, i32)> {
        if self.mouse_n == 0 && b & 0x08 == 0 {
            return None; // resynchronise on the header byte
        }
        self.mouse_packet[self.mouse_n] = b;
        self.mouse_n += 1;
        if self.mouse_n < 3 {
            return None;
        }
        self.mouse_n = 0;
        let [head, x, y] = self.mouse_packet;
        if head & 0xC0 != 0 {
            return None; // overflow
        }
        let dx = x as i32 - if head & 0x10 != 0 { 256 } else { 0 };
        let dy = y as i32 - if head & 0x20 != 0 { 256 } else { 0 };
        Some((dx, -dy, (head & 7) as i32))
    }
}

fn route(e: KeyEvent) {
    let typed = [e.typed];
    let chars = e.chars.or(if e.typed != 0 { Some(&typed[..]) } else { None });
    if wm::active() {
        wm::key(e.code, e.down, e.alt, chars);
    } else if let Some(chars) = chars.filter(|_| e.down) {
        tty_input(SHELL_TTY, chars);
    }
}

fn ps2_wait_write() {
    let mut spins = 0;
    while spins < 100_000 && unsafe { inb(0x64) } & 2 != 0 {
        spins += 1;
    }
}

fn ps2_read() -> Option<u8> {
    (0..100_000).find_map(|_| unsafe { (inb(0x64) & 1 != 0).then(|| inb(0x60)) })
}

fn mouse_cmd(cmd: u8) {
    ps2_wait_write();
    unsafe { outb(0x64, 0xD4) };
    ps2_wait_write();
    unsafe { outb(0x60, cmd) };
    ps2_read(); // ACK
}

// VMware-compatible absolute pointer ("vmmouse"), provided by QEMU on PCs.
// It reports where the host cursor is, so the pointer needs no grabbing and
// never drifts from the host's cursor, also when QEMU zooms the picture.
const VMPORT_MAGIC: u32 = 0x564D_5868;
const VMPORT_PORT: u32 = 0x5658;
const VMMOUSE_DATA: u32 = 39;
const VMMOUSE_STATUS: u32 = 40;
const VMMOUSE_COMMAND: u32 = 41;

/// One call through the hypervisor's "backdoor" port; returns eax..edx.
fn vmport(cmd: u32, arg: u32) -> [u32; 4] {
    let (mut a, mut b, mut c, mut d) = (VMPORT_MAGIC, arg as u64, cmd, VMPORT_PORT);
    // SAFETY: on real hardware this is an ordinary read of an unused port;
    // rbx is swapped in and out because the compiler reserves it
    unsafe {
        asm!("xchg rbx, {b}", "in eax, dx", "xchg rbx, {b}",
             b = inout(reg) b, inout("eax") a, inout("ecx") c, inout("edx") d,
             options(nomem, nostack, preserves_flags));
    }
    [a, b as u32, c, d]
}

fn vmmouse_init() {
    if vmport(10, 0)[1] != VMPORT_MAGIC {
        return; // GETVERSION: no such hypervisor
    }
    vmport(VMMOUSE_COMMAND, 0x4541_4552); // READ_ID
    let status = vmport(VMMOUSE_STATUS, 0)[0];
    if status & 0xFFFF_0000 == 0xFFFF_0000 || status & 0xFFFF == 0 {
        return;
    }
    if vmport(VMMOUSE_DATA, 1)[0] != 0x3442_554A {
        return;
    }
    vmport(VMMOUSE_COMMAND, 0x5342_4152); // REQUEST_ABSOLUTE
    VMMOUSE.store(true, Ordering::Relaxed);
}

/// The host pointer is followed exactly (vmmouse).
pub fn mouse_absolute() -> bool {
    VMMOUSE.load(Ordering::Relaxed)
}

fn vmmouse_poll() {
    for _ in 0..64 {
        let status = vmport(VMMOUSE_STATUS, 0)[0];
        if status & 0xFFFF_0000 == 0xFFFF_0000 {
            // error: start over
            VMMOUSE.store(false, Ordering::Relaxed);
            vmmouse_init();
            return;
        }
        if status & 0xFFFF < 4 {
            return;
        }
        let r = vmport(VMMOUSE_DATA, 4);
        let bit = |mask: u32, button: i32| if r[0] & mask != 0 { button } else { 0 };
        let buttons = bit(0x20, 1) | bit(0x10, 2) | bit(0x08, 4);
        if wm::active() {
            wm::mouse_abs((r[1] & 0xFFFF) as i32, (r[2] & 0xFFFF) as i32, buttons, r[3] as i8 as i32);
        }
    }
}

pub fn mouse_init() {
    ps2_wait_write();
    unsafe { outb(0x64, 0xA8) }; // enable the auxiliary port
    ps2_wait_write();
    unsafe { outb(0x64, 0x20) };
    let Some(config) = ps2_read() else { return };
    ps2_wait_write();
    unsafe { outb(0x64, 0x60) };
    ps2_wait_write();
    unsafe { outb(0x60, config & !0x20) }; // enable the mouse clock
    mouse_cmd(0xF6); // defaults
    mouse_cmd(0xF4); // start reporting
    vmmouse_init();
}

/// Drains the serial line, the keyboard and the mouse. Called from the
/// scheduler's tick.
pub fn poll_input() {
    // When the queue is nearly full, leave bytes in the UART so the sender is held back.
    let room = || CONSOLE.with(|c| c.tty(SHELL_TTY).is_some_and(|t| t.input.len() < INPUT_QUEUE - 8));
    while room() && unsafe { inb(COM1 + 5) } & 1 != 0 {
        let c = match unsafe { inb(COM1) } {
            b'\r' => b'\n',
            127 => 8,
            c => c,
        };
        if c != 0 {
            tty_input(SHELL_TTY, &[c]);
        }
    }
    for _ in 0..64 {
        let status = unsafe { inb(0x64) };
        if status & 1 == 0 {
            break;
        }
        let b = unsafe { inb(0x60) };
        if status & 0x20 == 0 {
            if let Some(e) = DEVICES.with(|d| d.scancode(b)) {
                route(e);
            }
        } else if !mouse_absolute() {
            // (with vmmouse these are only wake-up packets)
            if let Some((dx, dy, buttons)) = DEVICES.with(|d| d.mouse_byte(b)) {
                if wm::active() {
                    wm::mouse(dx, dy, buttons);
                }
            }
        }
    }
    if mouse_absolute() {
        vmmouse_poll();
    }
}

// ----------------------------------------------------------------- input

/// True once if Ctrl-C was pressed since the last call.
pub fn break_pending() -> bool {
    CONSOLE.with(|c| c.tty(current()).is_some_and(|t| core::mem::take(&mut t.brk)))
}

pub fn clear_break() {
    break_pending();
}

/// The next input byte; `timeout_ms` < 0 waits forever. KEY_BREAK on
/// Ctrl-C or when the thread is being stopped, KEY_TIMEOUT when time is up.
pub fn getc(timeout_ms: i32) -> i32 {
    let id = current();
    let deadline = unsafe { sys::time_ms() } + timeout_ms.max(0) as u64;
    loop {
        let got = CONSOLE.with(|c| {
            let Some(t) = c.tty(id) else { return Some(KEY_BREAK) };
            if core::mem::take(&mut t.brk) {
                return Some(KEY_BREAK);
            }
            t.input.pop_front().map(|b| b as i32)
        });
        match got {
            Some(KEY_BREAK) => return KEY_BREAK,
            _ if unsafe { sys::thread_killed() } != 0 => return KEY_BREAK,
            Some(b) => return b,
            None => {}
        }
        if timeout_ms >= 0 && unsafe { sys::time_ms() } >= deadline {
            return KEY_TIMEOUT;
        }
        unsafe { sys::cpu_idle() };
    }
}

/// Columns a UTF-8 string occupies on a terminal.
fn display_width(mut s: &[u8]) -> i32 {
    let mut w = 0;
    while !s.is_empty() {
        let (cp, used) = utf8_decode(s);
        w += if font_wide(cp).is_some() { 2 } else { 1 };
        s = &s[used..];
    }
    w
}

const HISTORY: usize = 32;
static HISTORY_LINES: KCell<VecDeque<Vec<u8>>> = KCell::new(VecDeque::new());

fn history_add(line: &[u8]) {
    HISTORY_LINES.with(|h| {
        if line.is_empty() || h.back().is_some_and(|last| last == line) {
            return;
        }
        if h.len() == HISTORY {
            h.pop_front();
        }
        h.push_back(line.to_vec());
    })
}

// ------------------------------------------------- Japanese input method
// Romaji typed while the input method is on becomes hiragana in a
// "composition" shown highlighted after the cursor. Space asks the LLM to
// convert it to kanji (again for another candidate), Enter accepts,
// Ctrl+K turns it into katakana, Backspace / Esc go back.

static IME: AtomicBool = AtomicBool::new(false);

pub fn ime_enabled() -> bool {
    IME.load(Ordering::Relaxed)
}

fn ime_set(on: bool) {
    if IME.swap(on, Ordering::Relaxed) != on && wm::active() {
        wm::ime_changed();
    }
}

const ROMAJI: &[(&str, &str)] = &[
    ("a", "あ"), ("i", "い"), ("u", "う"), ("e", "え"), ("o", "お"),
    ("ka", "か"), ("ki", "き"), ("ku", "く"), ("ke", "け"), ("ko", "こ"),
    ("ga", "が"), ("gi", "ぎ"), ("gu", "ぐ"), ("ge", "げ"), ("go", "ご"),
    ("sa", "さ"), ("si", "し"), ("shi", "し"), ("su", "す"), ("se", "せ"), ("so", "そ"),
    ("za", "ざ"), ("zi", "じ"), ("ji", "じ"), ("zu", "ず"), ("ze", "ぜ"), ("zo", "ぞ"),
    ("ta", "た"), ("ti", "ち"), ("chi", "ち"), ("tu", "つ"), ("tsu", "つ"), ("te", "て"), ("to", "と"),
    ("da", "だ"), ("di", "ぢ"), ("du", "づ"), ("de", "で"), ("do", "ど"),
    ("na", "な"), ("ni", "に"), ("nu", "ぬ"), ("ne", "ね"), ("no", "の"),
    ("ha", "は"), ("hi", "ひ"), ("hu", "ふ"), ("fu", "ふ"), ("he", "へ"), ("ho", "ほ"),
    ("ba", "ば"), ("bi", "び"), ("bu", "ぶ"), ("be", "べ"), ("bo", "ぼ"),
    ("pa", "ぱ"), ("pi", "ぴ"), ("pu", "ぷ"), ("pe", "ぺ"), ("po", "ぽ"),
    ("ma", "ま"), ("mi", "み"), ("mu", "む"), ("me", "め"), ("mo", "も"),
    ("ya", "や"), ("yu", "ゆ"), ("ye", "いぇ"), ("yo", "よ"),
    ("ra", "ら"), ("ri", "り"), ("ru", "る"), ("re", "れ"), ("ro", "ろ"),
    ("la", "ぁ"), ("li", "ぃ"), ("lu", "ぅ"), ("le", "ぇ"), ("lo", "ぉ"),
    ("wa", "わ"), ("wi", "うぃ"), ("we", "うぇ"), ("wo", "を"), ("nn", "ん"), ("n'", "ん"),
    ("va", "ゔぁ"), ("vi", "ゔぃ"), ("vu", "ゔ"), ("ve", "ゔぇ"), ("vo", "ゔぉ"),
    ("kya", "きゃ"), ("kyi", "きぃ"), ("kyu", "きゅ"), ("kye", "きぇ"), ("kyo", "きょ"),
    ("gya", "ぎゃ"), ("gyu", "ぎゅ"), ("gyo", "ぎょ"),
    ("sya", "しゃ"), ("syu", "しゅ"), ("sye", "しぇ"), ("syo", "しょ"),
    ("sha", "しゃ"), ("shu", "しゅ"), ("she", "しぇ"), ("sho", "しょ"),
    ("ja", "じゃ"), ("ju", "じゅ"), ("je", "じぇ"), ("jo", "じょ"),
    ("jya", "じゃ"), ("jyu", "じゅ"), ("jye", "じぇ"), ("jyo", "じょ"),
    ("zya", "じゃ"), ("zyu", "じゅ"), ("zye", "じぇ"), ("zyo", "じょ"),
    ("tya", "ちゃ"), ("tyu", "ちゅ"), ("tye", "ちぇ"), ("tyo", "ちょ"),
    ("cha", "ちゃ"), ("chu", "ちゅ"), ("che", "ちぇ"), ("cho", "ちょ"),
    ("cya", "ちゃ"), ("cyu", "ちゅ"), ("cye", "ちぇ"), ("cyo", "ちょ"),
    ("dya", "ぢゃ"), ("dyu", "ぢゅ"), ("dyo", "ぢょ"),
    ("thi", "てぃ"), ("thu", "てゅ"), ("dhi", "でぃ"), ("dhu", "でゅ"),
    ("twu", "とぅ"), ("dwu", "どぅ"), ("tsa", "つぁ"), ("tsi", "つぃ"), ("tse", "つぇ"), ("tso", "つぉ"),
    ("nya", "にゃ"), ("nyu", "にゅ"), ("nyo", "にょ"),
    ("hya", "ひゃ"), ("hyu", "ひゅ"), ("hyo", "ひょ"),
    ("fa", "ふぁ"), ("fi", "ふぃ"), ("fe", "ふぇ"), ("fo", "ふぉ"), ("fyu", "ふゅ"),
    ("bya", "びゃ"), ("byu", "びゅ"), ("byo", "びょ"),
    ("pya", "ぴゃ"), ("pyu", "ぴゅ"), ("pyo", "ぴょ"),
    ("mya", "みゃ"), ("myu", "みゅ"), ("myo", "みょ"),
    ("rya", "りゃ"), ("ryu", "りゅ"), ("ryo", "りょ"),
    ("xa", "ぁ"), ("xi", "ぃ"), ("xu", "ぅ"), ("xe", "ぇ"), ("xo", "ぉ"),
    ("xya", "ゃ"), ("xyu", "ゅ"), ("xyo", "ょ"), ("lya", "ゃ"), ("lyu", "ゅ"), ("lyo", "ょ"),
    ("xtu", "っ"), ("ltu", "っ"), ("xtsu", "っ"), ("ltsu", "っ"), ("xwa", "ゎ"), ("lwa", "ゎ"),
    ("-", "ー"), (",", "、"), (".", "。"), ("[", "「"), ("]", "」"), ("~", "〜"),
    ("?", "？"), ("!", "！"), ("/", "・"),
];

fn is_romaji_char(c: i32) -> bool {
    u8::try_from(c).is_ok_and(|c| c.is_ascii_alphabetic() || b"-,.[]~?!/'".contains(&c))
}

/// The length of `s` without its last UTF-8 character.
fn without_last_char(s: &[u8]) -> usize {
    let mut i = s.len().saturating_sub(1);
    while i > 0 && s[i] & 0xC0 == 0x80 {
        i -= 1;
    }
    i
}

/// The line being edited: text, cursor, and the Japanese composition that
/// sits at the cursor until it is accepted.
struct Editor {
    buf: Vec<u8>,
    cur: usize,
    /// the line may not get longer than this
    max: usize,
    /// kana typed so far (UTF-8)
    pre: Vec<u8>,
    /// romaji not yet turned into kana
    rom: Vec<u8>,
    /// the conversion shown instead of `pre`
    cand: Vec<u8>,
    converted: bool,
    /// columns between the start of the line and the terminal cursor
    back: i32,
    /// the home screen shows the line in its input box
    home: bool,
}

impl Editor {
    fn composing(&self) -> bool {
        !self.pre.is_empty() || !self.rom.is_empty() || self.converted
    }

    /// End of input: a lone n is ん, other letters stay.
    fn rom_flush(&mut self) {
        if self.rom == b"n" {
            self.pre.extend_from_slice("ん".as_bytes());
        } else {
            self.pre.extend_from_slice(&self.rom);
        }
        self.rom.clear();
    }

    fn rom_feed(&mut self, c: u8) {
        if self.rom.len() >= 7 {
            self.rom_flush();
        }
        self.rom.push(c.to_ascii_lowercase());
        loop {
            if let Some(&(_, kana)) = ROMAJI.iter().find(|r| r.0.as_bytes() == self.rom) {
                self.pre.extend_from_slice(kana.as_bytes());
                self.rom.clear();
                return;
            }
            if self.rom.is_empty() || ROMAJI.iter().any(|r| r.0.as_bytes().starts_with(&self.rom)) {
                return;
            }
            let r0 = self.rom[0];
            let r1 = self.rom.get(1).copied().unwrap_or(0);
            if r0 == b'n' && r1 != 0 && !b"aiueoyn'".contains(&r1) {
                self.pre.extend_from_slice("ん".as_bytes()); // n before a consonant
            } else if r1 != 0 && r0 == r1 && !b"aiueon".contains(&r0) && r0.is_ascii_lowercase() {
                self.pre.extend_from_slice("っ".as_bytes()); // doubled consonant
            } else {
                self.pre.push(r0); // not romaji: keep the letter
            }
            self.rom.remove(0);
        }
    }

    fn composition(&self) -> Vec<u8> {
        if self.converted { self.cand.clone() } else { [self.pre.as_slice(), &self.rom].concat() }
    }

    fn show(&self) {
        if self.home {
            let comp = self.composition();
            home::with(|v| v.set_input(&self.buf, self.cur, &comp, self.converted));
        }
    }

    /// Redraws the line on the terminal: text before the cursor, the
    /// composition (highlighted, followed by `note`), the rest, and the
    /// terminal cursor back in its place.
    fn draw(&mut self, note: &[u8]) {
        let mut out = vec![8u8; self.back.max(0) as usize];
        out.extend_from_slice(&self.buf[..self.cur]);
        let mut col = display_width(&self.buf[..self.cur]);
        let mut comp = self.composition();
        comp.extend_from_slice(note);
        if !comp.is_empty() {
            out.extend_from_slice(if self.converted { b"\x1b[30;46m" } else { b"\x1b[30;43m" });
            out.extend_from_slice(&comp);
            out.extend_from_slice(b"\x1b[0m");
            col += display_width(&comp);
        }
        let rest = &self.buf[self.cur..];
        out.extend_from_slice(rest);
        out.extend_from_slice(b"\x1b[K");
        out.resize(out.len() + display_width(rest) as usize, 8);
        write(&out);
        self.back = col;
    }

    fn prev_char(&self, i: usize) -> usize {
        without_last_char(&self.buf[..i])
    }

    fn next_char(&self, mut i: usize) -> usize {
        if i >= self.buf.len() {
            return self.buf.len();
        }
        i += 1;
        while i < self.buf.len() && self.buf[i] & 0xC0 == 0x80 {
            i += 1;
        }
        i
    }

    /// Inserts at the cursor; false if the line would get too long.
    fn insert(&mut self, s: &[u8]) -> bool {
        if self.buf.len() + s.len() > self.max {
            return false;
        }
        self.buf.splice(self.cur..self.cur, s.iter().copied());
        self.cur += s.len();
        true
    }

    fn delete(&mut self, from: usize, to: usize) {
        self.buf.drain(from..to);
        self.cur = from;
    }

    /// Moves the composition into the line at the cursor.
    fn commit(&mut self) {
        self.rom_flush();
        let text = core::mem::take(if self.converted { &mut self.cand } else { &mut self.pre });
        let mut len = text.len().min(self.max.saturating_sub(self.buf.len()));
        while len > 0 && len < text.len() && text[len] & 0xC0 == 0x80 {
            len -= 1; // keep whole characters
        }
        self.insert(&text[..len]);
        self.pre.clear();
        self.cand.clear();
        self.converted = false;
        self.draw(b"");
    }

    /// Asks the LLM for kanji; asked again, for another candidate.
    fn convert(&mut self) {
        self.rom_flush();
        if self.pre.is_empty() {
            return;
        }
        let avoid = self.converted.then(|| self.cand.clone());
        self.converted = false;
        self.draw(" …".as_bytes());
        match crate::agent::convert_kana(&self.pre, avoid.as_deref()) {
            Ok(text) if !text.is_empty() => {
                self.cand = text;
                self.converted = true;
            }
            result => {
                // say why, briefly
                let why = result.err().unwrap_or_else(|| "no conversion".into());
                self.draw(alloc::format!(" ({why})").as_bytes());
                unsafe { sys::sleep_ms(1200) };
            }
        }
        self.draw(b"");
    }

    fn katakana(&mut self) {
        self.rom_flush();
        let mut i = 0;
        while i < self.pre.len() {
            let (cp, used) = utf8_decode(&self.pre[i..]);
            if used == 3 && (0x3041..=0x3096).contains(&cp) {
                let k = cp + 0x60; // hiragana -> katakana
                self.pre[i..i + 3].copy_from_slice(&[0xE0 | (k >> 12) as u8, 0x80 | (k >> 6 & 0x3F) as u8, 0x80 | (k & 0x3F) as u8]);
            }
            i += used;
        }
    }
}

/// Reads the rest of an escape sequence and returns the control key it
/// stands for (arrows, Home, End, Delete), or None; the flag says whether
/// it was a bare Esc.
fn read_escape() -> (Option<u8>, bool) {
    let mut c = getc(20);
    let lone = c < 0;
    if c != b'[' as i32 && c != b'O' as i32 {
        return (None, lone);
    }
    let mut param = 0;
    loop {
        c = getc(20);
        match c {
            _ if c < 0 => return (None, lone),
            0x30..=0x39 => param = param * 10 + (c - 0x30),
            0x40.. => break,
            _ => {}
        }
    }
    let key = match (c as u8, param) {
        (b'A', _) => 0x10,                   // up: Ctrl-P
        (b'B', _) => 0x0E,                   // down: Ctrl-N
        (b'C', _) => 0x06,                   // right: Ctrl-F
        (b'D', _) => 0x02,                   // left: Ctrl-B
        (b'H', _) | (b'~', 1 | 7) => 0x01,   // Home: Ctrl-A
        (b'F', _) | (b'~', 4 | 8) => 0x05,   // End: Ctrl-E
        (b'~', 3) => 0x04,                   // Delete: Ctrl-D
        _ => return (None, lone),
    };
    (Some(key), lone)
}

/// Line editor; returns the line (at most `max` bytes), or None on Ctrl-C.
/// Keys: left/right or Ctrl-B/F move, Home/End or Ctrl-A/E jump, Backspace
/// and Delete (Ctrl-D) remove, Ctrl-K / Ctrl-U cut to the end / start,
/// Ctrl-W removes a word, and with `use_history` up/down or Ctrl-P/N walk
/// the earlier lines.
pub fn readline(max: usize, use_history: bool) -> Option<Vec<u8>> {
    let id = current();
    let set_mute = |on: bool| CONSOLE.with(|c| c.tty(id).map(|t| core::mem::replace(&mut t.home_mute, on)));
    let muted = set_mute(true);
    let home = has_home(id);
    let mut e = Editor { buf: Vec::new(), cur: 0, max, pre: Vec::new(), rom: Vec::new(), cand: Vec::new(), converted: false, back: 0, home };
    let history_len = HISTORY_LINES.with(|h| h.len());
    let mut hpos = history_len;
    let result = loop {
        e.show();
        let mut c = getc(-1);
        if c == KEY_BREAK {
            write(b"^C\n");
            break None;
        }
        if let Ok(key @ (IME_TOGGLE | IME_ON | IME_OFF)) = u8::try_from(c) {
            let on = if key == IME_TOGGLE { !ime_enabled() } else { key == IME_ON };
            if !on && e.composing() {
                e.commit();
            }
            ime_set(on);
            continue;
        }
        if e.composing() {
            match c {
                0x1B => {
                    // Esc: back to kana, or drop the composition; arrows do nothing
                    if read_escape().1 {
                        if e.converted {
                            e.converted = false;
                        } else {
                            e.pre.clear();
                            e.rom.clear();
                        }
                        e.draw(b"");
                    }
                    continue;
                }
                0x0A => {
                    e.commit();
                    continue;
                }
                0x08 => {
                    if e.converted {
                        e.converted = false;
                    } else if e.rom.pop().is_none() {
                        e.pre.truncate(without_last_char(&e.pre));
                    }
                    e.draw(b"");
                    continue;
                }
                0x20 => {
                    e.convert(); // again: another candidate
                    continue;
                }
                0x0B if !e.converted => {
                    // Ctrl+K: katakana
                    e.katakana();
                    e.draw(b"");
                    continue;
                }
                // anything else accepts the composition, unless it continues the kana
                _ if e.converted || !(ime_enabled() && is_romaji_char(c)) => e.commit(),
                _ => {}
            }
        }
        // lines starting with / are commands: typed as plain letters
        let command = if e.buf.is_empty() { c == b'/' as i32 && !e.composing() } else { e.buf[0] == b'/' };
        if ime_enabled() && is_romaji_char(c) && !command {
            e.rom_feed(c as u8);
            e.draw(b"");
            continue;
        }
        if c == b'\n' as i32 {
            // leave the cursor after the line
            write(&[&e.buf[e.cur..], b"\n"].concat());
            if use_history {
                history_add(&e.buf);
            }
            break Some(core::mem::take(&mut e.buf));
        }
        if c == 0x1B {
            match read_escape().0 {
                Some(key) => c = key as i32,
                None => continue,
            }
        }
        match c {
            0x08 => {
                if e.cur > 0 {
                    e.delete(e.prev_char(e.cur), e.cur);
                }
            }
            0x04 => {
                // Ctrl-D / Delete
                if e.cur < e.buf.len() {
                    e.delete(e.cur, e.next_char(e.cur));
                }
            }
            0x01 => e.cur = 0,
            0x05 => e.cur = e.buf.len(),
            0x02 => e.cur = e.prev_char(e.cur),
            0x06 => e.cur = e.next_char(e.cur),
            0x0B => e.buf.truncate(e.cur),  // Ctrl-K
            0x15 => e.delete(0, e.cur),     // Ctrl-U
            0x17 => {
                // Ctrl-W
                let mut i = e.cur;
                while i > 0 && e.buf[i - 1] == b' ' {
                    i -= 1;
                }
                while i > 0 && e.buf[i - 1] != b' ' {
                    i -= 1;
                }
                e.delete(i, e.cur);
            }
            0x10 | 0x0E => {
                // history
                let npos = if c == 0x10 { hpos.checked_sub(1) } else { Some(hpos + 1) };
                let Some(npos) = npos.filter(|&n| use_history && n <= history_len) else { continue };
                hpos = npos;
                let mut line = HISTORY_LINES.with(|h| h.get(hpos).cloned()).unwrap_or_default();
                line.truncate(max);
                e.cur = line.len();
                e.buf = line;
            }
            0x20.. => {
                // one whole character (UTF-8 arrives byte by byte)
                let mut ch = vec![c as u8];
                let more = if c >= 0xF0 { 3 } else if c >= 0xE0 { 2 } else if c >= 0xC0 { 1 } else { 0 };
                for _ in 0..more {
                    let cc = getc(50);
                    if cc < 0x80 {
                        break;
                    }
                    ch.push(cc as u8);
                }
                let at_end = e.cur == e.buf.len();
                if !e.insert(&ch) {
                    continue;
                }
                if at_end {
                    // the common case needs no redraw
                    write(&ch);
                    e.back += display_width(&ch);
                    continue;
                }
            }
            _ => continue,
        }
        e.draw(b"");
    };
    if let Some(m) = muted {
        set_mute(m);
    }
    if home {
        home::with(|v| v.set_input(b"", 0, b"", false));
    }
    result
}

// ------------------------------------------------------------ C interface
// For the scheduler, the CPU exception handler and printf, which are
// still C.

/// # Safety
/// `s` must point to `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn console_write(s: *const c_char, n: usize) {
    if n > 0 {
        write(unsafe { core::slice::from_raw_parts(s as *const u8, n) });
    }
}

/// Ends any capture, so that what follows is seen (the arguments are what
/// the C interface used to take and are ignored).
#[unsafe(no_mangle)]
pub extern "C" fn console_capture(_buf: *mut c_void, _limit: usize) {
    CONSOLE.try_with(|c| {
        for t in &mut c.ttys {
            t.capture = None;
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn console_poll_input() {
    poll_input();
}
