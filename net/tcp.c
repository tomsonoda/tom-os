/* Minimal TCP client: one connection at a time, in-order receive,
 * go-back-N retransmission. */
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

#define F_FIN 0x01
#define F_SYN 0x02
#define F_RST 0x04
#define F_PSH 0x08
#define F_ACK 0x10

#define RX_SIZE 65536u
#define MAX_BURST 24 /* segments in flight */

enum { CLOSED, SYN_SENT, ESTABLISHED };

static struct {
    int state;
    u32 rip;
    u16 rport, lport;
    u32 snd_una, snd_nxt, snd_max, rcv_nxt;
    u32 snd_wnd;
    u16 mss;
    int peer_fin, reset;
    u8 *rx;
    u32 rx_head, rx_len;
    u32 adv_wnd;
} T;

static u16 get16(const u8 *p) { return (u16)((p[0] << 8) | p[1]); }
static u32 get32(const u8 *p) { return ((u32)p[0] << 24) | ((u32)p[1] << 16) | ((u32)p[2] << 8) | p[3]; }
static void put16(u8 *p, u16 v) { p[0] = (u8)(v >> 8); p[1] = (u8)v; }
static void put32(u8 *p, u32 v) { p[0] = (u8)(v >> 24); p[1] = (u8)(v >> 16); p[2] = (u8)(v >> 8); p[3] = (u8)v; }

static u32 pseudo_sum(u32 src, u32 dst, size_t len) {
    u8 ph[12];
    put32(ph, src);
    put32(ph + 4, dst);
    ph[8] = 0;
    ph[9] = 6;
    put16(ph + 10, (u16)len);
    return net_sum(0, ph, 12);
}

static u32 rx_window(void) {
    u32 space = RX_SIZE - T.rx_len;
    return space > 65535 ? 65535 : space;
}

static int tcp_out(u8 flags, u32 seq, const void *data, size_t len) {
    static u8 seg[1500];
    size_t hlen = (flags & F_SYN) ? 24 : 20;
    if (len > sizeof seg - hlen) return NET_EPROTO;
    put16(seg, T.lport);
    put16(seg + 2, T.rport);
    put32(seg + 4, seq);
    put32(seg + 8, (flags & F_ACK) ? T.rcv_nxt : 0);
    seg[12] = (u8)((hlen / 4) << 4);
    seg[13] = flags;
    T.adv_wnd = rx_window();
    put16(seg + 14, (u16)T.adv_wnd);
    put16(seg + 16, 0);
    put16(seg + 18, 0);
    if (flags & F_SYN) { /* MSS option */
        seg[20] = 2;
        seg[21] = 4;
        put16(seg + 22, 1460);
    }
    if (len) memcpy(seg + hlen, data, len);
    put16(seg + 16, net_checksum(pseudo_sum(net_ip(), T.rip, hlen + len), seg, hlen + len));
    return ip_send(T.rip, 6, seg, hlen + len);
}

void tcp_input(u32 src, const u8 *seg, size_t len) {
    if (len < 20 || T.state == CLOSED) return;
    if (src != T.rip || get16(seg) != T.rport || get16(seg + 2) != T.lport) return;
    if (net_checksum(pseudo_sum(src, net_ip(), len), seg, len) != 0) return;

    u32 seq = get32(seg + 4), ack = get32(seg + 8);
    u8 flags = seg[13];
    size_t hlen = (size_t)(seg[12] >> 4) * 4;
    if (hlen < 20 || hlen > len) return;
    const u8 *data = seg + hlen;
    size_t dlen = len - hlen;

    if (flags & F_RST) {
        T.reset = 1;
        T.state = CLOSED;
        return;
    }

    if (T.state == SYN_SENT) {
        if ((flags & (F_SYN | F_ACK)) != (F_SYN | F_ACK) || ack != T.snd_nxt) return;
        T.rcv_nxt = seq + 1;
        T.snd_una = ack;
        T.snd_wnd = get16(seg + 14);
        for (size_t i = 20; i + 1 < hlen;) { /* options */
            u8 kind = seg[i];
            if (kind == 0) break;
            if (kind == 1) {
                i++;
                continue;
            }
            u8 olen = seg[i + 1];
            if (olen < 2 || i + olen > hlen) break;
            if (kind == 2 && olen == 4) {
                u16 m = get16(seg + i + 2);
                if (m >= 64 && m < T.mss) T.mss = m;
            }
            i += olen;
        }
        T.state = ESTABLISHED;
        tcp_out(F_ACK, T.snd_nxt, NULL, 0);
        return;
    }

    if (flags & F_ACK) {
        if ((int32_t)(ack - T.snd_una) > 0 && (int32_t)(T.snd_max - ack) >= 0) {
            T.snd_una = ack;
            if ((int32_t)(T.snd_nxt - ack) < 0) T.snd_nxt = ack;
        }
        T.snd_wnd = get16(seg + 14);
    }

    if (dlen == 0 && !(flags & F_FIN)) return;

    /* trim anything we already have */
    int32_t skip = (int32_t)(T.rcv_nxt - seq);
    if (skip > 0) {
        if ((size_t)skip > dlen || ((size_t)skip == dlen && !(flags & F_FIN))) {
            tcp_out(F_ACK, T.snd_nxt, NULL, 0); /* pure duplicate */
            return;
        }
        data += skip;
        dlen -= (size_t)skip;
        seq += (u32)skip;
    }
    if (seq == T.rcv_nxt && !T.peer_fin) {
        size_t room = RX_SIZE - T.rx_len;
        size_t take = dlen < room ? dlen : room;
        for (size_t i = 0; i < take; i++) T.rx[(T.rx_head + T.rx_len + i) % RX_SIZE] = data[i];
        T.rx_len += (u32)take;
        T.rcv_nxt += (u32)take;
        if ((flags & F_FIN) && take == dlen) {
            T.rcv_nxt++;
            T.peer_fin = 1;
        }
    }
    tcp_out(F_ACK, T.snd_nxt, NULL, 0);
}

int tcp_connect(u32 ip, u16 port, int timeout_ms) {
    static u16 next_port;
    if (!net_up()) return NET_ENONET;
    if (!T.rx && !(T.rx = malloc(RX_SIZE))) return NET_EPROTO;
    if (!next_port) next_port = (u16)(49152 + (rdtsc() >> 8) % 8192);
    if (++next_port < 49152) next_port = 49152;

    u8 *rx = T.rx;
    memset(&T, 0, sizeof T);
    T.rx = rx;
    T.rip = ip;
    T.rport = port;
    T.lport = next_port;
    T.mss = 1460;
    T.snd_una = (u32)rdtsc();
    T.snd_nxt = T.snd_max = T.snd_una + 1;
    T.state = SYN_SENT;

    u64 start = time_ms(), rto = 400, last = 0;
    for (int first = 1;; first = 0) {
        u64 now = time_ms();
        if (first || now - last >= rto) {
            int r = tcp_out(F_SYN, T.snd_una, NULL, 0);
            if (r < 0) {
                T.state = CLOSED;
                return r;
            }
            if (!first) rto *= 2;
            last = now;
        }
        net_poll();
        if (T.state == ESTABLISHED) return 0;
        if (T.reset) return NET_ERESET;
        if (console_break_pending()) {
            T.state = CLOSED;
            return NET_EBREAK;
        }
        if (now - start > (u64)timeout_ms) {
            T.state = CLOSED;
            return NET_ETIMEOUT;
        }
        cpu_idle();
    }
}

int tcp_send(const void *data, size_t len) {
    const u8 *p = data;
    u32 start = T.snd_nxt, end = start + (u32)len;
    u64 last_progress = time_ms(), rto = 400;
    int retries = 0;

    while ((int32_t)(end - T.snd_una) > 0) {
        if (T.state != ESTABLISHED) return T.reset ? NET_ERESET : NET_EPROTO;
        u32 wnd = T.snd_wnd;
        if (wnd > (u32)MAX_BURST * T.mss) wnd = (u32)MAX_BURST * T.mss;
        if (wnd < T.mss) wnd = T.mss; /* also serves as a zero-window probe */
        while ((int32_t)(end - T.snd_nxt) > 0 && T.snd_nxt - T.snd_una < wnd) {
            u32 n = end - T.snd_nxt;
            u32 room = wnd - (T.snd_nxt - T.snd_una);
            if (n > T.mss) n = T.mss;
            if (n > room) n = room;
            u8 fl = F_ACK | (T.snd_nxt + n == end ? F_PSH : 0);
            int r = tcp_out(fl, T.snd_nxt, p + (T.snd_nxt - start), n);
            if (r < 0) return r;
            T.snd_nxt += n;
            if ((int32_t)(T.snd_nxt - T.snd_max) > 0) T.snd_max = T.snd_nxt;
        }
        u32 una = T.snd_una;
        net_poll();
        u64 now = time_ms();
        if (T.snd_una != una) {
            last_progress = now;
            rto = 400;
            retries = 0;
            continue;
        }
        if (now - last_progress > rto) {
            if (++retries > 8) return NET_ETIMEOUT;
            T.snd_nxt = T.snd_una; /* go back and resend */
            rto *= 2;
            last_progress = now;
            continue;
        }
        if (console_break_pending()) return NET_EBREAK;
        cpu_idle();
    }
    return 0;
}

int tcp_recv(void *buf, size_t max, int timeout_ms) {
    u64 start = time_ms();
    for (;;) {
        net_poll();
        if (T.rx_len) {
            size_t n = T.rx_len < max ? T.rx_len : max;
            u8 *out = buf;
            for (size_t i = 0; i < n; i++) out[i] = T.rx[(T.rx_head + i) % RX_SIZE];
            T.rx_head = (T.rx_head + (u32)n) % RX_SIZE;
            T.rx_len -= (u32)n;
            /* reopen the window if we had advertised a small one */
            if (T.state == ESTABLISHED && rx_window() >= T.adv_wnd + 16384)
                tcp_out(F_ACK, T.snd_nxt, NULL, 0);
            return (int)n;
        }
        if (T.peer_fin) return 0;
        if (T.state != ESTABLISHED) return T.reset ? NET_ERESET : NET_EPROTO;
        if (console_break_pending()) return NET_EBREAK;
        if (time_ms() - start > (u64)timeout_ms) return NET_ETIMEOUT;
        cpu_idle();
    }
}

void tcp_close(void) {
    if (T.state == ESTABLISHED) {
        u32 fin_seq = T.snd_nxt;
        tcp_out(F_FIN | F_ACK, fin_seq, NULL, 0);
        T.snd_nxt = T.snd_max = fin_seq + 1;
        u64 end = time_ms() + 1000;
        while (time_ms() < end && T.state == ESTABLISHED) {
            net_poll();
            T.rx_len = 0; /* discard anything still arriving */
            if (T.peer_fin && T.snd_una == T.snd_nxt) break;
            cpu_idle();
        }
        if (T.state == ESTABLISHED && !(T.peer_fin && T.snd_una == T.snd_nxt))
            tcp_out(F_RST | F_ACK, T.snd_nxt, NULL, 0);
    } else if (T.state == SYN_SENT) {
        tcp_out(F_RST, T.snd_nxt, NULL, 0);
    }
    T.state = CLOSED;
}
