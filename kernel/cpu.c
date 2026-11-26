/* IDT, PIC, timer, RTC, power. */
#include <string.h>
#include "tomos.h"

/* ------------------------------------------------------------------- IDT */
struct idt_entry {
    u16 off_lo, sel;
    u8 ist, type;
    u16 off_mid;
    u32 off_hi, zero;
} __attribute__((packed));

static struct idt_entry idt[256];
extern u64 exc_table[32];
extern void irq_master(void);
extern void irq_slave(void);

static void idt_set(int vec, u64 handler) {
    idt[vec].off_lo = (u16)handler;
    idt[vec].sel = 0x08;
    idt[vec].ist = 0;
    idt[vec].type = 0x8E;
    idt[vec].off_mid = (u16)(handler >> 16);
    idt[vec].off_hi = (u32)(handler >> 32);
    idt[vec].zero = 0;
}

void exception_handler(u64 vec, u64 err, u64 rip, u64 cr2) {
    console_capture(NULL, 0);
    kprintf("\n*** CPU exception %llu (error %#llx) at rip=%#llx cr2=%#llx\n*** system halted\n",
            vec, err, rip, cr2);
    for (;;) __asm__ volatile("cli; hlt");
}

/* ------------------------------------------------------------------ time */
static u64 tsc_per_ms = 1, tsc_boot;

static void tsc_calibrate(void) {
    /* Time 10 ms on PIT channel 2 and count TSC ticks. */
    outb(0x61, (u8)((inb(0x61) & ~0x02) | 0x01));
    outb(0x43, 0xB0);
    outb(0x42, 11932 & 0xFF);
    outb(0x42, 11932 >> 8);
    u64 t0 = rdtsc();
    while (!(inb(0x61) & 0x20)) {}
    u64 t1 = rdtsc();
    tsc_per_ms = (t1 - t0) / 10;
    if (!tsc_per_ms) tsc_per_ms = 1;
    tsc_boot = t1;
}

u64 time_ms(void) { return (rdtsc() - tsc_boot) / tsc_per_ms; }

void cpu_halt(void) { __asm__ volatile("sti; hlt"); }

void sleep_ms(u64 ms) {
    u64 end = time_ms() + ms;
    while (time_ms() < end) {
        net_poll();
        cpu_idle();
    }
}

static u8 cmos(u8 reg) {
    outb(0x70, reg);
    return inb(0x71);
}

static u64 rtc_base, rtc_base_ms;

static u64 rtc_read(void) {
    u8 s = 0, m = 0, h = 0, d = 1, mo = 1, y = 0, regb;
    for (int tries = 0; tries < 1000; tries++) {
        if (cmos(0x0A) & 0x80) continue; /* update in progress */
        s = cmos(0);
        m = cmos(2);
        h = cmos(4);
        d = cmos(7);
        mo = cmos(8);
        y = cmos(9);
        if (s == cmos(0)) break;
    }
    regb = cmos(0x0B);
    if (!(regb & 4)) {
#define BCD(x) (u8)(((x) & 0x0F) + ((x) >> 4) * 10)
        s = BCD(s); m = BCD(m); h = BCD(h & 0x7F) | (h & 0x80); d = BCD(d); mo = BCD(mo); y = BCD(y);
    }
    if (!(regb & 2) && (h & 0x80)) h = (u8)(((h & 0x7F) + 12) % 24);
    int year = 2000 + y;
    static const int mdays[12] = {0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334};
    u64 days = (u64)(year - 1970) * 365 + (u64)((year - 1969) / 4) + (u64)mdays[(mo - 1) % 12] +
               (u64)(d - 1);
    if (year % 4 == 0 && mo > 2) days++;
    return days * 86400 + (u64)h * 3600 + (u64)m * 60 + s;
}

u64 rtc_unix_time(void) { return rtc_base + (time_ms() - rtc_base_ms) / 1000; }

/* ----------------------------------------------------------------- setup */
void cpu_init(void) {
    for (int i = 0; i < 32; i++) idt_set(i, exc_table[i]);
    for (int i = 32; i < 40; i++) idt_set(i, (u64)irq_master);
    for (int i = 40; i < 48; i++) idt_set(i, (u64)irq_slave);
    struct {
        u16 limit;
        u64 base;
    } __attribute__((packed)) idtr = {sizeof idt - 1, (u64)idt};
    __asm__ volatile("lidt %0" : : "m"(idtr));

    /* Remap the PIC to vectors 32..47; unmask timer, keyboard, COM1. */
    outb(0x20, 0x11); outb(0xA0, 0x11);
    outb(0x21, 0x20); outb(0xA1, 0x28);
    outb(0x21, 0x04); outb(0xA1, 0x02);
    outb(0x21, 0x01); outb(0xA1, 0x01);
    outb(0x21, (u8)~((1 << 0) | (1 << 1) | (1 << 4)));
    outb(0xA1, 0xFF);

    /* PIT channel 0: 1 kHz tick, only used to wake HLT. */
    outb(0x43, 0x36);
    outb(0x40, 1193 & 0xFF);
    outb(0x40, 1193 >> 8);

    tsc_calibrate();
    rtc_base = rtc_read();
    rtc_base_ms = time_ms();
    __asm__ volatile("sti");
}

void cpu_poweroff(void) {
    outw(0x604, 0x2000);  /* QEMU (PIIX4 ACPI) */
    outw(0xB004, 0x2000); /* Bochs / old QEMU */
    for (;;) __asm__ volatile("cli; hlt");
}

void cpu_reboot(void) {
    outb(0x64, 0xFE);
    for (;;) __asm__ volatile("cli; hlt");
}
