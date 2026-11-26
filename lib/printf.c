/* printf family with floating point support (%e %f %g %a). */
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "../kernel/tomos.h"

typedef long double ld;
ld __log2l(ld x);
ld __pow10l(int n);

typedef struct {
    void (*put)(void *ctx, const char *s, size_t n);
    void *ctx;
    int count;
} outp;

static void emit(outp *o, const char *s, size_t n) {
    if (n) o->put(o->ctx, s, n);
    o->count += (int)n;
}

static void pad(outp *o, char c, int n) {
    char buf[32];
    memset(buf, c, sizeof buf);
    while (n > 0) {
        int k = n > 32 ? 32 : n;
        emit(o, buf, (size_t)k);
        n -= k;
    }
}

static void field(outp *o, const char *prefix, int plen, const char *body, int blen, int width,
                  int left, int zero) {
    int total = plen + blen;
    if (!left && !zero) pad(o, ' ', width - total);
    emit(o, prefix, (size_t)plen);
    if (!left && zero) pad(o, '0', width - total);
    emit(o, body, (size_t)blen);
    if (left) pad(o, ' ', width - total);
}

/* ---- floating point ---- */
static ld scale10(ld v, int k) { return k >= 0 ? v * __pow10l(k) : v / __pow10l(-k); }

static int est_exp10(ld v) {
    ld l = __log2l(v) * 0.30102999566398119521L;
    int e = (int)l;
    if (l < 0 && (ld)e != l) e--;
    return e;
}

/* nd significant decimal digits of v > 0 (1 <= nd <= 18); value = d.ddd * 10^*e */
static void gen_digits(ld v, int nd, char *digs, int *e) {
    int e10 = est_exp10(v);
    uint64_t lim = 1, m;
    for (int i = 0; i < nd; i++) lim *= 10;
    for (int tries = 0;; tries++) {
        m = (uint64_t)(scale10(v, nd - 1 - e10) + 0.5L);
        if (tries > 4) break;
        if (m >= lim) e10++;
        else if (m < lim / 10) e10--;
        else break;
    }
    if (m >= lim) m = lim - 1;
    for (int i = nd - 1; i >= 0; i--) {
        digs[i] = (char)('0' + m % 10);
        m /= 10;
    }
    *e = e10;
}

static int lay_fixed(char *o, const char *digs, int nd, int e, int prec, int alt) {
    char *s = o;
    int i = 0;
    if (e < 0) *o++ = '0';
    else
        for (int k = 0; k <= e; k++) *o++ = i < nd ? digs[i++] : '0';
    if (prec > 0 || alt) *o++ = '.';
    int z = e < 0 ? -e - 1 : 0;
    for (int k = 0; k < prec; k++) {
        if (z > 0) {
            *o++ = '0';
            z--;
        } else {
            *o++ = i < nd ? digs[i++] : '0';
        }
    }
    return (int)(o - s);
}

static int lay_exp(char *o, const char *digs, int nd, int e, int prec, int alt, int upper) {
    char *s = o;
    *o++ = digs[0];
    if (prec > 0 || alt) *o++ = '.';
    for (int k = 1; k <= prec; k++) *o++ = k < nd ? digs[k] : '0';
    *o++ = upper ? 'E' : 'e';
    *o++ = e < 0 ? '-' : '+';
    if (e < 0) e = -e;
    char t[8];
    int n = 0;
    do {
        t[n++] = (char)('0' + e % 10);
        e /= 10;
    } while (e);
    if (n < 2) t[n++] = '0';
    while (n) *o++ = t[--n];
    return (int)(o - s);
}

static int fmt_hexfloat(char *o, double d, int upper) {
    char *s = o;
    uint64_t bits;
    memcpy(&bits, &d, 8);
    uint64_t mant = bits & ((1ULL << 52) - 1);
    int ex = (int)((bits >> 52) & 0x7ff);
    const char *hx = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    *o++ = '0';
    *o++ = upper ? 'X' : 'x';
    if (ex == 0 && mant == 0) {
        *o++ = '0';
        ex = 0;
    } else {
        *o++ = ex ? '1' : '0';
        ex = ex ? ex - 1023 : -1022;
        if (mant) {
            *o++ = '.';
            for (int sh = 48; sh >= 0 && mant; sh -= 4) {
                *o++ = hx[(mant >> sh) & 15];
                mant &= (1ULL << sh) - 1;
            }
        }
    }
    *o++ = upper ? 'P' : 'p';
    *o++ = ex < 0 ? '-' : '+';
    if (ex < 0) ex = -ex;
    char t[8];
    int n = 0;
    do {
        t[n++] = (char)('0' + ex % 10);
        ex /= 10;
    } while (ex);
    while (n) *o++ = t[--n];
    return (int)(o - s);
}

/* Formats |v| (finite); returns length written to o (needs ~512 bytes). */
static int fmt_float(char *o, ld v, char conv, int prec, int alt) {
    char digs[20];
    int e = 0, nd;
    int upper = conv >= 'A' && conv <= 'Z';
    char c = (char)(conv | 0x20);
    if (prec < 0) prec = 6;
    if (prec > 99) prec = 99;

    if (c == 'a') return fmt_hexfloat(o, (double)v, upper);

    if (c == 'f') {
        if (v == 0) return lay_fixed(o, "0", 1, 0, prec, alt);
        nd = est_exp10(v) + 1 + prec;
        if (nd <= 0) {
            /* rounds to zero or to the smallest representable step */
            int one = nd == 0 && scale10(v, prec) >= 0.5L;
            digs[0] = one ? '1' : '0';
            return lay_fixed(o, digs, 1, one ? -prec : 0, prec, alt);
        }
        if (nd > 18) nd = 18;
        gen_digits(v, nd, digs, &e);
        return lay_fixed(o, digs, nd, e, prec, alt);
    }

    if (c == 'e') {
        nd = prec + 1 > 18 ? 18 : prec + 1;
        if (v == 0) memset(digs, '0', (size_t)nd);
        else gen_digits(v, nd, digs, &e);
        return lay_exp(o, digs, nd, e, prec, alt, upper);
    }

    /* %g */
    int P = prec == 0 ? 1 : prec;
    nd = P > 18 ? 18 : P;
    if (v == 0) memset(digs, '0', (size_t)nd);
    else gen_digits(v, nd, digs, &e);
    if (!alt)
        while (nd > 1 && digs[nd - 1] == '0') nd--;
    if (e < -4 || e >= P) return lay_exp(o, digs, nd, e, alt ? P - 1 : nd - 1, alt, upper);
    int fp = alt ? P - 1 - e : nd - 1 - e;
    return lay_fixed(o, digs, nd, e, fp < 0 ? 0 : fp, alt);
}

/* ---- core ---- */
static int vformat(outp *o, const char *fmt, va_list ap) {
    char buf[520];
    for (; *fmt; fmt++) {
        if (*fmt != '%') {
            const char *s = fmt;
            while (fmt[1] && fmt[1] != '%') fmt++;
            emit(o, s, (size_t)(fmt - s + 1));
            continue;
        }
        fmt++;
        int left = 0, plus = 0, space = 0, alt = 0, zero = 0;
        for (;; fmt++) {
            if (*fmt == '-') left = 1;
            else if (*fmt == '+') plus = 1;
            else if (*fmt == ' ') space = 1;
            else if (*fmt == '#') alt = 1;
            else if (*fmt == '0') zero = 1;
            else break;
        }
        int width = 0, prec = -1;
        if (*fmt == '*') {
            width = va_arg(ap, int);
            if (width < 0) {
                left = 1;
                width = -width;
            }
            fmt++;
        } else {
            while (*fmt >= '0' && *fmt <= '9') width = width * 10 + (*fmt++ - '0');
        }
        if (*fmt == '.') {
            fmt++;
            prec = 0;
            if (*fmt == '*') {
                prec = va_arg(ap, int);
                fmt++;
            } else {
                while (*fmt >= '0' && *fmt <= '9') prec = prec * 10 + (*fmt++ - '0');
            }
        }
        int lng = 0; /* 0 int, 1 long, 2 long long/size, 3 long double */
        for (;; fmt++) {
            if (*fmt == 'l') lng = lng ? 2 : 1;
            else if (*fmt == 'z' || *fmt == 'j' || *fmt == 't') lng = 2;
            else if (*fmt == 'L') lng = 3;
            else if (*fmt != 'h') break;
        }
        char c = *fmt;
        if (!c) break;
        if (left) zero = 0;

        switch (c) {
        case '%':
            emit(o, "%", 1);
            break;
        case 'c': {
            buf[0] = (char)va_arg(ap, int);
            field(o, "", 0, buf, 1, width, left, 0);
            break;
        }
        case 's': {
            const char *s = va_arg(ap, const char *);
            if (!s) s = "(null)";
            size_t n = 0;
            while ((prec < 0 || n < (size_t)prec) && s[n]) n++;
            field(o, "", 0, s, (int)n, width, left, 0);
            break;
        }
        case 'd': case 'i': case 'u': case 'x': case 'X': case 'o': case 'p': {
            unsigned long long u;
            int neg = 0, base = 10;
            const char *digits = "0123456789abcdef";
            char prefix[3];
            int plen = 0;
            if (c == 'p') {
                u = (uintptr_t)va_arg(ap, void *);
                base = 16;
                alt = 1;
            } else if (c == 'd' || c == 'i') {
                long long v = lng >= 2 ? va_arg(ap, long long) : lng == 1 ? va_arg(ap, long)
                                                                         : va_arg(ap, int);
                neg = v < 0;
                u = neg ? 0ULL - (unsigned long long)v : (unsigned long long)v;
            } else {
                u = lng >= 2 ? va_arg(ap, unsigned long long)
                             : lng == 1 ? va_arg(ap, unsigned long) : va_arg(ap, unsigned int);
                if (c == 'x' || c == 'X') base = 16;
                if (c == 'o') base = 8;
                if (c == 'X') digits = "0123456789ABCDEF";
            }
            char *e = buf + sizeof buf, *s = e;
            unsigned long long t = u;
            while (t) {
                *--s = digits[t % (unsigned)base];
                t /= (unsigned)base;
            }
            int nd = (int)(e - s);
            int minimum = prec < 0 ? 1 : prec;
            while (nd < minimum && nd < 500) {
                *--s = '0';
                nd++;
            }
            if (prec >= 0) zero = 0;
            if (neg) prefix[plen++] = '-';
            else if (plus && (c == 'd' || c == 'i')) prefix[plen++] = '+';
            else if (space && (c == 'd' || c == 'i')) prefix[plen++] = ' ';
            if (alt && base == 16 && (u || c == 'p')) {
                prefix[plen++] = '0';
                prefix[plen++] = c == 'X' ? 'X' : 'x';
            }
            if (alt && base == 8 && *s != '0') {
                *--s = '0';
                nd++;
            }
            field(o, prefix, plen, s, nd, width, left, zero);
            break;
        }
        case 'e': case 'E': case 'f': case 'F': case 'g': case 'G': case 'a': case 'A': {
            ld v = lng == 3 ? va_arg(ap, ld) : (ld)va_arg(ap, double);
            char prefix[1];
            int plen = 0, n;
            int neg = __builtin_signbit(v);
            if (neg) {
                prefix[plen++] = '-';
                v = -v;
            } else if (plus) {
                prefix[plen++] = '+';
            } else if (space) {
                prefix[plen++] = ' ';
            }
            int upper = c >= 'A' && c <= 'Z';
            if (v != v) {
                memcpy(buf, upper ? "NAN" : "nan", 3);
                n = 3;
                zero = 0;
                plen = neg ? 1 : plen;
            } else if (v > 1.18e4932L) {
                memcpy(buf, upper ? "INF" : "inf", 3);
                n = 3;
                zero = 0;
            } else {
                n = fmt_float(buf, v, c, prec, alt);
            }
            field(o, prefix, plen, buf, n, width, left, zero);
            break;
        }
        default:
            emit(o, "%", 1);
            emit(o, &c, 1);
            break;
        }
    }
    return o->count;
}

/* ---- sinks ---- */
typedef struct {
    char *buf;
    size_t n, pos;
} bufsink;

static void put_buf(void *ctx, const char *s, size_t n) {
    bufsink *b = ctx;
    for (size_t i = 0; i < n; i++, b->pos++)
        if (b->pos + 1 < b->n) b->buf[b->pos] = s[i];
}

static void put_console(void *ctx, const char *s, size_t n) {
    (void)ctx;
    console_write(s, n);
}

int vsnprintf(char *buf, size_t n, const char *fmt, va_list ap) {
    bufsink b = {buf, n, 0};
    outp o = {put_buf, &b, 0};
    vformat(&o, fmt, ap);
    if (n) buf[b.pos < n ? b.pos : n - 1] = 0;
    return o.count;
}

int snprintf(char *buf, size_t n, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(buf, n, fmt, ap);
    va_end(ap);
    return r;
}

int sprintf(char *buf, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(buf, (size_t)1 << 30, fmt, ap);
    va_end(ap);
    return r;
}

int vfprintf(FILE *f, const char *fmt, va_list ap) {
    (void)f;
    outp o = {put_console, NULL, 0};
    return vformat(&o, fmt, ap);
}

int fprintf(FILE *f, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vfprintf(f, fmt, ap);
    va_end(ap);
    return r;
}

int printf(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vfprintf(stdout, fmt, ap);
    va_end(ap);
    return r;
}

int kprintf(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vfprintf(stdout, fmt, ap);
    va_end(ap);
    return r;
}
