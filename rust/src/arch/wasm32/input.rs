//! Keyboard and pointer: the page's events. The page turns key events
//! into PC scancodes; text from on-screen keyboards and input methods
//! arrives as text.

use super::js;
use crate::{console, wm};

const EVENT_KEY: i32 = 1;
const EVENT_POINTER: i32 = 2;
const EVENT_TEXT: i32 = 3;
/// a key that types a character is held or released; the character comes
/// as text
const EVENT_KEY_STATE: i32 = 4;

/// The pointer is where the user points.
pub fn pointer_absolute() -> bool {
    true
}

pub fn init_pointer() {}

/// Takes the events that have arrived.
pub fn poll() {
    for _ in 0..256 {
        let mut e = [0i32; 4];
        // SAFETY: room for the four numbers of an event
        match unsafe { js::js_event(e.as_mut_ptr()) } {
            EVENT_KEY => console::key(e[0] as u8, e[1] != 0, e[2] != 0),
            EVENT_POINTER => {
                if wm::active() {
                    wm::mouse_abs(e[0], e[1], e[2], e[3]); // position 0..65535
                }
            }
            EVENT_KEY_STATE => {
                if wm::active() {
                    wm::key(e[0], e[2] != 0, false, None);
                }
            }
            EVENT_TEXT => {
                let mut text = [0u8; 256];
                // SAFETY: at most `text.len()` bytes are written
                let n = unsafe { js::js_event_text(text.as_mut_ptr(), text.len()) }.min(text.len());
                console::type_text(&text[..n]);
            }
            _ => return,
        }
    }
}
