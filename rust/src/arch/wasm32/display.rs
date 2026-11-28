//! The display: a canvas on the page. The pixels are in our memory; the
//! page copies the changed part to the canvas when told to.

use alloc::alloc::{Layout, alloc_zeroed, dealloc};

use super::js;
use crate::rt::KCell;

/// The hint in the home screen's input box. The browser version's is in
/// English: Japanese is typed with the input method of the browser's own
/// system.
pub const INPUT_HINT: &str = "Ask tom-os anything...";

/// The smallest screen the desktop is laid out for (phones are narrow).
pub const MIN_SIZE: (i32, i32) = (360, 360);

pub const TEXT_SCREEN: Option<(i32, i32)> = None;

pub fn text_flush(_cells: &[u16], _cols: usize, _dirty: u64, _cursor: (i32, i32)) {}

/// The pixels shown, and their layout for freeing them.
static PIXELS: KCell<Option<(*mut u8, Layout)>> = KCell::new(None);

/// Memory the display may use, in bytes.
pub fn vram() -> usize {
    32 << 20
}

/// Switches to a picture of `w` x `h` pixels of 32 bits and returns where
/// the pixels are.
pub fn set_mode(w: i32, h: i32) -> Option<*mut u32> {
    let layout = Layout::from_size_align(w as usize * h as usize * 4, 16).ok()?;
    // SAFETY: the layout has a non-zero size (the caller checked w and h)
    let pixels = unsafe { alloc_zeroed(layout) };
    if pixels.is_null() {
        return None;
    }
    // SAFETY: w * h pixels at `pixels`, which stay until the next mode
    unsafe { js::js_mode(pixels as *const u32, w as u32, h as u32) };
    if let Some((old, old_layout)) = PIXELS.with(|p| p.replace((pixels, layout))) {
        // SAFETY: the previous picture, which the page no longer reads
        unsafe { dealloc(old, old_layout) };
    }
    Some(pixels as *mut u32)
}

/// The pixels in the rectangle x0..x1, y0..y1 have changed: show them.
pub fn present(x0: usize, y0: usize, x1: usize, y1: usize) {
    js::js_present(x0 as u32, y0 as u32, x1 as u32, y1 as u32);
}
