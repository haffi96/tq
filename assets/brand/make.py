#!/usr/bin/env python3
"""Derives every icon from source.png (1024x1024, full-bleed rounded square on white).

    python3 assets/brand/make.py      # needs Pillow; rewrites the outputs below

Outputs (committed, so builds need no Pillow):
    assets/brand/icon.png             1024, rounded square, transparent corners
    assets/brand/github-avatar.png    1024, opaque (GitHub crops it to a circle)
    assets/brand/social-preview.png   1280x640, the repo's social preview
    web/favicon.ico                   16/32/48, tighter crop so TQ reads at that size
    web/icons/icon-32.png             same
    web/icons/icon-{192,512}.png
    web/icons/apple-touch-icon.png    180, opaque
    packaging/app/tq-app.icns         macOS, inset on Apple's 824/1024 grid
    crates/tq-app/assets/tq-app.png   512, Linux window icon and .desktop icon
"""
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[2]
BRAND = ROOT / "assets/brand"
BG = (11, 12, 16)  # the source's background
WHITE = 250  # the source's white is 253-255, not quite 255
CORNER = 300  # the corner rounding sits within this many px of each corner


def master() -> Image.Image:
    """The source with its white corners made transparent (antialiased)."""
    im = Image.open(BRAND / "source.png").convert("RGBA")
    n = im.width
    px = im.load()
    for y in range(n):
        for x in range(n):
            if min(x, n - 1 - x) >= CORNER or min(y, n - 1 - y) >= CORNER:
                continue
            r, g, b, _ = px[x, y]
            # White outside, BG inside: coverage from the darkest channel.
            a = max(0.0, min(1.0, (WHITE - min(r, g, b)) / (WHITE - BG[2])))
            px[x, y] = (*BG, round(a * 255)) if a > 0 else (0, 0, 0, 0)
    return im


def wordmark(icon: Image.Image) -> Image.Image:
    """The TQ and its speed lines, without the rounded square's padding."""
    return icon.crop((40, 270, 984, 754))


def small(icon: Image.Image) -> Image.Image:
    """For 16-48 px: the wordmark nearly edge to edge on a tighter square."""
    n = 1024
    out = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    ImageDraw.Draw(out).rounded_rectangle((0, 0, n - 1, n - 1), radius=200, fill=(*BG, 255))
    mark = wordmark(icon)
    w = int(n * 0.96)
    mark = mark.resize((w, mark.height * w // mark.width), Image.LANCZOS)
    out.alpha_composite(mark, ((n - mark.width) // 2, (n - mark.height) // 2))
    return out


def on_bg(im: Image.Image) -> Image.Image:
    out = Image.new("RGBA", im.size, (*BG, 255))
    out.alpha_composite(im)
    return out.convert("RGB")


def scaled(im: Image.Image, n: int) -> Image.Image:
    return im.resize((n, n), Image.LANCZOS)


def save(im: Image.Image, rel: str, **kw) -> None:
    path = ROOT / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    im.save(path, optimize=True, **kw)
    print(rel)


def main() -> None:
    icon = master()
    save(icon, "assets/brand/icon.png")
    save(on_bg(icon), "assets/brand/github-avatar.png")

    # Social preview: the wordmark (the icon minus its padding) centred on BG.
    mark = wordmark(icon)
    mark = mark.resize((int(mark.width * 0.9), int(mark.height * 0.9)), Image.LANCZOS)
    social = Image.new("RGBA", (1280, 640), (*BG, 255))
    social.alpha_composite(mark, ((1280 - mark.width) // 2, (640 - mark.height) // 2))
    save(social.convert("RGB"), "assets/brand/social-preview.png")

    save(scaled(small(icon), 48), "web/favicon.ico", sizes=[(16, 16), (32, 32), (48, 48)])
    save(scaled(small(icon), 32), "web/icons/icon-32.png")
    for n in (192, 512):
        save(scaled(icon, n), f"web/icons/icon-{n}.png")
    save(scaled(on_bg(icon), 180), "web/icons/apple-touch-icon.png")

    # macOS draws app icons at 824/1024 of the canvas, centred.
    mac = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    mac.alpha_composite(scaled(icon, 824), (100, 100))
    save(mac, "packaging/app/tq-app.icns")

    save(scaled(icon, 512), "crates/tq-app/assets/tq-app.png")


if __name__ == "__main__":
    main()
