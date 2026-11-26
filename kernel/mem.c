/* Boot info and the kernel heap (boundary-tag, first-fit allocator). */
#include <stdlib.h>
#include <string.h>
#include "tomos.h"

extern char __kernel_end[];

/* ------------------------------------------------------------ boot info */
static char cmdline[512];

const char *boot_cmdline(void) { return cmdline; }

int cmdline_get(const char *key, char *out, size_t n) {
    size_t kl = strlen(key);
    const char *p = cmdline;
    while (*p) {
        while (*p == ' ') p++;
        const char *tok = p;
        while (*p && *p != ' ') p++;
        if ((size_t)(p - tok) > kl && strncmp(tok, key, kl) == 0 && tok[kl] == '=') {
            size_t vl = (size_t)(p - tok) - kl - 1;
            if (vl >= n) vl = n - 1;
            memcpy(out, tok + kl + 1, vl);
            out[vl] = 0;
            return 1;
        }
    }
    return 0;
}

/* ------------------------------------------------------------------ heap */
typedef struct blk {
    size_t size; /* whole block including header; bit 0 = in use */
    size_t prev; /* size of the previous block, 0 for the first */
} blk;

typedef struct fblk {
    blk h;
    struct fblk *next, *prevf;
} fblk;

#define HDR sizeof(blk)
#define MINBLK 32
#define BSIZE(b) ((b)->size & ~(size_t)1)
#define USED(b) ((b)->size & 1)

static char *heap_lo, *heap_hi;
static fblk *free_list;
static size_t used_bytes;

static blk *next_blk(blk *b) {
    char *n = (char *)b + BSIZE(b);
    return n < heap_hi ? (blk *)n : NULL;
}

static void fl_insert(fblk *f) {
    f->next = free_list;
    f->prevf = NULL;
    if (free_list) free_list->prevf = f;
    free_list = f;
}

static void fl_remove(fblk *f) {
    if (f->prevf) f->prevf->next = f->next;
    else free_list = f->next;
    if (f->next) f->next->prevf = f->prevf;
}

void mem_init(u32 mb_info) {
    const u32 *mb = (const u32 *)(u64)mb_info;
    u32 flags = mb[0];
    u64 best_base = 0, best_len = 0;

    if (flags & (1 << 2)) {
        const char *c = (const char *)(u64)mb[4];
        strncpy(cmdline, c, sizeof cmdline - 1);
    }
    if (flags & (1 << 6)) {
        u32 len = mb[11];
        const u8 *p = (const u8 *)(u64)mb[12], *end = p + len;
        while (p < end) {
            u32 size, type;
            u64 base, l;
            memcpy(&size, p, 4);
            memcpy(&base, p + 4, 8);
            memcpy(&l, p + 12, 8);
            memcpy(&type, p + 20, 4);
            if (type == 1 && base < 0x100000000ULL) {
                u64 top = base + l;
                if (top > 0x100000000ULL) top = 0x100000000ULL;
                u64 lo = base;
                u64 kend = ((u64)__kernel_end + 0xFFFF) & ~0xFFFFULL;
                if (lo < kend) lo = kend;
                if (top > lo && top - lo > best_len) {
                    best_base = lo;
                    best_len = top - lo;
                }
            }
            p += size + 4;
        }
    }
    if (!best_len) { /* no memory map: assume 64 MiB */
        best_base = ((u64)__kernel_end + 0xFFFF) & ~0xFFFFULL;
        best_len = 0x4000000 - best_base;
    }

    heap_lo = (char *)best_base;
    heap_hi = heap_lo + (best_len & ~(u64)15);
    fblk *f = (fblk *)heap_lo;
    f->h.size = (size_t)(heap_hi - heap_lo);
    f->h.prev = 0;
    free_list = NULL;
    fl_insert(f);
}

size_t mem_total(void) { return (size_t)(heap_hi - heap_lo); }
size_t mem_used(void) { return used_bytes; }

/* Mark `b` (currently free, removed from the list, size `have`) as used with
 * size `need`, returning the tail to the free list when it is big enough. */
static void carve(blk *b, size_t have, size_t need) {
    if (have - need >= MINBLK) {
        blk *rest = (blk *)((char *)b + need);
        rest->size = have - need;
        rest->prev = need;
        blk *after = next_blk(rest);
        if (after) after->prev = rest->size;
        fl_insert((fblk *)rest);
        have = need;
    }
    b->size = have | 1;
    used_bytes += have;
}

void *malloc(size_t n) {
    if (n > (size_t)1 << 40) return NULL;
    size_t need = (n + HDR + 15) & ~(size_t)15;
    if (need < MINBLK) need = MINBLK;
    for (fblk *f = free_list; f; f = f->next) {
        size_t have = BSIZE(&f->h);
        if (have >= need) {
            fl_remove(f);
            carve(&f->h, have, need);
            return (char *)f + HDR;
        }
    }
    return NULL;
}

void free(void *p) {
    if (!p) return;
    blk *b = (blk *)((char *)p - HDR);
    size_t size = BSIZE(b);
    used_bytes -= size;
    b->size = size;

    blk *n = next_blk(b);
    if (n && !USED(n)) {
        fl_remove((fblk *)n);
        size += BSIZE(n);
        b->size = size;
    }
    if (b->prev) {
        blk *pb = (blk *)((char *)b - b->prev);
        if (!USED(pb)) {
            fl_remove((fblk *)pb);
            size += BSIZE(pb);
            pb->size = size;
            b = pb;
        }
    }
    n = next_blk(b);
    if (n) n->prev = size;
    fl_insert((fblk *)b);
}

void *realloc(void *p, size_t n) {
    if (!p) return malloc(n);
    if (n == 0) {
        free(p);
        return NULL;
    }
    blk *b = (blk *)((char *)p - HDR);
    size_t have = BSIZE(b);
    size_t need = (n + HDR + 15) & ~(size_t)15;
    if (need < MINBLK) need = MINBLK;
    if (need <= have) return p;

    blk *nx = next_blk(b);
    if (nx && !USED(nx) && have + BSIZE(nx) >= need) {
        size_t total = have + BSIZE(nx);
        fl_remove((fblk *)nx);
        used_bytes -= have;
        b->size = total;
        blk *after = next_blk(b);
        if (after) after->prev = total;
        carve(b, total, need);
        return p;
    }
    void *np = malloc(n);
    if (!np) return NULL;
    memcpy(np, p, have - HDR);
    free(p);
    return np;
}

void *calloc(size_t n, size_t m) {
    size_t total = n * m;
    void *p = malloc(total);
    if (p) memset(p, 0, total);
    return p;
}
