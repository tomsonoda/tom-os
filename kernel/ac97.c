/* Sound: Intel AC'97 audio driver (QEMU -device AC97) and a small software
 * synthesizer. Output is 48 kHz, 16-bit stereo. A ring of 32 DMA buffers is
 * kept filled a little ahead of the play position by sound_poll(), which
 * runs from the scheduler tick; no interrupts are used. */
#include <math.h>
#include <stdlib.h>
#include <string.h>
#include "tomos.h"

#define RATE 48000
#define NBUF 32
#define FRAMES 512 /* per DMA buffer: 10.7 ms */
#define AHEAD 8    /* buffers mixed ahead of the play position: ~85 ms */
#define MAX_VOICES 96

/* bus master (NABM) registers, PCM out channel */
#define PO_BDBAR 0x10
#define PO_CIV 0x14
#define PO_LVI 0x15
#define PO_SR 0x16
#define PO_CR 0x1B
#define GLOB_CNT 0x2C
#define GLOB_STA 0x30

struct bd {
    u32 addr;
    u16 samples; /* 16-bit samples in the buffer (both channels) */
    u16 flags;
} __attribute__((packed));

typedef struct {
    int used;
    int wave;
    u32 phase, inc;
    u32 noise;
    int delay;      /* frames before it starts */
    int len, pos;   /* frames */
    int amp;        /* peak amplitude */
    const s16 *pcm; /* sampled sound (owned), else a waveform */
    u32 pcm_pos, pcm_step; /* 16.16 fixed point */
    u32 pcm_len;
    void *owner;
} voice;

static u16 nam, nabm;
static int present;
static struct bd *bdl;
static s16 *bufs;
static int filled; /* next buffer index to mix */
static voice voices[MAX_VOICES];
static s16 sine[256];

int sound_available(void) { return present; }

static int find_ac97(u8 *bus_out, u8 *dev_out) {
    for (int bus = 0; bus < 256; bus++)
        for (int dev = 0; dev < 32; dev++) {
            u32 id = pci_read((u8)bus, (u8)dev, 0, 0);
            if (id == 0x24158086u) { /* Intel 82801AA AC'97 */
                *bus_out = (u8)bus;
                *dev_out = (u8)dev;
                return 1;
            }
        }
    return 0;
}

int sound_init(void) {
    u8 bus, dev;
    if (!find_ac97(&bus, &dev)) return -1;
    nam = (u16)(pci_read(bus, dev, 0, 0x10) & ~3u);
    nabm = (u16)(pci_read(bus, dev, 0, 0x14) & ~3u);
    pci_write(bus, dev, 0, 0x04, pci_read(bus, dev, 0, 0x04) | 0x05); /* I/O + bus master */

    outl(nabm + GLOB_CNT, 0x2); /* cold reset done: codec running */
    for (u64 end = time_ms() + 200; !(inl(nabm + GLOB_STA) & 0x100) && time_ms() < end;) {}
    outw(nam + 0x00, 0);      /* reset the mixer */
    outw(nam + 0x02, 0x0000); /* master volume: 0 dB, unmuted */
    outw(nam + 0x18, 0x0808); /* PCM out volume: 0 dB */

    bdl = calloc(NBUF, sizeof *bdl);
    bufs = calloc((size_t)NBUF * FRAMES * 2, sizeof *bufs);
    if (!bdl || !bufs) return -1;
    for (int i = 0; i < NBUF; i++) {
        bdl[i].addr = (u32)(u64)(bufs + (size_t)i * FRAMES * 2);
        bdl[i].samples = FRAMES * 2;
        bdl[i].flags = 0;
    }
    for (int i = 0; i < 256; i++) sine[i] = (s16)(sin(i * 2 * 3.14159265358979 / 256) * 32767);

    outb(nabm + PO_CR, 0x02); /* reset the channel */
    for (int i = 0; i < 1000 && (inb(nabm + PO_CR) & 0x02); i++) {}
    outl(nabm + PO_BDBAR, (u32)(u64)bdl);
    outb(nabm + PO_LVI, NBUF - 1);
    outb(nabm + PO_CR, 0x01); /* run */
    present = 1;
    filled = 0;
    return 0;
}

/* ---------------------------------------------------------------- mixer */
static int sample(voice *v) {
    u32 p = v->phase;
    v->phase += v->inc;
    switch (v->wave) {
    case SOUND_SINE: return sine[p >> 24];
    case SOUND_TRIANGLE: {
        int t = (int)(p >> 16);                   /* 0..65535 */
        return t < 32768 ? t * 2 - 32768 : 98303 - t * 2;
    }
    case SOUND_SAW: return (int)(p >> 16) - 32768;
    case SOUND_NOISE:
        if (p + v->inc < p || !v->noise) { /* a new random value each period */
            v->noise ^= v->noise << 13;
            v->noise ^= v->noise >> 17;
            v->noise ^= v->noise << 5;
            if (!v->noise) v->noise = 0x1234567;
        }
        return (int)(v->noise & 0xFFFF) - 32768;
    default: return p < 0x80000000u ? 24000 : -24000; /* square, a little softer */
    }
}

static void mix(s16 *out) {
    static int acc[FRAMES];
    memset(acc, 0, sizeof acc);
    for (int i = 0; i < MAX_VOICES; i++) {
        voice *v = &voices[i];
        if (!v->used) continue;
        for (int f = 0; f < FRAMES; f++) {
            if (v->delay > 0) {
                v->delay--;
                continue;
            }
            if (v->pos >= v->len) {
                v->used = 0;
                free((void *)v->pcm);
                v->pcm = NULL;
                break;
            }
            int s;
            if (v->pcm) {
                u32 idx = v->pcm_pos >> 16;
                s = idx < v->pcm_len ? v->pcm[idx] : 0;
                v->pcm_pos += v->pcm_step;
            } else {
                s = sample(v);
            }
            /* short fade in and out, so notes do not click */
            int env = 256, a = v->pos, r = v->len - v->pos;
            if (a < 96) env = a * 256 / 96;
            if (r < 480 && r * 256 / 480 < env) env = r * 256 / 480;
            acc[f] += (s * v->amp >> 15) * env >> 8;
            v->pos++;
        }
    }
    for (int f = 0; f < FRAMES; f++) {
        int s = acc[f];
        if (s > 32767) s = 32767;
        if (s < -32768) s = -32768;
        out[2 * f] = out[2 * f + 1] = (s16)s;
    }
}

void sound_poll(void) {
    if (!present) return;
    int civ = inb(nabm + PO_CIV) & 31;
    while (((filled - civ) & 31) < AHEAD) {
        if (filled == civ) { /* fell behind: skip to just after the play position */
            filled = (civ + 1) & 31;
            continue;
        }
        mix(bufs + (size_t)filled * FRAMES * 2);
        filled = (filled + 1) & 31;
    }
    /* never let the DMA reach the last valid buffer: it would stop */
    outb(nabm + PO_LVI, (u8)((civ - 1) & 31));
    u16 sr = inw(nabm + PO_SR);
    if (sr & 0x1C) outw(nabm + PO_SR, sr & 0x1C); /* clear status bits */
    if (sr & 0x01) outb(nabm + PO_CR, 0x01);      /* halted: run again */
}

static voice *alloc_voice(void) {
    for (int i = 0; i < MAX_VOICES; i++)
        if (!voices[i].used) {
            memset(&voices[i], 0, sizeof voices[i]);
            return &voices[i];
        }
    return NULL;
}

static int amp_for(double vol) {
    if (vol < 0) vol = 0;
    if (vol > 1) vol = 1;
    return (int)(vol * 16000); /* room for several voices at once */
}

int sound_tone(double freq, int ms, double vol, int wave, int delay_ms, void *owner) {
    if (!present || freq <= 0 || freq > 20000 || ms <= 0) return -1;
    voice *v = alloc_voice();
    if (!v) return -1;
    v->wave = wave;
    v->inc = (u32)(freq / RATE * 4294967296.0);
    v->len = (int)((s64)ms * RATE / 1000);
    v->delay = (int)((s64)delay_ms * RATE / 1000);
    v->amp = amp_for(vol);
    v->noise = 0x9E3779B9u ^ (u32)rdtsc();
    v->owner = owner;
    v->used = 1;
    return 0;
}

int sound_pcm(const float *samples, size_t n, int rate, double vol, int delay_ms, void *owner) {
    if (!present || !n || rate < 1000 || rate > 96000) return -1;
    s16 *copy = malloc(n * sizeof *copy);
    voice *v = copy ? alloc_voice() : NULL;
    if (!v) {
        free(copy);
        return -1;
    }
    for (size_t i = 0; i < n; i++) {
        float f = samples[i];
        if (f > 1) f = 1;
        if (f < -1) f = -1;
        copy[i] = (s16)(f * 32767);
    }
    v->pcm = copy;
    v->pcm_len = (u32)n;
    v->pcm_step = (u32)((u64)rate * 65536 / RATE);
    v->len = (int)((u64)n * RATE / (u64)rate);
    v->delay = (int)((s64)delay_ms * RATE / 1000);
    v->amp = amp_for(vol) * 2;
    v->owner = owner;
    v->used = 1;
    return 0;
}

void sound_stop(void *owner) {
    for (int i = 0; i < MAX_VOICES; i++) {
        voice *v = &voices[i];
        if (v->used && (!owner || v->owner == owner)) {
            v->used = 0;
            free((void *)v->pcm);
            v->pcm = NULL;
        }
    }
}

int sound_busy(void *owner) {
    for (int i = 0; i < MAX_VOICES; i++)
        if (voices[i].used && (!owner || voices[i].owner == owner)) return 1;
    return 0;
}

/* "C4:200 E4 G4:400 R:100 C#5" -> notes; returns the total length in ms. */
int sound_melody(const char *s, int default_ms, double vol, int wave, void *owner) {
    static const int semis[7] = {9, 11, 0, 2, 4, 5, 7}; /* A B C D E F G */
    int t = 0;
    if (default_ms <= 0) default_ms = 200;
    while (*s) {
        while (*s == ' ' || *s == ',' || *s == '\n') s++;
        if (!*s) break;
        char c = *s;
        int is_rest = c == 'R' || c == 'r' || c == '-';
        int midi = -1;
        if (!is_rest) {
            if (c >= 'a' && c <= 'g') c = (char)(c - 32);
            if (c < 'A' || c > 'G') { /* skip what we cannot read */
                while (*s && *s != ' ' && *s != ',') s++;
                continue;
            }
            int semi = semis[c - 'A'];
            s++;
            if (*s == '#') semi++, s++;
            else if (*s == 'b') semi--, s++;
            int octave = 4;
            if (*s >= '0' && *s <= '8') octave = *s++ - '0';
            midi = 12 * (octave + 1) + semi;
        } else {
            s++;
        }
        int ms = default_ms;
        if (*s == ':' || *s == '/') {
            s++;
            ms = atoi(s);
            while (*s >= '0' && *s <= '9') s++;
            if (ms <= 0) ms = default_ms;
        }
        while (*s && *s != ' ' && *s != ',' && *s != '\n') s++;
        if (midi >= 0) sound_tone(440.0 * pow(2.0, (midi - 69) / 12.0), ms * 9 / 10, vol, wave, t, owner);
        t += ms;
    }
    return t;
}
