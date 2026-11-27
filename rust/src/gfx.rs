//! Linear framebuffer (Bochs/QEMU "BGA" display interface), fonts and 2D
//! drawing.

use alloc::vec::Vec;
use core::ffi::c_char;

use crate::pci::{self, inw, outw};
use crate::rt::KCell;

/// A rectangle of pixels (0x00RRGGBB) with a clip rectangle. C code shares
/// this layout (`surface` in kernel/tomos.h).
///
/// Whoever sets `px`, `w` and `h` promises that `px` points to `w * h`
/// pixels. The clip rectangle is not trusted: drawing clamps it to the
/// surface, so the methods cannot write outside the pixels.
#[repr(C)]
pub struct Surface {
    px: *mut u32,
    pub w: i32,
    pub h: i32,
    pub cx0: i32,
    pub cy0: i32,
    /// exclusive upper bounds
    pub cx1: i32,
    pub cy1: i32,
}

impl Surface {
    pub const fn empty() -> Surface {
        Surface { px: core::ptr::null_mut(), w: 0, h: 0, cx0: 0, cy0: 0, cx1: 0, cy1: 0 }
    }

    /// # Safety
    /// `px` must point to `w * h` pixels that stay valid, and are not used
    /// through references elsewhere, while the surface is drawn on.
    pub unsafe fn new(px: *mut u32, w: i32, h: i32) -> Surface {
        Surface { px, w, h, cx0: 0, cy0: 0, cx1: w, cy1: h }
    }

    /// The part of a rectangle inside the clip rectangle and the surface,
    /// as (x0, y0, x1, y1).
    fn visible(&self, x: i32, y: i32, w: i32, h: i32) -> Option<(usize, usize, usize, usize)> {
        if self.px.is_null() {
            return None;
        }
        let x0 = x.max(self.cx0).max(0);
        let y0 = y.max(self.cy0).max(0);
        let x1 = x.saturating_add(w).min(self.cx1).min(self.w);
        let y1 = y.saturating_add(h).min(self.cy1).min(self.h);
        (x0 < x1 && y0 < y1).then_some((x0 as usize, y0 as usize, x1 as usize, y1 as usize))
    }

    /// The pixels x0..x1 of row y; the caller got the range from `visible`.
    fn row(&mut self, y: usize, x0: usize, x1: usize) -> &mut [u32] {
        assert!(x0 <= x1 && x1 <= self.w as usize && y < self.h as usize);
        // SAFETY: inside the w * h pixels (checked above)
        unsafe { core::slice::from_raw_parts_mut(self.px.add(y * self.w as usize + x0), x1 - x0) }
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32) {
        let Some((x0, y0, x1, y1)) = self.visible(x, y, w, h) else { return };
        for yy in y0..y1 {
            self.row(yy, x0, x1).fill(c);
        }
    }

    /// A filled rectangle with rounded corners.
    pub fn round(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, c: u32) {
        let r = r.min(h / 2).min(w / 2);
        for row in 0..h {
            // distance into the corner: inset where the circle starts
            let d = if row < r {
                r - row
            } else if row >= h - r {
                row - (h - r - 1)
            } else {
                0
            };
            let mut inset = 0;
            if d > 0 {
                let dy = d as f64 - 0.5;
                inset = r - (libm::sqrt((r * r) as f64 - dy * dy) + 0.5) as i32;
            }
            self.fill(x + inset, y + row, w - 2 * inset, 1, c);
        }
    }

    pub fn pixel(&mut self, x: i32, y: i32, c: u32) {
        self.fill(x, y, 1, 1, c);
    }

    pub fn line(&mut self, mut x0: i32, mut y0: i32, x1: i32, y1: i32, c: u32) {
        let (dx, dy) = ((x1 as i64 - x0 as i64).abs(), -(y1 as i64 - y0 as i64).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let mut err = dx + dy;
        // bound the work for wild coordinates
        for _ in 0..100_000 {
            self.pixel(x0, y0, c);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    pub fn circle(&mut self, cx: i32, cy: i32, r: i32, c: u32, fill: bool) {
        if !(0..=4096).contains(&r) {
            return;
        }
        let (cx, cy) = (cx.clamp(-1_000_000, 1_000_000), cy.clamp(-1_000_000, 1_000_000));
        let (mut x, mut y, mut err) = (r, 0, 1 - r);
        while x >= y {
            if fill {
                self.fill(cx - x, cy + y, 2 * x + 1, 1, c);
                self.fill(cx - x, cy - y, 2 * x + 1, 1, c);
                self.fill(cx - y, cy + x, 2 * y + 1, 1, c);
                self.fill(cx - y, cy - x, 2 * y + 1, 1, c);
            } else {
                for (px, py) in [(x, y), (-x, y), (x, -y), (-x, -y), (y, x), (-y, x), (y, -x), (-y, -x)] {
                    self.pixel(cx + px, cy + py, c);
                }
            }
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    /// Draws a glyph of `width` (8 or 16) x 16 dots, one bit per dot, rows
    /// from the top, left dot in the highest bit. `bg` None is transparent.
    fn bitmap(&mut self, x: i32, y: i32, width: i32, rows: impl Fn(usize) -> u32, fg: u32, bg: Option<u32>, scale: i32) {
        let scale = scale.clamp(1, 64);
        if self.visible(x, y, width * scale, 16 * scale).is_none() {
            return;
        }
        let top = 1u32 << (width - 1);
        if scale == 1 {
            if let Some((x0, y0, x1, y1)) = self.visible(x, y, width, 16) {
                for yy in y0..y1 {
                    let bits = rows((yy as i32 - y) as usize);
                    let first = (x0 as i32 - x) as u32;
                    for (col, p) in self.row(yy, x0, x1).iter_mut().enumerate() {
                        if bits & (top >> (first + col as u32)) != 0 {
                            *p = fg;
                        } else if let Some(bg) = bg {
                            *p = bg;
                        }
                    }
                }
            }
            return;
        }
        for row in 0..16 {
            let bits = rows(row as usize);
            for col in 0..width {
                let c = if bits & (top >> col) != 0 { Some(fg) } else { bg };
                if let Some(c) = c {
                    self.fill(x + col * scale, y + row * scale, scale, scale, c);
                }
            }
        }
    }

    /// An 8x16 character of the VGA font.
    pub fn glyph(&mut self, x: i32, y: i32, ch: u8, fg: u32, bg: Option<u32>, scale: i32) {
        let g = FONT.with(|f| f[ch as usize]);
        self.bitmap(x, y, 8, |row| g[row] as u32, fg, bg, scale);
    }

    /// A 16x16 character of the Japanese font (see `font_wide`).
    pub fn glyph16(&mut self, x: i32, y: i32, g: &[u8; 32], fg: u32, bg: Option<u32>, scale: i32) {
        self.bitmap(x, y, 16, |row| (g[2 * row] as u32) << 8 | g[2 * row + 1] as u32, fg, bg, scale);
    }

    /// UTF-8 text; characters without a glyph are shown as '?'.
    pub fn text(&mut self, mut x: i32, y: i32, text: &[u8], fg: u32, bg: Option<u32>, scale: i32) {
        let scale = scale.clamp(1, 64);
        let mut rest = text;
        while !rest.is_empty() {
            let (cp, used) = utf8_decode(rest);
            rest = &rest[used..];
            if let Some(g) = font_wide(cp) {
                self.glyph16(x, y, g, fg, bg, scale);
                x = x.saturating_add(16 * scale);
            } else {
                self.glyph(x, y, if cp < 0x80 { cp as u8 } else { b'?' }, fg, bg, scale);
                x = x.saturating_add(8 * scale);
            }
        }
    }

    /// Copies an image of `sw` pixels per row to (dx, dy).
    pub fn blit(&mut self, dx: i32, dy: i32, src: &[u32], sw: i32) {
        if sw <= 0 {
            return;
        }
        let sh = (src.len() / sw as usize) as i32;
        let Some((x0, y0, x1, y1)) = self.visible(dx, dy, sw, sh) else { return };
        for y in y0..y1 {
            let from = (y as i32 - dy) as usize * sw as usize + (x0 as i32 - dx) as usize;
            self.row(y, x0, x1).copy_from_slice(&src[from..from + (x1 - x0)]);
        }
    }

    /// Draws an image stretched to dw x dh with nearest-neighbour sampling.
    pub fn blit_resize(&mut self, dx: i32, dy: i32, dw: i32, dh: i32, src: &[u32], sw: i32) {
        if dw <= 0 || dh <= 0 || sw <= 0 {
            return;
        }
        let sh = (src.len() / sw as usize) as i64;
        if sh == 0 {
            return;
        }
        let Some((x0, y0, x1, y1)) = self.visible(dx, dy, dw, dh) else { return };
        for y in y0..y1 {
            let sy = ((y as i64 - dy as i64) * sh / dh as i64) as usize;
            let line = &src[sy * sw as usize..(sy + 1) * sw as usize];
            for (i, p) in self.row(y, x0, x1).iter_mut().enumerate() {
                *p = line[((x0 as i64 + i as i64 - dx as i64) * sw as i64 / dw as i64) as usize];
            }
        }
    }
}

// ------------------------------------------------------------------ text

/// Decodes one UTF-8 character: (code point, bytes used, at least 1).
/// Broken input gives '?'. `s` must not be empty.
pub fn utf8_decode(s: &[u8]) -> (u32, usize) {
    let c = s[0];
    let need = match c {
        0xF0.. => 3,
        0xE0.. => 2,
        0xC0.. => 1,
        _ => 0,
    };
    if c < 0x80 {
        return (c as u32, 1);
    }
    if c < 0xC0 || need >= s.len() {
        return ('?' as u32, 1);
    }
    let mut v = (c & (0x3F >> need)) as u32;
    for i in 1..=need {
        if s[i] & 0xC0 != 0x80 {
            return ('?' as u32, i);
        }
        v = v << 6 | (s[i] & 0x3F) as u32;
    }
    (v, need + 1)
}

/// The 8x16 font, copied from the VGA adapter at start-up.
static FONT: KCell<[[u8; 16]; 256]> = KCell::new([[0; 16]; 256]);

/// The Japanese font (see tools/make_font.py): "TJF1", the number of
/// glyphs, their code points in ascending order, then 32 bytes per glyph.
static FONT_JA: &[u8] = include_bytes!("../assets/font_ja.bin");

/// The 16x16 glyph of a full-width character (two bytes per row), if the
/// font has it.
pub fn font_wide(cp: u32) -> Option<&'static [u8; 32]> {
    if !(0x80..=0xFFFF).contains(&cp) || !FONT_JA.starts_with(b"TJF1") {
        return None;
    }
    let n = u32::from_le_bytes(FONT_JA.get(4..8)?.try_into().ok()?) as usize;
    let codes = FONT_JA.get(8..8 + 2 * n)?;
    let code = |i: usize| u16::from_le_bytes([codes[2 * i], codes[2 * i + 1]]) as u32;
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if code(mid) == cp {
            let at = 8 + 2 * n + 32 * mid;
            return FONT_JA.get(at..at + 32)?.try_into().ok();
        }
        if code(mid) < cp {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    None
}

// ----------------------------------------------------------- framebuffer

const BGA_INDEX: u16 = 0x1CE;
const BGA_DATA: u16 = 0x1CF;

// SAFETY (both): the Bochs display interface's own ports
fn bga_write(index: u16, value: u16) {
    unsafe {
        outw(BGA_INDEX, index);
        outw(BGA_DATA, value);
    }
}

fn bga_read(index: u16) -> u16 {
    unsafe {
        outw(BGA_INDEX, index);
        inw(BGA_DATA)
    }
}

/// Copies the 8x16 text-mode font out of VGA memory plane 2. Must run while
/// the adapter is still in text mode. False if nothing sensible was read.
fn read_vga_font() -> bool {
    let mut font = [[0u8; 16]; 256];
    // SAFETY: the VGA sequencer and graphics controller ports, and the VGA
    // memory window at 0xA0000, which the kernel maps and nothing else uses
    unsafe {
        outw(0x3CE, 0x0005); // graphics mode: no odd/even addressing
        outw(0x3CE, 0x0406); // map A0000, graphics addressing
        outw(0x3CE, 0x0204); // read plane 2
        outw(0x3C4, 0x0402); // write plane 2
        outw(0x3C4, 0x0604); // sequential addressing
        let plane = 0xA0000 as *const u8;
        for (ch, glyph) in font.iter_mut().enumerate() {
            for (row, b) in glyph.iter_mut().enumerate() {
                *b = plane.add(ch * 32 + row).read_volatile();
            }
        }
        outw(0x3C4, 0x0302); // back to text-mode settings
        outw(0x3C4, 0x0204);
        outw(0x3CE, 0x0004);
        outw(0x3CE, 0x1005);
        outw(0x3CE, 0x0E06);
    }
    let found = font[b'A' as usize].iter().chain(&font[b'g' as usize]).any(|&b| b != 0);
    FONT.with(|f| *f = font);
    found
}

/// The framebuffer's address: None = not looked for yet, Some(0) = no
/// usable display adapter.
static FB_ADDR: KCell<Option<u64>> = KCell::new(None);

/// Finds the adapter and reads the font; only possible once, while the
/// adapter is still in text mode.
fn probe() -> Option<u64> {
    let addr = FB_ADDR.with(|a| {
        *a.get_or_insert_with(|| {
            if bga_read(0) & 0xFFF0 != 0xB0C0 {
                return 0;
            }
            // The framebuffer is BAR0 of the display adapter.
            let mut addr = 0;
            'scan: for bus in 0..=255u8 {
                for dev in 0..32u8 {
                    if pci::pci_read(bus, dev, 0, 0) & 0xFFFF == 0xFFFF || pci::pci_read(bus, dev, 0, 0x08) >> 24 != 0x03 {
                        continue;
                    }
                    let bar0 = pci::pci_read(bus, dev, 0, 0x10);
                    if bar0 & 1 == 0 && bar0 & !0xF != 0 {
                        addr = (bar0 & !0xF) as u64;
                        break 'scan;
                    }
                }
            }
            if addr != 0 && read_vga_font() { addr } else { 0 }
        })
    });
    (addr != 0).then_some(addr)
}

/// Video memory in bytes; 0 without a usable adapter.
pub fn fb_vram() -> usize {
    if probe().is_none() {
        return 0;
    }
    match bga_read(0x0A) as usize * 65536 {
        0 => 16 << 20,
        v => v,
    }
}

/// The screen; set by `fb_init` only.
static SCREEN: KCell<Surface> = KCell::new(Surface::empty());

/// The screen's size in pixels (0 x 0 before `fb_init`).
pub fn screen_size() -> (i32, i32) {
    SCREEN.with(|s| (s.w, s.h))
}

/// Copies the rectangle x0..x1, y0..y1 of an image with `sw` pixels per row
/// to the screen, every pixel enlarged to a `scale` x `scale` block.
/// `line` is scratch space.
pub fn show(src: &[u32], sw: usize, (x0, y0, x1, y1): (usize, usize, usize, usize), scale: usize, line: &mut Vec<u32>) {
    SCREEN.with(|fb| {
        let (fw, fh) = (fb.w as usize, fb.h as usize);
        if fb.px.is_null() || x0 >= x1 || x1 > sw || x1 * scale > fw {
            return;
        }
        for y in y0..y1 {
            let Some(row) = src.get(y * sw + x0..y * sw + x1) else { return };
            line.clear();
            for &p in row {
                line.extend(core::iter::repeat_n(p, scale));
            }
            for k in 0..scale {
                if y * scale + k < fh {
                    fb.row(y * scale + k, x0 * scale, x1 * scale).copy_from_slice(line);
                }
            }
        }
    })
}

/// Runs `f` with a surface over a buffer of `w` x `h` pixels.
pub fn draw_on<R>(px: &mut [u32], w: i32, h: i32, f: impl FnOnce(&mut Surface) -> R) -> R {
    assert!(w >= 0 && h >= 0 && px.len() >= w as usize * h as usize);
    // SAFETY: the buffer is large enough and is borrowed for as long as the surface lives
    f(&mut unsafe { Surface::new(px.as_mut_ptr(), w, h) })
}

/// Switches to a linear framebuffer: 0 on success, -2 if the adapter has
/// too little video memory, -1 if graphics are unavailable or the mode
/// failed.
pub fn fb_init(w: i32, h: i32) -> i32 {
    let Some(addr) = probe() else { return -1 };
    if w <= 0 || h <= 0 || w > 0xFFFF || h > 0xFFFF {
        return -1;
    }
    let bytes = w as u64 * h as u64 * 4;
    if bytes > fb_vram() as u64 {
        return -2;
    }
    if addr + bytes > 0x1_0000_0000 {
        return -1; // only 4 GiB are mapped
    }
    bga_write(4, 0); // disable while changing the mode
    bga_write(1, w as u16);
    bga_write(2, h as u16);
    bga_write(3, 32);
    bga_write(4, 0x41); // enabled + linear framebuffer
    if bga_read(1) as i32 != w || bga_read(2) as i32 != h {
        return -1;
    }
    // SAFETY: the adapter now shows w * h pixels at `addr`, which is mapped
    // and has at least that much video memory (checked above)
    SCREEN.with(|s| *s = unsafe { Surface::new(addr as *mut u32, w, h) });
    0
}

// ------------------------------------------------------------ C interface
// For the console, which is still C.

/// Decodes one character of `n` (at least 1) bytes; returns the bytes used.
///
/// # Safety
/// `s` must point to `n` bytes and `cp` to a writable u32.
#[unsafe(export_name = "utf8_decode")]
pub unsafe extern "C" fn utf8_decode_c(s: *const c_char, n: usize, cp: *mut u32) -> i32 {
    let (c, used) = utf8_decode(unsafe { core::slice::from_raw_parts(s as *const u8, n.max(1)) });
    unsafe { cp.write(c) };
    used as i32
}

/// Screen columns a character takes: 2 for full-width Japanese, else 1.
#[unsafe(no_mangle)]
pub extern "C" fn font_cells(cp: u32) -> i32 {
    if font_wide(cp).is_some() { 2 } else { 1 }
}
