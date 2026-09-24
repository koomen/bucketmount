#!/usr/bin/env python3
"""Render assets/icon.png (1024x1024): a rounded blue tile with a white bucket.

Pure Python (zlib only) so it runs anywhere. Re-run after tweaking; the
result is committed so normal builds do not need it.
"""
import math, struct, sys, zlib, os

N = 1024

def png(w, h, rows):
    def chunk(t, d):
        c = struct.pack(">I", len(d)) + t + d
        return c + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    raw = b"".join(b"\x00" + bytes(r) for r in rows)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))

def rounded_rect(x, y, cx, cy, half, r):
    dx, dy = abs(x - cx) - (half - r), abs(y - cy) - (half - r)
    return math.hypot(max(dx, 0), max(dy, 0)) + min(max(dx, dy), 0) - r

def bucket(x, y):
    """Signed distance to a bucket silhouette (trapezoid body + rim + handle)."""
    # body: trapezoid, wider at top
    top, bottom = 300, 760
    if not (top <= y <= bottom):
        body = 1e9
    else:
        t = (y - top) / (bottom - top)
        halfw = 250 - 70 * t
        body = abs(x - 512) - halfw
    # rim: rounded bar on top
    rim = rounded_rect(x, y, 512, 300, 290, 28) if True else 1e9
    rim = max(rim, abs(y - 300) - 34)
    # handle: arc
    d = math.hypot(x - 512, y - 330)
    handle = abs(d - 200) - 22
    if y > 300:
        handle = 1e9
    return min(body, rim, handle)

def main(out):
    rows = []
    for y in range(N):
        row = bytearray()
        for x in range(N):
            # tile
            d = rounded_rect(x + 0.5, y + 0.5, N / 2, N / 2, 460, 200)
            a = min(max(0.5 - d, 0.0), 1.0)
            if a <= 0:
                row += b"\x00\x00\x00\x00"
                continue
            t = (y / N)
            r, g, b = int(20 + 30 * t), int(110 + 40 * (1 - t)), int(230 - 40 * t)
            db = bucket(x + 0.5, y + 0.5)
            wb = min(max(0.5 - db / 1.5, 0.0), 1.0)
            r = int(r + (255 - r) * wb)
            g = int(g + (255 - g) * wb)
            b = int(b + (255 - b) * wb)
            row += bytes((r, g, b, int(a * 255)))
        rows.append(row)
    with open(out, "wb") as f:
        f.write(png(N, N, rows))
    print("wrote", out)

if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "assets", "icon.png"))
