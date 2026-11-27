//! Reaching the LLM: the page sends the request (browsers cannot open
//! plain network connections, and the page knows the user's settings).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::js;
use crate::{console, cpu, http, sched};

/// True if `post` can be used even without a network card.
pub const AVAILABLE: bool = true;

/// Sends a chat request and waits for the answer: (HTTP status, body).
/// `wait` is called while waiting (progress display).
pub fn post(request: &[u8], timeout_ms: u64, wait: &dyn Fn()) -> Option<Result<(i32, Vec<u8>), String>> {
    // SAFETY: the page copies `request` before this returns
    unsafe { js::js_llm_start(request.as_ptr(), request.len()) };
    http::waiting();
    let start = cpu::time_ms();
    let len = loop {
        let len = js::js_llm_poll();
        if len >= 0 {
            break len as usize;
        }
        if console::break_pending() {
            js::js_llm_abort();
            return Some(Err(String::from("interrupted")));
        }
        if cpu::time_ms() - start > timeout_ms {
            js::js_llm_abort();
            return Some(Err(String::from("the LLM did not answer in time")));
        }
        wait();
        sched::idle();
    };
    let mut body = vec![0u8; len];
    // SAFETY: the page writes exactly the `len` bytes it announced
    unsafe { js::js_llm_read(body.as_mut_ptr()) };
    Some(match js::js_llm_status() {
        0 => Err(String::from_utf8_lossy(&body).into_owned()),
        status => Ok((status, body)),
    })
}
