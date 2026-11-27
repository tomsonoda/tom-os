//! virtio devices, "legacy" interface: the queues shared with a device.
//! How a device is found and its registers are reached differs between
//! machines (PCI I/O ports on a PC, memory-mapped registers on ARM); that
//! is behind `Transport`, provided by `arch::virtio`.

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

/// Kinds of device (their virtio device ids).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Net = 1,
    Input = 18,
}

/// The registers of one device.
pub trait Transport {
    fn device_features(&mut self) -> u32;
    fn set_driver_features(&mut self, features: u32);
    /// Device status bits: 1 acknowledge, 2 driver, 4 driver ok; 0 resets.
    fn set_status(&mut self, status: u8);
    /// A byte of the device-specific configuration.
    fn config(&mut self, offset: usize) -> u8;
    /// True for version 1 of the virtio standard (as opposed to "legacy"):
    /// among other things, network packets then have a longer header.
    fn modern(&self) -> bool {
        false
    }
    /// The largest size of queue `index`; 0 if there is no such queue.
    fn queue_max(&mut self, index: u16) -> u16;
    /// True if a queue must have exactly its largest size.
    fn queue_size_fixed(&self) -> bool;
    /// Tells the device where queue `index` of `size` entries is; `addr`
    /// is page-aligned memory laid out as the legacy interface says.
    fn queue_set(&mut self, index: u16, size: u16, addr: usize);
    /// Tells the device that queue `index` has new buffers.
    fn notify(&mut self, index: u16);
}

#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// The device writes into the buffer (descriptor flag).
pub const DEVICE_WRITES: u16 = 2;

/// One virtqueue: the ring memory shared with the device, and our buffers
/// of `buf_size` bytes each. The device reads and writes this memory on
/// its own, so every access is volatile. The memory is never freed.
pub struct Queue {
    index: u16,
    size: u16,
    /// buffers in use: min(size, what was asked for)
    pub slots: u16,
    buf_size: usize,
    desc: *mut Desc,
    /// flags, idx, ring[size]
    avail: *mut u16,
    /// flags, idx, then (id: u32, len: u32)[size]
    used: *mut u16,
    last_used: u16,
    bufs: *mut u8,
}

impl Queue {
    pub fn new(t: &mut dyn Transport, index: u16, want: u16, buf_size: usize) -> Option<Queue> {
        let max = t.queue_max(index);
        if max == 0 {
            return None;
        }
        let size = if t.queue_size_fixed() { max } else { max.min(256) };
        let n = size as usize;
        let part1 = (16 * n + 6 + 2 * n + 4095) & !4095;
        let part2 = (6 + 8 * n + 4095) & !4095;
        let slots = size.min(want);
        // SAFETY: both layouts have a non-zero size
        let (mem, bufs) = unsafe {
            (
                alloc_zeroed(Layout::from_size_align(part1 + part2, 4096).ok()?),
                alloc_zeroed(Layout::from_size_align(slots as usize * buf_size, 16).ok()?),
            )
        };
        if mem.is_null() || bufs.is_null() {
            return None;
        }
        // SAFETY: the offsets are inside the block allocated above
        let q = unsafe {
            Queue { index, size, slots, buf_size, desc: mem.cast(), avail: mem.add(16 * n).cast(), used: mem.add(part1).cast(), last_used: 0, bufs }
        };
        // SAFETY: as above; the device learns the address only here
        unsafe { write_volatile(q.avail, 1) }; // no interrupts, we poll
        t.queue_set(index, size, mem as usize);
        Some(q)
    }

    // The accessors below stay inside the ring: indexes are reduced modulo
    // the queue size, slots are below `slots`.

    pub fn buf(&mut self, slot: u16) -> &mut [u8] {
        assert!(slot < self.slots);
        // SAFETY: slot is in range, and the device only touches a buffer
        // between our handing it over and its return through the used ring
        unsafe { core::slice::from_raw_parts_mut(self.bufs.add(slot as usize * self.buf_size), self.buf_size) }
    }

    pub fn set_desc(&mut self, slot: u16, len: u32, flags: u16) {
        assert!(slot < self.slots);
        let addr = self.bufs as u64 + slot as u64 * self.buf_size as u64;
        unsafe { write_volatile(self.desc.add(slot as usize), Desc { addr, len, flags, next: 0 }) }
    }

    fn avail_idx(&self) -> u16 {
        unsafe { read_volatile(self.avail.add(1)) }
    }

    /// Hands a buffer to the device (`notify` tells it).
    pub fn push_avail(&mut self, slot: u16) {
        let idx = self.avail_idx();
        unsafe { write_volatile(self.avail.add(2 + (idx % self.size) as usize), slot) };
        fence(Ordering::SeqCst);
        unsafe { write_volatile(self.avail.add(1), idx.wrapping_add(1)) };
        fence(Ordering::SeqCst);
    }

    fn used_idx(&self) -> u16 {
        unsafe { read_volatile(self.used.add(1)) }
    }

    /// The next buffer the device has finished with, as (slot, length).
    pub fn pop_used(&mut self) -> Option<(u32, u32)> {
        if self.last_used == self.used_idx() {
            return None;
        }
        let elem = unsafe { self.used.add(2).cast::<u32>().add(2 * (self.last_used % self.size) as usize) };
        self.last_used = self.last_used.wrapping_add(1);
        Some(unsafe { (read_volatile(elem), read_volatile(elem.add(1))) })
    }

    /// A slot for sending that the device is not using, if there is one.
    pub fn free_slot(&self) -> Option<u16> {
        let idx = self.avail_idx();
        (idx.wrapping_sub(self.used_idx()) < self.slots).then_some(idx % self.slots)
    }

    pub fn notify(&self, t: &mut dyn Transport) {
        t.notify(self.index);
    }

    /// Gives all buffers to the device to write into (a receive queue).
    pub fn fill(&mut self, t: &mut dyn Transport) {
        for slot in 0..self.slots {
            self.set_desc(slot, self.buf_size as u32, DEVICE_WRITES);
            self.push_avail(slot);
        }
        self.notify(t);
    }
}

/// The devices of one kind that the machine has.
pub fn find(kind: Kind) -> Vec<alloc::boxed::Box<dyn Transport>> {
    crate::arch::virtio::find(kind)
}
