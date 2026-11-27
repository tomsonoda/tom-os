//! Sound: Intel AC'97 audio driver (QEMU -device AC97) and a small
//! software synthesizer. Output is 48 kHz, 16-bit stereo. A ring of 32 DMA
//! buffers is kept filled a little ahead of the play position by `poll`,
//! which runs from the scheduler's tick; no interrupts are used.

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec::Vec;

use crate::pci::{self, inb, inl, inw, outb, outl, outw};
use crate::rt::KCell;
use crate::cpu;

const RATE: i64 = 48000;
const BUFFERS: usize = 32;
/// frames per DMA buffer: 10.7 ms
const FRAMES: usize = 512;
/// buffers mixed ahead of the play position: about 85 ms
const AHEAD: usize = 8;
const MAX_VOICES: usize = 96;

// bus master registers, PCM out channel
const PO_BDBAR: u16 = 0x10;
const PO_CIV: u16 = 0x14;
const PO_LVI: u16 = 0x15;
const PO_SR: u16 = 0x16;
const PO_CR: u16 = 0x1B;
const GLOB_CNT: u16 = 0x2C;
const GLOB_STA: u16 = 0x30;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wave {
    Square,
    Sine,
    Triangle,
    Saw,
    Noise,
}

/// What a voice plays.
enum Source {
    Wave { wave: Wave, phase: u32, step: u32, noise: u32 },
    /// sampled sound; position and step are 16.16 fixed point
    Samples { data: Vec<i16>, pos: u32, step: u32 },
}

struct Voice {
    source: Source,
    /// frames before it starts
    delay: i64,
    /// frames
    len: i64,
    pos: i64,
    /// peak amplitude
    amp: i32,
    /// groups sounds for `stop` and `busy`
    owner: usize,
}

/// One entry of the buffer descriptor list the device reads.
#[repr(C)]
struct Descriptor {
    addr: u32,
    /// 16-bit samples in the buffer (both channels)
    samples: u16,
    flags: u16,
}

struct Sound {
    /// the device's bus master ports
    io: u16,
    /// the DMA buffers: BUFFERS * FRAMES * 2 samples, never freed
    buffers: *mut i16,
    /// the next buffer to mix
    filled: usize,
    voices: Vec<Voice>,
    sine: [i16; 256],
    mix: [i32; FRAMES],
}

static SOUND: KCell<Option<Sound>> = KCell::new(None);

/// Finds and starts the sound card; false if there is none.
pub fn init() -> bool {
    let Some((bus, dev)) = pci::find(0x8086, 0x2415) else { return false }; // Intel 82801AA AC'97
    let mixer = (pci::pci_read(bus, dev, 0, 0x10) & !3) as u16;
    let io = (pci::pci_read(bus, dev, 0, 0x14) & !3) as u16;
    pci::pci_write(bus, dev, 0, 0x04, pci::pci_read(bus, dev, 0, 0x04) | 0x05); // I/O + bus master

    // SAFETY: both layouts have a non-zero size
    let (list, buffers) = unsafe {
        (
            alloc_zeroed(Layout::new::<[Descriptor; BUFFERS]>()) as *mut Descriptor,
            alloc_zeroed(Layout::new::<[i16; BUFFERS * FRAMES * 2]>()) as *mut i16,
        )
    };
    // the device takes 32-bit addresses; the heap lies below 4 GiB
    if list.is_null() || buffers.is_null() || buffers as usize > 0xFFFF_0000 || list as usize > 0xFFFF_0000 {
        return false;
    }
    // SAFETY: the device's ports, as given by its configuration space, and
    // the list allocated above
    unsafe {
        outl(io + GLOB_CNT, 0x2); // cold reset done: codec running
        let end = cpu::time_ms() + 200;
        while inl(io + GLOB_STA) & 0x100 == 0 && cpu::time_ms() < end {}
        outw(mixer, 0); // reset the mixer
        outw(mixer + 0x02, 0x0000); // master volume: 0 dB, unmuted
        outw(mixer + 0x18, 0x0808); // PCM out volume: 0 dB
        for i in 0..BUFFERS {
            list.add(i).write(Descriptor { addr: buffers.add(i * FRAMES * 2) as u32, samples: (FRAMES * 2) as u16, flags: 0 });
        }
        outb(io + PO_CR, 0x02); // reset the channel
        let mut tries = 0;
        while tries < 1000 && inb(io + PO_CR) & 0x02 != 0 {
            tries += 1;
        }
        outl(io + PO_BDBAR, list as u32);
        outb(io + PO_LVI, (BUFFERS - 1) as u8);
        outb(io + PO_CR, 0x01); // run
    }
    let mut sine = [0i16; 256];
    for (i, s) in sine.iter_mut().enumerate() {
        *s = (libm::sin(i as f64 * 2.0 * core::f64::consts::PI / 256.0) * 32767.0) as i16;
    }
    SOUND.with(|s| *s = Some(Sound { io, buffers, filled: 0, voices: Vec::new(), sine, mix: [0; FRAMES] }));
    true
}

pub fn available() -> bool {
    SOUND.with(|s| s.is_some())
}

impl Source {
    fn next(&mut self, sine: &[i16; 256]) -> i32 {
        match self {
            Source::Samples { data, pos, step } => {
                let s = data.get((*pos >> 16) as usize).copied().unwrap_or(0) as i32;
                *pos = pos.wrapping_add(*step);
                s
            }
            Source::Wave { wave, phase, step, noise } => {
                let p = *phase;
                *phase = p.wrapping_add(*step);
                match wave {
                    Wave::Sine => sine[(p >> 24) as usize] as i32,
                    Wave::Triangle => {
                        let t = (p >> 16) as i32; // 0..65535
                        if t < 32768 { t * 2 - 32768 } else { 98303 - t * 2 }
                    }
                    Wave::Saw => (p >> 16) as i32 - 32768,
                    Wave::Noise => {
                        if p.checked_add(*step).is_none() || *noise == 0 {
                            // a new random value each period
                            *noise ^= *noise << 13;
                            *noise ^= *noise >> 17;
                            *noise ^= *noise << 5;
                            if *noise == 0 {
                                *noise = 0x123_4567;
                            }
                        }
                        (*noise & 0xFFFF) as i32 - 32768
                    }
                    Wave::Square => {
                        if p < 0x8000_0000 { 24000 } else { -24000 } // a little softer
                    }
                }
            }
        }
    }
}

impl Sound {
    /// Mixes the next FRAMES frames of all voices into buffer `index`.
    fn mix_into(&mut self, index: usize) {
        self.mix.fill(0);
        let sine = &self.sine;
        let acc = &mut self.mix;
        self.voices.retain_mut(|v| {
            for slot in acc.iter_mut() {
                if v.delay > 0 {
                    v.delay -= 1;
                    continue;
                }
                if v.pos >= v.len {
                    return false; // finished
                }
                let s = v.source.next(sine);
                // short fade in and out, so notes do not click
                let (attack, release) = (v.pos, v.len - v.pos);
                let mut env = 256;
                if attack < 96 {
                    env = attack * 256 / 96;
                }
                if release < 480 {
                    env = env.min(release * 256 / 480);
                }
                *slot += ((s * v.amp) >> 15) * env as i32 >> 8;
                v.pos += 1;
            }
            true
        });
        for (f, &s) in self.mix.iter().enumerate() {
            let s = s.clamp(-32768, 32767) as i16;
            // SAFETY: index < BUFFERS and f < FRAMES, inside the DMA
            // buffers; the device only reads them
            unsafe {
                let out = self.buffers.add((index * FRAMES + f) * 2);
                out.write_volatile(s);
                out.add(1).write_volatile(s);
            }
        }
    }

    fn poll(&mut self) {
        // SAFETY: the device's own ports
        let playing = (unsafe { inb(self.io + PO_CIV) } & 31) as usize;
        while self.filled.wrapping_sub(playing) & 31 < AHEAD {
            if self.filled == playing {
                // fell behind: skip to just after the play position
                self.filled = (playing + 1) & 31;
                continue;
            }
            self.mix_into(self.filled);
            self.filled = (self.filled + 1) & 31;
        }
        unsafe {
            // never let the device reach the last valid buffer: it would stop
            outb(self.io + PO_LVI, (playing.wrapping_sub(1) & 31) as u8);
            let status = inw(self.io + PO_SR);
            if status & 0x1C != 0 {
                outw(self.io + PO_SR, status & 0x1C); // clear status bits
            }
            if status & 0x01 != 0 {
                outb(self.io + PO_CR, 0x01); // halted: run again
            }
        }
    }

    fn add(&mut self, voice: Voice) -> bool {
        let room = self.voices.len() < MAX_VOICES;
        if room {
            self.voices.push(voice);
        }
        room
    }
}

/// Keeps the output buffers filled; called from the scheduler's tick.
pub fn poll() {
    SOUND.try_with(|s| {
        if let Some(s) = s {
            s.poll();
        }
    });
}

fn amplitude(vol: f64) -> i32 {
    (vol.clamp(0.0, 1.0) * 16000.0) as i32 // room for several voices at once
}

fn frames(ms: i32) -> i64 {
    ms as i64 * RATE / 1000
}

/// Starts a tone without waiting for it; false if it cannot be played.
/// `owner` groups sounds for `stop` and `busy`.
pub fn tone(freq: f64, ms: i32, vol: f64, wave: Wave, delay_ms: i32, owner: usize) -> bool {
    if !(freq > 0.0 && freq <= 20000.0) || ms <= 0 {
        return false;
    }
    let step = (freq / RATE as f64 * 4294967296.0) as u32;
    let noise = 0x9E37_79B9 ^ cpu::time_ms() as u32 ^ step;
    let source = Source::Wave { wave, phase: 0, step, noise };
    SOUND.with(|s| {
        s.as_mut().is_some_and(|s| s.add(Voice { source, delay: frames(delay_ms), len: frames(ms), pos: 0, amp: amplitude(vol), owner }))
    })
}

/// Plays samples (-1..1) recorded at `rate` Hz.
pub fn pcm(samples: &[f32], rate: i32, vol: f64, delay_ms: i32, owner: usize) -> bool {
    if samples.is_empty() || !(1000..=96000).contains(&rate) {
        return false;
    }
    let mut data = Vec::new();
    if data.try_reserve_exact(samples.len()).is_err() {
        return false;
    }
    data.extend(samples.iter().map(|f| (f.clamp(-1.0, 1.0) * 32767.0) as i16));
    let len = samples.len() as i64 * RATE / rate as i64;
    let source = Source::Samples { data, pos: 0, step: (rate as u64 * 65536 / RATE as u64) as u32 };
    SOUND.with(|s| {
        s.as_mut().is_some_and(|s| s.add(Voice { source, delay: frames(delay_ms), len, pos: 0, amp: amplitude(vol) * 2, owner }))
    })
}

/// Silences the sounds of `owner`; 0 means everything.
pub fn stop(owner: usize) {
    SOUND.with(|s| {
        if let Some(s) = s {
            s.voices.retain(|v| owner != 0 && v.owner != owner);
        }
    })
}

/// Is a sound of `owner` (0: any) playing or waiting to play?
pub fn busy(owner: usize) -> bool {
    SOUND.with(|s| s.as_ref().is_some_and(|s| s.voices.iter().any(|v| owner == 0 || v.owner == owner)))
}

/// Plays notes such as "C4:200 E4 G4:400 R:100 C#5" one after the other
/// and returns the total length in ms.
pub fn melody(notes: &[u8], default_ms: i32, vol: f64, wave: Wave, owner: usize) -> i32 {
    const SEMITONES: [i32; 7] = [9, 11, 0, 2, 4, 5, 7]; // A B C D E F G
    let default_ms = if default_ms <= 0 { 200 } else { default_ms };
    let mut at = 0;
    for word in notes.split(|b| matches!(b, b' ' | b',' | b'\n')).filter(|w| !w.is_empty()) {
        let (head, length) = match word.iter().position(|b| matches!(b, b':' | b'/')) {
            Some(i) => (&word[..i], Some(&word[i + 1..])),
            None => (word, None),
        };
        let digits = length.map_or(0, |l| l.iter().take_while(|b| b.is_ascii_digit()).count());
        let ms = length
            .and_then(|l| core::str::from_utf8(&l[..digits]).ok()?.parse::<i32>().ok())
            .filter(|&ms| ms > 0)
            .unwrap_or(default_ms);
        let name = head[0].to_ascii_uppercase();
        if matches!(name, b'R' | b'-') {
            at += ms; // a rest
            continue;
        }
        if !name.is_ascii_uppercase() || name > b'G' {
            continue; // skip what we cannot read
        }
        let mut semitone = SEMITONES[(name - b'A') as usize];
        let mut rest = &head[1..];
        match rest.first() {
            Some(b'#') => (semitone, rest) = (semitone + 1, &rest[1..]),
            Some(b'b') => (semitone, rest) = (semitone - 1, &rest[1..]),
            _ => {}
        }
        let octave = rest.first().filter(|b| (b'0'..=b'8').contains(b)).map_or(4, |b| (b - b'0') as i32);
        let midi = 12 * (octave + 1) + semitone;
        tone(440.0 * libm::pow(2.0, (midi - 69) as f64 / 12.0), ms * 9 / 10, vol, wave, at, owner);
        at += ms;
    }
    at
}
