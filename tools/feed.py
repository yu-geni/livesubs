"""Stream a media file to the server at real-time speed and print the subtitles (no browser needed).

    uv run tools/feed.py sample.mp3 [--speed 1.0] [--url ws://localhost:8765/ws]
"""

from __future__ import annotations

import argparse
import asyncio
import json
import subprocess
import time

import aiohttp

FRAME = 3200  # 100 ms of s16le mono 16 kHz


async def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("file")
    p.add_argument("--url", default="ws://localhost:8765/ws")
    p.add_argument("--speed", type=float, default=1.0)
    p.add_argument("--interim", action="store_true", help="also print interim lines")
    args = p.parse_args()

    pcm = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", args.file, "-ac", "1", "-ar", "16000", "-f", "s16le", "-"],
        check=True,
        capture_output=True,
    ).stdout
    pcm += b"\0" * (FRAME * 30)  # trailing silence so the last segment closes
    t0 = time.monotonic()
    lags: list[int] = []

    async with aiohttp.ClientSession() as http, http.ws_connect(args.url) as ws:

        async def send() -> None:
            for i in range(0, len(pcm), FRAME):
                await ws.send_bytes(pcm[i : i + FRAME])
                await asyncio.sleep(max(0.0, t0 + (i / FRAME + 1) * 0.1 / args.speed - time.monotonic()))
            await asyncio.sleep(3)
            await ws.close()

        sender = asyncio.create_task(send())
        async for msg in ws:
            if msg.type != aiohttp.WSMsgType.TEXT:
                continue
            ev = json.loads(msg.data)
            t = (time.monotonic() - t0) * args.speed
            if ev["type"] == "final":
                lags.append(ev["lag_ms"])
                print(f"[{t:6.1f}s lag {ev['lag_ms']:4d}ms] #{ev['id']} {ev['en']}\n{'':24}→ {ev['ja']}")
            elif ev["type"] == "interim":
                if args.interim:
                    print(f"[{t:6.1f}s  interim   ] #{ev['id']} {ev['en']} → {ev['ja']}")
            else:
                print(ev)
        await sender
    if lags:
        lags.sort()
        print(f"\n{len(lags)} segments, lag median {lags[len(lags) // 2]} ms, max {lags[-1]} ms")


asyncio.run(main())
