#!/usr/bin/env python3
"""Smoke test for the tab-audio Chrome extension (extension/), run as:

    uv run tools/exttest.py

It drives a headless Chrome for Testing over the CDP pipe, plays a tone in two tabs,
clicks the toolbar button through Extensions.triggerAction and checks what arrives on
ws://127.0.0.1:8766/tab. It prints one ok/FAIL line per check and exits 0 only if all
of them pass.

Silent by design: every Chrome launch gets --headless=new and --disable-audio-output and
runs with PULSE_SERVER=/nonexistent, so nothing reaches the speakers. --mute-audio is NOT
used: it would zero the captured audio as well. No autoplay flag either; the tone is
started with a real synthetic mouse click.

It needs port 8766 free, because that is the only port the extension talks to; with --port
another one, it runs a copy of the extension that talks to that port instead, which leaves a
running LiveSubs app and the real Chrome alone.

Check 6 presses the button twice in a row and asks where the extension ends up. --gap sets
the ms between the two presses and --rounds how often to try. The clicks used to interleave,
and with --gap 30 the second press tore down the capture the first was still starting and
left the × badge behind with nothing captured, about 2 rounds in 10; the listener now runs
presses one after another.
"""

import argparse
import asyncio
import json
import os
import shutil
import sys
import tempfile
import time

import numpy as np
from aiohttp import web

HERE = os.path.dirname(os.path.abspath(__file__))
EXT_DIR = os.path.normpath(os.path.join(HERE, "..", "extension"))
EXT_ID = "jdedekchhjaibbiphpcolempoknocimf"
DEFAULT_CHROME = os.path.expanduser("~/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome")
PORT = 8766          # must match APP in extension/offscreen.js
REPLACED = 4001      # must match REPLACED in extension/offscreen.js and TabAudio.cs
SR = 16000           # the extension sends mono 16 kHz s16le
FRAME_BYTES = 3200   # 100 ms per message
ROUNDS = 5           # double-click rounds in check 6
GAP_MS = 0.0         # time between the two clicks of check 6; a real double click is 50-250 ms

TONE_PAGE = """<!doctype html><meta charset="utf-8"><title>tone</title>
<body style="font:16px sans-serif">
<button id="go" style="width:320px;height:160px;font-size:32px">START</button>
<script>
var hz = Number(new URLSearchParams(location.search).get('hz') || 1000), ctx = null;
document.getElementById('go').addEventListener('click', function () {
  if (ctx) return;                       // a real gesture, so the page may make sound
  ctx = new AudioContext();
  var o = ctx.createOscillator(), g = ctx.createGain();
  o.type = 'sine'; o.frequency.value = hz; g.gain.value = 0.25;
  o.connect(g); g.connect(ctx.destination); o.start(); ctx.resume();
  document.title = 'tone ' + hz;
});
window.probe = function () { return ctx ? {state: ctx.state, t: ctx.currentTime, hz: hz} : null; };
</script>"""

# Wraps the calls background.js makes, so the worker itself can say whether two clicks
# overlap. Installed only for the evidence run at the end.
PROBE_JS = r"""
(() => {
  globalThis.__xt = []; globalThis.__xtn = 0;
  const T = () => Math.round(performance.now() * 10) / 10;
  const got = [];
  const wrap = (obj, name, label) => {
    try {
      const orig = obj && obj[name];
      if (typeof orig !== 'function' || orig.__xt) return;
      const f = function (...a) {
        const n = ++globalThis.__xtn;
        globalThis.__xt.push([T(), 'enter', label, n]);
        let r;
        try { r = orig.apply(this, a); }
        catch (e) { globalThis.__xt.push([T(), 'throw ' + e, label, n]); throw e; }
        if (r && typeof r.then === 'function') {
          return r.then((v) => { globalThis.__xt.push([T(), 'exit', label, n]); return v; },
                        (e) => { globalThis.__xt.push([T(), 'reject ' + ((e && e.message) || e), label, n]); throw e; });
        }
        globalThis.__xt.push([T(), 'exit', label, n]);
        return r;
      };
      f.__xt = true; obj[name] = f; got.push(label);
    } catch (e) { /* not patchable */ }
  };
  wrap(chrome.runtime, 'getContexts', 'getContexts');
  wrap(chrome.tabCapture, 'getMediaStreamId', 'getMediaStreamId');
  wrap(chrome.offscreen, 'createDocument', 'createDocument');
  wrap(chrome.offscreen, 'closeDocument', 'closeDocument');
  wrap(chrome.runtime, 'sendMessage', 'sendMessage');
  ['toggle', 'capturedTab', 'stop', 'start', 'hasOffscreen'].forEach((n) => wrap(globalThis, n, 'fn:' + n));
  try {
    const sbt = chrome.action.setBadgeText;
    if (!sbt.__xt) {
      const f = function (o) { globalThis.__xt.push([T(), 'badge=' + JSON.stringify(o && o.text), 'setBadgeText', 0]); return sbt.call(chrome.action, o); };
      f.__xt = true; chrome.action.setBadgeText = f; got.push('setBadgeText');
    }
  } catch (e) { /* ignore */ }
  return got.join(' ');
})()
"""


# ---------------------------------------------------------------- CDP over the pipe

def _move_above(fd, floor=10):
    """Chrome wants the pipe on fd 3/4, so keep our own ends out of the way."""
    if fd >= floor:
        return fd
    for cand in range(floor, floor + 400):
        try:
            os.fstat(cand)
        except OSError:
            os.dup2(fd, cand)
            os.close(fd)
            os.set_inheritable(cand, True)
            return cand
    raise RuntimeError("no free file descriptor")


class Bus(asyncio.Protocol):
    """CDP messages, JSON terminated by a NUL byte: Chrome reads fd 3 and writes fd 4."""

    def __init__(self):
        self.buf = bytearray()
        self.seq = 0
        self.pending = {}
        self.tx = None
        self.dead = False

    def data_received(self, data):
        self.buf += data
        while True:
            i = self.buf.find(b"\0")
            if i < 0:
                return
            raw = bytes(self.buf[:i])
            del self.buf[: i + 1]
            try:
                self._dispatch(json.loads(raw))
            except ValueError:
                pass

    def connection_lost(self, exc):
        self.dead = True
        for fut in self.pending.values():
            if not fut.done():
                fut.set_exception(RuntimeError("the browser pipe closed"))
        self.pending.clear()

    def _dispatch(self, m):
        if "id" not in m:
            return                       # events are not needed; the checks poll
        fut = self.pending.pop(m["id"], None)
        if not fut or fut.done():
            return
        if "error" in m:
            fut.set_exception(RuntimeError(json.dumps(m["error"])))
        else:
            fut.set_result(m.get("result", {}))

    def post(self, method, params=None, session=None):
        """Queue a request and return its future without waiting for the answer."""
        if self.dead:
            raise RuntimeError("the browser pipe closed")
        self.seq += 1
        msg = {"id": self.seq, "method": method, "params": params or {}}
        if session:
            msg["sessionId"] = session
        fut = asyncio.get_running_loop().create_future()
        self.pending[self.seq] = fut
        self.tx.write(json.dumps(msg).encode() + b"\0")
        return fut

    async def send(self, method, params=None, session=None, timeout=20):
        return await asyncio.wait_for(self.post(method, params, session), timeout)


SILENT = ("--headless=new", "--disable-audio-output")


class Browser:
    def __init__(self, chrome, tmp, ext_dir=EXT_DIR):
        self.chrome = chrome
        self.tmp = tmp
        self.ext_dir = ext_dir
        self.pid = None
        self.bus = None

    async def launch(self):
        argv = [self.chrome, *SILENT, "--remote-debugging-pipe",
                "--enable-unsafe-extension-debugging",
                "--disable-extensions-except=" + self.ext_dir, "--load-extension=" + self.ext_dir,
                "--user-data-dir=" + os.path.join(self.tmp, "profile"),
                "--no-first-run", "--no-default-browser-check",
                "--password-store=basic", "--use-mock-keychain"]
        # Never make a sound, never open a window; refuse to start otherwise.
        for flag in SILENT:
            assert flag in argv, flag
        assert not any("mute-audio" in a or "autoplay" in a for a in argv)

        r_in, w_in = os.pipe()           # parent -> Chrome fd 3
        r_out, w_out = os.pipe()         # Chrome fd 4 -> parent
        r_in, w_in, r_out, w_out = (_move_above(fd) for fd in (r_in, w_in, r_out, w_out))
        log = os.open(os.path.join(self.tmp, "chrome.log"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC)
        null = os.open("/dev/null", os.O_RDONLY)
        env = dict(os.environ, PULSE_SERVER="/nonexistent")
        env.pop("DISPLAY", None)
        env.pop("WAYLAND_DISPLAY", None)
        self.pid = os.posix_spawn(self.chrome, argv, env, file_actions=[
            (os.POSIX_SPAWN_DUP2, null, 0), (os.POSIX_SPAWN_DUP2, log, 1),
            (os.POSIX_SPAWN_DUP2, log, 2),
            (os.POSIX_SPAWN_DUP2, r_in, 3), (os.POSIX_SPAWN_DUP2, w_out, 4)])
        for fd in (null, log, r_in, w_out):
            os.close(fd)
        loop = asyncio.get_running_loop()
        self.bus = Bus()
        await loop.connect_read_pipe(lambda: self.bus, os.fdopen(r_out, "rb", 0))
        tx, _ = await loop.connect_write_pipe(asyncio.BaseProtocol, os.fdopen(w_in, "wb", 0))
        self.bus.tx = tx
        return self.bus

    async def close(self):
        if self.pid is None:
            return
        try:
            await self.bus.send("Browser.close", timeout=5)
        except Exception:
            pass
        for _ in range(50):
            pid, _st = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.pid = None
                return
            await asyncio.sleep(0.1)
        try:
            os.kill(self.pid, 9)          # only ever our own pid
        except ProcessLookupError:
            pass
        os.waitpid(self.pid, 0)
        self.pid = None


# ---------------------------------------------------------------- the LiveSubs stand-in

class Conn:
    def __init__(self, origin):
        self.origin = origin
        self.opened = time.monotonic()
        self.closed = None
        self.msgs = []                   # (arrival time, payload)

    def recent(self, secs):
        cut = time.monotonic() - secs
        return [m for m in self.msgs if m[0] >= cut]


class App:
    """The receiver on /tab plus the tone pages, so the test pages are http."""

    def __init__(self, port):
        self.port = port
        self.conns = []
        self.runner = None

    async def start(self):
        app = web.Application()
        app.router.add_get("/tab", self._tab)
        app.router.add_get("/tone", lambda r: web.Response(text=TONE_PAGE, content_type="text/html"))
        self.runner = web.AppRunner(app, access_log=None)
        await self.runner.setup()
        await web.TCPSite(self.runner, "127.0.0.1", self.port).start()

    async def stop(self):
        if self.runner:
            await self.runner.cleanup()

    async def _tab(self, request):
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        conn = Conn(request.headers.get("Origin"))
        conn.ws = ws
        self.conns.append(conn)
        async for m in ws:
            if m.type == web.WSMsgType.BINARY:
                conn.msgs.append((time.monotonic(), m.data))
        conn.closed = time.monotonic()
        return ws

    def url(self, hz):
        return "http://127.0.0.1:%d/tone?hz=%d" % (self.port, hz)


def analyse(msgs, skip=10):
    """Frame sizes, message rate and the spectrum of a run of messages."""
    msgs = msgs[skip:]
    pcm = np.frombuffer(b"".join(d for _, d in msgs), "<i2").astype(np.float64) / 32768.0
    window = np.hanning(len(pcm))
    mag = np.abs(np.fft.rfft(pcm * window))
    freq = np.fft.rfftfreq(len(pcm), 1.0 / SR)
    k = int(np.argmax(mag[1:])) + 1
    peak = freq[k]
    if 1 <= k < len(mag) - 1:            # interpolate the top of the peak
        a, b, c = (float(np.log(mag[k + d] + 1e-30)) for d in (-1, 0, 1))
        den = a - 2 * b + c
        if den:
            peak += 0.5 * (a - c) / den * (freq[1] - freq[0])

    def band(hz, half=25.0):
        m = (freq >= hz - half) & (freq <= hz + half)
        return float(mag[m].max()) if m.any() else 0.0

    return {
        "n": len(msgs),
        "sizes": sorted({len(d) for _, d in msgs}),
        "secs": msgs[-1][0] - msgs[0][0],
        "rate": (len(msgs) - 1) / (msgs[-1][0] - msgs[0][0]),
        "peak": peak,
        "rms": float(np.sqrt((pcm ** 2).mean())),
        "band": band,
    }


# ---------------------------------------------------------------- the extension, seen from CDP

class Ext:
    def __init__(self, bus):
        self.bus = bus
        self.sw = None

    async def targets(self):
        r = await self.bus.send("Target.getTargets", {"filter": [{}]})
        return r["targetInfos"]

    async def sw_session(self, fresh=False):
        """The service worker target; it is re-resolved if the worker was replaced."""
        if self.sw and not fresh:
            try:
                await self.eval("1")
                return self.sw
            except Exception:
                self.sw = None
        for _ in range(100):
            t = next((t for t in await self.targets()
                      if t["type"] == "service_worker" and EXT_ID in t["url"]), None)
            if t:
                r = await self.bus.send("Target.attachToTarget",
                                        {"targetId": t["targetId"], "flatten": True})
                self.sw = r["sessionId"]
                # The target can be there a moment before the extension APIs are bound.
                for _ in range(60):
                    try:
                        if await self.eval("!!(globalThis.chrome && chrome.runtime && chrome.runtime.id)"):
                            return self.sw
                    except Exception:
                        pass
                    await asyncio.sleep(0.1)
                raise RuntimeError("the service worker never bound the chrome APIs")
            await asyncio.sleep(0.2)
        raise RuntimeError("the extension service worker never showed up")

    async def eval(self, expression, promise=False, session=None, gesture=False):
        r = await self.bus.send("Runtime.evaluate",
                                {"expression": expression, "awaitPromise": promise,
                                 "returnByValue": True, "userGesture": gesture}, session or self.sw)
        if r.get("exceptionDetails"):
            raise RuntimeError(json.dumps(r["exceptionDetails"])[:300])
        return r["result"].get("value")

    async def badge(self):
        await self.sw_session()
        # the badge is set per tab: the one that counts is the active tab's
        return await self.eval("chrome.tabs.query({active: true, lastFocusedWindow: true})"
                               ".then(t => chrome.action.getBadgeText(t.length ? {tabId: t[0].id} : {}))",
                               promise=True)

    async def contexts(self):
        await self.sw_session()
        txt = await self.eval(
            "chrome.runtime.getContexts({}).then(c => c.map(x => x.contextType).sort().join(','))",
            promise=True)
        return txt or ""

    async def offscreen(self):
        return "OFFSCREEN_DOCUMENT" in await self.contexts()

    async def tab_ids(self):
        return {t["targetId"] for t in await self.targets() if t["type"] == "tab"}

    async def open_tab(self, url):
        """Returns (page target, tab target). Both are needed: triggerAction wants the tab
        target, activateTarget and Input want the page."""
        before = await self.tab_ids()
        page = (await self.bus.send("Target.createTarget", {"url": url}))["targetId"]
        for _ in range(60):
            new = await self.tab_ids() - before
            if len(new) == 1:
                return page, new.pop()
            await asyncio.sleep(0.1)
        raise RuntimeError("no new tab target for " + url)

    async def start_tone(self, page):
        s = (await self.bus.send("Target.attachToTarget",
                                 {"targetId": page, "flatten": True}))["sessionId"]
        for _ in range(60):
            ready = await self.eval("typeof window.probe === 'function'", session=s)
            if ready:
                break
            await asyncio.sleep(0.1)
        box = json.loads(await self.eval(
            "JSON.stringify(document.getElementById('go').getBoundingClientRect())", session=s))
        x, y = box["x"] + box["width"] / 2, box["y"] + box["height"] / 2
        for kind, buttons in (("mousePressed", 1), ("mouseReleased", 0)):
            await self.bus.send("Input.dispatchMouseEvent",
                                {"type": kind, "x": x, "y": y, "button": "left",
                                 "clickCount": 1, "buttons": buttons}, s)
        for _ in range(50):               # the audio clock must really be running
            probe = await self.eval("JSON.stringify(window.probe())", session=s)
            if probe and json.loads(probe)["t"] > 0.2:
                return json.loads(probe)
            await asyncio.sleep(0.1)
        raise RuntimeError("the page never started its oscillator (no audio clock)")

    async def activate(self, page, tab):
        """triggerAction fires onClicked for the active tab whatever targetId says, so the
        wanted tab has to be the active one first."""
        await self.bus.send("Target.activateTarget", {"targetId": page})
        for _ in range(40):
            t = next((t for t in await self.targets() if t["targetId"] == tab), None)
            if t and t.get("embedderData", {}).get("tabActive"):
                return
            await asyncio.sleep(0.1)
        raise RuntimeError("tab %s never became active" % tab)

    async def click(self, page, tab):
        """The toolbar button: a real invocation, which is what grants tab capture."""
        await self.activate(page, tab)
        await self.bus.send("Extensions.triggerAction", {"id": EXT_ID, "targetId": tab})

    async def until(self, want, secs=10.0):
        """Wait for a badge text; returns the last one seen."""
        end = time.monotonic() + secs
        text = await self.badge()
        while text != want and time.monotonic() < end:
            await asyncio.sleep(0.2)
            text = await self.badge()
        return text


# ---------------------------------------------------------------- the checks

class Report:
    def __init__(self):
        self.failed = 0

    def check(self, ok, text):
        print(("ok   " if ok else "FAIL ") + text, flush=True)
        if not ok:
            self.failed += 1
        return ok

    def info(self, text):
        print("     " + text, flush=True)


async def wait_frames(app, index, want, secs):
    """Wait until connection `index` has `want` messages."""
    end = time.monotonic() + secs
    while time.monotonic() < end:
        if len(app.conns) > index and len(app.conns[index].msgs) >= want:
            return True
        await asyncio.sleep(0.2)
    return False


def tone_line(label, a, hz, other, min_secs=8.0):
    rel = 20 * np.log10(max(a["band"](other), 1e-30) / max(a["band"](hz), 1e-30))
    ok = (a["sizes"] == [FRAME_BYTES] and a["secs"] >= min_secs - 0.05
          and abs(a["rate"] - 10.0) <= 0.3 and abs(a["peak"] - hz) <= 5.0 and rel <= -40.0)
    return ok, ("%s: %d msgs over %.1f s, sizes=%s, %.3f/s, peak %.1f Hz, %d Hz at %.1f dB"
                % (label, a["n"], a["secs"], a["sizes"], a["rate"], a["peak"], other, rel))


async def run(app, ext, rep, rounds=ROUNDS, gap_ms=GAP_MS):
    # 1. the extension is there
    sw = await ext.sw_session()
    rid = await ext.eval("chrome.runtime.id")
    ctxs = await ext.contexts()
    rep.check(rid == EXT_ID and sw is not None and "BACKGROUND" in ctxs,
              "1 extension loaded: id=%s, service worker attached, contexts=%s" % (rid, ctxs))

    page_a, tab_a = await ext.open_tab(app.url(1000))
    page_b, tab_b = await ext.open_tab(app.url(3000))
    pa = await ext.start_tone(page_a)
    pb = await ext.start_tone(page_b)
    rep.info("tab A %d Hz (%s), tab B %d Hz (%s) both playing"
             % (pa["hz"], pa["state"], pb["hz"], pb["state"]))

    # 2. click on A: only A's audio, at the right size and rate
    await ext.click(page_a, tab_a)
    badge = await ext.until("ON", 12)
    got = await wait_frames(app, 0, 95, 20)
    ok_conn = len(app.conns) == 1 and app.conns[0].origin == "chrome-extension://" + EXT_ID
    rep.check(badge == "ON" and ok_conn and got,
              "2a badge=%r, %d connection(s), Origin=%s, %d frames"
              % (badge, len(app.conns), app.conns[0].origin if app.conns else None,
                 len(app.conns[0].msgs) if app.conns else 0))
    if app.conns and len(app.conns[0].msgs) > 20:
        a = analyse(app.conns[0].msgs[:95])
        rep.check(*tone_line("2b tab A audio", a, 1000, 3000))
    else:
        rep.check(False, "2b tab A audio: no frames to analyse")

    # 2b'. the button says ON on the captured tab only: on another tab a press would start that one
    await ext.activate(page_b, tab_b)
    other = await ext.badge()
    await ext.activate(page_a, tab_a)
    own = await ext.badge()
    rep.check(other == "" and own == "ON", "2b' badge on another tab %r, on the captured tab %r" % (other, own))
    # 2c. full screen in the captured tab: Chrome keeps it inside the tab, so the extension makes
    # the window full screen with it, and puts the window back when the page leaves full screen
    rep.check(*await fullscreen_follows(ext, page_a))
    # 2d. the same after the captured page was loaded again (the capture goes on)
    rep.check(*await fullscreen_follows(ext, page_a, reload=True))
    # 2e. the capture stops while the page is full screen: the window is put back all the same
    rep.check(*await fullscreen_then_stop(ext, page_a, tab_a))
    # 3. click on B: the capture switches over
    await ext.click(page_b, tab_b)
    badge = await ext.until("ON", 12)
    got = await wait_frames(app, 1, 50, 20)
    if got:
        a = analyse(app.conns[1].msgs[:50])
        ok, line = tone_line("3 switch to tab B", a, 3000, 1000, min_secs=3.0)
        rep.check(ok and badge == "ON" and len(app.conns) == 2,
                  line + ", badge=%r, connections=%d" % (badge, len(app.conns)))
    else:
        rep.check(False, "3 switch to tab B: no new connection with frames (connections=%d)"
                  % len(app.conns))

    # 4. click on B again: everything stops
    before = len(app.conns)
    await ext.click(page_b, tab_b)
    badge = await ext.until("", 8)
    await asyncio.sleep(0.5)
    off = await ext.offscreen()
    closed = app.conns[-1].closed is not None if app.conns else False
    rep.check(badge == "" and not off and closed and len(app.conns) == before,
              "4 second click on B: badge=%r, offscreen=%s, socket closed=%s, connections=%d"
              % (badge, off, closed, len(app.conns)))

    # 5. a page that cannot be captured
    page_c, tab_c = await ext.open_tab("chrome://version")
    before = len(app.conns)
    await ext.click(page_c, tab_c)
    badge = await ext.until("×", 8)
    await asyncio.sleep(1.0)
    off = await ext.offscreen()
    rep.check(badge == "×" and not off and len(app.conns) == before,
              "5 click on chrome://version: badge=%r, offscreen=%s, new connections=%d"
              % (badge, off, len(app.conns) - before))

    # 5b. the app says another tab has taken this one's place: let go, do not come back
    await ext.click(page_a, tab_a)
    badge = await ext.until("ON", 8)
    await wait_frames(app, len(app.conns) - 1, 5, 5)
    before = len(app.conns)
    await app.conns[-1].ws.close(code=REPLACED, message=b"replaced")
    gone = await ext.until("", 8)
    await asyncio.sleep(3.0)          # longer than the extension's reconnect delay
    off = await ext.offscreen()
    rep.check(badge == "ON" and gone == "" and not off and len(app.conns) == before,
              "5b replaced by the app (close code %d): badge %r -> %r, offscreen=%s, reconnections=%d"
              % (REPLACED, badge, gone, off, len(app.conns) - before))

    # 6. two clicks in a row, without waiting for the first one to settle
    states = []
    for i in range(rounds):
        await make_idle(ext, page_a, tab_a)
        before = len(app.conns)
        await ext.activate(page_a, tab_a)
        f1 = ext.bus.post("Extensions.triggerAction", {"id": EXT_ID, "targetId": tab_a})
        if gap_ms:
            await asyncio.sleep(gap_ms / 1000.0)
        f2 = ext.bus.post("Extensions.triggerAction", {"id": EXT_ID, "targetId": tab_a})
        await asyncio.sleep(3.0)
        errs = []
        for f in (f1, f2):
            try:
                await asyncio.wait_for(f, 5)
            except Exception as e:
                errs.append(str(e)[:60])
        badge = await ext.badge()
        off = await ext.offscreen()
        fresh = app.conns[-1] if len(app.conns) > before else None
        flowing = bool(fresh and fresh.closed is None and len(fresh.recent(1.5)) >= 8)
        if badge == "" and not off:
            verdict, ok = "nothing captured", True
        elif badge == "ON" and off and flowing:
            verdict, ok = "tab A captured", True
        else:
            verdict, ok = "BAD", False
        states.append((verdict, ok))
        rep.info("6 round %d: %s (badge=%r offscreen=%s frames/1.5s=%d%s)"
                 % (i + 1, verdict, badge, off,
                    len(fresh.recent(1.5)) if fresh else 0,
                    (" triggerAction errors=" + ";".join(errs)) if errs else ""))
    good = sum(1 for _v, ok in states if ok)
    rep.check(good == rounds, "6 double click (%.0f ms apart) %d/%d rounds ended well: %s"
              % (gap_ms, good, rounds, ", ".join(v for v, _ in states)))
    await make_idle(ext, page_a, tab_a)
    return page_a, tab_a


async def fullscreen_follows(ext, page, reload=False):
    s = (await ext.bus.send("Target.attachToTarget", {"targetId": page, "flatten": True}))["sessionId"]
    if reload:
        await ext.bus.send("Page.reload", {}, s)
        for _ in range(60):
            await asyncio.sleep(0.25)
            try:
                if await ext.eval("document.readyState", session=s) == "complete":
                    break
            except Exception:
                pass
        await asyncio.sleep(1.0)   # the extension hears of the load a moment later
    state = ("chrome.tabs.query({active: true, lastFocusedWindow: true})"
             ".then(t => chrome.windows.get(t[0].windowId)).then(w => w.state)")

    async def window_becomes(want, secs=6.0):
        end = time.monotonic() + secs
        while True:
            await ext.sw_session()
            got = await ext.eval(state, promise=True)
            if got == want or time.monotonic() > end:
                return got
            await asyncio.sleep(0.2)

    await ext.sw_session()
    before = await ext.eval(state, promise=True)
    await ext.eval("document.documentElement.requestFullscreen().then(() => true)",
                   promise=True, session=s, gesture=True)
    during = await window_becomes("fullscreen")
    page_full = await ext.eval("!!document.fullscreenElement", session=s)
    await ext.eval("document.exitFullscreen().then(() => true)", promise=True, session=s)
    after = await window_becomes(before)
    return (during == "fullscreen" and page_full and after == before,
            "%s full screen in the captured tab%s: window %s -> %s (page still full screen: %s) -> %s"
            % ("2d" if reload else "2c", " after a reload" if reload else "", before, during, page_full, after))


async def fullscreen_then_stop(ext, page, tab):
    s = (await ext.bus.send("Target.attachToTarget", {"targetId": page, "flatten": True}))["sessionId"]
    state = ("chrome.tabs.query({active: true, lastFocusedWindow: true})"
             ".then(t => chrome.windows.get(t[0].windowId)).then(w => w.state)")

    async def window_becomes(want, secs=6.0):
        end = time.monotonic() + secs
        while True:
            await ext.sw_session()
            got = await ext.eval(state, promise=True)
            if got == want or time.monotonic() > end:
                return got
            await asyncio.sleep(0.2)

    await ext.sw_session()
    before = await ext.eval(state, promise=True)
    await ext.eval("document.documentElement.requestFullscreen().then(() => true)",
                   promise=True, session=s, gesture=True)
    during = await window_becomes("fullscreen")
    await ext.click(page, tab)                       # the second press on the captured tab stops
    badge = await ext.until("", 8)
    after = await window_becomes(before)
    if await ext.eval("!!document.fullscreenElement", session=s):
        await ext.eval("document.exitFullscreen().then(() => true)", promise=True, session=s)
    return (during == "fullscreen" and badge == "" and after == before,
            "2e capture stopped while full screen: window %s -> %s -> %s, badge %r"
            % (before, during, after, badge))


async def make_idle(ext, page_a, tab_a):
    """Leave the extension with nothing captured and a clear badge."""
    if await ext.offscreen():
        await ext.click(page_a, tab_a)
        for _ in range(40):
            if not await ext.offscreen():
                break
            await asyncio.sleep(0.2)
    if await ext.badge() != "":
        # Not a capture state, only a leftover badge from the previous round.
        await ext.eval("chrome.action.setBadgeText({text: ''})")


async def overlap_evidence(ext, page_a, tab_a, gap_ms=GAP_MS):
    """Do the two triggerAction calls really interleave inside the worker?"""
    await ext.sw_session()
    installed = await ext.eval(PROBE_JS)
    await ext.eval("globalThis.__xt = []")
    await ext.activate(page_a, tab_a)
    ext.bus.post("Extensions.triggerAction", {"id": EXT_ID, "targetId": tab_a})
    if gap_ms:
        await asyncio.sleep(gap_ms / 1000.0)
    ext.bus.post("Extensions.triggerAction", {"id": EXT_ID, "targetId": tab_a})
    await asyncio.sleep(3.0)
    log = json.loads(await ext.eval("JSON.stringify(globalThis.__xt)") or "[]")
    print("\n-- worker timeline for two clicks %.0f ms apart (wrapped: %s)"
          % (gap_ms, installed), flush=True)
    live = {}
    # Nothing in background.js recurses, so two calls of the same kind in flight at once
    # can only mean two click handlers running into each other.
    doubled = set()
    for t, phase, label, n in log:
        if phase == "enter":
            if label in live.values():
                doubled.add(label)
            live[n] = label
            others = [live[k] for k in live if k != n]
            print("   %8.1f ms  enter %-18s #%-3d%s"
                  % (t, label, n, ("   <- while " + ", ".join(others)) if others else ""), flush=True)
        else:
            live.pop(n, None)
            print("   %8.1f ms  %-5s %-18s #%-3d" % (t, phase.split(" ")[0], label, n), flush=True)
            if phase.startswith(("reject", "throw")):
                print("              %s" % phase, flush=True)
    if doubled:
        print("   verdict: the two clicks DO overlap in the worker: two of %s in flight at once"
              % ", ".join(sorted(doubled)), flush=True)
    else:
        print("   verdict: the clicks did NOT overlap; no two calls of the same kind were ever "
              "in flight at once (nesting in the listing is one click's own call tree)", flush=True)
    return bool(doubled)


def extension_for(port, tmp):
    """A copy of the extension that talks to another port (same key, so the same id)."""
    ext_dir = os.path.join(tmp, "extension")
    shutil.copytree(EXT_DIR, ext_dir)
    js = os.path.join(ext_dir, "offscreen.js")
    with open(js, encoding="utf-8") as f:
        code = f.read()
    if "const PORT = %d;" % PORT not in code:
        return None
    with open(js, "w", encoding="utf-8") as f:
        f.write(code.replace("const PORT = %d;" % PORT, "const PORT = %d;" % port))
    return ext_dir


async def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--chrome", default=DEFAULT_CHROME, help="Chrome for Testing binary")
    ap.add_argument("--port", type=int, default=PORT, help="must match the port in offscreen.js")
    ap.add_argument("--tmp", default=None, help="where to put the throw-away profile")
    ap.add_argument("--keep", action="store_true", help="keep the profile and the Chrome log")
    ap.add_argument("--force", action="store_true", help="run even though the LiveSubs app is running")
    ap.add_argument("--rounds", type=int, default=ROUNDS, help="double-click rounds in check 6")
    ap.add_argument("--gap", type=float, default=GAP_MS,
                    help="ms between the two clicks of check 6 (0 = back to back)")
    args = ap.parse_args()

    if not os.path.exists(args.chrome):
        print("no Chrome for Testing at %s\n"
              "get one with:  npx -y playwright@latest install chromium\n"
              "or point --chrome at a Chrome/Chromium binary of your own." % args.chrome)
        return 2
    if not os.path.isdir(EXT_DIR):
        print("no extension at %s" % EXT_DIR)
        return 2
    # WSL forwards this port to Windows localhost: with the app running, the extension in the
    # real Chrome would send its tab to this test instead, or the app would find its port taken.
    tasklist = "/mnt/c/Windows/System32/tasklist.exe"
    if os.path.exists(tasklist) and not args.force and args.port == PORT:
        import subprocess
        out = subprocess.run([tasklist, "/FI", "IMAGENAME eq LiveSubs.exe"], capture_output=True).stdout
        if b"LiveSubs.exe" in out:
            print("LiveSubs.exe is running; close it first (or pass --force): this test listens on the port it uses.")
            return 2

    app = App(args.port)
    try:
        await app.start()
    except OSError as e:
        print("cannot listen on 127.0.0.1:%d (%s).\n"
              "The extension only talks to that port, so stop whatever holds it "
              "(the LiveSubs app, most likely) and run this again." % (args.port, e))
        return 2

    tmp = tempfile.mkdtemp(prefix="exttest-", dir=args.tmp)
    ext_dir = EXT_DIR if args.port == PORT else extension_for(args.port, tmp)
    if ext_dir is None:
        print("offscreen.js does not say const PORT = %d; cannot point a copy elsewhere" % PORT)
        return 2
    browser = Browser(args.chrome, tmp, ext_dir)
    rep = Report()
    t0 = time.monotonic()
    try:
        bus = await browser.launch()
        ext = Ext(bus)
        try:
            page_a, tab_a = await run(app, ext, rep, args.rounds, args.gap)
            await overlap_evidence(ext, page_a, tab_a, args.gap)
        except Exception as e:
            rep.check(False, "the run stopped early: %r" % (e,))
    finally:
        await browser.close()
        await app.stop()
        if args.keep:
            print("\nprofile and Chrome log kept in %s" % tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    print("\n%s in %.0f s" % ("all checks passed" if not rep.failed
                              else "%d check(s) FAILED" % rep.failed, time.monotonic() - t0))
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
