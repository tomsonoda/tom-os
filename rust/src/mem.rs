//! Boot information and the kernel heap (boundary-tag, first-fit allocator).

use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::null_mut;

use crate::arch::boot;
use crate::rt::KCell;

// ------------------------------------------------------------------ heap

/// The header of every block. Free blocks continue with two list pointers.
#[repr(C)]
struct Block {
    /// the whole block including this header; bit 0 = in use
    size: usize,
    /// size of the block before this one, 0 for the first
    prev: usize,
}

#[repr(C)]
struct FreeBlock {
    head: Block,
    next: *mut FreeBlock,
    prev_free: *mut FreeBlock,
}

const HEADER: usize = size_of::<Block>();
const MIN_BLOCK: usize = 32;

struct Heap {
    lo: usize,
    hi: usize,
    free_list: *mut FreeBlock,
    used: usize,
}

/// The heap is used from everywhere, also while other kernel state is
/// borrowed, so it has no borrow flag of its own.
struct HeapCell(UnsafeCell<Heap>);

// SAFETY: one CPU, cooperative threads, and the allocator never yields or
// calls out, so only one of its functions runs at a time.
unsafe impl Sync for HeapCell {}

static HEAP: HeapCell = HeapCell(UnsafeCell::new(Heap { lo: 0, hi: 0, free_list: null_mut(), used: 0 }));

// Everything below upholds one invariant: the range lo..hi is covered by
// blocks whose headers describe their sizes exactly, and the free list
// links exactly the blocks that are not in use.
impl Heap {
    unsafe fn size(b: *mut Block) -> usize {
        unsafe { (*b).size & !1 }
    }

    unsafe fn in_use(b: *mut Block) -> bool {
        unsafe { (*b).size & 1 != 0 }
    }

    /// The block after `b`, or null at the end of the heap.
    unsafe fn next(&self, b: *mut Block) -> *mut Block {
        let n = b as usize + unsafe { Self::size(b) };
        if n < self.hi { n as *mut Block } else { null_mut() }
    }

    unsafe fn list_insert(&mut self, f: *mut FreeBlock) {
        unsafe {
            (*f).next = self.free_list;
            (*f).prev_free = null_mut();
            if !self.free_list.is_null() {
                (*self.free_list).prev_free = f;
            }
        }
        self.free_list = f;
    }

    unsafe fn list_remove(&mut self, f: *mut FreeBlock) {
        unsafe {
            if (*f).prev_free.is_null() {
                self.free_list = (*f).next;
            } else {
                (*(*f).prev_free).next = (*f).next;
            }
            if !(*f).next.is_null() {
                (*(*f).next).prev_free = (*f).prev_free;
            }
        }
    }

    /// Marks `b` (free, off the list, `have` bytes) as used with `need`
    /// bytes, returning the tail to the free list when it is big enough.
    unsafe fn carve(&mut self, b: *mut Block, mut have: usize, need: usize) {
        unsafe {
            if have - need >= MIN_BLOCK {
                let rest = (b as usize + need) as *mut Block;
                (*rest).size = have - need;
                (*rest).prev = need;
                let after = self.next(rest);
                if !after.is_null() {
                    (*after).prev = have - need;
                }
                self.list_insert(rest.cast());
                have = need;
            }
            (*b).size = have | 1;
        }
        self.used += have;
    }

    /// The block size for a request of `n` bytes.
    fn block_size(n: usize) -> Option<usize> {
        if n > 1 << 40 {
            return None;
        }
        Some(((n + HEADER + 15) & !15).max(MIN_BLOCK))
    }

    unsafe fn alloc(&mut self, n: usize) -> *mut u8 {
        let Some(need) = Self::block_size(n) else { return null_mut() };
        let mut f = self.free_list;
        while !f.is_null() {
            unsafe {
                let have = Self::size(f.cast());
                if have >= need {
                    self.list_remove(f);
                    self.carve(f.cast(), have, need);
                    return (f as usize + HEADER) as *mut u8;
                }
                f = (*f).next;
            }
        }
        null_mut()
    }

    unsafe fn free(&mut self, p: *mut u8) {
        if p.is_null() {
            return;
        }
        unsafe {
            let mut b = (p as usize - HEADER) as *mut Block;
            let mut size = Self::size(b);
            self.used -= size;
            (*b).size = size;
            // join with free neighbours
            let n = self.next(b);
            if !n.is_null() && !Self::in_use(n) {
                self.list_remove(n.cast());
                size += Self::size(n);
                (*b).size = size;
            }
            if (*b).prev != 0 {
                let before = (b as usize - (*b).prev) as *mut Block;
                if !Self::in_use(before) {
                    self.list_remove(before.cast());
                    size += Self::size(before);
                    (*before).size = size;
                    b = before;
                }
            }
            let n = self.next(b);
            if !n.is_null() {
                (*n).prev = size;
            }
            self.list_insert(b.cast());
        }
    }

    /// Grows a block in place if the block after it is free and large
    /// enough; true if the block now holds `n` bytes.
    unsafe fn grow(&mut self, p: *mut u8, n: usize) -> bool {
        let Some(need) = Self::block_size(n) else { return false };
        unsafe {
            let b = (p as usize - HEADER) as *mut Block;
            let have = Self::size(b);
            if need <= have {
                return true;
            }
            let next = self.next(b);
            if next.is_null() || Self::in_use(next) || have + Self::size(next) < need {
                return false;
            }
            let total = have + Self::size(next);
            self.list_remove(next.cast());
            self.used -= have;
            (*b).size = total;
            let after = self.next(b);
            if !after.is_null() {
                (*after).prev = total;
            }
            self.carve(b, total, need);
            true
        }
    }
}

fn heap() -> &'static mut Heap {
    // SAFETY: see HeapCell: no two uses overlap
    unsafe { &mut *HEAP.0.get() }
}

/// Rust's allocator. Blocks are aligned to 16 bytes; for a larger alignment
/// the block is over-allocated and its real start is kept just below the
/// aligned address.
struct KernelHeap;

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe {
            if layout.align() <= 16 {
                return heap().alloc(layout.size().max(1));
            }
            let raw = heap().alloc(layout.size() + layout.align() + 8);
            if raw.is_null() {
                return raw;
            }
            let aligned = (raw as usize + 8 + layout.align() - 1) & !(layout.align() - 1);
            *((aligned - 8) as *mut usize) = raw as usize;
            aligned as *mut u8
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            if layout.align() <= 16 {
                heap().free(ptr)
            } else {
                heap().free(*((ptr as usize - 8) as *const usize) as *mut u8)
            }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe {
            if layout.align() <= 16 && heap().grow(ptr, new_size) {
                return ptr;
            }
            let new = self.alloc(Layout::from_size_align_unchecked(new_size, layout.align()));
            if !new.is_null() {
                core::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
            new
        }
    }
}

#[global_allocator]
static ALLOCATOR: KernelHeap = KernelHeap;

/// The size of the heap in bytes.
pub fn total() -> usize {
    heap().hi - heap().lo
}

/// Bytes of the heap in use.
pub fn used() -> usize {
    heap().used
}

// ------------------------------------------------------------- boot info

static COMMAND_LINE: KCell<Vec<u8>> = KCell::new(Vec::new());

/// A kernel argument: the value of `key=value` on the command line.
pub fn arg(key: &str) -> Option<String> {
    COMMAND_LINE.with(|line| {
        line.split(|&b| b == b' ').find_map(|token| {
            let value = token.strip_prefix(key.as_bytes())?.strip_prefix(b"=")?;
            Some(String::from_utf8_lossy(value).into_owned())
        })
    })
}

/// Reads what the boot loader passed and sets up the heap.
///
/// # Safety
/// `boot_arg` must be what the boot loader gave to the kernel, and this
/// must run once, before anything allocates.
pub unsafe fn init(boot_arg: usize) {
    let info = unsafe { boot::info(boot_arg) };
    let h = heap();
    h.lo = (info.heap_base + 15) & !15;
    h.hi = (info.heap_base + info.heap_len) & !15;
    let first = h.lo as *mut FreeBlock;
    // SAFETY: the range is free memory according to the boot loader
    unsafe {
        (*first).head = Block { size: h.hi - h.lo, prev: 0 };
        h.free_list = null_mut();
        h.list_insert(first);
    }

    COMMAND_LINE.with(|line| *line = info.command_line.to_vec());
}
