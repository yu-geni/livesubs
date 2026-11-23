"""Local server: serves the subtitle page and turns streamed PCM into Japanese subtitles.

    uv run server.py            # then open http://localhost:8765 in Chrome/Edge
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import json
import logging
import signal
from pathlib import Path
from urllib.parse import urlsplit

from aiohttp import WSCloseCode, WSMsgType, web

from pipeline import MT_MODELS, Config, Engine, Session

WEB_DIR = Path(__file__).parent / "web"
LOOPBACK = ("localhost", "127.0.0.1", "::1")
ENGINE = web.AppKey("engine", Engine)
CLIENTS = web.AppKey("clients", set)
log = logging.getLogger("rvt")


async def index(request: web.Request) -> web.FileResponse:
    return web.FileResponse(WEB_DIR / "index.html")


async def ws_handler(request: web.Request) -> web.WebSocketResponse:
    # Browsers let any web page open a WebSocket to localhost; only our own page may use the engine.
    # (The Windows app and tools/feed.py are not browsers and send no Origin.) Comparing Origin
    # with Host is not enough on its own: a page served from attacker.com whose name resolves to
    # 127.0.0.1 sets both headers to attacker.com and they match, so local_only below insists on
    # the name the page and the app really use.
    origin = request.headers.get("Origin")
    if origin and urlsplit(origin).netloc != request.host:
        raise web.HTTPForbidden(text="cross-origin WebSocket refused")
    engine = request.app[ENGINE]
    ws = web.WebSocketResponse(max_msg_size=1 << 20, heartbeat=20)
    await ws.prepare(request)
    loop = asyncio.get_running_loop()
    out: asyncio.Queue[dict] = asyncio.Queue()
    session = Session(engine, lambda ev: loop.call_soon_threadsafe(out.put_nowait, ev))

    async def pump() -> None:
        try:
            while True:
                await ws.send_str(json.dumps(await out.get(), ensure_ascii=False))
        except ConnectionError:
            pass  # the client went away mid-send; the receive loop below notices and cleans up
        except Exception:
            # Anything else used to end the sender quietly: the picture froze while the session
            # went on decoding at full GPU load. Close instead, so the session is torn down.
            log.exception("sending to the client failed")
            with contextlib.suppress(Exception):
                await ws.close(code=WSCloseCode.INTERNAL_ERROR, message=b"sender failed")

    sender = asyncio.create_task(pump())
    request.app[CLIENTS].add(ws)
    log.info("client connected: %s", request.remote)
    try:
        await ws.send_json({"type": "ready", "asr": engine.asr_name, "mt": engine.mt_name, "device": engine.device})
        async for msg in ws:
            if msg.type == WSMsgType.BINARY:
                session.feed(msg.data)
            elif msg.type == WSMsgType.TEXT:
                try:
                    data = json.loads(msg.data)
                    if data.get("type") == "reset":
                        session.reset(new_stream=bool(data.get("new_stream")))
                    elif data.get("type") == "config" and "topics" in data:
                        session.set_topics(bool(data["topics"]))
                        log.info("client turned the outline %s", "on" if data["topics"] else "off")
                except (ValueError, AttributeError):
                    pass
    finally:
        request.app[CLIENTS].discard(ws)
        session.close()
        sender.cancel()
        log.info("client disconnected: %s", request.remote)
    return ws


@web.middleware
async def local_only(request: web.Request, handler):
    """Answer only what was addressed to the loopback name itself.

    The page is opened at http://localhost:<port>, the Windows app connects to 127.0.0.1: a
    request for any other name reached the engine through someone else's DNS record, and that
    page would be able to read the subtitles (and the outline) of this machine.
    """
    try:
        name = urlsplit(f"//{request.host or ''}").hostname
    except ValueError:  # a malformed Host (bad IPv6 brackets)
        name = None
    if name not in LOOPBACK:
        raise web.HTTPForbidden(text="this engine answers on localhost only")
    return await handler(request)


@web.middleware
async def no_cache(request: web.Request, handler):
    resp = await handler(request)
    resp.headers.setdefault("Cache-Control", "no-cache")
    return resp


async def close_clients(app: web.Application) -> None:
    """Without this a shutdown (Ctrl+C, SIGTERM) waits on every open WebSocket until it times out."""
    for ws in list(app[CLIENTS]):
        await ws.close(code=WSCloseCode.GOING_AWAY, message=b"server shutting down")


async def stop_workers(app: web.Application) -> None:
    """An outline worker is a process of its own: it would hold the GPU after the server is gone."""
    app[ENGINE].stop_topic_procs()


def exit_when_idle(limit: int):
    """Stop the server (freeing the GPU) once nobody has been connected for `limit` seconds."""

    async def watch(app: web.Application):
        async def loop() -> None:
            idle = 0
            while idle < limit:
                await asyncio.sleep(1)
                idle = 0 if app[CLIENTS] else idle + 1
            log.info("no client for %ds: exiting", limit)
            signal.raise_signal(signal.SIGINT)

        task = asyncio.create_task(loop())
        yield
        task.cancel()

    return watch


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=8765)
    p.add_argument("--device", default="auto", choices=["auto", "cuda", "cpu"])
    p.add_argument("--asr-model", default="", help="faster-whisper model (default: distil-large-v3.5 on GPU, distil-small.en on CPU)")
    p.add_argument("--compute-type", default="", help="CTranslate2 compute type (default: int8_float16 on GPU, int8 on CPU)")
    p.add_argument("--mt", default=Config.mt_model, choices=list(MT_MODELS), help="translation model (default %(default)s)")
    p.add_argument("--mt-quant", default="Q4_K_M", help="GGUF quantization of the translation model")
    p.add_argument("--mt-threads", type=int, default=4)
    p.add_argument("--no-interim", action="store_true", help="translate finished sentences only (less load)")
    p.add_argument("--open-pause", type=float, default=Config.open_pause_s, metavar="SEC",
                   help="silence that closes a sentence the speaker has not finished (default %(default)s; 0.7 = as soon as any pause)")
    p.add_argument("--no-topics", action="store_true", help="do not generate the topic outline")
    p.add_argument("--exit-when-idle", type=int, default=0, metavar="SEC", help="quit after SEC seconds without a client (used by the Windows app)")
    p.add_argument("--debug", action="store_true", help="log every recognition pass")
    args = p.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(message)s", datefmt="%H:%M:%S")
    for noisy in ("faster_whisper", "httpx", "httpx2", "aiohttp.access"):
        logging.getLogger(noisy).setLevel(logging.WARNING)
    if args.debug:
        log.setLevel(logging.DEBUG)

    engine = Engine(
        Config(
            asr_model=args.asr_model,
            device=args.device,
            compute_type=args.compute_type,
            mt_model=args.mt,
            mt_quant=args.mt_quant,
            mt_threads=args.mt_threads,
            interim=not args.no_interim,
            open_pause_s=args.open_pause,
            topics=not args.no_topics,
        )
    )
    app = web.Application(middlewares=[local_only, no_cache])
    app[ENGINE] = engine
    app[CLIENTS] = set()
    app.on_shutdown.append(close_clients)
    app.on_cleanup.append(stop_workers)
    if args.exit_when_idle > 0:
        app.cleanup_ctx.append(exit_when_idle(args.exit_when_idle))
    app.router.add_get("/", index)
    app.router.add_get("/ws", ws_handler)
    app.router.add_static("/", WEB_DIR)
    log.info("ready: open http://localhost:%d in Chrome or Edge", args.port)
    web.run_app(app, host=args.host, port=args.port, print=None)


if __name__ == "__main__":
    main()
