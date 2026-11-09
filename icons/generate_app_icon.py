#!/usr/bin/env python3
"""Regenerate the macOS/Windows app icon from a source artwork.

Fixes the 2026-10-31 Dock "overflow": the committed icon.icns was a full-bleed
1024px artwork (alpha bbox = 100% of canvas) with an irregular silhouette.
Stock macOS app icons keep the squircle at 824/1024 (~80.5%) plus a soft
shadow reaching ~88-91% (measured on Calculator/Notes/Automator), so a
full-bleed icon renders visibly larger than every Dock neighbor.

Pipeline: extract the largest raster layer from the previous icns (the only
art source in-repo) -> composite onto a Big Sur style squircle plate
(824/1024, radius 22.5% of plate, vertical gradient sampled neutral dark)
with a soft drop shadow -> emit iconset -> iconutil -> icon.icns, plus
icon.png (256) and icon.ico (multi-size) from the same master.

Usage: python3 icons/generate_app_icon.py [--source IMAGE]
(requires Pillow + /usr/bin/iconutil; run from anywhere)
"""
import argparse
import os
import struct
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw, ImageFilter

HERE = os.path.dirname(os.path.abspath(__file__))
ICNS = os.path.join(HERE, "icon.icns")

PLATE = 824            # Apple grid: 824/1024
RADIUS_RATIO = 0.225   # squircle corner radius / plate
ART_RATIO = 0.70       # artwork width / plate — inset so it never reads full-bleed
                       # (2026-11-09: cover-to-plate made hair touch all four edges,
                       #  indistinguishable from the original bleed at Dock size)
ART_RADIUS_RATIO = 0.12  # art card corner radius / art size
GRAD_TOP = (52, 52, 58)
GRAD_BOTTOM = (26, 26, 30)
SHADOW_OFFSET = 18
SHADOW_BLUR = 22
SHADOW_ALPHA = 110

ICNS_SIZES = [16, 32, 64, 128, 256, 512, 1024]  # covers 16..1024 incl @2x
ICO_SIZES = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]


def extract_master(path=ICNS):
    """Largest PNG layer inside an .icns (ic10/ic14 = 1024).

    Crops to the near-opaque bbox so re-harvesting an already-composed icon
    (plate + margins + shadow) yields just the artwork, keeping compose()
    idempotent across regeneration rounds."""
    data = open(path, "rb").read()
    assert data[:4] == b"icns", f"{path}: not an icns"
    off, best = 8, None
    while off < len(data):
        tag, n = struct.unpack(">4sI", data[off:off + 8])
        if tag in (b"ic10", b"ic14") and (best is None or n > best[1]):
            best = (tag, n, off)
        off += n
    if best is None:
        sys.exit(f"{path}: no 512/1024 PNG layer to harvest")
    tag, n, o = best
    png = data[o + 8:o + n]
    open("/tmp/_app_icon_master.png", "wb").write(png)
    art = Image.open("/tmp/_app_icon_master.png").convert("RGBA")
    solid = art.getchannel("A").point(lambda a: 255 if a > 200 else 0)
    bbox = solid.getbbox()
    if bbox and (bbox[2] - bbox[0]) < art.width * 0.98:
        art = art.crop(bbox)
    return art


def squircle_mask(size):
    m = Image.new("L", (size, size), 0)
    ImageDraw.Draw(m).rounded_rectangle(
        [0, 0, size - 1, size - 1], radius=int(size * RADIUS_RATIO), fill=255)
    return m


def compose(art):
    canvas = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    grad = Image.new("RGBA", (PLATE, PLATE))
    gd = ImageDraw.Draw(grad)
    for y in range(PLATE):
        t = y / PLATE
        gd.line([(0, y), (PLATE, y)],
                fill=tuple(int(GRAD_TOP[i] + (GRAD_BOTTOM[i] - GRAD_TOP[i]) * t)
                           for i in range(3)) + (255,))
    # artwork inset as a rounded card on the plate (never edge-to-edge)
    side = int(PLATE * ART_RATIO)
    cover = art.resize((side, side), Image.LANCZOS)
    card = Image.new("L", (side, side), 0)
    ImageDraw.Draw(card).rounded_rectangle(
        [0, 0, side - 1, side - 1], radius=int(side * ART_RADIUS_RATIO), fill=255)
    plate = grad.copy()
    plate.paste(cover, ((PLATE - side) // 2, (PLATE - side) // 2), card)
    composed = Image.composite(plate, grad, squircle_mask(PLATE))
    off = (1024 - PLATE) // 2
    shadow = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    shadow.paste((0, 0, 0, SHADOW_ALPHA),
                 (off, off + SHADOW_OFFSET, off + PLATE, off + PLATE + SHADOW_OFFSET),
                 squircle_mask(PLATE))
    canvas.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(SHADOW_BLUR)))
    canvas.paste(composed, (off, off), composed)
    return canvas


def emit(master):
    with tempfile.TemporaryDirectory() as tmp:
        iconset = os.path.join(tmp, "icon.iconset")
        os.makedirs(iconset)
        for px in ICNS_SIZES:
            for scale, suffix in ((1, ""), (2, "@2x")):
                if px * scale > 1024:
                    continue
                master.resize((px * scale, px * scale), Image.LANCZOS).save(
                    os.path.join(iconset, f"icon_{px}x{px}{suffix}.png"))
        out = os.path.join(HERE, "icon.icns")
        subprocess.run(["iconutil", "-c", "icns", iconset, "-o", out], check=True)
        print(f"wrote {out}")
    master.resize((256, 256), Image.LANCZOS).save(os.path.join(HERE, "icon.png"))
    print(f"wrote {os.path.join(HERE, 'icon.png')}")
    master.save(os.path.join(HERE, "icon.ico"), sizes=ICO_SIZES)
    print(f"wrote {os.path.join(HERE, 'icon.ico')}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--source", help="artwork image; default = harvest old icon.icns")
    args = ap.parse_args()
    art = Image.open(args.source).convert("RGBA") if args.source else extract_master()
    if art.size != (1024, 1024):
        art = art.resize((1024, 1024), Image.LANCZOS)
    emit(compose(art))


if __name__ == "__main__":
    main()
