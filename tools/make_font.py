#!/usr/bin/env python3
"""Builds rust/assets/font_ja.bin, the Japanese font tom-os draws with.

Source: the Shinonome (東雲) 16-dot gothic font, shnmk16.pcf(.gz), which is
in the public domain (Debian package xfonts-shinonome). It covers JIS X 0208:
kana, kanji (levels 1 and 2), full-width Latin, Greek, Cyrillic and symbols.

Output format (little endian):
    "TJF1"  u32 count
    count x u16 code point (sorted, Basic Multilingual Plane)
    count x 32 bytes: 16 rows of 16 pixels, two bytes per row, MSB = left

Usage: python3 tools/make_font.py shnmk16.pcf.gz rust/assets/font_ja.bin

With --ascii it builds rust/assets/font_8x16.bin instead, from the 8x16
half-width font of the same family (shnm8x16a.pcf.gz): 256 glyphs of 16
bytes, one byte per row, MSB = left; code points without a glyph are blank.

       python3 tools/make_font.py --ascii shnm8x16a.pcf.gz rust/assets/font_8x16.bin
"""
import gzip
import struct
import sys

PCF_METRICS = 1 << 2
PCF_BITMAPS = 1 << 3
PCF_BDF_ENCODINGS = 1 << 5


def read_pcf(path):
    data = gzip.open(path).read() if path.endswith(".gz") else open(path, "rb").read()
    assert data[:4] == b"\x01fcp", "not a PCF font"
    (count,) = struct.unpack_from("<I", data, 4)
    tables = {}
    for i in range(count):
        typ, fmt, size, off = struct.unpack_from("<IIII", data, 8 + 16 * i)
        tables[typ] = (fmt, off)

    def table(typ):
        fmt, off = tables[typ]
        (fmt,) = struct.unpack_from("<I", data, off)
        big = fmt & 4  # PCF_BYTE_MASK: most significant byte first
        e = ">" if big else "<"
        return fmt, off + 4, e

    # metrics: only needed to check the glyphs are 16x16
    fmt, p, e = table(PCF_METRICS)
    if fmt & 0x100:  # compressed metrics
        (n,) = struct.unpack_from(e + "H", data, p)
        metrics = [tuple(b - 0x80 for b in data[p + 2 + 5 * i: p + 7 + 5 * i]) for i in range(n)]
    else:
        (n,) = struct.unpack_from(e + "I", data, p)
        metrics = [struct.unpack_from(e + "hhhhhH", data, p + 4 + 12 * i)[:5] for i in range(n)]

    fmt, p, e = table(PCF_BITMAPS)
    (n,) = struct.unpack_from(e + "I", data, p)
    offsets = struct.unpack_from(e + "%dI" % n, data, p + 4)
    sizes = struct.unpack_from(e + "4I", data, p + 4 + 4 * n)
    pad = 1 << (fmt & 3)            # row padding in bytes
    bit_msb = fmt & 8               # PCF_BIT_MASK
    bitmap_base = p + 4 + 4 * n + 16
    bitmaps_size = sizes[fmt & 3]

    fmt, p, e = table(PCF_BDF_ENCODINGS)
    min2, max2, min1, max1, default = struct.unpack_from(e + "hhhhh", data, p)
    cols = max2 - min2 + 1
    nidx = cols * (max1 - min1 + 1)
    idx = struct.unpack_from(e + "%dH" % nidx, data, p + 10)

    glyphs = {}
    for i, g in enumerate(idx):
        if g == 0xFFFF:
            continue
        b1, b2 = min1 + i // cols, min2 + i % cols
        lsb, rsb, width, asc, desc = metrics[g]
        h = asc + desc
        row_bytes = ((rsb - lsb + 7) // 8 + pad - 1) // pad * pad
        start = bitmap_base + offsets[g]
        assert offsets[g] + row_bytes * h <= bitmaps_size
        rows = []
        for r in range(16):
            if r >= h:
                rows.append(0)
                continue
            raw = data[start + r * row_bytes: start + r * row_bytes + 2]
            if not bit_msb:  # reverse bits in each byte
                raw = bytes(int("{:08b}".format(b)[::-1], 2) for b in raw)
            v = (raw[0] << 8) | (raw[1] if len(raw) > 1 else 0)
            rows.append(v >> max(lsb, 0) if lsb > 0 else v)
        glyphs[(b1, b2)] = rows
    return glyphs


def unicode_for(b1, b2):
    """JIS X 0208 row/cell bytes (0x21..0x7E) -> the code points that should
    use this glyph (EUC-JP and Windows/Mac CP932 differ for a few symbols)."""
    out = set()
    try:
        out.add(bytes([b1 | 0x80, b2 | 0x80]).decode("euc_jp"))
    except UnicodeDecodeError:
        pass
    # JIS -> Shift_JIS
    s1 = (b1 + 1) // 2 + (0x70 if b1 <= 0x5E else 0xB0)
    s2 = b2 + (0x1F if b1 % 2 else 0x7D)
    if s2 >= 0x7F and b1 % 2:
        s2 += 1
    try:
        out.add(bytes([s1, s2]).decode("cp932"))
    except UnicodeDecodeError:
        pass
    return [ord(c) for c in out if len(c) == 1 and ord(c) < 0x10000]


def main_ascii(src, dst):
    glyphs = read_pcf(src)
    out = bytearray(256 * 16)
    n = 0
    for (b1, b2), rows in glyphs.items():
        if b1 == 0 and 0x20 <= b2 < 0x7F:
            out[b2 * 16:b2 * 16 + 16] = bytes(r >> 8 for r in rows)
            n += 1
    open(dst, "wb").write(out)
    print("%s: %d glyphs, %d bytes" % (dst, n, len(out)))


def main():
    if sys.argv[1] == "--ascii":
        return main_ascii(sys.argv[2], sys.argv[3])
    src, dst = sys.argv[1], sys.argv[2]
    glyphs = read_pcf(src)
    table = {}
    for (b1, b2), rows in glyphs.items():
        for cp in unicode_for(b1, b2):
            if cp >= 0x80:  # ASCII comes from the 8x16 font
                table.setdefault(cp, rows)
    cps = sorted(table)
    with open(dst, "wb") as f:
        f.write(b"TJF1" + struct.pack("<I", len(cps)))
        f.write(struct.pack("<%dH" % len(cps), *cps))
        for cp in cps:
            f.write(b"".join(struct.pack(">H", r & 0xFFFF) for r in table[cp]))
    print("%s: %d glyphs, %d bytes" % (dst, len(cps), 8 + 34 * len(cps)))


if __name__ == "__main__":
    main()
