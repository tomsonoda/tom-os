/* HTTP/1.1 client: one POST per connection, Content-Length or chunked reply. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

/* Value of a header (case-insensitive name) inside raw[0..hdr_len), or NULL. */
static const char *find_header(const char *raw, size_t hdr_len, const char *name) {
    size_t nl = strlen(name);
    const char *p = raw, *end = raw + hdr_len;
    while (p < end) {
        const char *eol = memchr(p, '\n', (size_t)(end - p));
        if (!eol) break;
        if ((size_t)(eol - p) > nl && strncasecmp(p, name, nl) == 0 && p[nl] == ':') {
            p += nl + 1;
            while (*p == ' ') p++;
            return p;
        }
        p = eol + 1;
    }
    return NULL;
}

/* Decodes a chunked body. Returns 1 when the terminating chunk was seen. */
static int dechunk(const char *p, size_t n, strbuf *out) {
    const char *end = p + n;
    out->len = 0;
    for (;;) {
        const char *eol = memchr(p, '\n', (size_t)(end - p));
        if (!eol) return 0;
        size_t size = strtoul(p, NULL, 16);
        p = eol + 1;
        if (size == 0) return 1;
        if ((size_t)(end - p) < size + 2) return 0;
        sb_put(out, p, size);
        p += size + 2;
    }
}

const char *http_phase = "connecting";

int http_post_json(u32 ip, u16 port, const char *path, const char *body, size_t body_len,
                   strbuf *resp, int *status, int timeout_ms) {
    char head[512], host[16];
    strbuf raw;
    int r;

    net_fmt_ip(ip, host);
    http_phase = "connecting";
    r = tcp_connect(ip, port, 5000);
    if (r < 0) return r;

    int hn = snprintf(head, sizeof head,
                      "POST %s HTTP/1.1\r\n"
                      "Host: %s:%u\r\n"
                      "User-Agent: tom-os/" TOMOS_VERSION "\r\n"
                      "Content-Type: application/json\r\n"
                      "Accept: application/json\r\n"
                      "Content-Length: %zu\r\n"
                      "Connection: close\r\n\r\n",
                      path, host, port, body_len);
    http_phase = "sending the request";
    if ((r = tcp_send(head, (size_t)hn)) < 0 || (r = tcp_send(body, body_len)) < 0) {
        tcp_close();
        return r;
    }

    sb_init(&raw);
    size_t hdr_len = 0;
    long content_len = -1;
    int chunked = 0, done = 0;
    static char buf[8192];
    http_phase = "waiting for the reply";
    while (!done) {
        int n = tcp_recv(buf, sizeof buf, timeout_ms);
        if (n < 0) {
            sb_free(&raw);
            tcp_close();
            return n;
        }
        if (n == 0) break;
        sb_put(&raw, buf, (size_t)n);

        if (!hdr_len) {
            const char *sep = strstr(raw.p, "\r\n\r\n");
            if (!sep) continue;
            hdr_len = (size_t)(sep - raw.p) + 4;
            const char *v;
            if ((v = find_header(raw.p, hdr_len, "content-length"))) content_len = strtol(v, NULL, 10);
            if ((v = find_header(raw.p, hdr_len, "transfer-encoding")))
                chunked = strncasecmp(v, "chunked", 7) == 0;
        }
        /* Servers may send whitespace while they wait (keep-alive); the reply
         * proper starts with the JSON object. */
        if (memchr(raw.p + hdr_len, '{', raw.len - hdr_len)) http_phase = "receiving the reply";
        if (chunked) done = dechunk(raw.p + hdr_len, raw.len - hdr_len, resp);
        else if (content_len >= 0) done = raw.len - hdr_len >= (size_t)content_len;
    }
    tcp_close();

    if (!hdr_len || raw.len < 12 || strncmp(raw.p, "HTTP/", 5) != 0) {
        sb_free(&raw);
        return NET_EPROTO;
    }
    *status = atoi(raw.p + 9);
    if (chunked) {
        if (!done) dechunk(raw.p + hdr_len, raw.len - hdr_len, resp);
    } else {
        size_t blen = raw.len - hdr_len;
        if (content_len >= 0 && (size_t)content_len < blen) blen = (size_t)content_len;
        resp->len = 0;
        sb_put(resp, raw.p + hdr_len, blen);
    }
    sb_free(&raw);
    return 0;
}
