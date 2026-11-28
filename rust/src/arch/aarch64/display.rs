//! The display: QEMU's "ramfb" (a framebuffer in ordinary memory whose
//! address and size the guest tells QEMU through the fw_cfg device) or a
//! virtio-gpu device, whichever the machine has. There is no text screen.

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use alloc::boxed::Box;
use alloc::vec::Vec;

use super::fdt;
use super::mmio::Registers;
use crate::mem;
use crate::rt::KCell;
use crate::virtio::{self, Kind, Queue, Transport};

/// The hint in the home screen's input box.
pub const INPUT_HINT: &str = "Ask tom-os anything...   (Option/Alt+Space: 日本語)";

/// The smallest screen the desktop is laid out for.
pub const MIN_SIZE: (i32, i32) = (360, 360); // phones are narrow

pub const TEXT_SCREEN: Option<(i32, i32)> = None;

pub fn text_flush(_cells: &[u16], _cols: usize, _dirty: u64, _cursor: (i32, i32)) {}

// fw_cfg registers
const DATA: usize = 0;
const SELECTOR: usize = 8;
const DMA: usize = 16;
const FILE_DIRECTORY: u16 = 0x19;

/// QEMU's configuration device, if the machine has it.
fn fw_cfg() -> Option<Registers> {
    let (base, _) = fdt::tree()?.compatible("qemu,fw-cfg-mmio")?.reg(0)?;
    // SAFETY: the device's registers, as the device tree says
    Some(unsafe { Registers::new(base) })
}

fn read_bytes(fw: Registers, out: &mut [u8]) {
    for b in out {
        *b = fw.read8(DATA);
    }
}

/// The fw_cfg selector of the file `name`, if QEMU has it.
fn find_file(name: &[u8]) -> Option<u16> {
    let fw = fw_cfg()?;
    fw.write16(SELECTOR, FILE_DIRECTORY.to_be());
    let mut count = [0u8; 4];
    read_bytes(fw, &mut count);
    for _ in 0..u32::from_be_bytes(count).min(1024) {
        // size (4), selector (2), reserved (2), name (56)
        let mut entry = [0u8; 64];
        read_bytes(fw, &mut entry);
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
    if selector().is_none() && !gpu_present() {
        return 0;
    }
    // the pixels come from the heap: leave most of it to everything else
    ((mem::total() - mem::used()) / 4).min(64 << 20)
}

/// Switches to a mode of `w` x `h` pixels of 32 bits and returns where the
/// pixels are, or None if that is not possible.
pub fn set_mode(w: i32, h: i32) -> Option<*mut u32> {
    match selector() {
        Some(select) => ramfb_set_mode(select, w, h),
        None => gpu_set_mode(w as u32, h as u32),
    }
}

fn ramfb_set_mode(select: u16, w: i32, h: i32) -> Option<*mut u32> {
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
    fw_cfg()?.write64(DMA, (&raw const access as u64).to_be());
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

// ------------------------------------------------------------ virtio-gpu
// The pixels are in our memory; the device shows a copy that it takes
// when told to (`present`).

const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESPONSE_OK: u32 = 0x1100;
/// 32 bits per pixel, blue first: 0x00RRGGBB as a little-endian word
const FORMAT_BGRX: u32 = 2;

struct Gpu {
    dev: Box<dyn Transport>,
    control: Queue,
    /// the picture being shown: its id (0 = none), size, pixels
    resource: u32,
    width: u32,
    height: u32,
    pixels: *mut u8,
    layout: Option<Layout>,
}

/// None = not looked for yet.
static GPU: KCell<Option<Option<Gpu>>> = KCell::new(None);

fn gpu_start() -> Option<Gpu> {
    let mut dev = virtio::find(Kind::Gpu).into_iter().next()?;
    dev.set_status(0);
    dev.set_status(1); // acknowledge
    dev.set_status(1 | 2); // driver
    dev.set_driver_features(0);
    let control = Queue::new(dev.as_mut(), 0, 2, 128)?;
    dev.set_status(1 | 2 | 4); // driver ok
    Some(Gpu { dev, control, resource: 0, width: 0, height: 0, pixels: core::ptr::null_mut(), layout: None })
}

fn with_gpu<R>(f: impl FnOnce(&mut Gpu) -> Option<R>) -> Option<R> {
    GPU.with(|slot| slot.get_or_insert_with(gpu_start).as_mut().and_then(f))
}

fn gpu_present() -> bool {
    with_gpu(|_| Some(())).is_some()
}

impl Gpu {
    /// Sends a command (its type, then 32-bit words after the 24-byte
    /// header); true if the device says OK.
    fn command(&mut self, kind: u32, words: &[u32]) -> bool {
        let mut request = Vec::with_capacity(24 + 4 * words.len());
        request.extend_from_slice(&kind.to_le_bytes());
        request.extend_from_slice(&[0; 20]); // flags, fence, context, padding
        for w in words {
            request.extend_from_slice(&w.to_le_bytes());
        }
        self.control
            .request(self.dev.as_mut(), &request, 24)
            .is_some_and(|answer| u32::from_le_bytes([answer[0], answer[1], answer[2], answer[3]]) == RESPONSE_OK)
    }

    fn set_mode(&mut self, w: u32, h: u32) -> Option<*mut u32> {
        let layout = Layout::from_size_align(w as usize * h as usize * 4, 4096).ok()?;
        // SAFETY: the layout has a non-zero size (the caller checked w and h)
        let pixels = unsafe { alloc_zeroed(layout) };
        if pixels.is_null() {
            return None;
        }
        let id = self.resource + 1;
        let (addr, len) = (pixels as u64, layout.size() as u32);
        let ok = self.command(CMD_RESOURCE_CREATE_2D, &[id, FORMAT_BGRX, w, h])
            && self.command(CMD_RESOURCE_ATTACH_BACKING, &[id, 1, addr as u32, (addr >> 32) as u32, len, 0])
            && self.command(CMD_SET_SCANOUT, &[0, 0, w, h, 0, id]);
        if !ok {
            self.command(CMD_RESOURCE_UNREF, &[id, 0]);
            // SAFETY: allocated above with this layout; the device has let go of it
            unsafe { dealloc(pixels, layout) };
            return None;
        }
        if let Some(old) = self.layout.replace(layout) {
            self.command(CMD_RESOURCE_UNREF, &[self.resource, 0]);
            // SAFETY: the previous picture's pixels, which the device no longer uses
            unsafe { dealloc(self.pixels, old) };
        }
        (self.resource, self.width, self.height, self.pixels) = (id, w, h, pixels);
        Some(pixels as *mut u32)
    }

    fn present(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) {
        let (x1, y1) = (x1.min(self.width), y1.min(self.height));
        if self.resource == 0 || x0 >= x1 || y0 >= y1 {
            return;
        }
        let rect = [x0, y0, x1 - x0, y1 - y0];
        let offset = (y0 as u64 * self.width as u64 + x0 as u64) * 4;
        self.command(CMD_TRANSFER_TO_HOST_2D, &[rect[0], rect[1], rect[2], rect[3], offset as u32, (offset >> 32) as u32, self.resource, 0]);
        self.command(CMD_RESOURCE_FLUSH, &[rect[0], rect[1], rect[2], rect[3], self.resource, 0]);
    }
}

fn gpu_set_mode(w: u32, h: u32) -> Option<*mut u32> {
    with_gpu(|g| g.set_mode(w, h))
}

/// The pixels in the rectangle x0..x1, y0..y1 have changed: show them.
pub fn present(x0: usize, y0: usize, x1: usize, y1: usize) {
    if selector().is_none() {
        with_gpu(|g| Some(g.present(x0 as u32, y0 as u32, x1 as u32, y1 as u32)));
    }
}
