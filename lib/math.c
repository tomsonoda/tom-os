/* Minimal libm built on x87 instructions, plus strtod. */
#include <ctype.h>
#include <math.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef long double ld;

#define LOG2E 1.44269504088896340736L
#define LN2 0.69314718055994530942L
#define LOG10_2 0.30102999566398119521L

static ld x_sqrt(ld x) {
    __asm__("fsqrt" : "+t"(x));
    return x;
}
static ld x_sin(ld x) {
    __asm__("fsin" : "+t"(x));
    return x;
}
static ld x_cos(ld x) {
    __asm__("fcos" : "+t"(x));
    return x;
}
static ld x_rint(ld x) {
    __asm__("frndint" : "+t"(x));
    return x;
}
static ld x_atan2(ld y, ld x) {
    ld r;
    __asm__("fpatan" : "=t"(r) : "0"(x), "u"(y) : "st(1)");
    return r;
}
/* y * log2(x) */
static ld x_ylog2(ld y, ld x) {
    ld r;
    __asm__("fyl2x" : "=t"(r) : "0"(x), "u"(y) : "st(1)");
    return r;
}
static ld x_exp2(ld x) {
    if (x != x) return x;
    if (x > 16400) return __builtin_huge_vall();
    if (x < -16500) return 0;
    ld n = x_rint(x), f = x - n, r;
    __asm__("f2xm1" : "+t"(f));
    f += 1.0L;
    __asm__("fscale" : "=t"(r) : "0"(f), "u"(n));
    return r;
}

ld __log2l(ld x) { return x_ylog2(1.0L, x); }

double fabs(double x) { return __builtin_fabs(x); }

double floor(double x) {
    if (x != x || x >= 9.2e18 || x <= -9.2e18) return x;
    double t = (double)(long long)x;
    return t > x ? t - 1 : t;
}

double ceil(double x) {
    if (x != x || x >= 9.2e18 || x <= -9.2e18) return x;
    double t = (double)(long long)x;
    return t < x ? t + 1 : t;
}

double fmod(double x, double y) {
    if (x != x || y != y || y == 0 || isinf(x)) return __builtin_nan("");
    if (isinf(y)) return x;
    ld a = x, b = y;
    unsigned short sw;
    do {
        __asm__("fprem\n\tfnstsw %%ax" : "+t"(a), "=a"(sw) : "u"(b));
    } while (sw & 0x400);
    return (double)a;
}

double sqrt(double x) { return (double)x_sqrt(x); }
double sin(double x) { return (double)x_sin(x); }
double cos(double x) { return (double)x_cos(x); }
double tan(double x) { return (double)(x_sin(x) / x_cos(x)); }
double atan2(double y, double x) { return (double)x_atan2(y, x); }
double atan(double x) { return (double)x_atan2(x, 1.0L); }
double asin(double x) { return (double)x_atan2(x, x_sqrt(1.0L - (ld)x * x)); }
double acos(double x) { return (double)x_atan2(x_sqrt(1.0L - (ld)x * x), x); }

double exp(double x) { return (double)x_exp2((ld)x * LOG2E); }

static double log_scaled(double x, ld scale) {
    if (x != x) return x;
    if (x == 0) return -HUGE_VAL;
    if (x < 0) return __builtin_nan("");
    return (double)x_ylog2(scale, x);
}
double log(double x) { return log_scaled(x, LN2); }
double log2(double x) { return log_scaled(x, 1.0L); }
double log10(double x) { return log_scaled(x, LOG10_2); }

double pow(double x, double y) {
    if (y == 0 || x == 1) return 1;
    if (x != x || y != y) return __builtin_nan("");
    if (y == floor(y) && y > -2147483648.0 && y < 2147483648.0) {
        long long n = (long long)y;
        unsigned long long k = n < 0 ? -n : n;
        ld r = 1, b = x;
        while (k) {
            if (k & 1) r *= b;
            b *= b;
            k >>= 1;
        }
        return (double)(n < 0 ? 1.0L / r : r);
    }
    if (x == 0) return y > 0 ? 0 : HUGE_VAL;
    if (x < 0) return __builtin_nan("");
    return (double)x_exp2(x_ylog2(y, x));
}

double ldexp(double x, int e) {
    ld r, v = x, n = e;
    __asm__("fscale" : "=t"(r) : "0"(v), "u"(n));
    return (double)r;
}

double frexp(double x, int *e) {
    uint64_t bits;
    *e = 0;
    if (x == 0 || x != x || isinf(x)) return x;
    memcpy(&bits, &x, 8);
    int ex = (int)((bits >> 52) & 0x7ff);
    if (ex == 0) { /* subnormal: normalise first */
        x *= 18014398509481984.0; /* 2^54 */
        memcpy(&bits, &x, 8);
        ex = (int)((bits >> 52) & 0x7ff) - 54;
    }
    *e = ex - 1022;
    bits = (bits & ~(0x7ffULL << 52)) | (1022ULL << 52);
    memcpy(&x, &bits, 8);
    return x;
}

/* 10^n for n >= 0 */
ld __pow10l(int n) {
    ld r = 1, b = 10;
    for (unsigned k = (unsigned)n; k; k >>= 1) {
        if (k & 1) r *= b;
        b *= b;
    }
    return r;
}

static int hexval(int c) {
    if (isdigit(c)) return c - '0';
    c = tolower(c);
    return (c >= 'a' && c <= 'f') ? c - 'a' + 10 : -1;
}

double strtod(const char *s, char **end) {
    const char *p = s;
    int neg = 0, any = 0;
    ld v;
    while (isspace((unsigned char)*p)) p++;
    if (*p == '+' || *p == '-') neg = *p++ == '-';

    if (strncasecmp(p, "inf", 3) == 0) {
        p += strncasecmp(p, "infinity", 8) == 0 ? 8 : 3;
        if (end) *end = (char *)p;
        return neg ? -HUGE_VAL : HUGE_VAL;
    }
    if (strncasecmp(p, "nan", 3) == 0) {
        if (end) *end = (char *)(p + 3);
        return __builtin_nan("");
    }

    if (p[0] == '0' && (p[1] == 'x' || p[1] == 'X')) {
        uint64_t m = 0;
        int bexp = 0, seen_dot = 0;
        const char *q = p + 2;
        for (;; q++) {
            if (*q == '.' && !seen_dot) {
                seen_dot = 1;
                continue;
            }
            int h = hexval((unsigned char)*q);
            if (h < 0) break;
            any = 1;
            if (m >> 60) {
                if (!seen_dot) bexp += 4;
            } else {
                m = (m << 4) | (unsigned)h;
                if (seen_dot) bexp -= 4;
            }
        }
        if (!any) { /* just "0" followed by 'x' */
            if (end) *end = (char *)(p + 1);
            return neg ? -0.0 : 0.0;
        }
        if ((*q == 'p' || *q == 'P') &&
            (isdigit((unsigned char)q[1]) ||
             ((q[1] == '+' || q[1] == '-') && isdigit((unsigned char)q[2])))) {
            int eneg = 0, e = 0;
            q++;
            if (*q == '+' || *q == '-') eneg = *q++ == '-';
            while (isdigit((unsigned char)*q)) {
                if (e < 100000) e = e * 10 + (*q - '0');
                q++;
            }
            bexp += eneg ? -e : e;
        }
        ld r, n = bexp;
        v = (ld)m;
        __asm__("fscale" : "=t"(r) : "0"(v), "u"(n));
        if (end) *end = (char *)q;
        return (double)(neg ? -r : r);
    }

    uint64_t m = 0;
    int dexp = 0, seen_dot = 0;
    for (;; p++) {
        if (*p == '.' && !seen_dot) {
            seen_dot = 1;
            continue;
        }
        if (!isdigit((unsigned char)*p)) break;
        any = 1;
        if (m < 1844674407370955160ULL) {
            m = m * 10 + (unsigned)(*p - '0');
            if (seen_dot) dexp--;
        } else if (!seen_dot) {
            dexp++;
        }
    }
    if (!any) {
        if (end) *end = (char *)s;
        return 0;
    }
    if ((*p == 'e' || *p == 'E') &&
        (isdigit((unsigned char)p[1]) ||
         ((p[1] == '+' || p[1] == '-') && isdigit((unsigned char)p[2])))) {
        int eneg = 0, e = 0;
        p++;
        if (*p == '+' || *p == '-') eneg = *p++ == '-';
        while (isdigit((unsigned char)*p)) {
            if (e < 100000) e = e * 10 + (*p - '0');
            p++;
        }
        dexp += eneg ? -e : e;
    }
    if (dexp > 4900) dexp = 4900;
    if (dexp < -4900) dexp = -4900;
    v = (ld)m;
    v = dexp >= 0 ? v * __pow10l(dexp) : v / __pow10l(-dexp);
    if (end) *end = (char *)p;
    return (double)(neg ? -v : v);
}
