/* PCI scan + virtio-net driver (legacy I/O-port interface, polled). */
#include <stdlib.h>
#include <string.h>
#include "tomos.h"

/* ------------------------------------------------------------------- PCI */
static u32 pci_read(u8 bus, u8 dev, u8 fn, u8 off) {
    outl(0xCF8, 0x80000000u | ((u32)bus << 16) | ((u32)dev << 11) | ((u32)fn << 8) | (off & 0xFC));
    return inl(0xCFC);
}

static void pci_write(u8 bus, u8 dev, u8 fn, u8 off, u32 v) {
    outl(0xCF8, 0x80000000u | ((u32)bus << 16) | ((u32)dev << 11) | ((u32)fn << 8) | (off & 0xFC));
    outl(0xCFC, v);
}

/* ---------------------------------------------------------------- virtio */
#define VIRTIO_VENDOR 0x1AF4
#define VIRTIO_NET_LEGACY 0x1000

#define REG_DEV_FEATURES 0x00
#define REG_DRV_FEATURES 0x04
#define REG_QUEUE_PFN 0x08
#define REG_QUEUE_SIZE 0x0C
#define REG_QUEUE_SEL 0x0E
#define REG_QUEUE_NOTIFY 0x10
#define REG_STATUS 0x12
#define REG_MAC 0x14

#define FEATURE_MAC (1u << 5)
#define HDR_LEN 10 /* struct virtio_net_hdr without mergeable buffers */
#define BUF_SIZE 2048
#define RX_BUFS 128

struct vring_desc {
    u64 addr;
    u32 len;
    u16 flags, next;
} __attribute__((packed));

struct vring_avail {
    u16 flags, idx;
    u16 ring[];
} __attribute__((packed));

struct vring_used_elem {
    u32 id, len;
} __attribute__((packed));

struct vring_used {
    u16 flags, idx;
    struct vring_used_elem ring[];
} __attribute__((packed));

struct vq {
    u16 size;
    volatile struct vring_desc *desc;
    volatile struct vring_avail *avail;
    volatile struct vring_used *used;
    u16 last_used;
    u8 *bufs;
};

static u16 iob;
static u8 mac_addr[6];
static struct vq rxq, txq;
static int present;

static void *alloc_pages(size_t bytes) {
    char *raw = malloc(bytes + 4096);
    if (!raw) return NULL;
    char *p = (char *)(((u64)raw + 4095) & ~4095ULL);
    memset(p, 0, bytes);
    return p;
}

static int vq_setup(struct vq *q, u16 index, size_t nbufs) {
    outw(iob + REG_QUEUE_SEL, index);
    q->size = inw(iob + REG_QUEUE_SIZE);
    if (!q->size) return -1;
    size_t part1 = (16u * q->size + 6 + 2u * q->size + 4095) & ~(size_t)4095;
    size_t part2 = (6 + 8u * q->size + 4095) & ~(size_t)4095;
    char *mem = alloc_pages(part1 + part2);
    if (!mem) return -1;
    q->desc = (void *)mem;
    q->avail = (void *)(mem + 16u * q->size);
    q->used = (void *)(mem + part1);
    q->last_used = 0;
    if (nbufs > q->size) nbufs = q->size;
    q->bufs = malloc(nbufs * BUF_SIZE);
    if (!q->bufs) return -1;
    q->avail->flags = 1; /* no interrupts, we poll */
    outl(iob + REG_QUEUE_PFN, (u32)((u64)mem >> 12));
    return 0;
}

int nic_init(void) {
    for (int bus = 0; bus < 256 && !present; bus++) {
        for (int dev = 0; dev < 32 && !present; dev++) {
            u32 id = pci_read((u8)bus, (u8)dev, 0, 0);
            if ((id & 0xFFFF) != VIRTIO_VENDOR || (id >> 16) != VIRTIO_NET_LEGACY) continue;
            u32 bar0 = pci_read((u8)bus, (u8)dev, 0, 0x10);
            if (!(bar0 & 1)) continue; /* need the I/O-port BAR */
            iob = (u16)(bar0 & ~3u);
            u32 cmd = pci_read((u8)bus, (u8)dev, 0, 0x04);
            pci_write((u8)bus, (u8)dev, 0, 0x04, cmd | 0x05); /* I/O space + bus master */
            present = 1;
        }
    }
    if (!present) return -1;

    outb(iob + REG_STATUS, 0);
    outb(iob + REG_STATUS, 1);     /* ACKNOWLEDGE */
    outb(iob + REG_STATUS, 1 | 2); /* DRIVER */
    u32 feats = inl(iob + REG_DEV_FEATURES);
    outl(iob + REG_DRV_FEATURES, feats & FEATURE_MAC);
    for (int i = 0; i < 6; i++) mac_addr[i] = inb(iob + REG_MAC + i);

    if (vq_setup(&rxq, 0, RX_BUFS) || vq_setup(&txq, 1, 256)) {
        present = 0;
        return -1;
    }
    u16 nrx = rxq.size < RX_BUFS ? rxq.size : RX_BUFS;
    for (u16 i = 0; i < nrx; i++) {
        rxq.desc[i].addr = (u64)(rxq.bufs + (size_t)i * BUF_SIZE);
        rxq.desc[i].len = BUF_SIZE;
        rxq.desc[i].flags = 2; /* device writes */
        rxq.avail->ring[i] = i;
    }
    rxq.avail->idx = nrx;
    outb(iob + REG_STATUS, 1 | 2 | 4); /* DRIVER_OK */
    outw(iob + REG_QUEUE_NOTIFY, 0);
    return 0;
}

void nic_mac(u8 mac[6]) { memcpy(mac, mac_addr, 6); }

void nic_send(const void *frame, size_t len) {
    if (!present || len > BUF_SIZE - HDR_LEN) return;
    u16 nslots = txq.size < 256 ? txq.size : 256;
    /* wait for a free slot (the device consumes them almost immediately) */
    for (int spin = 0; (u16)(txq.avail->idx - txq.used->idx) >= nslots; spin++) {
        if (spin > 10000000) return;
        __asm__ volatile("pause");
    }
    u16 slot = txq.avail->idx % nslots;
    u8 *buf = txq.bufs + (size_t)slot * BUF_SIZE;
    memset(buf, 0, HDR_LEN);
    memcpy(buf + HDR_LEN, frame, len);
    if (len < 60) {
        memset(buf + HDR_LEN + len, 0, 60 - len);
        len = 60;
    }
    txq.desc[slot].addr = (u64)buf;
    txq.desc[slot].len = (u32)(len + HDR_LEN);
    txq.desc[slot].flags = 0;
    txq.avail->ring[txq.avail->idx % txq.size] = slot;
    __asm__ volatile("mfence");
    txq.avail->idx = (u16)(txq.avail->idx + 1);
    __asm__ volatile("mfence");
    outw(iob + REG_QUEUE_NOTIFY, 1);
}

void nic_poll(void (*handler)(const u8 *frame, size_t len)) {
    static int busy;
    if (!present || busy) return;
    busy = 1;
    int refilled = 0;
    while (rxq.last_used != rxq.used->idx) {
        u32 id = rxq.used->ring[rxq.last_used % rxq.size].id;
        u32 len = rxq.used->ring[rxq.last_used % rxq.size].len;
        rxq.last_used++;
        if (id < RX_BUFS && len > HDR_LEN && len <= BUF_SIZE)
            handler(rxq.bufs + (size_t)id * BUF_SIZE + HDR_LEN, len - HDR_LEN);
        rxq.avail->ring[rxq.avail->idx % rxq.size] = (u16)id;
        __asm__ volatile("mfence");
        rxq.avail->idx = (u16)(rxq.avail->idx + 1);
        refilled = 1;
    }
    if (refilled) {
        __asm__ volatile("mfence");
        outw(iob + REG_QUEUE_NOTIFY, 0);
    }
    busy = 0;
}
