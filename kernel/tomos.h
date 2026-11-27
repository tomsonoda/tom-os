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

/* ---- terminals and the console (kernel/console.c) ---- */
/* A character-cell terminal. Cells use the VGA text layout (attr << 8 | char),
 * so the hardware text screen and terminal windows share one implementation. */
typedef struct term {
    int cols, rows;
    u16 *cells;
    int cx, cy;
    u8 attr;
    int esc_state, esc_args[4], esc_n, esc_private;
    u64 dirty; /* one bit per row that needs redrawing (windows only; up to 64 rows) */
    u32 *uc;   /* per cell: Unicode code point if not ASCII, TERM_CONT for the right half
                  of a wide character, 0 otherwise (windows only) */
    u32 utf_cp;
    int utf_need; /* UTF-8 continuation bytes still expected */
    int hw;    /* backed by the VGA text screen */
} term;

#define TERM_CONT 0xFFFFFFFFu

/* Markers in the shell's output for the home screen's chat view (gui/home.c).
 * They are private escape sequences, which terminals ignore. */
#define UI_REPLY "\x1b[?7101h"  /* the agent's answer */
#define UI_NOTE "\x1b[?7102h"   /* a step such as a tool call */
#define UI_ERROR "\x1b[?7103h"
#define UI_STATUS "\x1b[?7104h" /* replaces the waiting indicator; empty clears it */
#define UI_HIDE "\x1b[?7105h"   /* not shown in the chat (prompt, line editing) */
#define UI_END "\x1b[?7100l"
typedef struct homeview homeview;

/* Where a thread's console I/O goes. */
typedef struct tty {
    term *term;      /* may be NULL */
    strbuf *cap;     /* when set, output is captured here instead */
    size_t cap_limit;
    int serial;      /* mirror output to COM1 */
    u8 inq[1024];
    unsigned head, tail;
    int brk;         /* Ctrl-C seen */
    homeview *home;  /* also show the output in the home screen's chat view */
    int home_mute;   /* line editing: echo goes to the terminal only */
} tty;

void console_init(void);
tty *console_tty0(void);              /* the shell's tty (serial + first screen) */
tty *tty_create(int cols, int rows);  /* tty with its own terminal buffer */
void term_resize(term *t, int cols, int rows); /* keeps the lines around the cursor */
void tty_destroy(tty *t);
void tty_input(tty *t, const char *bytes); /* deliver typed characters */
void console_poll_input(void);        /* drain serial / keyboard / mouse */
void mouse_init(void);
int mouse_absolute(void);             /* the host pointer is followed exactly (vmmouse) */

/* All of these act on the calling thread's tty. */
void console_write(const char *s, size_t n);
void console_puts(const char *s);
/* While capturing, console output goes to the buffer instead of the screen. */
void console_capture(strbuf *b, size_t limit);
#define KEY_TIMEOUT (-1)
#define KEY_BREAK (-2)
/* Next input byte; timeout_ms < 0 waits forever. KEY_BREAK on Ctrl-C or when
 * the thread is being killed. */
int console_getc(int timeout_ms);
/* True once if Ctrl-C was pressed since the last call. */
int console_break_pending(void);
void console_clear_break(void);
/* Line editor; with `history`, the up/down arrows recall earlier lines.
 * Returns the length, or KEY_BREAK. */
int console_readline(char *buf, size_t max, int history);
/* Japanese input: typed letters become kana while it is on. Toggled by
 * Alt+Space / Ctrl+Space / the kana and eisu keys, or the bytes 0x1D (toggle),
 * 0x1E (on), 0x1F (off) from the serial line. */
int ime_enabled(void);
/* rust/src/agent.rs: asks the LLM to convert hiragana to kanji. `avoid` (may be
 * NULL) is a previous answer to replace with another one. */
int agent_convert_kana(const char *kana, const char *avoid, strbuf *out, strbuf *err);

/* ---- threads (kernel/sched.c): cooperative, round-robin ---- */
typedef struct thread thread;
struct window;
thread *thread_spawn(void (*fn)(void *), void *arg, tty *t);
void thread_exit(void) __attribute__((noreturn));
void thread_kill(thread *t);          /* ask a thread to stop */
int thread_killed(void);              /* has the calling thread been asked to stop? */
tty *thread_tty(void);
struct homeview *thread_home(void);    /* the calling thread's home view, or NULL */
void thread_set_tty(tty *t);
struct window *thread_window(void);
void thread_set_window(struct window *w);
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

/* ---- PCI ---- */
u32 pci_read(u8 bus, u8 dev, u8 fn, u8 off); /* rust/src/pci.rs */
void pci_write(u8 bus, u8 dev, u8 fn, u8 off, u32 v);

/* ---- graphics (rust/src/gfx.rs) ---- */
typedef struct surface {
    u32 *px; /* 0x00RRGGBB */
    int w, h;
    int cx0, cy0, cx1, cy1; /* clip rectangle, exclusive upper bounds */
} surface;
/* Switches to a linear framebuffer: 0 on success, -2 if the adapter has too
 * little video memory, -1 if graphics are unavailable or the mode failed. */
int fb_init(int w, int h);
size_t fb_vram(void);
extern surface fb_screen;
void surface_init(surface *s, u32 *px, int w, int h);
void gfx_fill(surface *s, int x, int y, int w, int h, u32 c);
void gfx_pixel(surface *s, int x, int y, u32 c);
void gfx_line(surface *s, int x0, int y0, int x1, int y1, u32 c);
void gfx_circle(surface *s, int cx, int cy, int r, u32 c, int fill);
void gfx_glyph(surface *s, int x, int y, u8 ch, u32 fg, long bg, int scale); /* bg < 0: transparent */
void gfx_text(surface *s, int x, int y, const char *str, size_t n, u32 fg, long bg, int scale);
void gfx_blit(surface *dst, int dx, int dy, const u32 *src, int sw, int sh);
/* UTF-8: decodes one character, returns the bytes used (at least 1). */
int utf8_decode(const char *s, size_t n, u32 *cp);
/* Japanese font: 16x16 glyph (two bytes per row) or NULL if there is none. */
const u8 *font_wide(u32 cp);
/* Screen columns a character takes: 2 for full-width Japanese, else 1. */
int font_cells(u32 cp);
void gfx_glyph16(surface *s, int x, int y, const u8 *bits, u32 fg, long bg, int scale);
void gfx_round(surface *s, int x, int y, int w, int h, int r, u32 c); /* filled, rounded corners */
void gfx_blit_resize(surface *dst, int dx, int dy, int dw, int dh, const u32 *src, int sw, int sh);

/* ---- window manager (gui/wm.c) ---- */
typedef struct window {
    int used;
    int x, y, w, h;   /* content rectangle on screen */
    char title[48];
    u32 *front;       /* what the compositor shows */
    u32 *back;        /* canvas the app draws on (graphics windows) */
    int canvas;       /* 0: shows `tty->term`, 1: shows the canvas */
    tty *tty;
    thread *owner;    /* killed when the window is closed; NULL = not closable */
    u8 keys[256];     /* keys currently held (scancode, +0x80 for extended) */
    int mx, my, mb;   /* mouse position inside the content, buttons */
    int mb_latch;     /* buttons pressed since the app last looked (short clicks) */
    u64 last_flip;
} window;
/* The screen is w x h device pixels; everything is drawn at 1/scale of that
 * and enlarged by `scale` (1 to 4) on the way out. scale 0 picks a default. */
int wm_init(int w, int h, int scale); /* 0 if graphics are available */
int wm_set_display(int w, int h, int scale, strbuf *err); /* change at run time */
int wm_display_w(void);
int wm_display_h(void);
int wm_scale(void);
int wm_default_scale(int w, int h);
int wm_active(void);
void wm_tick(void);
window *wm_create(const char *title, tty *t, thread *owner);
void wm_destroy(window *w);
int wm_set_canvas(window *w, int width, int height);
void wm_set_text(window *w);
int wm_is_canvas(const window *w);        /* for Rust, which does not know the struct */
void wm_set_owner(window *w, thread *owner);
void wm_present(window *w);
void wm_set_title(window *w, const char *title);
void wm_key(int code, int down, int alt, const char *chars);
void wm_mouse(int dx, int dy, int buttons);          /* relative pointer (PS/2) */
void wm_mouse_abs(int x, int y, int buttons, int wheel); /* absolute pointer, 0..65535 */
/* Layouts: full screen (one app at a time, like a phone) or overlapping windows. */
enum { LAYOUT_FULL, LAYOUT_WINDOWS };
void wm_set_layout(int layout);
void wm_set_home(window *w); /* the shell: the home screen apps open on top of */
void wm_ime_changed(void);   /* redraw the input-method indicator */

/* ---- home screen chat view (rust/src/home.rs) ---- */
homeview *home_create(tty *t);
void home_feed(homeview *h, const char *s, size_t n);
void home_add_user(homeview *h, const char *line);
/* The line being typed: text, cursor (byte offset), and the composition at the cursor. */
void home_set_input(homeview *h, const char *line, size_t n, size_t cur, const char *comp, size_t cn,
                    int converted);
void home_render(homeview *h, surface *s);
int home_dirty(const homeview *h);
void home_invalidate(homeview *h);
void home_click(homeview *h, int x, int y);
void home_scroll(homeview *h, int dy);
int wm_layout(void);
void wm_canvas_max(int *w, int *h); /* largest canvas an app can use */
int wm_screen_w(void);
int wm_screen_h(void);

/* ---- memory (kernel/mem.c) ---- */
void mem_init(u32 mb_info);
size_t mem_total(void);
size_t mem_used(void);
const char *boot_cmdline(void);
int cmdline_get(const char *key, char *out, size_t n);

/* ---- network (rust/src/net.rs) ---- */
void net_init(void);
int net_up(void);
void net_poll(void);

/* ---- Lua runtime (lua/lbind.c) ---- */
/* Runs a chunk in a fresh Lua state. Interactive runs own the console; others
 * capture output into `out` and see EOF on input. Returns 0 on success; on
 * failure the message is appended to `err`. */
int lua_run(const char *name, const char *code, size_t len, int interactive, int timeout_ms,
            strbuf *out, strbuf *err);

/* ---- shell (rust/src/shell.rs) ---- */
void shell_run(void);

int kprintf(const char *fmt, ...);

#endif
