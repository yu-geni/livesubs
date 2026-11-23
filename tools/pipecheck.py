"""Checks the pipeline's own logic, without the models (and without a GPU).

    uv run tools/pipecheck.py [--vad PCM]

The sentence-end rule, the reset handling, the interim-translation throttle and the audio
backlog are driven with a stub engine. With --vad (a 16 kHz mono s16le file) the incremental
VAD is replayed against a full pass over the whole buffer on real audio, tick by tick: both
must report the same speech edges, since the rest of the pipeline decides everything from them.
"""

from __future__ import annotations

import argparse
import logging
import sys
import threading
import time
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import pipeline
from pipeline import KEEP, SR, TICK, Config, Session, _ends_with

fails: list[str] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    print(f"{'ok  ' if ok else 'FAIL'} {name}{'  ' + detail if detail else ''}")
    if not ok:
        fails.append(name)


class StubEngine:
    """What Session uses of an Engine: two models that answer instantly."""

    def __init__(self, cfg: Config, spans=None, words=None):
        self.cfg = cfg
        self.device = "cpu"
        self.topic_slot = threading.Semaphore(1)
        self.mt: list[str] = []  # every translate() call, in order
        self._spans = spans or (lambda audio: [])
        self._words = words or (lambda audio, prompt: [])

    def translate(self, text: str) -> str:
        self.mt.append(text)
        return f"<{text}>"

    def transcribe(self, audio, prompt):
        return self._words(audio, prompt)

    def speech_spans(self, audio):
        return self._spans(audio)

    def hold_topic_proc(self, proc, live: bool) -> None:
        pass


def session(engine: StubEngine) -> tuple[Session, list[dict]]:
    """A session whose own thread has gone, so a test can call _tick() itself."""
    out: list[dict] = []
    s = Session(engine, out.append)
    s._closed = True
    s._wake.set()
    time.sleep(0.05)
    s._closed = False
    s.set_topics(True)  # as the Windows app asks on connecting; see check_topics_asked_for
    return s, out


# --- 5. a word ending in "." is not always the end of a sentence -------------------------------


def check_sentence_ends() -> None:
    END = pipeline.SENT_END
    for word in ("year.", "Tolkien.", "done!", "really?", '"Hello."', "No.5.", "hospital.)"):
        check(f"ends a sentence: {word!r}", _ends_with(word, END))
    for word in (
        " U.S.", "u.s.", "U.S.A.", "e.g.", "i.e.", "a.m.", "J.", " R.", "No.", "Prof.", "Sgt.", "Mt.", "St.",
        "Jan.", "Dec.", "vs.", "etc.", "Dr.", "Mrs.", "sept.", "year...", "(No.", '"St.',
    ):
        check(f"does not end a sentence: {word!r}", not _ends_with(word, END))
    check("May. is still an end (the month is ambiguous)", _ends_with("May.", END))
    check("a comma is still a soft end", _ends_with("later,", pipeline.SENT_END + pipeline.SOFT_END))
    check("an abbreviation is not even a soft end", not _ends_with("No.", pipeline.SENT_END + pipeline.SOFT_END))


# --- 7. two resets in one tick: "new" wins and stays ------------------------------------------


def check_game_words() -> None:
    """The translator's music / theatre / playback / atlas readings put right on a game stream,
    and left alone where the English is about those."""
    g = pipeline.game_words
    for en, ja, want in [
        ("I played it yesterday and it was great.", "昨日それを演奏して、とても良かった。", "昨日それをプレイして、とても良かった。"),
        ("He plays support.", "彼はサポートとして演奏します。", "彼はサポートとしてプレイします。"),
        ("That was a great play.", "それは素晴らしい演劇だった。", "それは素晴らしいプレーだった。"),
        ("We're going to play the new map.", "新しい地図を再生しよう。", "新しいマップをプレイしよう。"),
        ("Who's the best player here?", "ここで一番の演奏者は誰？", "ここで一番のプレイヤーは誰？"),
        ("Let's play a song for chat.", "歌を一曲演奏しましょう。", "歌を一曲演奏しましょう。"),
        ("She plays the piano beautifully.", "彼女はピアノを美しく演奏します。", "彼女はピアノを美しく演奏します。"),
        ("Watch the replay of that clip.", "そのクリップの再生を見て。", "そのクリップの再生を見て。"),
        ("Find the treasure map.", "宝の地図を探して。", "宝の地図を探して。"),
        ("Check the map, the zone is closing.", "地図を確認して。", "マップを確認して。"),
        ("The music is great.", "音楽が最高。", "音楽が最高。"),
    ]:
        got = g(en, ja)
        check(f"game sense: {en}", got == want, f"({got})")


def check_sticky_reset() -> None:
    eng = StubEngine(Config())
    s, _ = session(eng)
    s.reset(new_stream=True)
    s.reset()  # the client sends a plain reset on connect, right after switching streams
    check("new_stream survives a plain reset", s._reset_req == "new", f"({s._reset_req})")
    s.reset()
    s.reset(new_stream=True)
    check("a plain reset is upgraded", s._reset_req == "new", f"({s._reset_req})")
    s.seg_id, s._topic_buf = 7, ["something old"]
    s._tick()
    check("the new stream was handled", (s.seg_id, s._topic_buf) == (8, []), f"({s.seg_id}, {s._topic_buf})")
    check("and only once", s._reset_req == "")
    # an outline entry still being written belongs to the stream that was left
    stopped: list = []
    was, pipeline.topics.stop = pipeline.topics.stop, stopped.append
    try:
        s._topic_proc = "worker"
        s.reset(new_stream=True)
        s._tick()
        check("a new stream ends the outline worker", stopped == ["worker"], str(stopped))
        stopped.clear()
        s.reset()
        s._tick()
        check("...a plain reset leaves it alone", stopped == [], str(stopped))
    finally:
        pipeline.topics.stop = was
        s._topic_proc = None
    s.reset()
    s.reset()
    check("two plain resets stay plain", s._reset_req == "soft", f"({s._reset_req})")


# --- 1. a plain reset finalizes the open line, a new stream drops it ---------------------------


def check_soft_reset_finalizes() -> None:
    eng = StubEngine(Config())
    s, out = session(eng)
    s._interim = ("The queue just popped.", "<The queue just popped.>")
    s._mt_done = ("The queue just popped.", time.monotonic())
    s.reset()
    s._tick()
    finals = [ev for ev in out if ev["type"] == "final"]
    check("a plain reset commits the open line", len(finals) == 1 and finals[0]["en"] == "The queue just popped.", str(finals))
    check("...with the translation it already had", finals and finals[0]["ja"] == "<The queue just popped.>" and eng.mt == [])
    check("...and it is gone from the screen state", s._interim is None)
    check("...and the outline heard about it", s._topic_buf == ["The queue just popped."], str(s._topic_buf))

    eng2 = StubEngine(Config())
    s2, out2 = session(eng2)
    s2._interim = ("Half a sentence", "")  # never translated (interim off, say)
    s2._mt_done = ("", 0.0)
    s2.reset()
    s2._tick()
    finals2 = [ev for ev in out2 if ev["type"] == "final"]
    check("a stale interim is translated at the commit", eng2.mt == ["Half a sentence"], str(eng2.mt))
    check("...and committed with the fresh translation", finals2 and finals2[0]["ja"] == "<Half a sentence>")

    eng3 = StubEngine(Config())
    s3, out3 = session(eng3)
    s3._interim = ("Half a sentence", "<Half a sentence>")
    s3._mt_done = ("Half a sentence", time.monotonic())
    s3.reset(new_stream=True)
    s3._tick()
    check("a new stream still drops the open line", not [ev for ev in out3 if ev["type"] == "final"], str(out3))


# --- 3. the unfinished line is not translated again at every step ------------------------------


def check_interim_throttle() -> None:
    eng = StubEngine(Config())
    s, _ = session(eng)
    steps = [
        "Okay,",                                                   # first one: translated
        "Okay, the queue",                                         # +10 chars: kept
        "Okay, the queue just popped,",                            # +23 since the translation: new
        "Okay, the queue just popped, let's",                      # +6: kept
        "Okay, the queue just popped, let's go, wish me luck.",     # +24: new
    ]
    shown = []
    for en in steps:
        ja = s._interim_ja(en, ended=False)
        s._interim = (en, ja)
        shown.append(ja)
    check("only the growth steps were translated", eng.mt == [steps[0], steps[2], steps[4]], str(eng.mt))
    check("in between the Japanese on screen is kept", shown[1] == shown[0] and shown[3] == shown[2])
    check("the commit would translate a kept line afresh", s._mt_done[0] == steps[4])

    s._mt_done = (steps[4], time.monotonic() - pipeline.MT_AGE - 0.1)
    s._interim_ja(steps[4] + ".", ended=False)
    check("a translation older than MT_AGE is redone", eng.mt[-1] == steps[4] + ".", str(eng.mt[-1]))

    s._mt_done = (steps[4], time.monotonic())
    s._interim = (steps[4], "<kept>")
    before = len(eng.mt)
    s._interim_ja(steps[4] + " Right.", ended=True)
    check("a line the speaker has finished is translated at once", len(eng.mt) == before + 1, str(eng.mt[-1]))

    off = StubEngine(Config(interim=False))
    s_off, _ = session(off)
    check("nothing is translated with --no-interim", s_off._interim_ja("Hello there", ended=True) == "" and off.mt == [])


# --- 4. the audio backlog is capped ------------------------------------------------------------


def check_pending_cap(caplog: list[str]) -> None:
    eng = StubEngine(Config())
    s, out = session(eng)
    frame = b"\0" * 3200  # 100 ms
    for _ in range(600):  # a minute of audio for a thread that never wakes
        s.feed(frame)
    held = sum(len(c) for c in s._pending)
    check("the backlog is capped", held <= pipeline.PENDING_MAX, f"({held / (2 * SR):.1f}s held)")
    check("...and it is logged", any("not keeping up" in m for m in caplog), str(caplog[-1:]))
    told = [ev for ev in out if ev.get("type") == "behind"]
    check("...and the client is told", bool(told) and told[0]["dropped_s"] > 0, str(told[:1]))
    check("...once in a while, not per frame", len(told) <= 2, f"({len(told)} times)")


# --- 4. the thread survives a failure it cannot report ----------------------------------------


def check_thread_survives() -> None:
    eng = StubEngine(Config())
    out: list[dict] = []

    def emit(ev):
        raise RuntimeError("event loop is closed")

    s = Session(eng, emit)
    s._tick = lambda: (_ for _ in ()).throw(ValueError("bad chunk"))
    s._wake.set()
    time.sleep(0.2)
    alive = [t for t in threading.enumerate() if t.name.startswith("Thread") and t.is_alive()]
    s.feed(b"\0" * 3200)
    time.sleep(0.2)
    check("the thread is still there after an unreportable error", any(t.is_alive() for t in alive), f"({len(alive)} threads)")
    s._tick = lambda: None
    s.close()


# --- 9. the music escape does not leave a fragment behind -------------------------------------


def check_noise_escape() -> None:
    cfg = Config()
    eng = StubEngine(cfg, spans=lambda audio: [{"start": 0, "end": len(audio)}])  # the VAD hears speech everywhere
    s, out = session(eng)
    s._interim = ("some lyrics", "<some lyrics>")
    s._mt_done = ("some lyrics", time.monotonic())
    s.buf = np.zeros(int(1.6 * cfg.max_s * SR), dtype=np.float32)
    s._tick()
    check("the stale fragment is dropped with the audio", s._interim is None)
    check("...and was not committed as a line", not [ev for ev in out if ev["type"] == "final"], str(out))


# --- 2. one outline worker at a time ----------------------------------------------------------


def check_topic_slot() -> None:
    eng = StubEngine(Config())
    a, _ = session(eng)
    b, _ = session(eng)
    for s in (a, b):
        s._topic_buf = ["x" * pipeline.TOPIC_CHARS]
        s._topic_seg = 0
    started: list[str] = []
    a._topic_job = lambda text, seg: started.append("a")
    b._topic_job = lambda text, seg: started.append("b")
    a._maybe_topic()
    b._maybe_topic()
    time.sleep(0.2)
    check("the second session does not start a worker", started == ["a"], str(started))
    check("...and keeps its text for the next entry", b._topic_buf and not a._topic_buf)
    eng.topic_slot.release()
    b._maybe_topic()
    time.sleep(0.2)
    check("it gets its turn once the slot is free", started == ["a", "b"], str(started))


# --- 7. no outline for a client that has not asked for one -------------------------------------


def check_topics_asked_for() -> None:
    eng = StubEngine(Config())
    s = Session(eng, lambda ev: None)  # as the browser page and tools/feed.py connect: no "config"
    s.close()
    started: list[str] = []
    s._topic_job = lambda text, seg: started.append("x")
    s._note_topic(0, "x" * pipeline.TOPIC_CHARS)
    time.sleep(0.1)
    check("a client that did not ask gets no outline", started == [] and s._topic_buf == [], f"{started} {len(s._topic_buf)}")
    s.set_topics(True)
    s._note_topic(1, "x" * pipeline.TOPIC_CHARS)
    time.sleep(0.2)
    check("...and one that asks does", started == ["x"], str(started))
    eng.topic_slot.release()


# --- 6. the incremental VAD sees what a full pass sees -----------------------------------------


def check_vad(pcm_path: Path) -> None:
    from faster_whisper.vad import VadOptions, get_speech_timestamps

    opts = VadOptions(threshold=0.5, min_speech_duration_ms=150, min_silence_duration_ms=200, speech_pad_ms=0)
    audio = np.frombuffer(pcm_path.read_bytes(), dtype="<i2").astype(np.float32) / 32768.0
    eng = StubEngine(Config(), spans=lambda a: get_speech_timestamps(a, opts))
    s, _ = session(eng)

    def decisions(edges, n):
        """What _tick() makes of the edges: pause (closes a sentence), stop (decode now), extent."""
        if edges is None:
            return None
        start, end = edges
        return (n - end) >= Config.pause_s * SR, (n - end) >= KEEP, (end - start) < 0.3 * SR

    full_samples = inc_samples = ticks = pauses = 0
    edge_diffs, pause_diffs, stop_diffs, shift = [], [], 0, 0
    pos = 0
    while pos < len(audio):
        s.buf = np.concatenate([s.buf, audio[pos : pos + TICK]])
        pos += TICK
        n = len(s.buf)
        spans = get_speech_timestamps(s.buf, opts)  # what the old code did: the whole buffer, every tick
        want = (spans[0]["start"], spans[-1]["end"]) if spans else None
        before = s._vad_scanned
        got = s._speech_edges(n)
        full_samples += n
        inc_samples += n - max(0, before - pipeline.VAD_OVERLAP) // 512 * 512 if n > before else 0
        ticks += 1
        if got != want:
            edge_diffs.append(f"at {n / SR:5.2f}s full={want} incremental={got}")
            if want and got:
                shift = max(shift, abs(want[1] - got[1]))
        d_want, d_got = decisions(want, n), decisions(got, n)
        if d_want != d_got:
            if d_want is None or d_got is None or d_want[0] != d_got[0] or d_want[2] != d_got[2]:
                pause_diffs.append(f"at {n / SR:5.2f}s full={d_want} incremental={d_got}")
            else:
                stop_diffs += 1
        pauses += bool(d_want and d_want[0])
        # the drops the pipeline makes, so both go on from the same buffer
        if want is None:
            s._drop_front(n - KEEP)
        elif (n - want[1]) >= Config.pause_s * SR:  # a pause: the sentence is committed and cut away
            s._drop_front(max(0, want[1] - KEEP))

    check(f"the same sentence cuts on all {ticks} ticks ({pauses} of them a pause)", not pause_diffs,
          "\n     " + "\n     ".join(pause_diffs[:10]) if pause_diffs else "")
    print(f"     {len(edge_diffs)} ticks put the last speech end up to {shift / SR * 1000:.0f} ms elsewhere"
          f" (the VAD's own state starts cold on a partial pass); on {stop_diffs} of them the"
          f" \"speech has just stopped\" hint comes one tick later")
    print(f"     audio through the VAD: {full_samples / SR:.0f}s for the full passes,"
          f" {inc_samples / SR:.0f}s incremental ({100 * inc_samples / full_samples:.0f}%)")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--vad", metavar="PCM", help="also replay the incremental VAD against this 16 kHz s16le file")
    args = p.parse_args()
    caplog: list[str] = []
    # Collected instead of printed: one check makes the pipeline log a failure on purpose.
    pipeline.log.addHandler(type("H", (logging.Handler,), {"emit": lambda self, r: caplog.append(r.getMessage())})())
    pipeline.log.propagate = False
    logging.basicConfig(level=logging.WARNING)

    check_sentence_ends()
    check_game_words()
    check_sticky_reset()
    check_soft_reset_finalizes()
    check_interim_throttle()
    check_pending_cap(caplog)
    check_thread_survives()
    check_noise_escape()
    check_topic_slot()
    check_topics_asked_for()
    if args.vad:
        check_vad(Path(args.vad))

    print(f"\n{'all checks passed' if not fails else str(len(fails)) + ' FAILED: ' + ', '.join(fails)}")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
