#!/usr/bin/env python3
"""Draws every icon from the joystick-Q geometry in source.svg (kept in sync by hand).

    python3 assets/brand/make.py      # needs Pillow; rewrites the outputs below

Outputs (committed, so builds need no Pillow):
    assets/brand/icon.png             1024, rounded square, transparent corners
    assets/brand/github-avatar.png    1024, opaque (GitHub crops it to a circle)
    assets/brand/social-preview.png   1280x640, the repo's social preview
    web/favicon.ico                   16/32/48, zoomed in so the Q reads at that size
    web/icons/icon-32.png             same
    web/icons/icon-{192,512}.png
    web/icons/apple-touch-icon.png    180, opaque
    packaging/app/tq-app.icns         macOS, inset on Apple's 824/1024 grid
    crates/tq-app/assets/tq-app.png   512, Linux window icon and .desktop icon
"""
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[2]
BG = (0xF5, 0xC4, 0x00)  # hazard yellow
INK = (0x14, 0x14, 0x14)
KNOB = (0xFF, 0x5A, 0x1F)  # safety orange
SS = 4  # supersampling for antialiasing


def draw(n: int, zoom: float = 1.0, tile: bool = True) -> Image.Image:
    """The icon at n px. Coordinates are source.svg's 100-unit viewBox; zoom scales
    the mark about its centre (52, 52), leaving the tile as is."""
    s = n * SS / 100
    im = Image.new("RGBA", (n * SS, n * SS), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)

    def p(v: float, axis_c: float = 52) -> float:
        return (axis_c + (v - axis_c) * zoom) * s

    def disc(cx: float, cy: float, r: float, fill) -> None:
        r *= zoom * s
        d.ellipse((p(cx) - r, p(cy) - r, p(cx) + r, p(cy) + r), fill=fill)

    if tile:
        d.rounded_rectangle((0, 0, n * SS - 1, n * SS - 1), radius=22 * s, fill=(*BG, 255))
    else:
        d.rectangle((0, 0, n * SS, n * SS), fill=(*BG, 255))
    # Ring: r 22, stroke 10.
    disc(46, 46, 27, INK)
    disc(46, 46, 17, BG)
    # Stick: width 8, round caps; hub; knob.
    d.line((p(46), p(46), p(74), p(74)), fill=INK, width=round(8 * zoom * s))
    disc(74, 74, 4, INK)
    disc(46, 46, 5, INK)
    disc(76, 76, 9, KNOB)
    return im.resize((n, n), Image.LANCZOS)


def opaque(im: Image.Image) -> Image.Image:
    out = Image.new("RGBA", im.size, (*BG, 255))
    out.alpha_composite(im)
    return out.convert("RGB")


def save(im: Image.Image, rel: str, **kw) -> None:
    path = ROOT / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    im.save(path, optimize=True, **kw)
    print(rel)


def main() -> None:
    icon = draw(1024)
    save(icon, "assets/brand/icon.png")
    save(opaque(icon), "assets/brand/github-avatar.png")

    # Social preview: the mark, untiled, centred on a 1280x640 yellow field.
    social = Image.new("RGBA", (1280, 640), (*BG, 255))
    social.alpha_composite(draw(640, tile=False), (320, 0))
    save(social.convert("RGB"), "assets/brand/social-preview.png")

    small = draw(1024, zoom=1.3)
    save(small.resize((48, 48), Image.LANCZOS), "web/favicon.ico", sizes=[(16, 16), (32, 32), (48, 48)])
    save(draw(32, zoom=1.3), "web/icons/icon-32.png")
    for n in (192, 512):
        save(draw(n), f"web/icons/icon-{n}.png")
    save(opaque(draw(180)), "web/icons/apple-touch-icon.png")

    # macOS draws app icons at 824/1024 of the canvas, centred.
    mac = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    mac.alpha_composite(draw(824), (100, 100))
    save(mac, "packaging/app/tq-app.icns")

    save(draw(512), "crates/tq-app/assets/tq-app.png")


if __name__ == "__main__":
    main()
