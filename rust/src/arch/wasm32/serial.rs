//! The serial console: a text log on the page.

use super::js;

pub fn init() {}

pub fn put(c: u8) {
    js::js_serial_put(c as u32);
}

/// A received byte, if there is one.
pub fn get() -> Option<u8> {
    u8::try_from(js::js_serial_get()).ok()
}
