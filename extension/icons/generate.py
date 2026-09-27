#!/usr/bin/env python3
"""Regenerate the toolbar icons: a rounded violet tile reading "67".

Chrome's action icons must be raster, so they are generated here rather than
shipped as SVG. Run from this directory: python3 generate.py
"""
from PIL import Image, ImageDraw, ImageFont

VIOLET = (124, 106, 247, 255)
INK = (255, 255, 255, 255)
# Render at 8x and downsample: PIL has no antialiased rounded rectangle.
SCALE = 8


def font_for(px):
    for path in (
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/Library/Fonts/Arial Bold.ttf",
    ):
        try:
            return ImageFont.truetype(path, px)
        except OSError:
            continue
    return ImageFont.load_default()


def tile(size):
    big = size * SCALE
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([0, 0, big - 1, big - 1], radius=big * 0.22, fill=VIOLET)

    font = font_for(int(big * 0.58))
    # anchor="mm" centres on the glyph box; nudge up to sit on the optical centre.
    d.text((big / 2, big * 0.46), "67", font=font, fill=INK, anchor="mm")
    return img.resize((size, size), Image.LANCZOS)


for size in (16, 32, 48, 128):
    tile(size).save(f"icon{size}.png")
    print(f"wrote icon{size}.png")
