//! Interrupt controller, timer, clock and power of the ARM "virt" machine.

use core::arch::asm;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use super::fdt;
use super::mmio::Registers;
use crate::rt;

/// Called by the exception vectors in boot.rs; never returns.
#[unsafe(no_mangle)]
extern "C" fn exception_handler(syndrome: u64, pc: u64, address: u64) -> ! {
    let mut text = rt::StackText::new();
    let _ = write!(text, "syndrome {syndrome:#x} at pc={pc:#x} address={address:#x}");
    rt::halt("CPU exception", text.as_bytes())
}

/// The virtual timer's interrupt (a per-CPU interrupt).
const TIMER_INTERRUPT: usize = 27;

// ------------------------------------------------------------------ time

static TICKS_PER_SECOND: AtomicU64 = AtomicU64::new(1);
static TICKS_AT_BOOT: AtomicU64 = AtomicU64::new(0);

/// The generic timer's counter (the virtual one: the timer a guest of a
/// hypervisor is meant to use).
fn counter() -> u64 {
    let v: u64;
    // SAFETY: reading the counter has no side effects
    unsafe { asm!("mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
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
            "msr cntv_tval_el0, {ticks}",
            "msr cntv_ctl_el0, {on}",
            "wfi",
            "msr cntv_ctl_el0, xzr",
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
    let Some(tree) = fdt::tree() else { return };
    // SAFETY (the register blocks below): device registers, at the
    // addresses the device tree gives
    if let Some((base, _)) = tree.compatible("arm,pl031").and_then(|n| n.reg(0)) {
        let clock = unsafe { Registers::new(base) };
        CLOCK_AT_BOOT.store(clock.read32(0) as u64, Ordering::Relaxed); // seconds since 1970
    }

    // The interrupt controller: enable it and the timer's interrupt, so
    // that the timer can wake the CPU in `halt`.
    if let Some(gic) = tree.compatible("arm,gic-v3") {
        let (Some((distributor, _)), Some((redistributor, _))) = (gic.reg(0), gic.reg(1)) else { return };
        let (distributor, redistributor) = unsafe { (Registers::new(distributor), Registers::new(redistributor)) };
        // SAFETY: the system registers of the CPU's interrupt interface
        unsafe {
            asm!(
                "mrs {t}, S3_0_C12_C12_5", // ICC_SRE_EL1: use system registers
                "orr {t}, {t}, #1",
                "msr S3_0_C12_C12_5, {t}",
                "isb",
                "msr S3_0_C4_C6_0, {all}", // ICC_PMR_EL1: let all priorities through
                "msr S3_0_C12_C12_7, {on}", // ICC_IGRPEN1_EL1: group 1 on
                "isb",
                t = out(reg) _,
                all = in(reg) 0xFFu64,
                on = in(reg) 1u64,
                options(nomem, nostack, preserves_flags)
            );
        }
        distributor.write32(0x000, 0x13); // affinity routing, groups on
        redistributor.write32(0x014, redistributor.read32(0x014) & !2); // this CPU is awake
        let mut spins = 0;
        while spins < 100_000 && redistributor.read32(0x014) & 4 != 0 {
            spins += 1;
        }
        let per_cpu = redistributor.at(0x10000); // the frame for this CPU's own interrupts
        per_cpu.write32(0x080, per_cpu.read32(0x080) | 1 << TIMER_INTERRUPT); // group 1
        per_cpu.write8(0x400 + TIMER_INTERRUPT, 0x80); // priority
        per_cpu.write32(0x100, 1 << TIMER_INTERRUPT); // enable
    } else if let Some(gic) = tree.compatible("arm,cortex-a15-gic").or_else(|| tree.compatible("arm,gic-400")) {
        let (Some((distributor, _)), Some((cpu, _))) = (gic.reg(0), gic.reg(1)) else { return };
        let (distributor, cpu) = unsafe { (Registers::new(distributor), Registers::new(cpu)) };
        distributor.write32(0x000, 1); // distributor on
        distributor.write32(0x100, 1 << TIMER_INTERRUPT); // enable
        cpu.write32(0x004, 0xFF); // let all priorities through
        cpu.write32(0x000, 1); // CPU interface on
    }
}

/// Stops the CPU for good.
pub fn stop() -> ! {
    loop {
        // SAFETY: with the timer off and interrupts masked nothing wakes it
        unsafe { asm!("msr cntv_ctl_el0, xzr", "wfi", options(nomem, nostack, preserves_flags)) };
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
