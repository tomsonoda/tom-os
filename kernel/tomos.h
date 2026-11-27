/* tom-os kernel interfaces */
#ifndef TOMOS_H
#define TOMOS_H

#include <stddef.h>
#include <stdint.h>
#include <stdarg.h>

#define TOMOS_VERSION "0.1"

typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef uint64_t u64;
typedef int16_t s16;
typedef int64_t s64;

/* ---- port I/O ---- */
static inline void outb(u16 p, u8 v) { __asm__ volatile("outb %0, %1" : : "a"(v), "Nd"(p)); }
static inline void outw(u16 p, u16 v) { __asm__ volatile("outw %0, %1" : : "a"(v), "Nd"(p)); }
static inline void outl(u16 p, u32 v) { __asm__ volatile("outl %0, %1" : : "a"(v), "Nd"(p)); }
static inline u8 inb(u16 p) { u8 v; __asm__ volatile("inb %1, %0" : "=a"(v) : "Nd"(p)); return v; }
static inline u16 inw(u16 p) { u16 v; __asm__ volatile("inw %1, %0" : "=a"(v) : "Nd"(p)); return v; }
static inline u32 inl(u16 p) { u32 v; __asm__ volatile("inl %1, %0" : "=a"(v) : "Nd"(p)); return v; }
static inline u64 rdtsc(void) {
    u32 lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return ((u64)hi << 32) | lo;
}

/* ---- growable byte buffer (lib/libc.c) ---- */
typedef struct {
    char *p;
    size_t len, cap;
} strbuf;
void sb_init(strbuf *b);
void sb_free(strbuf *b);
void sb_put(strbuf *b, const char *s, size_t n);
void sb_puts(strbuf *b, const char *s);
void sb_putc(strbuf *b, char c);
void sb_printf(strbuf *b, const char *fmt, ...);
char *sb_cstr(strbuf *b); /* NUL-terminated view, never NULL */

/* ---- console (rust/src/console.rs) ---- */
/* Writes to the calling thread's console. */
void console_write(const char *s, size_t n);
/* Ends any capture of console output, so that what follows is seen. */
void console_capture(void *unused, size_t unused2);
void console_poll_input(void);        /* drain serial / keyboard / mouse */
int kprintf(const char *fmt, ...);    /* lib/printf.c */

/* ---- threads (kernel/sched.c): cooperative, round-robin ---- */
typedef struct thread thread;
/* `tty` names the console the thread writes to (see rust/src/console.rs). */
thread *thread_spawn(void (*fn)(void *), void *arg, size_t tty);
void thread_exit(void) __attribute__((noreturn));
void thread_kill(thread *t);          /* ask a thread to stop */
int thread_killed(void);              /* has the calling thread been asked to stop? */
size_t thread_tty(void);
void thread_set_tty(size_t tty);
size_t thread_window(void);           /* the window id of the thread's app, or 0 */
void thread_set_window(size_t w);
void *thread_data(void);              /* per-thread slot used by the Lua runtime */
void thread_set_data(void *p);
void sched_yield(void);               /* let other threads run; caller stays busy */
void cpu_idle(void);                  /* caller is waiting: run others, HLT if all wait */

/* ---- CPU / interrupts / time (kernel/cpu.c) ---- */
void cpu_init(void);
void cpu_halt(void); /* HLT until the next interrupt (1 ms timer at the latest) */
u64 time_ms(void);   /* milliseconds since boot */
void sleep_ms(u64 ms);
u64 rtc_unix_time(void); /* wall clock, UTC */
void cpu_poweroff(void);
void cpu_reboot(void);

/* ---- sound (kernel/ac97.c) ---- */
enum { SOUND_SQUARE, SOUND_SINE, SOUND_TRIANGLE, SOUND_SAW, SOUND_NOISE };
int sound_init(void); /* 0 if an AC'97 device was found */
int sound_available(void);
void sound_poll(void); /* keeps the output buffers filled; called from the scheduler tick */
/* Non-blocking; owner (may be NULL) groups sounds for sound_stop/sound_busy. */
int sound_tone(double freq, int ms, double vol, int wave, int delay_ms, void *owner);
int sound_pcm(const float *samples, size_t n, int rate, double vol, int delay_ms, void *owner);
int sound_melody(const char *notes, int default_ms, double vol, int wave, void *owner);
void sound_stop(void *owner); /* NULL: everything */
int sound_busy(void *owner);

/* ---- PCI (rust/src/pci.rs) ---- */
u32 pci_read(u8 bus, u8 dev, u8 fn, u8 off);
void pci_write(u8 bus, u8 dev, u8 fn, u8 off, u32 v);

/* ---- window manager (rust/src/wm.rs) ---- */
int wm_active(void);
void wm_tick(void);

/* ---- memory (kernel/mem.c) ---- */
void mem_init(u32 mb_info);
size_t mem_total(void);
size_t mem_used(void);
const char *boot_cmdline(void);
int cmdline_get(const char *key, char *out, size_t n);

/* ---- network (rust/src/net.rs) ---- */
void net_poll(void);

#endif
