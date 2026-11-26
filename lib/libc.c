/* Minimal freestanding libc for the kernel and the embedded Lua. */
#include <ctype.h>
#include <errno.h>
#include <locale.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "../kernel/tomos.h"

int errno;

/* ---- memory ---- */
void *memcpy(void *d, const void *s, size_t n) {
    void *r = d;
    size_t words = n / 8, rest = n % 8; /* word copies are much faster under emulation */
    __asm__ volatile("rep movsq" : "+D"(d), "+S"(s), "+c"(words) : : "memory");
    __asm__ volatile("rep movsb" : "+D"(d), "+S"(s), "+c"(rest) : : "memory");
    return r;
}

void *memmove(void *d, const void *s, size_t n) {
    u8 *dp = d;
    const u8 *sp = s;
    if (dp == sp || n == 0) return d;
    if (dp < sp || dp >= sp + n) return memcpy(d, s, n);
    while (n--) dp[n] = sp[n];
    return d;
}

void *memset(void *d, int c, size_t n) {
    void *r = d;
    __asm__ volatile("rep stosb" : "+D"(d), "+c"(n) : "a"(c) : "memory");
    return r;
}

int memcmp(const void *a, const void *b, size_t n) {
    const u8 *x = a, *y = b;
    for (size_t i = 0; i < n; i++)
        if (x[i] != y[i]) return x[i] - y[i];
    return 0;
}

void *memchr(const void *s, int c, size_t n) {
    const u8 *p = s;
    for (size_t i = 0; i < n; i++)
        if (p[i] == (u8)c) return (void *)(p + i);
    return NULL;
}

/* ---- strings ---- */
size_t strlen(const char *s) {
    size_t n = 0;
    while (s[n]) n++;
    return n;
}

int strcmp(const char *a, const char *b) {
    while (*a && *a == *b) a++, b++;
    return (u8)*a - (u8)*b;
}

int strncmp(const char *a, const char *b, size_t n) {
    for (size_t i = 0; i < n; i++) {
        if (a[i] != b[i] || !a[i]) return (u8)a[i] - (u8)b[i];
    }
    return 0;
}

int strncasecmp(const char *a, const char *b, size_t n) {
    for (size_t i = 0; i < n; i++) {
        int x = tolower((u8)a[i]), y = tolower((u8)b[i]);
        if (x != y || !x) return x - y;
    }
    return 0;
}

int strcoll(const char *a, const char *b) { return strcmp(a, b); }

char *strcpy(char *d, const char *s) {
    char *r = d;
    while ((*d++ = *s++)) {}
    return r;
}

char *strncpy(char *d, const char *s, size_t n) {
    size_t i = 0;
    for (; i < n && s[i]; i++) d[i] = s[i];
    for (; i < n; i++) d[i] = 0;
    return d;
}

char *strcat(char *d, const char *s) {
    strcpy(d + strlen(d), s);
    return d;
}

char *strchr(const char *s, int c) {
    for (;; s++) {
        if (*s == (char)c) return (char *)s;
        if (!*s) return NULL;
    }
}

char *strrchr(const char *s, int c) {
    const char *r = NULL;
    for (;; s++) {
        if (*s == (char)c) r = s;
        if (!*s) return (char *)r;
    }
}

char *strstr(const char *h, const char *n) {
    size_t nl = strlen(n);
    if (!nl) return (char *)h;
    for (; *h; h++)
        if (*h == *n && strncmp(h, n, nl) == 0) return (char *)h;
    return NULL;
}

size_t strspn(const char *s, const char *accept) {
    size_t n = 0;
    while (s[n] && strchr(accept, s[n])) n++;
    return n;
}

size_t strcspn(const char *s, const char *reject) {
    size_t n = 0;
    while (s[n] && !strchr(reject, s[n])) n++;
    return n;
}

char *strpbrk(const char *s, const char *accept) {
    s += strcspn(s, accept);
    return *s ? (char *)s : NULL;
}

char *strndup(const char *s, size_t n) {
    char *r = malloc(n + 1);
    if (!r) return NULL;
    memcpy(r, s, n);
    r[n] = 0;
    return r;
}

char *strdup(const char *s) { return strndup(s, strlen(s)); }

char *strerror(int e) {
    switch (e) {
    case 0: return "no error";
    case ENOENT: return "no such file";
    case ENOMEM: return "out of memory";
    case EINVAL: return "invalid argument";
    case ERANGE: return "result out of range";
    default: return "error";
    }
}

/* ---- ctype (ASCII only) ---- */
int isdigit(int c) { return c >= '0' && c <= '9'; }
int isupper(int c) { return c >= 'A' && c <= 'Z'; }
int islower(int c) { return c >= 'a' && c <= 'z'; }
int isalpha(int c) { return isupper(c) || islower(c); }
int isalnum(int c) { return isalpha(c) || isdigit(c); }
int isspace(int c) { return c == ' ' || (c >= '\t' && c <= '\r'); }
int isxdigit(int c) { return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'); }
int iscntrl(int c) { return (c >= 0 && c < 32) || c == 127; }
int isprint(int c) { return c >= 32 && c < 127; }
int isgraph(int c) { return c > 32 && c < 127; }
int ispunct(int c) { return isgraph(c) && !isalnum(c); }
int toupper(int c) { return islower(c) ? c - 32 : c; }
int tolower(int c) { return isupper(c) ? c + 32 : c; }

/* ---- numbers ---- */
int abs(int x) { return x < 0 ? -x : x; }

static int digit_val(int c) {
    if (isdigit(c)) return c - '0';
    if (islower(c)) return c - 'a' + 10;
    if (isupper(c)) return c - 'A' + 10;
    return 99;
}

unsigned long long strtoull(const char *s, char **end, int base) {
    const char *p = s;
    int neg = 0, any = 0;
    unsigned long long v = 0;
    while (isspace((u8)*p)) p++;
    if (*p == '+' || *p == '-') neg = *p++ == '-';
    if ((base == 0 || base == 16) && p[0] == '0' && (p[1] == 'x' || p[1] == 'X') &&
        digit_val((u8)p[2]) < 16) {
        p += 2;
        base = 16;
    } else if (base == 0) {
        base = *p == '0' ? 8 : 10;
    }
    while (digit_val((u8)*p) < base) {
        v = v * base + digit_val((u8)*p++);
        any = 1;
    }
    if (end) *end = (char *)(any ? p : s);
    return neg ? -v : v;
}

long long strtoll(const char *s, char **end, int base) { return (long long)strtoull(s, end, base); }
long strtol(const char *s, char **end, int base) { return (long)strtoull(s, end, base); }
unsigned long strtoul(const char *s, char **end, int base) { return strtoull(s, end, base); }
int atoi(const char *s) { return (int)strtol(s, NULL, 10); }

/* ---- misc ---- */
char *getenv(const char *name) {
    (void)name;
    return NULL;
}

void abort(void) {
    kprintf("\n*** abort() called, system halted\n");
    for (;;) __asm__ volatile("cli; hlt");
}

void exit(int code) {
    (void)code;
    abort();
}

static struct lconv the_lconv = {"."};
struct lconv *localeconv(void) { return &the_lconv; }

time_t time(time_t *t) {
    time_t now = (time_t)rtc_unix_time();
    if (t) *t = now;
    return now;
}

clock_t clock(void) { return (clock_t)time_ms(); }

/* ---- strbuf ---- */
void sb_init(strbuf *b) {
    b->p = NULL;
    b->len = b->cap = 0;
}

void sb_free(strbuf *b) {
    free(b->p);
    sb_init(b);
}

static int sb_reserve(strbuf *b, size_t extra) {
    if (b->len + extra + 1 <= b->cap) return 1;
    size_t ncap = b->cap ? b->cap * 2 : 256;
    while (ncap < b->len + extra + 1) ncap *= 2;
    char *np = realloc(b->p, ncap);
    if (!np) return 0;
    b->p = np;
    b->cap = ncap;
    return 1;
}

void sb_put(strbuf *b, const char *s, size_t n) {
    if (!sb_reserve(b, n)) return;
    memcpy(b->p + b->len, s, n);
    b->len += n;
    b->p[b->len] = 0;
}

void sb_puts(strbuf *b, const char *s) { sb_put(b, s, strlen(s)); }
void sb_putc(strbuf *b, char c) { sb_put(b, &c, 1); }

void sb_printf(strbuf *b, const char *fmt, ...) {
    va_list ap, ap2;
    va_start(ap, fmt);
    va_copy(ap2, ap);
    int n = vsnprintf(NULL, 0, fmt, ap);
    va_end(ap);
    if (n > 0 && sb_reserve(b, (size_t)n)) {
        vsnprintf(b->p + b->len, (size_t)n + 1, fmt, ap2);
        b->len += (size_t)n;
    }
    va_end(ap2);
}

char *sb_cstr(strbuf *b) { return b->p ? b->p : ""; }

/* ---- stdio: stdout/stderr go to the console, files are read from the ramfs ---- */
struct FILE {
    int kind; /* 0 = ramfs reader, 1 = console */
    char *data;
    size_t len, pos;
    int eof;
};

static FILE f_stdout = {1, NULL, 0, 0, 0}, f_stderr = {1, NULL, 0, 0, 0}, f_stdin = {0, NULL, 0, 0, 1};
FILE *stdin = &f_stdin, *stdout = &f_stdout, *stderr = &f_stderr;

FILE *fopen(const char *path, const char *mode) {
    size_t len;
    const char *d;
    if (mode[0] != 'r' || !(d = fs_read(path, &len))) {
        errno = ENOENT;
        return NULL;
    }
    FILE *f = calloc(1, sizeof *f);
    if (!f) return NULL;
    f->data = malloc(len + 1);
    if (!f->data) {
        free(f);
        return NULL;
    }
    memcpy(f->data, d, len);
    f->len = len;
    return f;
}

FILE *freopen(const char *path, const char *mode, FILE *f) {
    (void)path;
    (void)mode;
    if (f->kind == 0) {
        f->pos = 0;
        f->eof = 0;
    }
    return f;
}

int fclose(FILE *f) {
    if (f && f != stdin && f != stdout && f != stderr) {
        free(f->data);
        free(f);
    }
    return 0;
}

int getc(FILE *f) {
    if (f->kind != 0 || f->pos >= f->len) {
        f->eof = 1;
        return EOF;
    }
    return (u8)f->data[f->pos++];
}

int fgetc(FILE *f) { return getc(f); }

int ungetc(int c, FILE *f) {
    if (c == EOF || f->kind != 0 || f->pos == 0) return EOF;
    f->pos--;
    f->eof = 0;
    return c;
}

size_t fread(void *p, size_t sz, size_t n, FILE *f) {
    if (f->kind != 0 || sz == 0) return 0;
    size_t want = sz * n, avail = f->len - f->pos;
    if (want > avail) {
        want = avail - avail % sz;
        f->eof = 1;
    }
    memcpy(p, f->data + f->pos, want);
    f->pos += want;
    return want / sz;
}

int feof(FILE *f) { return f->eof; }
int ferror(FILE *f) {
    (void)f;
    return 0;
}
int fflush(FILE *f) {
    (void)f;
    return 0;
}

size_t fwrite(const void *p, size_t sz, size_t n, FILE *f) {
    if (f->kind != 1) return 0;
    console_write(p, sz * n);
    return n;
}

int fputs(const char *s, FILE *f) {
    fwrite(s, 1, strlen(s), f);
    return 0;
}

int fputc(int c, FILE *f) {
    char ch = (char)c;
    fwrite(&ch, 1, 1, f);
    return c;
}

int putchar(int c) { return fputc(c, stdout); }

int puts(const char *s) {
    fputs(s, stdout);
    fputc('\n', stdout);
    return 0;
}
