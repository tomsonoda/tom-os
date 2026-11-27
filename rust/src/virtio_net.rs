//! virtio-net driver (legacy I/O-port interface, polled).

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use crate::pci::{self, inb, inl, inw, outb, outl, outw};

const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_NET_LEGACY: u16 = 0x1000;

const REG_DEV_FEATURES: u16 = 0x00;
const REG_DRV_FEATURES: u16 = 0x04;
const REG_QUEUE_PFN: u16 = 0x08;
const REG_QUEUE_SIZE: u16 = 0x0C;
const REG_QUEUE_SEL: u16 = 0x0E;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_STATUS: u16 = 0x12;
const REG_MAC: u16 = 0x14;

const FEATURE_MAC: u32 = 1 << 5;
/// struct virtio_net_hdr without mergeable buffers
const HDR_LEN: usize = 10;
const BUF_SIZE: usize = 2048;
const RX_BUFS: u16 = 128;
const TX_BUFS: u16 = 256;

#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// One virtqueue: the ring memory shared with the device, and our packet
/// buffers. The device reads and writes this memory on its own, so every
/// access is volatile. The memory is never freed.
struct Queue {
    size: u16,
    /// buffers in use: min(size, what we asked for)
    slots: u16,
    desc: *mut Desc,
    /// flags, idx, ring[size]
    avail: *mut u16,
    /// flags, idx, then (id: u32, len: u32)[size]
    used: *mut u16,
    last_used: u16,
    bufs: *mut u8,
}

impl Queue {
    fn new(io: u16, index: u16, want: u16) -> Option<Queue> {
        // SAFETY: `io` is the virtio device's port range (checked by Nic::find)
        let size = unsafe {
            outw(io + REG_QUEUE_SEL, index);
            inw(io + REG_QUEUE_SIZE)
        };
        if size == 0 {
            return None;
        }
        let n = size as usize;
        let part1 = (16 * n + 6 + 2 * n + 4095) & !4095;
        let part2 = (6 + 8 * n + 4095) & !4095;
        let slots = size.min(want);
        // SAFETY: both layouts have a non-zero size
        let (mem, bufs) = unsafe {
            (
                alloc_zeroed(Layout::from_size_align(part1 + part2, 4096).ok()?),
                alloc_zeroed(Layout::from_size_align(slots as usize * BUF_SIZE, 16).ok()?),
            )
        };
        if mem.is_null() || bufs.is_null() {
            return None;
        }
        // SAFETY: the offsets are inside the block allocated above
        let q = unsafe {
            Queue { size, slots, desc: mem.cast(), avail: mem.add(16 * n).cast(), used: mem.add(part1).cast(), last_used: 0, bufs }
        };
        // SAFETY: as above; the device learns the address only here
        unsafe {
            write_volatile(q.avail, 1); // no interrupts, we poll
            outl(io + REG_QUEUE_PFN, (mem as usize >> 12) as u32);
        }
        Some(q)
    }

    // The accessors below stay inside the ring: indexes are reduced modulo
    // the queue size, slots are below `slots`.

    fn buf(&mut self, slot: u16) -> &mut [u8] {
        assert!(slot < self.slots);
        // SAFETY: slot is in range, and the device only touches a buffer
        // between our handing it over and its return through the used ring
        unsafe { core::slice::from_raw_parts_mut(self.bufs.add(slot as usize * BUF_SIZE), BUF_SIZE) }
    }
    fn set_desc(&mut self, slot: u16, len: u32, flags: u16) {
        assert!(slot < self.slots);
        let addr = self.bufs as u64 + slot as u64 * BUF_SIZE as u64;
        unsafe { write_volatile(self.desc.add(slot as usize), Desc { addr, len, flags, next: 0 }) }
    }
    fn avail_idx(&self) -> u16 {
        unsafe { read_volatile(self.avail.add(1)) }
    }
    /// Hands a buffer to the device.
    fn push_avail(&mut self, slot: u16) {
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
    fn pop_used(&mut self) -> Option<(u32, u32)> {
        if self.last_used == self.used_idx() {
            return None;
        }
        let elem = unsafe { self.used.add(2).cast::<u32>().add(2 * (self.last_used % self.size) as usize) };
        self.last_used = self.last_used.wrapping_add(1);
        Some(unsafe { (read_volatile(elem), read_volatile(elem.add(1))) })
    }
}

pub struct Nic {
    io: u16,
    mac: [u8; 6],
    rx: Queue,
    tx: Queue,
    rx_returned: bool,
}

impl Nic {
    /// Finds and starts the network card.
    pub fn find() -> Option<Nic> {
        let (bus, dev) = pci::find(VIRTIO_VENDOR, VIRTIO_NET_LEGACY)?;
        let bar0 = pci::pci_read(bus, dev, 0, 0x10);
        if bar0 & 1 == 0 {
            return None; // need the I/O-port BAR
        }
        let io = (bar0 & !3) as u16;
        pci::pci_write(bus, dev, 0, 0x04, pci::pci_read(bus, dev, 0, 0x04) | 0x05); // I/O space + bus master

        // SAFETY: `io` is this device's register block, per its BAR
        let mac = unsafe {
            outb(io + REG_STATUS, 0);
            outb(io + REG_STATUS, 1); // ACKNOWLEDGE
            outb(io + REG_STATUS, 1 | 2); // DRIVER
            let features = inl(io + REG_DEV_FEATURES);
            outl(io + REG_DRV_FEATURES, features & FEATURE_MAC);
            let mut mac = [0u8; 6];
            for (i, b) in mac.iter_mut().enumerate() {
                *b = inb(io + REG_MAC + i as u16);
            }
            mac
        };
        let mut rx = Queue::new(io, 0, RX_BUFS)?;
        let tx = Queue::new(io, 1, TX_BUFS)?;
        for slot in 0..rx.slots {
            rx.set_desc(slot, BUF_SIZE as u32, 2); // device writes
            rx.push_avail(slot);
        }
        unsafe {
            outb(io + REG_STATUS, 1 | 2 | 4); // DRIVER_OK
            outw(io + REG_QUEUE_NOTIFY, 0);
        }
        Some(Nic { io, mac, rx, tx, rx_returned: false })
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Sends one Ethernet frame given in pieces.
    pub fn send(&mut self, parts: &[&[u8]]) {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        if len > BUF_SIZE - HDR_LEN {
            return;
        }
        // wait for a free slot (the device consumes them almost immediately)
        let mut spins = 0u32;
        while self.tx.avail_idx().wrapping_sub(self.tx.used_idx()) >= self.tx.slots {
            spins += 1;
            if spins > 10_000_000 {
                return;
            }
            core::hint::spin_loop();
        }
        let slot = self.tx.avail_idx() % self.tx.slots;
        let buf = self.tx.buf(slot);
        buf[..HDR_LEN].fill(0);
        let mut at = HDR_LEN;
        for p in parts {
            buf[at..at + p.len()].copy_from_slice(p);
            at += p.len();
        }
        if at < HDR_LEN + 60 {
            buf[at..HDR_LEN + 60].fill(0); // minimum frame size
            at = HDR_LEN + 60;
        }
        self.tx.set_desc(slot, at as u32, 0);
        self.tx.push_avail(slot);
        unsafe { outw(self.io + REG_QUEUE_NOTIFY, 1) };
    }

    /// Copies the next received frame into `frame`; false if there is none.
    pub fn receive(&mut self, frame: &mut Vec<u8>) -> bool {
        while let Some((id, len)) = self.rx.pop_used() {
            let (slot, len) = (id as u16, len as usize);
            if id >= self.rx.slots as u32 {
                continue; // not a buffer of ours: a broken device
            }
            let ok = len > HDR_LEN && len <= BUF_SIZE;
            if ok {
                frame.clear();
                frame.extend_from_slice(&self.rx.buf(slot)[HDR_LEN..len]);
            }
            self.rx.push_avail(slot); // the buffer goes back to the device
            self.rx_returned = true;
            if ok {
                return true;
            }
        }
        false
    }

    /// Tells the device about the buffers `receive` gave back.
    pub fn receive_done(&mut self) {
        if self.rx_returned {
            self.rx_returned = false;
            unsafe { outw(self.io + REG_QUEUE_NOTIFY, 0) };
        }
    }
}
