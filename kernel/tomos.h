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

/* ---- console (kernel/console.c) ---- */
void console_init(void);
void console_write(const char *s, size_t n);
void console_puts(const char *s);
/* While capturing, console output goes to the buffer instead of the screen. */
void console_capture(strbuf *b, size_t limit);
int console_capturing(void);
#define KEY_TIMEOUT (-1)
#define KEY_BREAK (-2)
/* Next input byte; timeout_ms < 0 waits forever. KEY_BREAK on Ctrl-C. */
int console_getc(int timeout_ms);
/* True once if Ctrl-C was pressed since the last call. */
int console_break_pending(void);
void console_clear_break(void);
/* Line editor. Returns length, or KEY_BREAK. */
int console_readline(char *buf, size_t max);

/* ---- CPU / interrupts / time (kernel/cpu.c) ---- */
void cpu_init(void);
void cpu_idle(void); /* HLT until the next interrupt (1 ms timer at the latest) */
u64 time_ms(void);   /* milliseconds since boot */
void sleep_ms(u64 ms);
u64 rtc_unix_time(void); /* wall clock, UTC */
void cpu_poweroff(void);
void cpu_reboot(void);

/* ---- memory (kernel/mem.c) ---- */
void mem_init(u32 mb_info);
size_t mem_total(void);
size_t mem_used(void);
const char *boot_cmdline(void);
int cmdline_get(const char *key, char *out, size_t n);

/* ---- network ---- */
int nic_init(void);                 /* kernel/virtio_net.c; 0 on success */
void nic_mac(u8 mac[6]);
void nic_send(const void *frame, size_t len);
void nic_poll(void (*handler)(const u8 *frame, size_t len));

void net_init(void);                /* net/net.c */
int net_up(void);
void net_poll(void);
u32 net_ip(void);
int net_parse_ip(const char *s, u32 *ip);
void net_fmt_ip(u32 ip, char out[16]);
int ip_send(u32 dst, u8 proto, const void *payload, size_t len);
u16 net_checksum(u32 sum, const void *data, size_t n);
u32 net_sum(u32 sum, const void *data, size_t n);
void tcp_input(u32 src, const u8 *seg, size_t len); /* net/tcp.c */

#define NET_ETIMEOUT (-2)
#define NET_EBREAK (-3)
#define NET_ERESET (-4)
#define NET_ENONET (-5)
#define NET_EPROTO (-6)
int tcp_connect(u32 ip, u16 port, int timeout_ms);
int tcp_send(const void *data, size_t len);
/* >0 bytes, 0 on EOF, <0 on error. */
int tcp_recv(void *buf, size_t max, int timeout_ms);
void tcp_close(void);
const char *net_strerror(int e);

/* net/http.c: POST a JSON body, collect the decoded response body. */
int http_post_json(u32 ip, u16 port, const char *path, const char *body, size_t body_len,
                   strbuf *resp, int *status, int timeout_ms);

/* ---- RAM filesystem (fs/ramfs.c) ---- */
int fs_write(const char *path, const char *data, size_t len);
const char *fs_read(const char *path, size_t *len); /* NULL if missing */
int fs_delete(const char *path);
int fs_rename(const char *from, const char *to);
/* Iterate: pass *it = NULL first; returns name or NULL at the end. */
const char *fs_next(void **it, size_t *len);

/* ---- JSON (lib/json.c) ---- */
enum { J_NULL, J_BOOL, J_NUM, J_STR, J_ARR, J_OBJ };
typedef struct jv {
    int type;
    struct jv *next;  /* sibling */
    struct jv *child; /* first element / member */
    char *key;        /* member name when inside an object */
    char *str;
    size_t len;
    double num;
} jv;
jv *json_parse(const char *s, size_t len);
void json_free(jv *v);
jv *json_get(const jv *obj, const char *key);
jv *json_at(const jv *arr, int idx);
const char *json_str(const jv *v); /* NULL unless a string */
void json_quote(strbuf *out, const char *s, size_t len);

/* ---- Lua runtime (lua/lbind.c) ---- */
/* Runs a chunk in a fresh Lua state. Interactive runs own the console; others
 * capture output into `out` and see EOF on input. Returns 0 on success; on
 * failure the message is appended to `err`. */
int lua_run(const char *name, const char *code, size_t len, int interactive, int timeout_ms,
            strbuf *out, strbuf *err);

/* ---- agent ---- */
void shell_run(void);                    /* agent/shell.c */
void agent_reset(void);                  /* agent/agent.c */
void agent_turn(const char *user_text);
int app_run(const char *name_or_path, strbuf *err); /* resolve + run interactively */

int kprintf(const char *fmt, ...);

#endif
