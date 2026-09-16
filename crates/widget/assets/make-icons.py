#!/usr/bin/env python3
"""Generates the widget app icon and the monochrome tray glyph.

Run from the crate root:
    python3 assets/make-icons.py

Outputs:
    assets/icon-source.png   1024x1024 coloured bundle icon
    assets/tray.png          64x64 black-on-transparent tray template glyph
"""

from PIL import Image, ImageDraw
import math
import os

HERE = os.path.dirname(os.path.abspath(__file__))
SIZE = 1024


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def bridge(draw, w, h, colour, scale=1.0, offset=(0, 0)):
    """Draws a suspension-bridge silhouette scaled to the canvas."""
    ox, oy = offset
    cx = w / 2 + ox
    deck_y = h * 0.66 + oy
    span = w * 0.78 * scale
    left = cx - span / 2
    right = cx + span / 2
    tower_h = h * 0.42 * scale
    tower_top = deck_y - tower_h
    stroke = max(2, round(h * 0.028 * scale))

    # Deck.
    draw.rounded_rectangle(
        [left, deck_y - stroke / 2, right, deck_y + stroke / 2],
        radius=stroke,
        fill=colour,
    )
    # Towers.
    for tx in (cx - span * 0.28, cx + span * 0.28):
        draw.rounded_rectangle(
            [tx - stroke * 0.8, tower_top, tx + stroke * 0.8, deck_y + stroke],
            radius=stroke * 0.8,
            fill=colour,
        )
    # Main catenary cables: two arcs from each tower top down to the deck ends.
    for tx in (cx - span * 0.28, cx + span * 0.28):
        for anchor in (left, right):
            mid_x = (tx + anchor) / 2
            sag = (deck_y - tower_top) * 0.55
            pts = []
            for i in range(41):
                t = i / 40
                x = tx + (anchor - tx) * t
                y = tower_top + math.sin(math.pi * t) * sag
                pts.append((x, y))
            draw.line(pts, fill=colour, width=stroke, joint="curve")
            _ = mid_x
    # Suspender cables.
    for i in range(1, 12):
        t = i / 12
        x = left + span * t
        d = min(abs(x - (cx - span * 0.28)), abs(x - (cx + span * 0.28)))
        top = tower_top + math.sin(math.pi * (d / (span * 0.28))) * (deck_y - tower_top) * 0.4
        draw.line([x, top, x, deck_y], fill=colour, width=max(2, stroke // 2))


def bundle_icon():
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    # Rounded-square gradient background.
    bg = Image.new("RGB", (SIZE, SIZE))
    px = bg.load()
    top, bottom = (28, 58, 132), (0, 170, 170)
    for y in range(SIZE):
        row = lerp(top, bottom, y / (SIZE - 1))
        for x in range(SIZE):
            px[x, y] = row
    mask = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, SIZE - 1, SIZE - 1], radius=int(SIZE * 0.22), fill=255)
    img.paste(bg, (0, 0), mask)

    d = ImageDraw.Draw(img)
    # Waterline hint.
    d.line([SIZE * 0.16, SIZE * 0.74, SIZE * 0.84, SIZE * 0.74],
           fill=(255, 255, 255, 90), width=int(SIZE * 0.012))
    bridge(d, SIZE, SIZE, (255, 255, 255, 255))
    img.save(os.path.join(HERE, "icon-source.png"))


def tray_glyph():
    s = 64
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    bridge(d, s, s, (0, 0, 0, 255), scale=0.92, offset=(0, -3))
    img.save(os.path.join(HERE, "tray.png"))


if __name__ == "__main__":
    bundle_icon()
    tray_glyph()
    print("wrote icon-source.png and tray.png")
