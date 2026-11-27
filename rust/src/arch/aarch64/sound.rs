//! Sound: this machine has no sound driver yet, so nothing plays. The
//! functions are those of the PC's driver (arch/x86_64/sound.rs).

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wave {
    Square,
    Sine,
    Triangle,
    Saw,
    Noise,
}

pub fn init() -> bool {
    false
}
pub fn available() -> bool {
    false
}
pub fn poll() {}
pub fn tone(_freq: f64, _ms: i32, _vol: f64, _wave: Wave, _delay_ms: i32, _owner: usize) -> bool {
    false
}
pub fn pcm(_samples: &[f32], _rate: i32, _vol: f64, _delay_ms: i32, _owner: usize) -> bool {
    false
}
pub fn stop(_owner: usize) {}
pub fn busy(_owner: usize) -> bool {
    false
}
pub fn melody(_notes: &[u8], _default_ms: i32, _vol: f64, _wave: Wave, _owner: usize) -> i32 {
    0
}
