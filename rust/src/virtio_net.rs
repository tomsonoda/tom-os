//! virtio-net driver (polled).

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::virtio::{self, Kind, Queue, Transport};

const FEATURE_MAC: u32 = 1 << 5;
const BUF_SIZE: usize = 2048;
const RX_BUFS: u16 = 128;
const TX_BUFS: u16 = 256;

pub struct Nic {
    dev: Box<dyn Transport>,
    mac: [u8; 6],
    rx: Queue,
    tx: Queue,
    rx_returned: bool,
    /// the length of the header before each packet: 10 bytes with the
    /// legacy interface, 12 with the current one
    header: usize,
}

impl Nic {
    /// Finds and starts the network card.
    pub fn find() -> Option<Nic> {
        let mut dev = virtio::find(Kind::Net).into_iter().next()?;
        dev.set_status(0);
        dev.set_status(1); // acknowledge
        dev.set_status(1 | 2); // driver
        let features = dev.device_features();
        dev.set_driver_features(features & FEATURE_MAC);
        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = dev.config(i);
        }
        let mut rx = Queue::new(dev.as_mut(), 0, RX_BUFS, BUF_SIZE)?;
        let tx = Queue::new(dev.as_mut(), 1, TX_BUFS, BUF_SIZE)?;
        dev.set_status(1 | 2 | 4); // driver ok
        rx.fill(dev.as_mut());
        let header = if dev.modern() { 12 } else { 10 };
        Some(Nic { dev, mac, rx, tx, rx_returned: false, header })
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Sends one Ethernet frame given in pieces.
    pub fn send(&mut self, parts: &[&[u8]]) {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        if len > BUF_SIZE - self.header {
            return;
        }
        // wait for a free slot (the device consumes them almost immediately)
        let mut spins = 0u32;
        let slot = loop {
            if let Some(slot) = self.tx.free_slot() {
                break slot;
            }
            spins += 1;
            if spins > 10_000_000 {
                return;
            }
            core::hint::spin_loop();
        };
        let header = self.header;
        let buf = self.tx.buf(slot);
        buf[..header].fill(0);
        let mut at = header;
        for p in parts {
            buf[at..at + p.len()].copy_from_slice(p);
            at += p.len();
        }
        if at < header + 60 {
            buf[at..header + 60].fill(0); // minimum frame size
            at = header + 60;
        }
        self.tx.set_desc(slot, at as u32, 0);
        self.tx.push_avail(slot);
        self.tx.notify(self.dev.as_mut());
    }

    /// Copies the next received frame into `frame`; false if there is none.
    pub fn receive(&mut self, frame: &mut Vec<u8>) -> bool {
        while let Some((id, len)) = self.rx.pop_used() {
            let (slot, len) = (id as u16, len as usize);
            if id >= self.rx.slots as u32 {
                continue; // not a buffer of ours: a broken device
            }
            let header = self.header;
            let ok = len > header && len <= BUF_SIZE;
            if ok {
                frame.clear();
                frame.extend_from_slice(&self.rx.buf(slot)[header..len]);
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
            self.rx.notify(self.dev.as_mut());
        }
    }
}
