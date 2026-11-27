//! HTTP/1.1 client: one POST per connection, Content-Length or chunked reply.

use alloc::format;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::sys;

/// What the current (or last) request is doing, for status and error text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Sending,
    Waiting,
    Receiving,
}

static PHASE: AtomicU8 = AtomicU8::new(0);

fn set_phase(p: Phase) {
    PHASE.store(p as u8, Ordering::Relaxed);
}

pub fn reset_phase() {
    set_phase(Phase::Connecting);
}

pub fn phase() -> Phase {
    match PHASE.load(Ordering::Relaxed) {
        1 => Phase::Sending,
        2 => Phase::Waiting,
        3 => Phase::Receiving,
        _ => Phase::Connecting,
    }
}

impl Phase {
    pub fn text(self) -> &'static str {
        match self {
            Phase::Connecting => "connecting",
            Phase::Sending => "sending the request",
            Phase::Waiting => "waiting for the reply",
            Phase::Receiving => "receiving the reply",
        }
    }
}

/// The value of a header (case-insensitive name) in the header block.
fn header<'a>(head: &'a [u8], name: &str) -> Option<&'a [u8]> {
    for line in head.split(|&b| b == b'\n') {
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        if line[..colon].eq_ignore_ascii_case(name.as_bytes()) {
            let v = &line[colon + 1..];
            let start = v.iter().position(|&b| b != b' ').unwrap_or(v.len());
            let end = v.iter().rposition(|&b| b != b'\r' && b != b' ').map_or(start, |e| e + 1);
            return Some(&v[start..end.max(start)]);
        }
    }
    None
}

/// Decodes a chunked body; the flag says whether the final chunk was seen.
fn dechunk(mut p: &[u8]) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    loop {
        let Some(eol) = p.iter().position(|&b| b == b'\n') else { return (out, false) };
        let digits: Vec<u8> = p[..eol].iter().copied().take_while(|b| b.is_ascii_hexdigit()).collect();
        let size = core::str::from_utf8(&digits).ok().and_then(|d| usize::from_str_radix(d, 16).ok()).unwrap_or(0);
        p = &p[eol + 1..];
        if size == 0 {
            return (out, true);
        }
        if p.len() < size + 2 {
            return (out, false);
        }
        out.extend_from_slice(&p[..size]);
        p = &p[size + 2..];
    }
}

struct Connection;

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe { sys::tcp_close() };
        BUSY.store(false, Ordering::Relaxed);
    }
}

static BUSY: AtomicBool = AtomicBool::new(false);

/// POSTs a JSON body and returns the status and the decoded response body,
/// or a network error code (`sys::NET_E*`).
pub fn post_json(ip: u32, port: u16, path: &str, body: &[u8], timeout_ms: i32) -> Result<(i32, Vec<u8>), i32> {
    // one connection at a time: the TCP layer has a single socket
    if BUSY.swap(true, Ordering::Relaxed) {
        return Err(sys::NET_EBUSY);
    }
    set_phase(Phase::Connecting);
    let r = unsafe { sys::tcp_connect(ip, port, 5000) };
    if r < 0 {
        BUSY.store(false, Ordering::Relaxed);
        return Err(r);
    }
    let _conn = Connection; // closes the socket on every way out

    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {}.{}.{}.{}:{port}\r\nUser-Agent: tom-os/0.1\r\n\
         Content-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        ip >> 24, (ip >> 16) & 255, (ip >> 8) & 255, ip & 255, body.len()
    );
    set_phase(Phase::Sending);
    for part in [head.as_bytes(), body] {
        let r = unsafe { sys::tcp_send(part.as_ptr(), part.len()) };
        if r < 0 {
            return Err(r);
        }
    }

    set_phase(Phase::Waiting);
    let mut raw: Vec<u8> = Vec::new();
    let mut head_len = 0;
    let mut content_len: Option<usize> = None;
    let mut chunked = false;
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { sys::tcp_recv(buf.as_mut_ptr(), buf.len(), timeout_ms) };
        if n < 0 {
            return Err(n);
        }
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n as usize]);
        if head_len == 0 {
            let Some(sep) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
            head_len = sep + 4;
            let head = &raw[..head_len];
            content_len = header(head, "content-length")
                .and_then(|v| core::str::from_utf8(v).ok())
                .and_then(|v| v.trim().parse().ok());
            chunked = header(head, "transfer-encoding").is_some_and(|v| v.len() >= 7 && v[..7].eq_ignore_ascii_case(b"chunked"));
        }
        let body = &raw[head_len..];
        // Servers may send whitespace while they wait (keep-alive); the reply
        // proper starts with the JSON object.
        if body.contains(&b'{') {
            set_phase(Phase::Receiving);
        }
        let done = if chunked { dechunk(body).1 } else { content_len.is_some_and(|l| body.len() >= l) };
        if done {
            break;
        }
    }

    if head_len == 0 || raw.len() < 12 || !raw.starts_with(b"HTTP/") {
        return Err(sys::NET_EPROTO);
    }
    let status = core::str::from_utf8(&raw[9..12]).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = &raw[head_len..];
    let decoded = if chunked {
        dechunk(body).0
    } else {
        body[..content_len.map_or(body.len(), |l| l.min(body.len()))].to_vec()
    };
    Ok((status, decoded))
}
