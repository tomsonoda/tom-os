//! Interrupt table, interrupt controller, timer, clock and power of the PC.

use core::arch::asm;
use core::arch::x86_64::_rdtsc;
use core::cell::UnsafeCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use super::pci::{inb, outb, outw};
use crate::rt;

// SAFETY (all port I/O in this file): the interrupt controllers, the
// timer, the CMOS clock and the power ports of a PC, used only here.

// ------------------------------------------------------------- interrupts

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    kind: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

struct Idt(UnsafeCell<[IdtEntry; 256]>);

// SAFETY: written once by `init`, before interrupts are enabled
unsafe impl Sync for Idt {}

static IDT: Idt = Idt(UnsafeCell::new(
    [IdtEntry { offset_low: 0, selector: 0, ist: 0, kind: 0, offset_mid: 0, offset_high: 0, zero: 0 }; 256],
));

unsafe extern "C" {
    // arch/x86_64/boot.asm
    static exc_table: [u64; 32];
    fn irq_master();
    fn irq_slave();
}

/// Called by the exception stubs in boot.asm; never returns.
#[unsafe(no_mangle)]
extern "C" fn exception_handler(vector: u64, error: u64, rip: u64, cr2: u64) -> ! {
    let mut text = rt::StackText::new();
    let _ = write!(text, "{vector} (error {error:#x}) at rip={rip:#x} cr2={cr2:#x}");
    rt::halt("CPU exception", text.as_bytes())
}

// ------------------------------------------------------------------ time

static TSC_PER_MS: AtomicU64 = AtomicU64::new(1);
static TSC_AT_BOOT: AtomicU64 = AtomicU64::new(0);

/// The CPU's time stamp counter.
fn tsc() -> u64 {
    // SAFETY: RDTSC is available on every x86-64 CPU
    unsafe { _rdtsc() }
}

/// Times 10 ms on PIT channel 2 and counts TSC ticks.
fn tsc_calibrate() {
    unsafe {
        outb(0x61, (inb(0x61) & !0x02) | 0x01);
        outb(0x43, 0xB0);
        outb(0x42, (11932 & 0xFF) as u8);
        outb(0x42, (11932 >> 8) as u8);
    }
    let t0 = tsc();
    while unsafe { inb(0x61) } & 0x20 == 0 {}
    let t1 = tsc();
    TSC_PER_MS.store(((t1 - t0) / 10).max(1), Ordering::Relaxed);
    TSC_AT_BOOT.store(t1, Ordering::Relaxed);
}

/// A number that is hard to predict (for sequence numbers and the like).
pub fn entropy() -> u64 {
    tsc()
}

/// Milliseconds since start-up.
pub fn time_ms() -> u64 {
    tsc().wrapping_sub(TSC_AT_BOOT.load(Ordering::Relaxed)) / TSC_PER_MS.load(Ordering::Relaxed)
}

/// Sleeps until the next interrupt (the 1 ms timer at the latest).
pub fn halt() {
    // SAFETY: interrupts only wake the CPU; their handlers touch nothing
    unsafe { asm!("sti", "hlt", options(nomem, nostack)) };
}

fn cmos(reg: u8) -> u8 {
    unsafe {
        outb(0x70, reg);
        inb(0x71)
    }
}

/// The real-time clock as seconds since 1970 (UTC).
fn rtc_read() -> u64 {
    let mut t = [0u8, 0, 0, 1, 1, 0]; // second, minute, hour, day, month, year
    for _ in 0..1000 {
        if cmos(0x0A) & 0x80 != 0 {
            continue; // update in progress
        }
        t = [cmos(0), cmos(2), cmos(4), cmos(7), cmos(8), cmos(9)];
        if t[0] == cmos(0) {
            break;
        }
    }
    let [mut s, mut m, mut h, mut d, mut mo, mut y] = t;
    let mode = cmos(0x0B);
    if mode & 4 == 0 {
        let bcd = |x: u8| (x & 0x0F) + (x >> 4) * 10;
        (s, m, d, mo, y) = (bcd(s), bcd(m), bcd(d), bcd(mo), bcd(y));
        h = bcd(h & 0x7F) | (h & 0x80);
    }
    if mode & 2 == 0 && h & 0x80 != 0 {
        h = ((h & 0x7F) + 12) % 24; // 12-hour clock, PM
    }
    let year = 2000 + y as u64;
    const MONTH_START: [u64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mut days = (year - 1970) * 365 + (year - 1969) / 4 + MONTH_START[(mo as usize + 11) % 12] + (d as u64).saturating_sub(1);
    if year % 4 == 0 && mo > 2 {
        days += 1;
    }
    days * 86400 + h as u64 * 3600 + m as u64 * 60 + s as u64
}

static CLOCK_AT_BOOT: AtomicU64 = AtomicU64::new(0);
static CLOCK_READ_AT_MS: AtomicU64 = AtomicU64::new(0);

/// Wall clock: seconds since 1970 (UTC).
pub fn unix_time() -> u64 {
    CLOCK_AT_BOOT.load(Ordering::Relaxed) + (time_ms() - CLOCK_READ_AT_MS.load(Ordering::Relaxed)) / 1000
}

// ----------------------------------------------------------------- setup

/// Sets up interrupts and the clocks.
pub fn init() {
    #[repr(C, packed)]
    struct Pointer {
        limit: u16,
        base: u64,
    }
    // SAFETY: start-up, with interrupts still off; the table and the stubs
    // it points to live for ever
    unsafe {
        let idt = &mut *IDT.0.get();
        let mut set = |vector: usize, handler: u64| {
            idt[vector] = IdtEntry {
                offset_low: handler as u16,
                selector: 0x08,
                ist: 0,
                kind: 0x8E,
                offset_mid: (handler >> 16) as u16,
                offset_high: (handler >> 32) as u32,
                zero: 0,
            };
        };
        for (vector, &stub) in (*(&raw const exc_table)).iter().enumerate() {
            set(vector, stub);
        }
        for vector in 32..40 {
            set(vector, irq_master as *const () as u64);
        }
        for vector in 40..48 {
            set(vector, irq_slave as *const () as u64);
        }
        let pointer = Pointer { limit: (size_of::<[IdtEntry; 256]>() - 1) as u16, base: IDT.0.get() as u64 };
        asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));

        // Remap the interrupt controllers to vectors 32..47; unmask timer, keyboard, COM1.
        for (port, value) in [(0x20, 0x11), (0xA0, 0x11), (0x21, 0x20), (0xA1, 0x28), (0x21, 0x04), (0xA1, 0x02), (0x21, 0x01), (0xA1, 0x01)] {
            outb(port, value);
        }
        outb(0x21, !((1 << 0) | (1 << 1) | (1 << 4)));
        outb(0xA1, 0xFF);

        // PIT channel 0: 1 kHz tick, only used to wake HLT.
        outb(0x43, 0x36);
        outb(0x40, (1193 & 0xFF) as u8);
        outb(0x40, (1193 >> 8) as u8);
    }
    tsc_calibrate();
    CLOCK_AT_BOOT.store(rtc_read(), Ordering::Relaxed);
    CLOCK_READ_AT_MS.store(time_ms(), Ordering::Relaxed);
    // SAFETY: the handlers are in place
    unsafe { asm!("sti", options(nomem, nostack)) };
}

/// Stops the CPU for good.
pub fn stop() -> ! {
    loop {
        // SAFETY: stops the CPU for good
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

pub fn poweroff() -> ! {
    unsafe {
        outw(0x604, 0x2000); // QEMU (PIIX4 ACPI)
        outw(0xB004, 0x2000); // Bochs / old QEMU
    }
    stop()
}

pub fn reboot() -> ! {
    unsafe { outb(0x64, 0xFE) };
    stop()
}
