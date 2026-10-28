#!/usr/bin/env python3
"""Pack the PNG icon set into mdread.ico for the Windows executable (Python 3, stdlib only).

The frames come from hicolor/ (16-256 px, rendered by render.sh) plus ico/ for the
Windows-only 20 and 40 px sizes (small-icon and 32 px slots at 125 % scaling). Frames
below 256 px are stored as 32-bit BMPs, which every Windows API reads; 256 px is stored
PNG-compressed, as Windows Vista and later expect. Run it after render.sh and commit the
result; the output is byte-for-byte reproducible.
"""
import struct
import sys
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def source(n):
    p = HERE / "hicolor" / f"{n}x{n}" / "apps" / "mdread.png"
    return p if p.exists() else HERE / "ico" / f"{n}x{n}.png"


def decode_png(data):
    """Decode an 8-bit RGB/RGBA non-interlaced PNG into (width, height, RGBA bytes)."""
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    pos, idat, ihdr = 8, b"", None
    while pos < len(data):
        length, kind = struct.unpack(">I4s", data[pos : pos + 8])
        body = data[pos + 8 : pos + 8 + length]
        if kind == b"IHDR":
            ihdr = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat += body
        pos += 12 + length
    w, h, depth, ctype, _, _, interlace = ihdr
    assert depth == 8 and ctype in (2, 6) and interlace == 0, f"unsupported PNG format {ihdr}"
    bpp = 4 if ctype == 6 else 3
    raw, stride = zlib.decompress(idat), w * bpp
    out, prev = bytearray(), bytearray(stride)
    for y in range(h):
        f, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1 : (y + 1) * (stride + 1)])
        for i in range(stride):
            a = line[i - bpp] if i >= bpp else 0
            b = prev[i]
            c = prev[i - bpp] if i >= bpp else 0
            if f == 1:
                line[i] = (line[i] + a) & 0xFF
            elif f == 2:
                line[i] = (line[i] + b) & 0xFF
            elif f == 3:
                line[i] = (line[i] + (a + b) // 2) & 0xFF
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[i] = (line[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 0xFF
        out += line if bpp == 4 else b"".join(line[i : i + 3] + b"\xff" for i in range(0, stride, 3))
        prev = line
    return w, h, bytes(out)


def bmp_frame(w, h, rgba):
    """BITMAPINFOHEADER + bottom-up BGRA pixels + 1-bit AND mask (set where fully transparent)."""
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    pixels, mask = bytearray(), bytearray()
    mask_stride = (w + 31) // 32 * 4
    for y in reversed(range(h)):
        row = rgba[y * w * 4 : (y + 1) * w * 4]
        bits = bytearray(mask_stride)
        for x in range(w):
            r, g, b, a = row[x * 4 : x * 4 + 4]
            pixels += bytes((b, g, r, a))
            if a == 0:
                bits[x // 8] |= 0x80 >> (x % 8)
        mask += bits
    return header + bytes(pixels) + bytes(mask)


def main():
    frames = []
    for n in SIZES:
        png = source(n).read_bytes()
        w, h, rgba = decode_png(png)
        assert (w, h) == (n, n), f"{source(n)} is {w}x{h}, expected {n}x{n}"
        frames.append((n, png if n >= 256 else bmp_frame(w, h, rgba)))
    out = bytearray(struct.pack("<HHH", 0, 1, len(frames)))
    offset = 6 + 16 * len(frames)
    for n, blob in frames:
        out += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    for _, blob in frames:
        out += blob
    dest = Path(sys.argv[1]) if len(sys.argv) > 1 else HERE / "mdread.ico"
    dest.write_bytes(out)
    print(f"wrote {dest} ({len(out)} bytes, sizes {', '.join(map(str, SIZES))})")


if __name__ == "__main__":
    main()
