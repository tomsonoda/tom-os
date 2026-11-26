/* Ethernet, ARP and IPv4 (static address, no fragmentation). */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

#define ETH_ARP 0x0806
#define ETH_IP 0x0800

static u8 my_mac[6];
static u32 my_ip, gw_ip, netmask;
static int up;

static struct {
    u32 ip;
    u8 mac[6];
} arp_cache[8];
static int arp_next;

static u16 get16(const u8 *p) { return (u16)((p[0] << 8) | p[1]); }
static u32 get32(const u8 *p) { return ((u32)p[0] << 24) | ((u32)p[1] << 16) | ((u32)p[2] << 8) | p[3]; }
static void put16(u8 *p, u16 v) { p[0] = (u8)(v >> 8); p[1] = (u8)v; }
static void put32(u8 *p, u32 v) { p[0] = (u8)(v >> 24); p[1] = (u8)(v >> 16); p[2] = (u8)(v >> 8); p[3] = (u8)v; }

u32 net_sum(u32 sum, const void *data, size_t n) {
    const u8 *p = data;
    for (; n > 1; n -= 2, p += 2) sum += (u32)((p[0] << 8) | p[1]);
    if (n) sum += (u32)(p[0] << 8);
    return sum;
}

u16 net_checksum(u32 sum, const void *data, size_t n) {
    sum = net_sum(sum, data, n);
    while (sum >> 16) sum = (sum & 0xFFFF) + (sum >> 16);
    return (u16)~sum;
}

int net_parse_ip(const char *s, u32 *ip) {
    u32 v = 0;
    for (int i = 0; i < 4; i++) {
        char *e;
        unsigned long part = strtoul(s, &e, 10);
        if (e == s || part > 255) return 0;
        v = (v << 8) | (u32)part;
        s = e;
        if (i < 3) {
            if (*s != '.') return 0;
            s++;
        }
    }
    *ip = v;
    return 1;
}

void net_fmt_ip(u32 ip, char out[16]) {
    snprintf(out, 16, "%u.%u.%u.%u", ip >> 24, (ip >> 16) & 255, (ip >> 8) & 255, ip & 255);
}

static void eth_send(const u8 *dst, u16 type, const void *payload, size_t len) {
    static u8 frame[1600];
    if (len > sizeof frame - 14) return;
    memcpy(frame, dst, 6);
    memcpy(frame + 6, my_mac, 6);
    put16(frame + 12, type);
    memcpy(frame + 14, payload, len);
    nic_send(frame, len + 14);
}

/* ------------------------------------------------------------------- ARP */
static void arp_learn(u32 ip, const u8 *mac) {
    for (int i = 0; i < 8; i++) {
        if (arp_cache[i].ip == ip) {
            memcpy(arp_cache[i].mac, mac, 6);
            return;
        }
    }
    arp_cache[arp_next].ip = ip;
    memcpy(arp_cache[arp_next].mac, mac, 6);
    arp_next = (arp_next + 1) % 8;
}

static void arp_send(u16 op, const u8 *dst_mac, const u8 *target_mac, u32 target_ip) {
    u8 p[28];
    put16(p, 1);
    put16(p + 2, ETH_IP);
    p[4] = 6;
    p[5] = 4;
    put16(p + 6, op);
    memcpy(p + 8, my_mac, 6);
    put32(p + 14, my_ip);
    memcpy(p + 18, target_mac, 6);
    put32(p + 24, target_ip);
    eth_send(dst_mac, ETH_ARP, p, sizeof p);
}

static void arp_input(const u8 *p, size_t len) {
    if (len < 28 || get16(p) != 1 || get16(p + 2) != ETH_IP) return;
    u16 op = get16(p + 6);
    u32 sender = get32(p + 14), target = get32(p + 24);
    if (sender) arp_learn(sender, p + 8);
    if (op == 1 && target == my_ip) arp_send(2, p + 8, p + 8, sender);
}

static int arp_resolve(u32 ip, u8 mac[6]) {
    static const u8 bcast[6] = {255, 255, 255, 255, 255, 255};
    static const u8 zero[6];
    for (int attempt = 0; attempt < 4; attempt++) {
        for (int i = 0; i < 8; i++) {
            if (arp_cache[i].ip == ip) {
                memcpy(mac, arp_cache[i].mac, 6);
                return 1;
            }
        }
        if (attempt == 3) break;
        arp_send(1, bcast, zero, ip);
        u64 end = time_ms() + 500;
        while (time_ms() < end) {
            int found = 0;
            net_poll();
            for (int i = 0; i < 8; i++) found |= arp_cache[i].ip == ip;
            if (found) break;
            cpu_idle();
        }
    }
    return 0;
}

/* ------------------------------------------------------------------ IPv4 */
int ip_send(u32 dst, u8 proto, const void *payload, size_t len) {
    static u8 pkt[1520];
    static u16 ident;
    u8 mac[6];
    if (!up) return NET_ENONET;
    if (len > sizeof pkt - 20) return NET_EPROTO;
    u32 hop = ((dst ^ my_ip) & netmask) ? gw_ip : dst;
    if (!arp_resolve(hop, mac)) return NET_ETIMEOUT;

    pkt[0] = 0x45;
    pkt[1] = 0;
    put16(pkt + 2, (u16)(len + 20));
    put16(pkt + 4, ++ident);
    put16(pkt + 6, 0x4000); /* don't fragment */
    pkt[8] = 64;
    pkt[9] = proto;
    put16(pkt + 10, 0);
    put32(pkt + 12, my_ip);
    put32(pkt + 16, dst);
    put16(pkt + 10, net_checksum(0, pkt, 20));
    memcpy(pkt + 20, payload, len);
    eth_send(mac, ETH_IP, pkt, len + 20);
    return 0;
}

static void ip_input(const u8 *p, size_t len) {
    if (len < 20 || (p[0] >> 4) != 4) return;
    size_t ihl = (size_t)(p[0] & 15) * 4, total = get16(p + 2);
    if (ihl < 20 || total < ihl || total > len) return;
    if (get32(p + 16) != my_ip) return;
    if (get16(p + 6) & 0x3FFF) return; /* fragments are not supported */
    if (p[9] == 6) tcp_input(get32(p + 12), p + ihl, total - ihl);
}

static void frame_input(const u8 *f, size_t len) {
    if (len < 14) return;
    u16 type = get16(f + 12);
    if (type == ETH_ARP) arp_input(f + 14, len - 14);
    else if (type == ETH_IP) ip_input(f + 14, len - 14);
}

void net_poll(void) {
    if (up) nic_poll(frame_input);
}

int net_up(void) { return up; }
u32 net_ip(void) { return my_ip; }

void net_init(void) {
    char v[32];
    /* QEMU user-mode networking defaults; override with ip= gw= mask= */
    net_parse_ip("10.0.2.15", &my_ip);
    net_parse_ip("10.0.2.2", &gw_ip);
    net_parse_ip("255.255.255.0", &netmask);
    if (cmdline_get("ip", v, sizeof v)) net_parse_ip(v, &my_ip);
    if (cmdline_get("gw", v, sizeof v)) net_parse_ip(v, &gw_ip);
    if (cmdline_get("mask", v, sizeof v)) net_parse_ip(v, &netmask);
    if (nic_init() != 0) return;
    nic_mac(my_mac);
    up = 1;
}

const char *net_strerror(int e) {
    switch (e) {
    case NET_ETIMEOUT: return "timed out";
    case NET_EBREAK: return "interrupted";
    case NET_ERESET: return "connection reset";
    case NET_ENONET: return "no network device";
    case NET_EPROTO: return "protocol error";
    default: return "network error";
    }
}
