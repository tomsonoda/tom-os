//! Reaching the LLM by other means than the kernel's own network stack:
//! not on this machine.

use alloc::string::String;
use alloc::vec::Vec;

/// True if `post` can be used even without a network card.
pub const AVAILABLE: bool = false;

/// Sends a chat request; None if this machine has no such channel.
pub fn post(_request: &[u8], _timeout_ms: u64, _wait: &dyn Fn()) -> Option<Result<(i32, Vec<u8>), String>> {
    None
}
