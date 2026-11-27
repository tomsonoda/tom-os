//! Ethernet, ARP and IPv4 (static address, no fragmentation).
//!
//! All network state lives in one `Net` value. Code that works on it never
//! waits; the functions that wait (`resolve` here, the calls in `tcp`) take
//! it for short steps and idle in between, which is when received frames
//! are processed.

use alloc::vec::Vec;
use core::ffi::c_char;

use crate::rt::KCell;
use crate::sys;
use crate::tcp::Tcp;
use crate::virtio_net::Nic;

const ETH_ARP: u16 = 0x0806;
const ETH_IP: u16 = 0x0800;
pub const PROTO_TCP: u8 = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetError {
    Timeout,
    Break,
    Reset,
    NoDevice,
    Protocol,
    Busy,
}

impl NetError {
    pub fn text(self) -> &'static str {
        match self {
            NetError::Timeout => "timed out",
            NetError::Break => "interrupted",
            NetError::Reset => "connection reset",
            NetError::NoDevice => "no network device",
            NetError::Protocol => "protocol error",
            NetError::Busy => "the network is busy with another request",
        }
    }
}

pub fn now() -> u64 {
    unsafe { sys::time_ms() }
}

/// The caller is waiting: lets other threads run (they poll the network).
pub fn idle() {
    unsafe { sys::cpu_idle() }
}

pub fn get16(p: &[u8]) -> u16 {
    u16::from_be_bytes([p[0], p[1]])
}
pub fn get32(p: &[u8]) -> u32 {
    u32::from_be_bytes([p[0], p[1], p[2], p[3]])
}
pub fn put16(p: &mut [u8], v: u16) {
    p[..2].copy_from_slice(&v.to_be_bytes());
}
pub fn put32(p: &mut [u8], v: u32) {
    p[..4].copy_from_slice(&v.to_be_bytes());
}

/// Internet checksum, part one: adds 16-bit words. `data` must have an even
/// length unless it is the last piece.
pub fn sum(mut acc: u32, data: &[u8]) -> u32 {
    let mut words = data.chunks_exact(2);
    for w in &mut words {
        acc = acc.wrapping_add(u16::from_be_bytes([w[0], w[1]]) as u32);
    }
    if let [last] = words.remainder() {
        acc = acc.wrapping_add((*last as u32) << 8);
    }
    acc
}

/// Part two: folds the sum into the checksum.
pub fn fold(mut acc: u32) -> u16 {
    while acc >> 16 != 0 {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    !(acc as u16)
}

pub fn parse_ip(s: &str) -> Option<u32> {
    let mut parts = s.split('.');
    let mut ip = 0u32;
    for _ in 0..4 {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        ip = ip << 8 | part.parse::<u8>().ok()? as u32;
    }
    parts.next().is_none().then_some(ip)
}

pub(crate) struct Net {
    nic: Option<Nic>,
    mac: [u8; 6],
    pub ip: u32,
    gateway: u32,
    netmask: u32,
    arp: [(u32, [u8; 6]); 8],
    arp_next: usize,
    ident: u16,
    /// the frame being processed; kept to reuse its memory
    frame: Vec<u8>,
    pub tcp: Tcp,
}

pub(crate) static NET: KCell<Net> = KCell::new(Net {
    nic: None,
    mac: [0; 6],
    ip: 0,
    gateway: 0,
    netmask: 0,
    arp: [(0, [0; 6]); 8],
    arp_next: 0,
    ident: 0,
    frame: Vec::new(),
    tcp: Tcp::new(),
});

impl Net {
    pub fn up(&self) -> bool {
        self.nic.is_some()
    }

    fn eth_send(&mut self, dst: &[u8; 6], ethertype: u16, a: &[u8], b: &[u8], c: &[u8]) {
        let mut head = [0u8; 14];
        head[..6].copy_from_slice(dst);
        head[6..12].copy_from_slice(&self.mac);
        put16(&mut head[12..], ethertype);
        if let Some(nic) = &mut self.nic {
            nic.send(&[&head, a, b, c]);
        }
    }

    // ---------------------------------------------------------------- ARP

    fn arp_learn(&mut self, ip: u32, mac: [u8; 6]) {
        if let Some(e) = self.arp.iter_mut().find(|e| e.0 == ip) {
            e.1 = mac;
        } else {
            self.arp[self.arp_next] = (ip, mac);
            self.arp_next = (self.arp_next + 1) % self.arp.len();
        }
    }

    fn arp_lookup(&self, ip: u32) -> Option<[u8; 6]> {
        self.arp.iter().find(|e| e.0 == ip && ip != 0).map(|e| e.1)
    }

    fn arp_send(&mut self, op: u16, dst_mac: &[u8; 6], target_mac: &[u8; 6], target_ip: u32) {
        let mut p = [0u8; 28];
        put16(&mut p[0..], 1);
        put16(&mut p[2..], ETH_IP);
        p[4] = 6;
        p[5] = 4;
        put16(&mut p[6..], op);
        p[8..14].copy_from_slice(&self.mac);
        put32(&mut p[14..], self.ip);
        p[18..24].copy_from_slice(target_mac);
        put32(&mut p[24..], target_ip);
        self.eth_send(dst_mac, ETH_ARP, &p, &[], &[]);
    }

    fn arp_request(&mut self, ip: u32) {
        self.arp_send(1, &[255; 6], &[0; 6], ip);
    }

    fn arp_input(&mut self, p: &[u8]) {
        if p.len() < 28 || get16(p) != 1 || get16(&p[2..]) != ETH_IP {
            return;
        }
        let op = get16(&p[6..]);
        let (sender, target) = (get32(&p[14..]), get32(&p[24..]));
        let mut sender_mac = [0u8; 6];
        sender_mac.copy_from_slice(&p[8..14]);
        if sender != 0 {
            self.arp_learn(sender, sender_mac);
        }
        if op == 1 && target == self.ip {
            self.arp_send(2, &sender_mac, &sender_mac, sender);
        }
    }

    // --------------------------------------------------------------- IPv4

    /// Where packets for `dst` go on the local network.
    fn next_hop(&self, dst: u32) -> u32 {
        if (dst ^ self.ip) & self.netmask != 0 { self.gateway } else { dst }
    }

    /// Sends one packet, its payload given in two pieces. If the hardware
    /// address of the next hop is not known yet, the packet is dropped and
    /// the address is asked for: like any lost packet, TCP sends it again.
    pub fn ip_send(&mut self, dst: u32, proto: u8, a: &[u8], b: &[u8]) -> Result<(), NetError> {
        if !self.up() {
            return Err(NetError::NoDevice);
        }
        let len = a.len() + b.len();
        if len > 1500 {
            return Err(NetError::Protocol);
        }
        let hop = self.next_hop(dst);
        let Some(mac) = self.arp_lookup(hop) else {
            self.arp_request(hop);
            return Ok(());
        };
        self.ident = self.ident.wrapping_add(1);
        let mut h = [0u8; 20];
        h[0] = 0x45;
        put16(&mut h[2..], (len + 20) as u16);
        put16(&mut h[4..], self.ident);
        put16(&mut h[6..], 0x4000); // don't fragment
        h[8] = 64;
        h[9] = proto;
        put32(&mut h[12..], self.ip);
        put32(&mut h[16..], dst);
        let check = fold(sum(0, &h));
        put16(&mut h[10..], check);
        self.eth_send(&mac, ETH_IP, &h, a, b);
        Ok(())
    }

    fn ip_input(&mut self, p: &[u8]) {
        if p.len() < 20 || p[0] >> 4 != 4 {
            return;
        }
        let header = (p[0] & 15) as usize * 4;
        let total = get16(&p[2..]) as usize;
        if header < 20 || total < header || total > p.len() {
            return;
        }
        if get32(&p[16..]) != self.ip {
            return;
        }
        if get16(&p[6..]) & 0x3FFF != 0 {
            return; // fragments are not supported
        }
        if p[9] == PROTO_TCP {
            self.tcp_input(get32(&p[12..]), &p[header..total]);
        }
    }

    fn frame_input(&mut self, f: &[u8]) {
        if f.len() < 14 {
            return;
        }
        match get16(&f[12..]) {
            ETH_ARP => self.arp_input(&f[14..]),
            ETH_IP => self.ip_input(&f[14..]),
            _ => {}
        }
    }

    fn poll(&mut self) {
        let mut frame = core::mem::take(&mut self.frame);
        while self.nic.as_mut().is_some_and(|nic| nic.receive(&mut frame)) {
            self.frame_input(&frame);
        }
        if let Some(nic) = &mut self.nic {
            nic.receive_done();
        }
        self.frame = frame;
    }
}

/// Processes the frames that have arrived. Called from the scheduler's tick
/// and by code that waits for the network.
pub fn poll() {
    // If the state is in use we are inside one of its steps: skip, the next
    // tick comes soon.
    NET.try_with(Net::poll);
}

pub fn up() -> bool {
    NET.with(|n| n.up())
}

/// Finds the hardware address packets for `dst` go to, asking on the
/// network if needed, so that the first packet is not lost.
pub fn resolve(dst: u32) -> Result<(), NetError> {
    let known = || NET.with(|n| n.arp_lookup(n.next_hop(dst)).is_some());
    if !up() {
        return Err(NetError::NoDevice);
    }
    for _ in 0..3 {
        if known() {
            return Ok(());
        }
        NET.with(|n| n.arp_request(n.next_hop(dst)));
        let end = now() + 500;
        while now() < end && !known() {
            poll();
            idle();
        }
    }
    if known() { Ok(()) } else { Err(NetError::Timeout) }
}

// ------------------------------------------------------------ C interface

/// Sets the addresses (QEMU user-mode networking by default; ip= gw= mask=
/// on the kernel command line override them) and starts the network card.
#[unsafe(no_mangle)]
pub extern "C" fn net_init() {
    let setting = |key: &core::ffi::CStr, default: u32| {
        let mut v = [0u8; 32];
        unsafe { sys::cmdline_get(key.as_ptr(), v.as_mut_ptr(), v.len()) };
        let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
        core::str::from_utf8(&v[..end]).ok().and_then(parse_ip).unwrap_or(default)
    };
    let ip = setting(c"ip", 0x0A00_020F); // 10.0.2.15
    let gateway = setting(c"gw", 0x0A00_0202); // 10.0.2.2
    let netmask = setting(c"mask", 0xFFFF_FF00);
    let nic = Nic::find();
    NET.with(|n| {
        (n.ip, n.gateway, n.netmask) = (ip, gateway, netmask);
        if let Some(nic) = nic {
            n.mac = nic.mac();
            n.nic = Some(nic);
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn net_up() -> i32 {
    up() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn net_poll() {
    poll();
}

#[unsafe(no_mangle)]
pub extern "C" fn net_ip() -> u32 {
    NET.with(|n| n.ip)
}

/// # Safety
/// `mac` must point to 6 writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nic_mac(mac: *mut u8) {
    let m = NET.with(|n| n.mac);
    unsafe { core::ptr::copy_nonoverlapping(m.as_ptr(), mac, 6) };
}

/// # Safety
/// `out` must point to 16 writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn net_fmt_ip(ip: u32, out: *mut c_char) {
    let text = alloc::format!("{}.{}.{}.{}\0", ip >> 24, (ip >> 16) & 255, (ip >> 8) & 255, ip & 255);
    unsafe { core::ptr::copy_nonoverlapping(text.as_ptr(), out as *mut u8, text.len()) };
}
