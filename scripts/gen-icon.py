#!/usr/bin/env python3
"""Render assets/icon.png (1024x1024): a rounded blue tile with a shaded
metal pail (open elliptical rim, two hoops, wire bail handle). The handle is
turned a little towards the viewer, so its left end is riveted to the front of
the bucket and its right end disappears behind it.

Pure Python (zlib only) so it runs anywhere; it takes about half a minute.
Re-run after tweaking; the result is committed so normal builds do not need
it. The menu bar icon in src/tray.rs is a simplified silhouette of the same
bucket, so keep the two in step.

Then regenerate icons/:

    scripts/gen-icon.py
    cp assets/icon.png icons/icon.png
    sips -z 128 128 assets/icon.png --out icons/128x128.png
    sips -z 32 32 assets/icon.png --out icons/32x32.png
    # icons/icon.icns: build an .iconset with sips, then iconutil -c icns
"""
import math, struct, sys, zlib, os

N = 1024
SS = 4  # supersampling per axis

def png(w, h, rows):
    def chunk(t, d):
        c = struct.pack(">I", len(d)) + t + d
        return c + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    raw = b"".join(b"\x00" + bytes(r) for r in rows)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))

def lerp(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))

def over(dst, src, a):
    return lerp(dst, src, a)

def rounded_rect(x, y, cx, cy, half, r):
    dx, dy = abs(x - cx) - (half - r), abs(y - cy) - (half - r)
    return math.hypot(max(dx, 0), max(dy, 0)) + min(max(dx, dy), 0) - r

def in_ellipse(x, y, cx, cy, rx, ry):
    return ((x - cx) / rx) ** 2 + ((y - cy) / ry) ** 2 <= 1.0

# Geometry (all in 1024 space). The body is a tapered cylinder between the
# top cross-section (centre TOP_Y) and the bottom one (centre BOT_Y).
CX = 512
TOP_Y, TOP_RX, TOP_RY = 360, 250, 60
BOT_Y, BOT_RX, BOT_RY = 770, 185, 45
RIM_OUT = (CX, 360, 266, 70)
RIM_IN = (CX, 356, 238, 54)
EAR_Y, EAR_R = 412, 22
HANDLE_R, HANDLE_H, HANDLE_W = 244, 282, 17
HANDLE_TURN = math.radians(45)  # how far the handle's plane is turned towards the viewer
HANDLE_SQUARE = 0.7  # < 1 squares off the arc
HOOPS = (505, 655)

def halfw(y):
    t = (y - TOP_Y) / (BOT_Y - TOP_Y)
    return TOP_RX + (BOT_RX - TOP_RX) * t

def ry_at(y):
    t = (y - TOP_Y) / (BOT_Y - TOP_Y)
    return TOP_RY + (BOT_RY - TOP_RY) * t

def front_curve(yc, x):
    """y of the front edge of the cross-section centred at yc, at column x."""
    u = (x - CX) / halfw(yc)
    return yc + ry_at(yc) * math.sqrt(max(0.0, 1 - u * u))

def handle_point(t):
    """Screen x, y and depth z (> 0 towards the viewer) of the handle at
    t in [0, pi]: t = 0 is the front-left rivet, t = pi the hidden back end."""
    # raising the height to a power < 1 squares off the arc, so the sides rise
    # nearly straight up from the rivets like a real bail
    c = math.cos(t)
    z = HANDLE_R * c * math.sin(HANDLE_TURN)
    x = CX - HANDLE_R * c * math.cos(HANDLE_TURN)
    y = EAR_Y - HANDLE_H * math.sin(t) ** HANDLE_SQUARE + z * TOP_RY / TOP_RX
    return x, y, z

HANDLE_PTS = [handle_point(math.pi * i / 600) for i in range(601)]
EAR = HANDLE_PTS[0][:2]
CELL = 32
HANDLE_GRID = {}
for i in range(len(HANDLE_PTS) - 1):
    (x0, y0, _), (x1, y1, _) = HANDLE_PTS[i], HANDLE_PTS[i + 1]
    pad = HANDLE_W
    for gx in range(int((min(x0, x1) - pad) // CELL), int((max(x0, x1) + pad) // CELL) + 1):
        for gy in range(int((min(y0, y1) - pad) // CELL), int((max(y0, y1) + pad) // CELL) + 1):
            HANDLE_GRID.setdefault((gx, gy), []).append(i)

def handle_hit(x, y):
    """(distance to the handle's centre line, depth there), or None if far."""
    best = None
    for i in HANDLE_GRID.get((int(x // CELL), int(y // CELL)), ()):
        (x0, y0, z0), (x1, y1, z1) = HANDLE_PTS[i], HANDLE_PTS[i + 1]
        dx, dy = x1 - x0, y1 - y0
        k = min(max(((x - x0) * dx + (y - y0) * dy) / (dx * dx + dy * dy), 0.0), 1.0)
        d = math.hypot(x - x0 - k * dx, y - y0 - k * dy)
        if best is None or d < best[0]:
            best = (d, z0 + (z1 - z0) * k)
    return best if best and best[0] < HANDLE_W / 2 else None

def handle_colour(d):
    k = d / (HANDLE_W / 2)
    return lerp((250, 252, 255), (160, 176, 200), k * k)

METAL_LIGHT = (246, 249, 253)
METAL_DARK = (132, 150, 178)

def metal(u, extra=0.0):
    """Cylinder shading for horizontal position u in [-1, 1]."""
    i = 1 - 0.55 * u * u + 0.18 * math.exp(-((u + 0.38) / 0.2) ** 2) + extra
    return lerp(METAL_DARK, METAL_LIGHT, min(max(i, 0.0), 1.0))

def scene(x, y):
    """Colour and alpha of one sample."""
    d = rounded_rect(x, y, N / 2, N / 2, 460, 200)
    if d > 0:
        return None
    t = y / N
    c = lerp((38, 152, 246), (28, 88, 200), t)

    # soft shadow under the bucket
    e = ((x - CX) / 250) ** 2 + ((y - 818) / 42) ** 2
    if e < 1:
        c = over(c, (8, 30, 80), 0.35 * (1 - e) ** 1.5)

    # the back half of the handle goes behind the bucket
    hh = handle_hit(x, y)
    if hh and hh[1] < 0:
        c = handle_colour(hh[0])

    # body
    if TOP_Y <= y and (y <= BOT_Y or in_ellipse(x, y, CX, BOT_Y, BOT_RX, BOT_RY)):
        yy = min(y, BOT_Y)
        hw = halfw(yy)
        if abs(x - CX) <= hw:
            u = (x - CX) / hw
            shade = -0.12 * (yy - TOP_Y) / (BOT_Y - TOP_Y)
            col = metal(u, shade)
            for hy in HOOPS:
                fc = front_curve(hy, x)
                if abs(y - fc) < 10:
                    col = metal(u, shade - 0.22)
                elif fc + 10 <= y < fc + 15:
                    col = metal(u, shade + 0.2)
            # darken just above the bottom edge
            bc = front_curve(BOT_Y, x)
            if y > bc - 14:
                col = lerp(col, METAL_DARK, 0.35)
            c = col

    # rolled rim and dark interior
    if in_ellipse(x, y, *RIM_OUT):
        u = (x - CX) / RIM_OUT[2]
        if in_ellipse(x, y, *RIM_IN):
            cx, cy, rx, ry = RIM_IN
            v = (y - (cy - ry)) / (2 * ry)
            c = lerp((122, 142, 172), (48, 62, 90), v ** 0.8)
            c = lerp(c, (40, 52, 78), 0.4 * abs((x - cx) / rx) ** 3)
        else:
            lower = y > RIM_OUT[1]
            c = metal(u, 0.12 if lower else 0.2)

    # the front half of the handle, ending in the front rivet
    if hh and hh[1] >= 0:
        c = handle_colour(hh[0])
    dd = math.hypot(x - EAR[0], y - EAR[1])
    if dd < EAR_R:
        c = lerp((250, 252, 255), (150, 166, 192), (dd / EAR_R) ** 2)

    return c

def main(out):
    rows = []
    offs = [(i + 0.5) / SS for i in range(SS)]
    for py in range(N):
        row = bytearray()
        for px in range(N):
            acc = [0.0, 0.0, 0.0]
            hits = 0
            for oy in offs:
                for ox in offs:
                    s = scene(px + ox, py + oy)
                    if s is not None:
                        hits += 1
                        for i in range(3):
                            acc[i] += s[i]
            if hits == 0:
                row += b"\x00\x00\x00\x00"
                continue
            row += bytes([int(round(v / hits)) for v in acc] + [int(round(255 * hits / SS / SS))])
        rows.append(row)
    with open(out, "wb") as f:
        f.write(png(N, N, rows))
    print("wrote", out)

if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "assets", "icon.png"))
