#!/usr/bin/env python3
"""Regenerate all tray-icon proposal assets (round 1 + round 2).

LESSON (2026-09-05): stroke-based shapes MUST set stroke="#000000" explicitly.
The SVG wrapper only provides fill; a bare stroke-width renders nothing when
rasterized to PNG (rsvg default stroke is none), which silently produced
fully-transparent "outline" icons. The HTML reports never exposed this
because CSS injects the stroke color per instance.

Checks: every exported PNG must (a) contain only pure-black RGB with alpha,
(b) have ink coverage (alpha>8 pixels) above a per-variant floor so a blank
or near-empty render fails loudly.

Usage: python3 icons/generate_assets.py   (requires rsvg-convert, Pillow)
"""
import os
import subprocess
import sys

from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
S = 'stroke="#000000"'


def svg(body, opacity=1.0):
    op = "" if opacity >= 1 else f' opacity="{opacity}"'
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" '
            f'viewBox="0 0 32 32"><g fill="#000000"{op}>{body}</g></svg>')


def stroked(inner, sw, cap="round"):
    return f'<g fill="none" {S} stroke-width="{sw}" stroke-linecap="{cap}">{inner}</g>'


def outlined(inner, sw=1.8):
    return f'<g fill="none" {S} stroke-width="{sw}" stroke-linejoin="round">{inner}</g>'


def paths(*ds):
    return "".join(f'<path d="{d}"/>' for d in ds)


def circle(cx, cy, r):
    return f'<circle cx="{cx}" cy="{cy}" r="{r}"/>'


# ---------------------------------------------------------------- round 1
A = [
    "M15.4 3.2 L23 5.7 L15.4 8.5 Z",
    "M15.4 10 C20.9 13.4 25.4 17.3 25.4 21.2 L15.4 21.2 Z",
    "M4.6 23.4 L27.4 23.4 L24.7 27 C24.3 27.7 23.5 28.2 22.6 28.2 "
    "L9.4 28.2 C8.5 28.2 7.7 27.7 7.3 27 Z",
]
B_SAIL = "M9.6 22.8 C9.6 13.6 13.8 7.4 22.8 5.2 C22.2 12.2 20 18.4 15.4 22.8 Z"
B_WAVE = "M6.2 26.8 q2.45 -2.6 4.9 0 t4.9 0 t4.9 0 t4.9 0"
C = [
    "M13.8 7.6 L25.2 15 L13.8 22.4 Z",
    "M6.2 24.6 L25.8 24.6 L23.4 27.6 C23.1 28.1 22.5 28.4 21.9 28.4 "
    "L10.1 28.4 C9.5 28.4 8.9 28.1 8.6 27.6 Z",
]
D_ARC = ("M18.5 6.4 a2.5 2.5 0 1 1 -5 0 a2.5 2.5 0 1 1 5 0")
D_ARMS = "M7.2 17.4 C7.2 22.8 10.4 25.6 16 25.6 C21.6 25.6 24.8 22.8 24.8 17.4"

import math
E_SPOKES = "".join(
    f'<path d="M{16 + 3.6 * math.cos(math.radians(k * 45)):.2f} '
    f'{16 + 3.6 * math.sin(math.radians(k * 45)):.2f} L'
    f'{16 + 12.2 * math.cos(math.radians(k * 45)):.2f} '
    f'{16 + 12.2 * math.sin(math.radians(k * 45)):.2f}"/>'
    for k in range(8)
)

# ---------------------------------------------------------------- round 2
N1_WEDGE = "M16 16 L16 5.4 A10.6 10.6 0 0 1 25.2 10.7 Z"
N1_BLIP = "M10.6 20.2 a1.9 1.9 0 1 0 0.01 0 Z"
N2_CURVE = "M9.6 21.4 C13.4 17.6 17.4 14.2 22.2 10.6"
N2_ARROW = "M26.6 6.6 L24.0 13.0 L20.4 8.2 Z"
N3_CHEV = "M7.6 10.2 L16.2 16 L7.6 21.8 L7.6 18.2 L11.4 16 L7.6 13.8 Z"
N4_ARC = "M11.3 8.6 A9.2 9.2 0 1 0 20.7 8.6"
N5_BODY = ("M16 3.8 C19 5.8 20.2 9 20.2 12.4 L20.2 17.6 L11.8 17.6 L11.8 12.4 "
           "C11.8 9 13 5.8 16 3.8 Z M16 10.85 a2.15 2.15 0 1 0 -0.01 0 Z")
N5_FINS = "M11.8 13.6 L8 19.2 L11.8 19.8 Z M20.2 13.6 L24 19.2 L20.2 19.8 Z"
N5_FLAME = "M16 21.6 C17.7 23.1 18 25.2 16 27.6 C14 25.2 14.3 23.1 16 21.6 Z"
N5_BODY_OUT = ("M16 3.8 C19 5.8 20.2 9 20.2 12.4 L20.2 17.6 L11.8 17.6 L11.8 12.4 "
               "C11.8 9 13 5.8 16 3.8 Z")

# pre-composed inner fragments (py<3.12 f-strings cannot nest escaped quotes)
P_B_SAIL = f'<path d="{B_SAIL}"/>'
P_N2_ARROW = f'<path d="{N2_ARROW}"/>'
P_N4_ARC = f'<path d="{N4_ARC}"/>'

# dir -> {base: (svg-body, ink-floor%)}
PROPOSALS = {
    "proposals/a-sailboat": {
        "tray-icon": (paths(*A), 12.0),
        "tray-icon-outline": (outlined(paths(*A)), 4.0),
    },
    "proposals/b-sailwave": {
        "tray-icon": (f'<path d="{B_SAIL}"/>'
                      f'<path d="{B_WAVE}" fill="none" {S} stroke-width="2.8" stroke-linecap="round"/>', 8.0),
        "tray-icon-outline": (f'{outlined(P_B_SAIL)}'
                              f'<path d="{B_WAVE}" fill="none" {S} stroke-width="2.8" stroke-linecap="round"/>', 6.0),
    },
    "proposals/c-playboat": {
        "tray-icon": (paths(*C), 12.0),
        "tray-icon-outline": (outlined(paths(*C)), 5.0),
    },
    "proposals/d-anchor": {
        "tray-icon": (stroked(paths(D_ARC, "M16 9.2 L16 25.4", "M10.8 12.6 L21.2 12.6", D_ARMS), 2.5), 6.0),
        "tray-icon-dim": (stroked(paths(D_ARC, "M16 9.2 L16 25.4", "M10.8 12.6 L21.2 12.6", D_ARMS), 2.5), 6.0),
    },
    "proposals/e-wheel": {
        "tray-icon": (stroked(circle(16, 16, 9.4) + E_SPOKES, 2.4) + circle(16, 16, 2.7), 10.0),
        "tray-icon-dim": (stroked(circle(16, 16, 9.4) + E_SPOKES, 2.4) + circle(16, 16, 2.7), 10.0),
    },
    "proposals-v2/n1-radar": {
        "tray-icon": (f'{stroked(circle(16, 16, 10.6), 2.4)}<path d="{N1_WEDGE}"/><path d="{N1_BLIP}"/>', 9.0),
        "tray-icon-outline": (outlined(circle(16, 16, 10.6) + f'<path d="{N1_WEDGE}"/>'
                                      + circle(10.6, 20.2, 1.9), 1.7), 5.0),
    },
    "proposals-v2/n2-route": {
        "tray-icon": (f'<circle cx="6.8" cy="23.2" r="2.6"/>'
                      f'<path d="{N2_CURVE}" fill="none" {S} stroke-width="2.5" '
                      f'stroke-linecap="round" stroke-dasharray="0.1 4.6"/>'
                      f'<path d="{N2_ARROW}"/>', 5.0),
        "tray-icon-outline": (f'{outlined(circle(6.8, 23.2, 2.6) + P_N2_ARROW)}'
                              f'<path d="{N2_CURVE}" fill="none" {S} stroke-width="2.5" '
                              f'stroke-linecap="round" stroke-dasharray="0.1 4.6"/>', 4.0),
    },
    "proposals-v2/n3-prompt": {
        "tray-icon": (f'<path d="{N3_CHEV}"/><rect x="17.4" y="18.2" width="7.6" height="3" rx="1.5"/>', 7.0),
        "tray-icon-outline": (outlined(f'<path d="{N3_CHEV}"/>'
                                       + '<rect x="17.4" y="18.2" width="7.6" height="3" rx="1.5"/>', 1.7), 5.0),
    },
    "proposals-v2/n4-power": {
        "tray-icon": (stroked(P_N4_ARC, 2.7)
                      + '<rect x="14.55" y="4.8" width="2.9" height="10.2" rx="1.45"/>', 6.0),
        "tray-icon-outline": (stroked(f'<path d="{N4_ARC}"/>'
                                      + '<rect x="14.55" y="4.8" width="2.9" height="10.2" rx="1.45"/>', 1.6), 4.0),
    },
    "proposals-v2/n5-rocket": {
        "tray-icon": (f'<path fill-rule="evenodd" d="{N5_BODY}"/><path d="{N5_FINS}"/><path d="{N5_FLAME}"/>', 10.0),
        "tray-icon-outline": (outlined(f'<path d="{N5_BODY_OUT}"/>' + circle(16, 13, 2.15)
                                       + f'<path d="{N5_FINS}"/><path d="{N5_FLAME}"/>', 1.6), 5.0),
    },
}

# dim variants render at 45% opacity (D/E "已停止" semantics)
DIM = {("proposals/d-anchor", "tray-icon-dim"): 0.45,
       ("proposals/e-wheel", "tray-icon-dim"): 0.45}


def export(body, out_png, opacity=1.0):
    tmp = "/tmp/_gen_assets.svg"
    with open(tmp, "w") as f:
        f.write(svg(body, opacity))
    for px, suffix in ((32, ""), (64, "@2x")):
        subprocess.run(["rsvg-convert", "-w", str(px), "-h", str(px),
                        "-o", out_png + suffix + ".png", tmp], check=True)


def check(path, floor):
    img = Image.open(path).convert("RGBA")
    data = list(img.getdata())
    bad = [c for c in set(data) if c[3] > 8 and c[:3] != (0, 0, 0)]
    ink = 100 * sum(1 for c in data if c[3] > 8) / len(data)
    assert not bad, f"{path}: non-black pixels {bad[:3]}"
    assert img.size in ((32, 32), (64, 64)), f"{path}: bad size {img.size}"
    assert ink >= floor, f"{path}: ink {ink:.1f}% < floor {floor}% (blank render?)"
    # Template-tint simulation: macOS recolors glyph pixels to the menu-bar
    # label color (near-white on dark bars, near-black on light bars). Fail
    # the export if the tinted glyph would not be clearly visible at real
    # status-item scale (18pt = 36px on a 2x display).
    for bg, tint, mode in (((38, 38, 40), (255, 255, 255), "dark"),
                           ((245, 245, 247), (26, 26, 28), "light")):
        visible = tinted_visible_pixels(path, bg, tint)
        assert visible >= 40, (f"{path}: only {visible} visible pixels on {mode} "
                               f"menu bar simulation — icon would be unreadable")


def tinted_visible_pixels(path, bg, tint, threshold=24):
    img = Image.open(path).convert("RGBA").resize((36, 36), Image.LANCZOS)
    count = 0
    for (r, g, b, a) in img.getdata():
        if a < 8:
            continue
        lum_glyph = (r * 299 + g * 587 + b * 114) // 1000
        lum_bg = (bg[0] * 299 + bg[1] * 587 + bg[2] * 114) // 1000
        alpha = a / 255.0
        # template tint: glyph luminance replaced by tint luminance, alpha-blended
        lum_tint = (tint[0] * 299 + tint[1] * 587 + tint[2] * 114) // 1000
        blended = lum_bg + (lum_tint - lum_bg) * alpha
        if abs(blended - lum_bg) >= threshold:
            count += 1
    return count


def main():
    failed = []
    for reldir, variants in PROPOSALS.items():
        d = os.path.join(HERE, reldir)
        os.makedirs(d, exist_ok=True)
        for base, (body, floor) in variants.items():
            opacity = DIM.get((reldir, base), 1.0)
            with open(os.path.join(d, base + ".svg"), "w") as f:
                f.write(svg(body, opacity))
            export(body, os.path.join(d, base), opacity)
            try:
                for suffix in ("", "@2x"):
                    check(os.path.join(d, base + suffix + ".png"), floor)
            except AssertionError as e:
                failed.append(str(e))
            print(f"ok {reldir}/{base}")
    if failed:
        print("\nFAILURES:")
        print("\n".join(failed))
        sys.exit(1)

    # promote chosen design (B 极简帆浪) to runtime assets loaded by tray.rs
    b = os.path.join(HERE, "proposals", "b-sailwave")
    for base in ("tray-icon", "tray-icon-outline"):
        with open(os.path.join(b, base + "@2x.png"), "rb") as src, \
                open(os.path.join(ROOT, "icons", base + ".png"), "wb") as dst:
            dst.write(src.read())
    check(os.path.join(ROOT, "icons", "tray-icon.png"), 8.0)
    check(os.path.join(ROOT, "icons", "tray-icon-outline.png"), 6.0)
    print("promoted icons/tray-icon{,-outline}.png (B 极简帆浪, wave 2.8)")


if __name__ == "__main__":
    main()
