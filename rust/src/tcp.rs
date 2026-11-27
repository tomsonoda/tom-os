//! Minimal TCP client: one connection at a time, in-order receive,
//! go-back-N retransmission.

use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::net::{self, NET, Net, NetError, PROTO_TCP, fold, get16, get32, idle, now, put16, put32, sum};
use crate::cpu;
use crate::rt::KCell;

const FIN: u8 = 0x01;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const PSH: u8 = 0x08;
const ACK: u8 = 0x10;

const RX_SIZE: usize = 65536;
/// segments in flight
const MAX_BURST: u32 = 24;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Closed,
    SynSent,
    Established,
}

pub(crate) struct Tcp {
    state: State,
    remote_ip: u32,
    remote_port: u16,
    local_port: u16,
    snd_una: u32,
    snd_nxt: u32,
    snd_max: u32,
    rcv_nxt: u32,
    snd_wnd: u32,
    mss: u32,
    peer_fin: bool,
    reset: bool,
    /// received bytes the caller has not taken yet
    rx: VecDeque<u8>,
    /// the receive window last told to the peer
    adv_wnd: u32,
}

impl Tcp {
    pub const fn new() -> Tcp {
        Tcp {
            state: State::Closed,
            remote_ip: 0,
            remote_port: 0,
            local_port: 0,
            snd_una: 0,
            snd_nxt: 0,
            snd_max: 0,
            rcv_nxt: 0,
            snd_wnd: 0,
            mss: 1460,
            peer_fin: false,
            reset: false,
            rx: VecDeque::new(),
            adv_wnd: 0,
        }
    }

    fn rx_window(&self) -> u32 {
        (RX_SIZE - self.rx.len()).min(65535) as u32
    }

    /// Why a connection that is no longer established stopped.
    fn failure(&self) -> NetError {
        if self.reset { NetError::Reset } else { NetError::Protocol }
    }

    /// The peer has our FIN and we have its.
    fn fully_closed(&self) -> bool {
        self.peer_fin && self.snd_una == self.snd_nxt
    }
}

/// a - b in sequence space
fn diff(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

fn pseudo_sum(src: u32, dst: u32, len: usize) -> u32 {
    let mut ph = [0u8; 12];
    put32(&mut ph[0..], src);
    put32(&mut ph[4..], dst);
    ph[9] = PROTO_TCP;
    put16(&mut ph[10..], len as u16);
    sum(0, &ph)
}

fn timestamp() -> u64 {
    cpu::tsc()
}

impl Net {
    fn tcp_out(&mut self, flags: u8, seq: u32, data: &[u8]) -> Result<(), NetError> {
        let mut h = [0u8; 24];
        let hlen = if flags & SYN != 0 { 24 } else { 20 };
        let t = &mut self.tcp;
        put16(&mut h[0..], t.local_port);
        put16(&mut h[2..], t.remote_port);
        put32(&mut h[4..], seq);
        put32(&mut h[8..], if flags & ACK != 0 { t.rcv_nxt } else { 0 });
        h[12] = ((hlen / 4) << 4) as u8;
        h[13] = flags;
        t.adv_wnd = t.rx_window();
        put16(&mut h[14..], t.adv_wnd as u16);
        if flags & SYN != 0 {
            h[20] = 2; // MSS option
            h[21] = 4;
            put16(&mut h[22..], 1460);
        }
        let remote = t.remote_ip;
        let h = &mut h[..hlen];
        let check = fold(sum(sum(pseudo_sum(self.ip, remote, hlen + data.len()), h), data));
        put16(&mut h[16..], check);
        self.ip_send(remote, PROTO_TCP, h, data)
    }

    fn tcp_ack(&mut self) {
        let seq = self.tcp.snd_nxt;
        let _ = self.tcp_out(ACK, seq, &[]);
    }

    pub(crate) fn tcp_input(&mut self, src: u32, seg: &[u8]) {
        let my_ip = self.ip;
        let t = &mut self.tcp;
        if seg.len() < 20 || t.state == State::Closed {
            return;
        }
        if src != t.remote_ip || get16(seg) != t.remote_port || get16(&seg[2..]) != t.local_port {
            return;
        }
        if fold(sum(pseudo_sum(src, my_ip, seg.len()), seg)) != 0 {
            return;
        }
        let (mut seq, ack) = (get32(&seg[4..]), get32(&seg[8..]));
        let flags = seg[13];
        let hlen = (seg[12] >> 4) as usize * 4;
        if hlen < 20 || hlen > seg.len() {
            return;
        }
        let mut data = &seg[hlen..];

        if flags & RST != 0 {
            t.reset = true;
            t.state = State::Closed;
            return;
        }

        if t.state == State::SynSent {
            if flags & (SYN | ACK) != (SYN | ACK) || ack != t.snd_nxt {
                return;
            }
            t.rcv_nxt = seq.wrapping_add(1);
            t.snd_una = ack;
            t.snd_wnd = get16(&seg[14..]) as u32;
            let mut i = 20; // options
            while i + 1 < hlen {
                let kind = seg[i];
                if kind == 0 {
                    break;
                }
                if kind == 1 {
                    i += 1;
                    continue;
                }
                let olen = seg[i + 1] as usize;
                if olen < 2 || i + olen > hlen {
                    break;
                }
                if kind == 2 && olen == 4 {
                    let m = get16(&seg[i + 2..]) as u32;
                    if (64..t.mss).contains(&m) {
                        t.mss = m;
                    }
                }
                i += olen;
            }
            t.state = State::Established;
            self.tcp_ack();
            return;
        }

        if flags & ACK != 0 {
            if diff(ack, t.snd_una) > 0 && diff(t.snd_max, ack) >= 0 {
                t.snd_una = ack;
                if diff(t.snd_nxt, ack) < 0 {
                    t.snd_nxt = ack;
                }
            }
            t.snd_wnd = get16(&seg[14..]) as u32;
        }

        let fin = flags & FIN != 0;
        if data.is_empty() && !fin {
            return;
        }

        // trim anything we already have
        let skip = diff(t.rcv_nxt, seq);
        if skip > 0 {
            let skip = skip as usize;
            if skip > data.len() || (skip == data.len() && !fin) {
                self.tcp_ack(); // pure duplicate
                return;
            }
            data = &data[skip..];
            seq = seq.wrapping_add(skip as u32);
        }
        if seq == t.rcv_nxt && !t.peer_fin {
            let take = data.len().min(RX_SIZE - t.rx.len());
            t.rx.extend(&data[..take]);
            t.rcv_nxt = t.rcv_nxt.wrapping_add(take as u32);
            if fin && take == data.len() {
                t.rcv_nxt = t.rcv_nxt.wrapping_add(1);
                t.peer_fin = true;
            }
        }
        self.tcp_ack();
    }

    fn tcp_open(&mut self, ip: u32, port: u16) -> Result<(), NetError> {
        let t = &mut self.tcp;
        if t.rx.capacity() < RX_SIZE && t.rx.try_reserve_exact(RX_SIZE).is_err() {
            return Err(NetError::Protocol);
        }
        let mut local = t.local_port;
        if local == 0 {
            local = 49152 + ((timestamp() >> 8) % 8192) as u16;
        }
        local = local.wrapping_add(1);
        if local < 49152 {
            local = 49152;
        }
        let iss = timestamp() as u32;
        *t = Tcp {
            state: State::SynSent,
            remote_ip: ip,
            remote_port: port,
            local_port: local,
            snd_una: iss,
            snd_nxt: iss.wrapping_add(1),
            snd_max: iss.wrapping_add(1),
            rx: core::mem::take(&mut t.rx),
            ..Tcp::new()
        };
        t.rx.clear();
        Ok(())
    }

    /// Sends as much of `data` (which starts at sequence number `start`) as
    /// the window allows.
    fn tcp_send_burst(&mut self, data: &[u8], start: u32) -> Result<(), NetError> {
        let end = start.wrapping_add(data.len() as u32);
        let t = &self.tcp;
        // at least one segment: this also serves as a zero-window probe
        let wnd = t.snd_wnd.min(MAX_BURST * t.mss).max(t.mss);
        loop {
            let t = &self.tcp;
            let in_flight = t.snd_nxt.wrapping_sub(t.snd_una);
            if diff(end, t.snd_nxt) <= 0 || in_flight >= wnd {
                return Ok(());
            }
            let n = end.wrapping_sub(t.snd_nxt).min(t.mss).min(wnd - in_flight);
            let seq = t.snd_nxt;
            let at = seq.wrapping_sub(start) as usize;
            let last = seq.wrapping_add(n) == end;
            self.tcp_out(if last { ACK | PSH } else { ACK }, seq, &data[at..at + n as usize])?;
            let t = &mut self.tcp;
            t.snd_nxt = seq.wrapping_add(n);
            if diff(t.snd_nxt, t.snd_max) > 0 {
                t.snd_max = t.snd_nxt;
            }
        }
    }
}

// ------------------------------------------------------- waiting, progress

static WAIT_HOOK: KCell<Option<fn()>> = KCell::new(None);
static LAST_TICK: AtomicU64 = AtomicU64::new(0);

/// Sets a function called about once per second while a TCP call waits
/// (progress display).
pub fn set_wait_hook(hook: Option<fn()>) {
    WAIT_HOOK.with(|h| *h = hook);
}

fn wait_tick() {
    let Some(hook) = WAIT_HOOK.with(|h| *h) else { return };
    if now() - LAST_TICK.load(Ordering::Relaxed) >= 1000 {
        LAST_TICK.store(now(), Ordering::Relaxed);
        hook();
    }
}

fn break_pending() -> bool {
    crate::console::break_pending()
}

// ------------------------------------------------------------ the socket

pub fn connect(ip: u32, port: u16, timeout_ms: u64) -> Result<(), NetError> {
    net::resolve(ip)?;
    NET.with(|n| n.tcp_open(ip, port))?;
    let give_up = |e| {
        NET.with(|n| n.tcp.state = State::Closed);
        Err(e)
    };
    let start = now();
    let (mut rto, mut last) = (400, 0);
    let mut first = true;
    loop {
        let t = now();
        if first || t - last >= rto {
            if let Err(e) = NET.with(|n| n.tcp_out(SYN, n.tcp.snd_una, &[])) {
                return give_up(e);
            }
            if !first {
                rto *= 2;
            }
            last = t;
            first = false;
        }
        net::poll();
        let (state, reset) = NET.with(|n| (n.tcp.state, n.tcp.reset));
        if state == State::Established {
            return Ok(());
        }
        if reset {
            return Err(NetError::Reset);
        }
        if break_pending() {
            return give_up(NetError::Break);
        }
        if t - start > timeout_ms {
            return give_up(NetError::Timeout);
        }
        idle();
    }
}

pub fn send(data: &[u8]) -> Result<(), NetError> {
    let start = NET.with(|n| n.tcp.snd_nxt);
    let end = start.wrapping_add(data.len() as u32);
    let mut last_progress = now();
    let mut rto = 400;
    let mut retries = 0;
    loop {
        let una = NET.with(|n| {
            if diff(end, n.tcp.snd_una) <= 0 {
                return Ok(None); // everything is acknowledged
            }
            if n.tcp.state != State::Established {
                return Err(n.tcp.failure());
            }
            n.tcp_send_burst(data, start)?;
            Ok(Some(n.tcp.snd_una))
        })?;
        let Some(una) = una else { return Ok(()) };
        net::poll();
        let t = now();
        if NET.with(|n| n.tcp.snd_una) != una {
            last_progress = t;
            rto = 400;
            retries = 0;
            continue;
        }
        if t - last_progress > rto {
            retries += 1;
            if retries > 8 {
                return Err(NetError::Timeout);
            }
            NET.with(|n| n.tcp.snd_nxt = n.tcp.snd_una); // go back and resend
            rto *= 2;
            last_progress = t;
            continue;
        }
        if break_pending() {
            return Err(NetError::Break);
        }
        wait_tick();
        idle();
    }
}

/// Receives into `buf`; `Ok(0)` means the peer has closed the connection.
pub fn recv(buf: &mut [u8], timeout_ms: u64) -> Result<usize, NetError> {
    let start = now();
    loop {
        net::poll();
        let got = NET.with(|n| {
            let t = &mut n.tcp;
            if !t.rx.is_empty() {
                let count = t.rx.len().min(buf.len());
                for (dst, b) in buf.iter_mut().zip(t.rx.drain(..count)) {
                    *dst = b;
                }
                // reopen the window if we had advertised a small one
                if t.state == State::Established && t.rx_window() >= t.adv_wnd + 16384 {
                    n.tcp_ack();
                }
                return Ok(Some(count));
            }
            if t.peer_fin {
                return Ok(Some(0));
            }
            if t.state != State::Established {
                return Err(t.failure());
            }
            Ok(None)
        })?;
        if let Some(count) = got {
            return Ok(count);
        }
        if break_pending() {
            return Err(NetError::Break);
        }
        if now() - start > timeout_ms {
            return Err(NetError::Timeout);
        }
        wait_tick();
        idle();
    }
}

pub fn close() {
    let state = NET.with(|n| {
        let state = n.tcp.state;
        let seq = n.tcp.snd_nxt;
        match state {
            State::Established => {
                let _ = n.tcp_out(FIN | ACK, seq, &[]);
                n.tcp.snd_nxt = seq.wrapping_add(1);
                n.tcp.snd_max = n.tcp.snd_nxt;
            }
            State::SynSent => {
                let _ = n.tcp_out(RST, seq, &[]);
            }
            State::Closed => {}
        }
        state
    });
    if state == State::Established {
        let end = now() + 1000;
        let finished = || {
            NET.with(|n| {
                n.tcp.rx.clear(); // discard anything still arriving
                n.tcp.state != State::Established || n.tcp.fully_closed()
            })
        };
        while now() < end && !finished() {
            net::poll();
            idle();
        }
        NET.with(|n| {
            if n.tcp.state == State::Established && !n.tcp.fully_closed() {
                let seq = n.tcp.snd_nxt;
                let _ = n.tcp_out(RST | ACK, seq, &[]);
            }
        });
    }
    NET.with(|n| n.tcp.state = State::Closed);
}
