"""A copy of the speech model with its weights already in 8 bits.

The published model stores 16-bit floats. With the compute type the engine uses on a GPU
(int8_float16), CTranslate2 turns them into 8-bit integers every time it loads the model: a
second and a half of every start. This writes the result of that step once, to
~/.cache/livesubs/asr/, and the engine loads the copy from then on. The copy is half the size.

The engine starts this file as a low-priority process after its first start with a model;
it can also be run by hand:  uv run asrcache.py distil-large-v3.5
"""

from __future__ import annotations

import os
import shutil
import struct
import subprocess
import sys
from pathlib import Path

import numpy as np

COMPUTE = "int8_float16"
FLOAT32, INT8, FLOAT16 = 0, 1, 4  # the DataType ids of CTranslate2's model file


def cache_dir(name: str) -> Path:
    return Path.home() / ".cache" / "livesubs" / "asr" / f"{name.replace('/', '--')}-{COMPUTE}"


def ready(name: str) -> Path | None:
    """The converted copy of this model, if a complete one exists."""
    d = cache_dir(name)
    return d if (d / "model.bin").is_file() and (d / "complete").is_file() else None


def discard(name: str) -> None:
    shutil.rmtree(cache_dir(name), ignore_errors=True)


def build_later(name: str) -> None:
    """Convert in a process of its own, so that the live subtitles do not wait for it."""
    if ready(name) is None:
        subprocess.Popen(
            ["nice", "-n", "15", sys.executable, __file__, name],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,  # the engine may exit when idle before this is done
        )


def _read_string(f) -> str:
    (n,) = struct.unpack("H", f.read(2))
    return f.read(n)[:-1].decode("utf-8")


def _write_string(f, s: str) -> None:
    raw = s.encode("utf-8")
    f.write(struct.pack("H", len(raw) + 1) + raw + b"\0")


def convert(src: Path, dst: Path) -> None:
    """Write `src`/model.bin to `dst` with every 2-D float "weight" as int8 plus a scale per row.

    The same rule and arithmetic as CTranslate2's own converter (specs/model_spec.py). The two
    convolution kernels are left as floats: the GPU runs those in floats anyway.
    """
    tmp = dst.with_name(dst.name + ".tmp")
    shutil.rmtree(tmp, ignore_errors=True)
    tmp.mkdir(parents=True)
    with open(src / "model.bin", "rb") as f, open(tmp / "model.bin", "wb") as out:
        version, = struct.unpack("I", f.read(4))
        spec = _read_string(f)
        revision, count = struct.unpack("II", f.read(8))
        index = {}  # name -> (shape, dtype id, offset, bytes)
        for _ in range(count):
            name = _read_string(f)
            (rank,) = struct.unpack("B", f.read(1))
            shape = struct.unpack(f"{rank}I", f.read(4 * rank))
            dtype, size = struct.unpack("=BI", f.read(5))
            index[name] = (shape, dtype, f.tell(), size)
            f.seek(size, os.SEEK_CUR)
        (n_alias,) = struct.unpack("I", f.read(4))
        aliases = [(_read_string(f), _read_string(f)) for _ in range(n_alias)]

        def quantized(name: str) -> bool:
            shape, dtype, _, _ = index[name]
            return name.endswith("weight") and len(shape) == 2 and dtype in (FLOAT32, FLOAT16)

        names = sorted(list(index) + [n + "_scale" for n in index if quantized(n)])
        out.write(struct.pack("I", version))
        _write_string(out, spec)
        out.write(struct.pack("II", revision, len(names)))
        scales = {}
        for name in names:
            if name in scales:  # sorted right after the weight it belongs to
                value, dtype = scales.pop(name), FLOAT32
            else:
                shape, dtype, offset, size = index[name]
                f.seek(offset)
                raw = f.read(size)
                if quantized(name):
                    w = np.frombuffer(raw, dtype=np.float16 if dtype == FLOAT16 else np.float32).astype(np.float32).reshape(shape)
                    amax = np.amax(np.absolute(w), axis=1)
                    amax[amax == 0] = 127.0
                    scale = (127.0 / amax).astype(np.float32)
                    value, dtype = np.rint(w * scale[:, None]).astype(np.int8), INT8
                    scales[name + "_scale"] = scale
                else:
                    value = None
            _write_string(out, name)
            if value is None:
                out.write(struct.pack("B", len(shape)) + struct.pack(f"{len(shape)}I", *shape) + struct.pack("=BI", dtype, size))
                out.write(raw)
            else:
                out.write(struct.pack("B", value.ndim) + struct.pack(f"{value.ndim}I", *value.shape) + struct.pack("=BI", dtype, value.nbytes))
                out.write(value.tobytes())
        # a weight shared under a second name shares its scales too
        aliases += [(a + "_scale", v + "_scale") for a, v in aliases if quantized(v)]
        out.write(struct.pack("I", len(aliases)))
        for alias, variable in sorted(aliases):
            _write_string(out, alias)
            _write_string(out, variable)
    for other in src.iterdir():
        if other.name != "model.bin" and other.is_file():
            shutil.copyfile(other, tmp / other.name)
    (tmp / "complete").write_text("converted from " + str(src) + "\n")
    shutil.rmtree(dst, ignore_errors=True)
    tmp.rename(dst)


def main() -> None:
    from faster_whisper.utils import download_model

    name = sys.argv[1]
    convert(Path(download_model(name, local_files_only=True)), cache_dir(name))
    print(cache_dir(name))


if __name__ == "__main__":
    main()
