"""Turn the Rust app's picture dumps into a PNG to look at.

    uv run --no-project --with pillow --with numpy tools/rawpng.py out.png a.raw [b.raw ...]

LiveSubs.exe --selftest-overlay-shot / --selftest-gui write what a window would show as
"LSRAW", width, height (u32, little endian) and premultiplied BGRA. The pictures are put side
by side over a calm backdrop, the way a see-through window looks over a picture (a checker
makes the text hard to judge), and reduced to at most 1500 px.
"""

import struct
import sys

import numpy as np
from PIL import Image


def main() -> None:
    ims = []
    for path in sys.argv[2:]:
        with open(path, "rb") as f:
            raw = f.read()
        if raw[:5] != b"LSRAW":
            sys.exit(f"{path}: not a picture dump")
        w, h = struct.unpack("<II", raw[5:13])
        a = np.frombuffer(raw, np.uint8, offset=13).reshape(h, w, 4).astype(np.float32)
        yy, xx = np.mgrid[0:h, 0:w].astype(np.float32)
        backdrop = np.stack([70 + 90 * xx / w + 30 * yy / h, 95 + 50 * yy / h, 140 - 60 * xx / w + 40 * yy / h], -1)
        ims.append(Image.fromarray((a[..., [2, 1, 0]] + backdrop * (1 - a[..., 3:4] / 255)).clip(0, 255).astype(np.uint8)))
    width = sum(i.width for i in ims) + 20 * (len(ims) - 1)
    height = max(i.height for i in ims)
    out = Image.new("RGB", (width, height), (40, 40, 40))
    x = 0
    for i in ims:
        out.paste(i, (x, 0))
        x += i.width + 20
    scale = min(1.0, 1500 / max(width, height))
    out.resize((int(width * scale), int(height * scale)), Image.LANCZOS).save(sys.argv[1])
    print(f"{sys.argv[1]}: {width} x {height} px, shown at {scale:.2f}")


if __name__ == "__main__":
    main()
