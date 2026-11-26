/* Small JSON parser (to a tree) and string quoting. */
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

typedef struct {
    const char *p, *end;
    int depth;
} parser;

static void skip_ws(parser *ps) {
    while (ps->p < ps->end && (*ps->p == ' ' || *ps->p == '\t' || *ps->p == '\n' || *ps->p == '\r'))
        ps->p++;
}

void json_free(jv *v) {
    while (v) {
        jv *next = v->next;
        json_free(v->child);
        free(v->key);
        free(v->str);
        free(v);
        v = next;
    }
}

static int hex4(const char *p, unsigned *out) {
    unsigned v = 0;
    for (int i = 0; i < 4; i++) {
        char c = p[i];
        v <<= 4;
        if (c >= '0' && c <= '9') v |= (unsigned)(c - '0');
        else if (c >= 'a' && c <= 'f') v |= (unsigned)(c - 'a' + 10);
        else if (c >= 'A' && c <= 'F') v |= (unsigned)(c - 'A' + 10);
        else return 0;
    }
    *out = v;
    return 1;
}

static void put_utf8(strbuf *b, unsigned cp) {
    if (cp < 0x80) {
        sb_putc(b, (char)cp);
    } else if (cp < 0x800) {
        sb_putc(b, (char)(0xC0 | (cp >> 6)));
        sb_putc(b, (char)(0x80 | (cp & 0x3F)));
    } else if (cp < 0x10000) {
        sb_putc(b, (char)(0xE0 | (cp >> 12)));
        sb_putc(b, (char)(0x80 | ((cp >> 6) & 0x3F)));
        sb_putc(b, (char)(0x80 | (cp & 0x3F)));
    } else {
        sb_putc(b, (char)(0xF0 | (cp >> 18)));
        sb_putc(b, (char)(0x80 | ((cp >> 12) & 0x3F)));
        sb_putc(b, (char)(0x80 | ((cp >> 6) & 0x3F)));
        sb_putc(b, (char)(0x80 | (cp & 0x3F)));
    }
}

/* Parses a string literal at ps->p (which points at the opening quote). */
static char *parse_string(parser *ps, size_t *len) {
    strbuf b;
    sb_init(&b);
    sb_put(&b, "", 0);
    ps->p++;
    while (ps->p < ps->end && *ps->p != '"') {
        char c = *ps->p++;
        if (c != '\\') {
            sb_putc(&b, c);
            continue;
        }
        if (ps->p >= ps->end) goto fail;
        c = *ps->p++;
        switch (c) {
        case 'n': sb_putc(&b, '\n'); break;
        case 't': sb_putc(&b, '\t'); break;
        case 'r': sb_putc(&b, '\r'); break;
        case 'b': sb_putc(&b, '\b'); break;
        case 'f': sb_putc(&b, '\f'); break;
        case 'u': {
            unsigned cp, lo;
            if (ps->end - ps->p < 4 || !hex4(ps->p, &cp)) goto fail;
            ps->p += 4;
            if (cp >= 0xD800 && cp < 0xDC00 && ps->end - ps->p >= 6 && ps->p[0] == '\\' &&
                ps->p[1] == 'u' && hex4(ps->p + 2, &lo) && lo >= 0xDC00 && lo < 0xE000) {
                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                ps->p += 6;
            }
            put_utf8(&b, cp);
            break;
        }
        default: sb_putc(&b, c); break; /* \" \\ \/ */
        }
    }
    if (ps->p >= ps->end || !b.p) goto fail;
    ps->p++;
    *len = b.len;
    return b.p;
fail:
    sb_free(&b);
    return NULL;
}

static jv *parse_value(parser *ps) {
    skip_ws(ps);
    if (ps->p >= ps->end || ps->depth > 64) return NULL;
    jv *v = calloc(1, sizeof *v);
    if (!v) return NULL;
    char c = *ps->p;
    size_t left = (size_t)(ps->end - ps->p);

    if (c == '"') {
        v->type = J_STR;
        if (!(v->str = parse_string(ps, &v->len))) goto fail;
    } else if (c == '{' || c == '[') {
        char close = c == '{' ? '}' : ']';
        jv **tail = &v->child;
        v->type = c == '{' ? J_OBJ : J_ARR;
        ps->p++;
        ps->depth++;
        skip_ws(ps);
        if (ps->p < ps->end && *ps->p == close) {
            ps->p++;
        } else {
            for (;;) {
                char *key = NULL;
                if (v->type == J_OBJ) {
                    size_t kl;
                    skip_ws(ps);
                    if (ps->p >= ps->end || *ps->p != '"' || !(key = parse_string(ps, &kl)))
                        goto fail;
                    skip_ws(ps);
                    if (ps->p >= ps->end || *ps->p != ':') {
                        free(key);
                        goto fail;
                    }
                    ps->p++;
                }
                jv *e = parse_value(ps);
                if (!e) {
                    free(key);
                    goto fail;
                }
                e->key = key;
                *tail = e;
                tail = &e->next;
                skip_ws(ps);
                if (ps->p >= ps->end) goto fail;
                if (*ps->p == ',') {
                    ps->p++;
                    continue;
                }
                if (*ps->p == close) {
                    ps->p++;
                    break;
                }
                goto fail;
            }
        }
        ps->depth--;
    } else if (left >= 4 && memcmp(ps->p, "true", 4) == 0) {
        v->type = J_BOOL;
        v->num = 1;
        ps->p += 4;
    } else if (left >= 5 && memcmp(ps->p, "false", 5) == 0) {
        v->type = J_BOOL;
        ps->p += 5;
    } else if (left >= 4 && memcmp(ps->p, "null", 4) == 0) {
        v->type = J_NULL;
        ps->p += 4;
    } else if (c == '-' || (c >= '0' && c <= '9')) {
        char tmp[64], *e;
        size_t n = left < sizeof tmp - 1 ? left : sizeof tmp - 1;
        memcpy(tmp, ps->p, n);
        tmp[n] = 0;
        v->type = J_NUM;
        v->num = strtod(tmp, &e);
        if (e == tmp) goto fail;
        ps->p += e - tmp;
    } else {
        goto fail;
    }
    return v;
fail:
    json_free(v);
    return NULL;
}

jv *json_parse(const char *s, size_t len) {
    parser ps = {s, s + len, 0};
    return parse_value(&ps);
}

jv *json_get(const jv *obj, const char *key) {
    if (!obj || obj->type != J_OBJ) return NULL;
    for (jv *c = obj->child; c; c = c->next)
        if (c->key && strcmp(c->key, key) == 0) return c;
    return NULL;
}

jv *json_at(const jv *arr, int idx) {
    if (!arr || arr->type != J_ARR) return NULL;
    jv *c = arr->child;
    while (c && idx-- > 0) c = c->next;
    return c;
}

const char *json_str(const jv *v) { return v && v->type == J_STR ? v->str : NULL; }

void json_quote(strbuf *out, const char *s, size_t len) {
    sb_putc(out, '"');
    for (size_t i = 0; i < len; i++) {
        unsigned char c = (unsigned char)s[i];
        switch (c) {
        case '"': sb_puts(out, "\\\""); break;
        case '\\': sb_puts(out, "\\\\"); break;
        case '\n': sb_puts(out, "\\n"); break;
        case '\r': sb_puts(out, "\\r"); break;
        case '\t': sb_puts(out, "\\t"); break;
        default:
            if (c < 0x20) sb_printf(out, "\\u%04x", c);
            else sb_putc(out, (char)c);
        }
    }
    sb_putc(out, '"');
}
