#!/usr/bin/env python3
"""Generate assets/app.ico with no third-party dependencies.

The icon is a toggle switch: a dark rounded pill with a bright knob pushed to the
right, which is what LinkSwitch is -- one control that moves your traffic from one
link to the other. Run `python assets/make_icon.py` from the repo root to regenerate.
"""
import math
import os
import struct

SIZES = (16, 24, 32, 48, 64, 256)

BG = (0x14, 0x16, 0x1C)  # near-black card, matches the widget background
TRACK = (0x2A, 0x2F, 0x3A)  # switch track
KNOB = (0x4C, 0xC2, 0x8C)  # green knob: "traffic is flowing over the link you chose"
KNOB_EDGE = (0x30, 0x8A, 0x62)


def _blend(dst, src, a):
    return tuple(round(d + (s - d) * a) for d, s in zip(dst, src))


def _coverage(px, py, inside, samples=4):
    """Box-filter antialiasing: fraction of a pixel covered by `inside`."""
    hit = 0
    step = 1.0 / samples
    for sy in range(samples):
        for sx in range(samples):
            x = px + (sx + 0.5) * step
            y = py + (sy + 0.5) * step
            if inside(x, y):
                hit += 1
    return hit / (samples * samples)


def _rounded_rect(x0, y0, x1, y1, r):
    def inside(x, y):
        cx = min(max(x, x0 + r), x1 - r)
        cy = min(max(y, y0 + r), y1 - r)
        return (x - cx) ** 2 + (y - cy) ** 2 <= r * r
    return inside


def _circle(cx, cy, r):
    def inside(x, y):
        return (x - cx) ** 2 + (y - cy) ** 2 <= r * r
    return inside


def render(n):
    """Return a bottom-up BGRA byte string for an n x n icon."""
    s = n / 32.0  # design grid is 32x32
    px = [[(0, 0, 0, 0)] * n for _ in range(n)]

    card = _rounded_rect(1.5 * s, 1.5 * s, 30.5 * s, 30.5 * s, 7.0 * s)
    track = _rounded_rect(6.0 * s, 11.5 * s, 26.0 * s, 20.5 * s, 4.5 * s)
    knob = _circle(21.0 * s, 16.0 * s, 5.6 * s)
    knob_in = _circle(21.0 * s, 16.0 * s, 4.4 * s)

    for y in range(n):
        for x in range(n):
            a_card = _coverage(x, y, card)
            if a_card <= 0.0:
                continue
            col = BG
            a_track = _coverage(x, y, track)
            if a_track > 0.0:
                col = _blend(col, TRACK, a_track)
            a_knob = _coverage(x, y, knob)
            if a_knob > 0.0:
                a_in = _coverage(x, y, knob_in)
                edge = _blend(col, KNOB_EDGE, a_knob)
                col = _blend(edge, KNOB, a_in)
            px[y][x] = (col[0], col[1], col[2], round(255 * a_card))

    out = bytearray()
    for y in range(n - 1, -1, -1):  # BMP rows are bottom-up
        for x in range(n):
            r, g, b, a = px[y][x]
            out += bytes((b, g, r, a))
    return bytes(out)


def bmp_payload(n, bgra):
    """BITMAPINFOHEADER + XOR pixels + AND mask, as an ICO entry expects."""
    header = struct.pack(
        "<IiiHHIIiiII",
        40,        # biSize
        n,         # biWidth
        n * 2,     # biHeight: XOR bitmap + AND mask stacked
        1,         # biPlanes
        32,        # biBitCount
        0,         # BI_RGB
        len(bgra),
        0, 0, 0, 0,
    )
    # AND mask: fully opaque (alpha in the BGRA data does the real work), rows padded to 4 bytes.
    row = ((n + 31) // 32) * 4
    mask = b"\x00" * (row * n)
    return header + bgra + mask


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    images = [(n, bmp_payload(n, render(n))) for n in SIZES]

    ico = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    offset = 6 + 16 * len(images)
    for n, data in images:
        ico += struct.pack(
            "<BBBBHHII",
            0 if n >= 256 else n,  # 0 means 256
            0 if n >= 256 else n,
            0,   # palette colours
            0,   # reserved
            1,   # planes
            32,  # bpp
            len(data),
            offset,
        )
        offset += len(data)
    for _, data in images:
        ico += data

    path = os.path.join(here, "app.ico")
    with open(path, "wb") as fh:
        fh.write(ico)
    print("wrote {} ({} bytes, sizes {})".format(path, len(ico), ", ".join(map(str, SIZES))))


if __name__ == "__main__":
    main()
