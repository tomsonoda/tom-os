//! Interrupt controller, timer, clock and power of the ARM "virt" machine.

use core::arch::asm;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use super::mmio::{GIC_CPU, GIC_DISTRIBUTOR, RTC};
use crate::rt;

/// Called by the exception vectors in boot.rs; never returns.
#[unsafe(no_mangle)]
extern "C" fn exception_handler(syndrome: u64, pc: u64, address: u64) -> ! {
    let mut text = rt::StackText::new();
    let _ = write!(text, "syndrome {syndrome:#x} at pc={pc:#x} address={address:#x}");
    rt::halt("CPU exception", text.as_bytes())
}

// ------------------------------------------------------------------ time

static TICKS_PER_SECOND: AtomicU64 = AtomicU64::new(1);
static TICKS_AT_BOOT: AtomicU64 = AtomicU64::new(0);

/// The generic timer's counter.
fn counter() -> u64 {
    let v: u64;
    // SAFETY: reading the counter has no side effects
    unsafe { asm!("mrs {}, cntpct_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// A number that is hard to predict (for sequence numbers and the like).
pub fn entropy() -> u64 {
    counter().wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Milliseconds since start-up.
pub fn time_ms() -> u64 {
    let ticks = counter().wrapping_sub(TICKS_AT_BOOT.load(Ordering::Relaxed));
    (ticks as u128 * 1000 / TICKS_PER_SECOND.load(Ordering::Relaxed) as u128) as u64
}

/// Sleeps until something happens, 1 ms at the latest.
///
/// Interrupts stay masked: a pending one wakes the CPU from WFI without
/// being taken, so no interrupt handler is needed. The timer is set to
/// fire in 1 ms and switched off again afterwards, which also withdraws
/// its interrupt.
pub fn halt() {
    let ticks = TICKS_PER_SECOND.load(Ordering::Relaxed) / 1000;
    // SAFETY: the timer is only used here
    unsafe {
        asm!(
            "msr cntp_tval_el0, {ticks}",
            "msr cntp_ctl_el0, {on}",
            "wfi",
            "msr cntp_ctl_el0, xzr",
            ticks = in(reg) ticks.max(1),
            on = in(reg) 1u64,
            options(nomem, nostack, preserves_flags)
        );
    }
}

static CLOCK_AT_BOOT: AtomicU64 = AtomicU64::new(0);

/// Wall clock: seconds since 1970 (UTC).
pub fn unix_time() -> u64 {
    CLOCK_AT_BOOT.load(Ordering::Relaxed) + time_ms() / 1000
}

// ----------------------------------------------------------------- setup

/// Sets up the clocks and lets the timer's interrupt reach the CPU.
pub fn init() {
    let frequency: u64;
    // SAFETY: reading the timer's frequency has no side effects
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) frequency, options(nomem, nostack, preserves_flags)) };
    TICKS_PER_SECOND.store(frequency.max(1000), Ordering::Relaxed);
    TICKS_AT_BOOT.store(counter(), Ordering::Relaxed);
    CLOCK_AT_BOOT.store(RTC.read32(0) as u64, Ordering::Relaxed); // PL031: seconds since 1970

    // Interrupt controller (GICv2): enable it and the timer's interrupt
    // (number 30), so that the timer can wake the CPU in `halt`.
    GIC_DISTRIBUTOR.write32(0x000, 1); // distributor on
    GIC_DISTRIBUTOR.write32(0x100, 1 << 30); // enable interrupt 30
    GIC_CPU.write32(0x004, 0xFF); // let all priorities through
    GIC_CPU.write32(0x000, 1); // CPU interface on
}

/// Stops the CPU for good.
pub fn stop() -> ! {
    loop {
        // SAFETY: with the timer off and interrupts masked nothing wakes it
        unsafe { asm!("msr cntp_ctl_el0, xzr", "wfi", options(nomem, nostack, preserves_flags)) };
    }
}

/// Asks the virtual machine monitor for a power state (PSCI).
fn psci(function: u64) {
    // SAFETY: a PSCI call; it takes its arguments in x0..x3
    unsafe { asm!("hvc #0", inout("x0") function => _, out("x1") _, out("x2") _, out("x3") _, options(nomem, nostack)) };
}

pub fn poweroff() -> ! {
    psci(0x8400_0008); // SYSTEM_OFF
    stop()
}

pub fn reboot() -> ! {
    psci(0x8400_0009); // SYSTEM_RESET
    stop()
}
