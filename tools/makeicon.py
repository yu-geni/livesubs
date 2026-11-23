"""Turn the icon artwork into the app's icon (windows/app.ico) and the extension's (extension/icons/*.png).

    uv run --no-project --with pillow tools/makeicon.py [artwork.png]
    uv run --no-project --with pillow tools/makeicon.py --sheet out.png a.png b.png ...

The artwork is a 1024 px picture of the icon on a white background (windows/icon-src/app-icon.png
unless another file is named). The white around it is cut away, the icon is fitted to the canvas,
and every size is reduced from it. --sheet draws several artworks side by side at the sizes
Windows uses, on a light and a dark strip, to choose between them.
"""

from __future__ import annotations

import io
import struct
import sys
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
ARTWORK = ROOT / "windows" / "icon-src" / "app-icon.png"
ICO_SIZES = (16, 20, 24, 32, 40, 48, 64, 256)
PNG_SIZES = (16, 32, 48, 128)


def cut_out(path: Path) -> Image.Image:
    """The icon alone, on a transparent square canvas that it nearly fills."""
    src = Image.open(path).convert("RGB")
    # everything that is white and connected to a corner is background
    probe = src.copy()
    marker = (255, 0, 255)
    for corner in ((0, 0), (src.width - 1, 0), (0, src.height - 1), (src.width - 1, src.height - 1)):
        ImageDraw.floodfill(probe, corner, marker, thresh=60)
    r, g, b = probe.split()
    outside = ImageChops.multiply(ImageChops.multiply(r.point(lambda v: 255 if v == 255 else 0), g.point(lambda v: 255 if v == 0 else 0)),
                                  b.point(lambda v: 255 if v == 255 else 0))
    inside = ImageChops.invert(outside)
    # pull the edge in past the pale fringe, then soften it again
    alpha = inside.filter(ImageFilter.MinFilter(9)).filter(ImageFilter.GaussianBlur(1.5))
    icon = src.convert("RGBA")
    icon.putalpha(alpha)
    box = alpha.point(lambda v: 255 if v > 8 else 0).getbbox()
    icon = icon.crop(box)
    side = max(icon.size)
    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    canvas.alpha_composite(icon, ((side - icon.width) // 2, (side - icon.height) // 2))
    return canvas


def at(icon: Image.Image, size: int) -> Image.Image:
    out = icon.resize((size, size), Image.LANCZOS)
    if size <= 48:  # small sizes go soft when reduced this far
        out = out.filter(ImageFilter.UnsharpMask(radius=0.6, percent=90, threshold=0))
    return out


def write_ico(path: Path, frames: dict[int, Image.Image]) -> None:
    """The 256 px frame as PNG, the others as plain bitmaps: .NET's Icon class (the notification
    area) reads noise out of small PNG frames, which is what Pillow's own writer would produce."""
    blobs = []
    for n, im in sorted(frames.items()):
        if n >= 256:
            buf = io.BytesIO()
            im.save(buf, format="PNG", optimize=True)
            blobs.append(buf.getvalue())
            continue
        pixels = im.transpose(Image.FLIP_TOP_BOTTOM).tobytes("raw", "BGRA")
        mask = bytes(((n + 31) // 32) * 4 * n)  # all zero: the alpha channel does the masking
        header = struct.pack("<IiiHHIIiiII", 40, n, 2 * n, 1, 32, 0, len(pixels) + len(mask), 0, 0, 0, 0)
        blobs.append(header + pixels + mask)
    out = struct.pack("<HHH", 0, 1, len(blobs))
    offset = len(out) + 16 * len(blobs)
    for n, blob in zip(sorted(frames), blobs):
        out += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    path.write_bytes(out + b"".join(blobs))


def sheet(out: Path, artworks: list[Path]) -> None:
    small = (16, 24, 32, 48)
    cell = 300
    img = Image.new("RGBA", (cell * len(artworks), 2 * 330), (243, 243, 243, 255))
    ImageDraw.Draw(img).rectangle([0, 330, img.width, 660], fill=(32, 32, 32, 255))
    draw = ImageDraw.Draw(img)
    for i, path in enumerate(artworks):
        icon = cut_out(path)
        for y0 in (0, 330):
            draw.text((i * cell + 14, y0 + 12), str(i + 1), fill=(120, 120, 120, 255), font_size=30)
            img.alpha_composite(at(icon, 200), (i * cell + 50, y0 + 20))
            x = i * cell + 50
            for n in small:
                img.alpha_composite(at(icon, n), (x, y0 + 250 + (48 - n) // 2))
                x += n + 22
    img.save(out)


def main() -> None:
    args = sys.argv[1:]
    if args[:1] == ["--sheet"]:
        sheet(Path(args[1]), [Path(a) for a in args[2:]])
        return
    icon = cut_out(Path(args[0]) if args else ARTWORK)
    ico = ROOT / "windows" / "app.ico"
    write_ico(ico, {n: at(icon, n) for n in ICO_SIZES})
    for n in PNG_SIZES:
        at(icon, n).save(ROOT / "extension" / "icons" / f"{n}.png", optimize=True)
    print(f"wrote {ico} ({ico.stat().st_size} bytes) and {len(PNG_SIZES)} extension icons")


if __name__ == "__main__":
    main()
