//! The display: QEMU's "ramfb", a framebuffer in ordinary memory whose
//! address and size the guest tells QEMU through the fw_cfg device. There
//! is no text screen.

use alloc::alloc::{Layout, alloc_zeroed, dealloc};

use super::mmio::FW_CFG;
use crate::mem;
use crate::rt::KCell;

pub const TEXT_SCREEN: Option<(i32, i32)> = None;

pub fn text_flush(_cells: &[u16], _cols: usize, _dirty: u64, _cursor: (i32, i32)) {}

// fw_cfg registers
const DATA: usize = 0;
const SELECTOR: usize = 8;
const DMA: usize = 16;
const FILE_DIRECTORY: u16 = 0x19;

fn read_bytes(out: &mut [u8]) {
    for b in out {
        *b = FW_CFG.read8(DATA);
    }
}

/// The fw_cfg selector of the file `name`, if QEMU has it.
fn find_file(name: &[u8]) -> Option<u16> {
    FW_CFG.write16(SELECTOR, FILE_DIRECTORY.to_be());
    let mut count = [0u8; 4];
    read_bytes(&mut count);
    for _ in 0..u32::from_be_bytes(count).min(1024) {
        // size (4), selector (2), reserved (2), name (56)
        let mut entry = [0u8; 64];
        read_bytes(&mut entry);
        if entry[8..].starts_with(name) && entry[8 + name.len()] == 0 {
            return Some(u16::from_be_bytes([entry[4], entry[5]]));
        }
    }
    None
}

struct Ramfb {
    /// None = not looked for yet, Some(0) = QEMU was started without it
    selector: Option<u16>,
    /// the pixels shown, and their layout for freeing them
    pixels: *mut u8,
    layout: Option<Layout>,
}

static RAMFB: KCell<Ramfb> = KCell::new(Ramfb { selector: None, pixels: core::ptr::null_mut(), layout: None });

fn selector() -> Option<u16> {
    let s = RAMFB.with(|r| *r.selector.get_or_insert_with(|| find_file(b"etc/ramfb").unwrap_or(0)));
    (s != 0).then_some(s)
}

/// Memory the display may use, in bytes; 0 without a display.
pub fn vram() -> usize {
    if selector().is_none() {
        return 0;
    }
    // the pixels come from the heap: leave most of it to everything else
    ((mem::total() - mem::used()) / 4).min(64 << 20)
}

/// Switches to a mode of `w` x `h` pixels of 32 bits and returns where the
/// pixels are, or None if that is not possible.
pub fn set_mode(w: i32, h: i32) -> Option<*mut u32> {
    let select = selector()?;
    let layout = Layout::from_size_align(w as usize * h as usize * 4, 4096).ok()?;
    // SAFETY: the layout has a non-zero size (the caller checked w and h)
    let pixels = unsafe { alloc_zeroed(layout) };
    if pixels.is_null() {
        return None;
    }
    // The configuration, big-endian: address, format, flags, width,
    // height, bytes per row.
    let mut config = [0u8; 28];
    config[0..8].copy_from_slice(&(pixels as u64).to_be_bytes());
    config[8..12].copy_from_slice(b"\x34\x32\x52\x58"); // "XR24": 32 bits, blue first
    config[16..20].copy_from_slice(&(w as u32).to_be_bytes());
    config[20..24].copy_from_slice(&(h as u32).to_be_bytes());
    config[24..28].copy_from_slice(&(w as u32 * 4).to_be_bytes());
    // Written through fw_cfg's DMA interface: control (select the file and
    // write), length, address.
    #[repr(C, align(8))]
    struct Access([u8; 16]);
    let mut access = Access([0; 16]);
    access.0[0..4].copy_from_slice(&((select as u32) << 16 | 0x08 | 0x10).to_be_bytes());
    access.0[4..8].copy_from_slice(&(config.len() as u32).to_be_bytes());
    access.0[8..16].copy_from_slice(&(config.as_ptr() as u64).to_be_bytes());
    FW_CFG.write64(DMA, (&raw const access as u64).to_be());
    let failed = core::hint::black_box(&access).0[..4] != [0; 4]; // QEMU clears the control word when done
    RAMFB.with(|r| {
        if failed {
            // SAFETY: allocated above with this layout, and not shown
            unsafe { dealloc(pixels, layout) };
            return None;
        }
        if let Some(old) = r.layout.replace(layout) {
            // SAFETY: the previous mode's pixels, which QEMU no longer shows
            unsafe { dealloc(r.pixels, old) };
        }
        r.pixels = pixels;
        Some(pixels as *mut u32)
    })
}
