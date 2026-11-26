/* Linear framebuffer (Bochs/QEMU "BGA" display interface) and 2D drawing. */
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

#define BGA_INDEX 0x1CE
#define BGA_DATA 0x1CF

surface fb_screen;
static u8 font[256][16];

static void bga_write(u16 index, u16 value) {
    outw(BGA_INDEX, index);
    outw(BGA_DATA, value);
}

static u16 bga_read(u16 index) {
    outw(BGA_INDEX, index);
    return inw(BGA_DATA);
}

/* Copies the 8x16 text-mode font out of VGA memory plane 2. Must run while
 * the adapter is still in text mode. Returns 0 if nothing sensible was read. */
static int read_vga_font(void) {
    outw(0x3CE, 0x0005); /* graphics mode: no odd/even addressing */
    outw(0x3CE, 0x0406); /* map A0000, graphics addressing */
    outw(0x3CE, 0x0204); /* read plane 2 */
    outw(0x3C4, 0x0402); /* write plane 2 */
    outw(0x3C4, 0x0604); /* sequential addressing */
    const u8 *plane = (const u8 *)0xA0000;
    for (int ch = 0; ch < 256; ch++) memcpy(font[ch], plane + ch * 32, 16);
    outw(0x3C4, 0x0302); /* back to text-mode settings */
    outw(0x3C4, 0x0204);
    outw(0x3CE, 0x0004);
    outw(0x3CE, 0x1005);
    outw(0x3CE, 0x0E06);

    int bits = 0;
    for (int i = 0; i < 16; i++) bits |= font['A'][i] | font['g'][i];
    return bits != 0;
}

int fb_init(int w, int h) {
    if ((bga_read(0) & 0xFFF0) != 0xB0C0) return -1;

    /* The framebuffer is BAR0 of the display adapter. */
    u64 addr = 0;
    for (int bus = 0; bus < 256 && !addr; bus++) {
        for (int dev = 0; dev < 32 && !addr; dev++) {
            if ((pci_read((u8)bus, (u8)dev, 0, 0) & 0xFFFF) == 0xFFFF) continue;
            if ((pci_read((u8)bus, (u8)dev, 0, 0x08) >> 24) != 0x03) continue;
            u32 bar0 = pci_read((u8)bus, (u8)dev, 0, 0x10);
            if (!(bar0 & 1)) addr = bar0 & ~0xFULL;
        }
    }
    if (!addr || addr >= 0xFFFFFFFFULL - (u64)w * (u64)h * 4) return -1;
    if (!read_vga_font()) return -1;

    bga_write(4, 0); /* disable while changing the mode */
    bga_write(1, (u16)w);
    bga_write(2, (u16)h);
    bga_write(3, 32);
    bga_write(4, 0x41); /* enabled + linear framebuffer */
    if (bga_read(1) != w || bga_read(2) != h) return -1;

    surface_init(&fb_screen, (u32 *)addr, w, h);
    return 0;
}

void surface_init(surface *s, u32 *px, int w, int h) {
    s->px = px;
    s->w = w;
    s->h = h;
    s->cx0 = s->cy0 = 0;
    s->cx1 = w;
    s->cy1 = h;
}

static void fill32(u32 *dst, u32 c, size_t n) {
    __asm__ volatile("rep stosl" : "+D"(dst), "+c"(n) : "a"(c) : "memory");
}

void gfx_fill(surface *s, int x, int y, int w, int h, u32 c) {
    int x0 = x < s->cx0 ? s->cx0 : x, y0 = y < s->cy0 ? s->cy0 : y;
    int x1 = x + w > s->cx1 ? s->cx1 : x + w, y1 = y + h > s->cy1 ? s->cy1 : y + h;
    if (x1 <= x0) return;
    for (int yy = y0; yy < y1; yy++) fill32(s->px + (size_t)yy * s->w + x0, c, (size_t)(x1 - x0));
}

void gfx_pixel(surface *s, int x, int y, u32 c) {
    if (x >= s->cx0 && x < s->cx1 && y >= s->cy0 && y < s->cy1) s->px[(size_t)y * s->w + x] = c;
}

void gfx_line(surface *s, int x0, int y0, int x1, int y1, u32 c) {
    int dx = abs(x1 - x0), dy = -abs(y1 - y0);
    int sx = x0 < x1 ? 1 : -1, sy = y0 < y1 ? 1 : -1, err = dx + dy;
    /* bound the work for wild coordinates */
    for (int guard = 0; guard < 100000; guard++) {
        gfx_pixel(s, x0, y0, c);
        if (x0 == x1 && y0 == y1) break;
        int e2 = 2 * err;
        if (e2 >= dy) {
            err += dy;
            x0 += sx;
        }
        if (e2 <= dx) {
            err += dx;
            y0 += sy;
        }
    }
}

void gfx_circle(surface *s, int cx, int cy, int r, u32 c, int fill) {
    if (r < 0 || r > 4096) return;
    int x = r, y = 0, err = 1 - r;
    while (x >= y) {
        if (fill) {
            gfx_fill(s, cx - x, cy + y, 2 * x + 1, 1, c);
            gfx_fill(s, cx - x, cy - y, 2 * x + 1, 1, c);
            gfx_fill(s, cx - y, cy + x, 2 * y + 1, 1, c);
            gfx_fill(s, cx - y, cy - x, 2 * y + 1, 1, c);
        } else {
            gfx_pixel(s, cx + x, cy + y, c); gfx_pixel(s, cx - x, cy + y, c);
            gfx_pixel(s, cx + x, cy - y, c); gfx_pixel(s, cx - x, cy - y, c);
            gfx_pixel(s, cx + y, cy + x, c); gfx_pixel(s, cx - y, cy + x, c);
            gfx_pixel(s, cx + y, cy - x, c); gfx_pixel(s, cx - y, cy - x, c);
        }
        y++;
        if (err < 0) {
            err += 2 * y + 1;
        } else {
            x--;
            err += 2 * (y - x) + 1;
        }
    }
}

void gfx_glyph(surface *s, int x, int y, u8 ch, u32 fg, long bg, int scale) {
    const u8 *g = font[ch];
    if (scale < 1) scale = 1;
    if (x >= s->cx1 || y >= s->cy1 || x + 8 * scale <= s->cx0 || y + 16 * scale <= s->cy0) return;
    if (scale == 1 && x >= s->cx0 && x + 8 <= s->cx1 && y >= s->cy0 && y + 16 <= s->cy1) {
        for (int row = 0; row < 16; row++) { /* fast path: fully visible */
            u32 *p = s->px + (size_t)(y + row) * s->w + x;
            u8 bits = g[row];
            for (int col = 0; col < 8; col++, bits <<= 1) {
                if (bits & 0x80) p[col] = fg;
                else if (bg >= 0) p[col] = (u32)bg;
            }
        }
        return;
    }
    for (int row = 0; row < 16; row++) {
        u8 bits = g[row];
        for (int col = 0; col < 8; col++, bits <<= 1) {
            if (bits & 0x80) gfx_fill(s, x + col * scale, y + row * scale, scale, scale, fg);
            else if (bg >= 0) gfx_fill(s, x + col * scale, y + row * scale, scale, scale, (u32)bg);
        }
    }
}

void gfx_text(surface *s, int x, int y, const char *str, size_t n, u32 fg, long bg, int scale) {
    if (scale < 1) scale = 1;
    for (size_t i = 0; i < n; i++) {
        u8 c = (u8)str[i];
        if (c >= 0x80 && c < 0xC0) continue; /* UTF-8 continuation */
        if (c >= 0x80) c = '?';
        gfx_glyph(s, x, y, c, fg, bg, scale);
        x += 8 * scale;
    }
}

void gfx_blit(surface *dst, int dx, int dy, const u32 *src, int sw, int sh) {
    int x0 = dx < dst->cx0 ? dst->cx0 : dx, y0 = dy < dst->cy0 ? dst->cy0 : dy;
    int x1 = dx + sw > dst->cx1 ? dst->cx1 : dx + sw, y1 = dy + sh > dst->cy1 ? dst->cy1 : dy + sh;
    if (x1 <= x0) return;
    for (int y = y0; y < y1; y++)
        memcpy(dst->px + (size_t)y * dst->w + x0, src + (size_t)(y - dy) * sw + (x0 - dx),
               (size_t)(x1 - x0) * 4);
}
