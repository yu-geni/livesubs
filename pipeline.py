"""Streaming English speech -> Japanese text.

Audio is re-decoded with Whisper every `step_s` while someone is speaking. A sentence is
committed (and translated once, as a whole sentence) as soon as it is stable; whatever is
left over is shown as a dimmed interim line. Only speech is ever decoded (Silero VAD gate).
"""

from __future__ import annotations

import ctypes
import importlib.util
import logging
import os
import re
import threading
import time
from collections import Counter
from dataclasses import dataclass

# The only BLAS work is one small mel-filterbank matmul per pass; left alone, OpenBLAS wakes a
# thread per core for it and they spin, which costs more CPU than the rest of the pipeline.
os.environ.setdefault("OPENBLAS_NUM_THREADS", "1")

import numpy as np

import asrcache
import topics

log = logging.getLogger("rvt")

SR = 16000
TICK = int(0.25 * SR)  # how much new audio must arrive before the VAD is consulted again
KEEP = int(0.2 * SR)  # silence kept in front of speech so word onsets are not clipped
VAD_WINDOW = 512  # the VAD's own window at 16 kHz; a partial pass starts on one of them
VAD_OVERLAP = int(0.6 * SR)  # how much already-seen audio a partial VAD pass sees again
PENDING_MAX = 5 * SR * 2  # bytes of audio feed() keeps for the thread (5 s of 16-bit mono)
MT_GROW = 20  # characters of new English that are worth translating the unfinished line again
MT_AGE = 1.6  # ...or this long since the last time it was translated, whichever comes first
SENT_END = (".", "?", "!")
SOFT_END = (",", ";", ":")
# Words whose full stop does not end a sentence. Without them "the U.S. last year", "J. R. R.
# Tolkien", "No. 5" and "Prof. Mori" were each cut in two and both halves translated on their
# own. Only forms that have no other reading are listed; a sentence that really does end in
# "...in the U.S." is still closed, by the pause rules a moment later.
ABBREV = {
    "mr.", "mrs.", "ms.", "mx.", "dr.", "prof.", "st.", "mt.", "sgt.", "capt.", "lt.", "col.", "gen.", "rev.", "sr.", "jr.",
    "no.", "vs.", "etc.", "e.g.", "i.e.",
    "jan.", "feb.", "mar.", "apr.", "jun.", "jul.", "aug.", "sep.", "sept.", "oct.", "nov.", "dec.",  # not "may."
}
ABBREV_DOTS = re.compile(r"^(?:[a-z]\.){2,}$")  # U.S., a.m., e.g.: the dots are inside the word
ABBREV_INITIAL = re.compile(r"^[a-z]\.$")  # an initial, as in "J. R. R. Tolkien"
# What Whisper "hears" in beeps, music and game sounds that slip past the VAD. A line that is
# nothing but one of these is dropped: in a real session 33 such lines came out of the intro
# music, and a real bare "thank you" is no loss on a subtitle.
HALLUCINATIONS = {"thankyou", "thanks", "thankyousomuch", "thankyouverymuch", "thanksforwatching", "thankyouforwatching", "bye", "byebye"}
ECHO_S = 8.0  # a line of up to three words is not shown again within this many seconds
# Translation models: --mt key -> (Hugging Face repo, GGUF file with {q} for the quantization,
# how it is asked). PLaMo 3 Translate (PFN) is asked in its own PFML3 format, not by chat messages.
# plamo-2b is the default: in tools/mtcompare.py it got stream talk (subs, nerf, GG, "3 PM
# Pacific") right where LFM2 went wrong, for ~35 ms more per sentence and ~1.4 GB more VRAM.
MT_MODELS = {
    "lfm2": ("LiquidAI/LFM2-350M-ENJP-MT-GGUF", "LFM2-350M-ENJP-MT-{q}.gguf", "lfm"),
    "plamo-0.8b": ("pfnet/plamo-3-translate-0.8b-GGUF", "plamo-3-translate-0.8b-{q}.gguf", "plamo"),
    "plamo-2b": ("pfnet/plamo-3-translate-2b-GGUF", "plamo-3-translate-2b-{q}.gguf", "plamo"),
    "plamo-8b": ("pfnet/plamo-3-translate-8b-GGUF", "plamo-3-translate-8b-{q}.gguf", "plamo"),
}
PLAMO_STOPS = ("<|plamo:tag|>", "<|plamo:bos|>")
# PFN's GGUFs name their tokenizer "plamo3", which llama.cpp (as of llama-cpp-python 0.3.36)
# does not know: loading fails with "unknown tokenizer: 'plamo3'". Its PLaMo 2 tokenizer reads
# the same vocabulary, and with it the translations and the stops come out right.
PLAMO_KV = {"tokenizer.ggml.model": "plamo2"}
# The MT model learned from subtitle files: it sometimes opens with a dialogue dash or a speaker tag.
MT_ARTIFACT = re.compile(r"^(?:[-‐–—]\s*|[（(][^）)]{1,12}[）)]\s*)+")
MT_LOOP = re.compile(r"(.{1,12}?)\1{3,}")  # one short piece four times or more in a row
# ...and it likes to wrap a whole line in quotation marks nobody spoke.
MT_WRAP = re.compile(r"^[「『\"“]([^「」『』\"“”]+)[」』\"”]([。.！？!?]?)$")
QUOTES = "\"“”「」『』"


def _slang(rules: list[tuple[str, str]], flags: int = re.IGNORECASE) -> list[tuple[re.Pattern, str]]:
    return [(re.compile(pattern, flags), plain) for pattern, plain in rules]


# Stream and game slang that the translation model gets wrong ("nerfed the shotgun" came out
# as "ate the shotgun up"), reworded in plain English before it is translated. The English
# line on screen stays as it was said. Only expressions with a single meaning in a stream
# are listed, and each one was checked to translate better this way (tools/slangcheck.py).
# Only for LFM2 (--mt lfm2): PLaMo got these right as they were said in tools/mtcompare.py.
SLANG = _slang(
    [
        (r"\bnerfed\b", "weakened"),
        (r"\bnerfing\b", "weakening"),
        (r"\bnerfs\b", "weakens"),
        (r"\bnerf\b(?! (?:guns?|blasters?|darts?)\b)", "weaken"),  # not the toy
        (r"\bbuffed\b", "strengthened"),
        (r"\bbuffing\b", "strengthening"),
        (r"\bstrats\b", "strategies"),
        (r"\bstrat\b", "strategy"),
        (r"\bwell played\b", "nicely done"),
        (r"\bcracked at\b", "incredibly good at"),
        (r"\bgoated\b", "amazing"),
        (r"\bsus\b", "suspicious"),
        (r"\bno cap\b", "seriously"),
        (r"\blowkey\b", "kind of"),
        (r"\bIRL\b", "in real life"),
        (r"\bgifted subs\b", "gift subscriptions"),
        (r"\bgifted sub\b", "gift subscription"),
        (r"\bresub\b", "renewed subscription"),
        (r"\bstreams\b", "live broadcasts"),
        (r"\bstream\b", "live broadcast"),
        (r"\bstreaming\b", "broadcasting live"),
        (r"\bpolls\b", "viewer votes"),
        (r"\bpoll\b", "viewer vote"),
        (r"\bguac\b", "guacamole"),
        (r"\bult\b", "ultimate ability"),
        (r"\bGGs?\b", "good game"),
    ]
) + _slang([(r"\b(so|too|pretty|really|super|kinda|is|are|it's|that's) OP\b(?! ?\d)", r"\1 overpowered")], 0)  # "OP 4" is a callout


# Words the translator reads the way a dictionary lists them first, where a stream about games
# means something else: "play" came out as 演奏 (music), 演劇 (theatre) or 再生 (playback),
# "map" as 地図. Put right in the Japanese, and only when the English is not about music,
# video or a real-world map.
PLAY_EN = re.compile(r"\bplay(?:s|ed|ing|ers?)?\b", re.IGNORECASE)
PLAY_NOT = re.compile(r"\b(?:songs?|music|musical|piano|guitars?|drums?|violin|instruments?|band|concerts?|sing|sang|singing|"
                      r"melody|album|orchestra|karaoke|chords?|tunes?|playlists?|audio|videos?|clips?|movies?|films?|replay|"
                      r"theat(?:er|re)|stage|actors?|actress)\b", re.IGNORECASE)
PLAY_JA = [("演奏者", "プレイヤー"), ("演奏", "プレイ"), ("演劇", "プレー"), ("再生", "プレイ")]
MAP_EN = re.compile(r"\bmaps?\b", re.IGNORECASE)
MAP_NOT = re.compile(r"\b(?:google|street|road|city|country|countries|world|treasure|subway|metro|train|geography)\b", re.IGNORECASE)


def game_words(en: str, ja: str) -> str:
    """The Japanese with the game sense of "play" and "map" where the translator took another."""
    if PLAY_EN.search(en) and not PLAY_NOT.search(en):
        for wrong, right in PLAY_JA:
            ja = ja.replace(wrong, right)
    if MAP_EN.search(en) and not MAP_NOT.search(en):
        ja = ja.replace("地図", "マップ")
    return ja


def plain_english(text: str) -> str:
    """The sentence as it is given to the translation model."""
    for pattern, plain in SLANG:
        text = pattern.sub(lambda m, plain=plain: _keep_case(m, plain), text)
    return text


def _keep_case(m: re.Match, plain: str) -> str:
    out = m.expand(plain)
    return out[:1].upper() + out[1:] if m.group(0)[:1].isupper() and not m.group(0).isupper() else out
TOPIC_CHARS = 1200  # transcript per outline entry: about a minute and a half of steady talk
TOPIC_IDLE = (400, 45.0)  # ...or at least this many chars once nobody has spoken for this many seconds


@dataclass
class Config:
    asr_model: str = ""  # "" = pick by device
    device: str = "auto"  # auto | cuda | cpu
    compute_type: str = ""  # "" = pick by device
    mt_model: str = "plamo-2b"  # a key of MT_MODELS
    mt_quant: str = "Q4_K_M"
    mt_threads: int = 4
    interim: bool = True  # translate the unfinished sentence too
    topics: bool = True  # outline the conversation every minute or two (see topics.py)
    step_s: float = 0.8  # re-decode interval while speech is ongoing
    pause_s: float = 0.7  # trailing silence that closes a finished sentence
    open_pause_s: float = 1.8  # ...and an unfinished one: people stop for breath in the middle
    stable_s: float = 0.8  # a sentence end must be this far from the live edge to be committed
    max_s: float = 12.0  # force a cut when a sentence runs longer than this


def _preload_cuda_libs() -> None:
    """CTranslate2 and llama.cpp load the CUDA libs by soname, but the pip wheels are not on the loader path.

    Only the entry points are loaded here; cuDNN finds its own (very large) sub-libraries next to
    itself when it needs them, which keeps a few hundred MB of them out of memory.
    """
    for pkg, names in (("nvidia.cuda_runtime", ["libcudart.so.12"]), ("nvidia.cublas", ["libcublasLt.so.12", "libcublas.so.12"]), ("nvidia.cudnn", ["libcudnn.so.9"])):
        spec = importlib.util.find_spec(pkg)
        if spec is None:
            continue
        for name in names:
            for d in spec.submodule_search_locations:
                try:
                    ctypes.CDLL(os.path.join(d, "lib", name), mode=ctypes.RTLD_GLOBAL)
                    break
                except OSError:
                    pass


def _trim_heap() -> None:
    """Loading a model leaves hundreds of MB of freed heap that glibc would otherwise keep for good."""
    try:
        ctypes.CDLL("libc.so.6").malloc_trim(0)
    except (OSError, AttributeError):
        pass


def _cached_first(fetch):
    """Look a model up in the local cache before asking the Hub: no network round-trips at
    startup, and the tool keeps working offline. `fetch(local_only)` is retried online on a miss."""
    try:
        return fetch(True)
    except Exception:  # not cached yet (the Hub's error types differ between versions)
        return fetch(False)


class Engine:
    """The two models. Shared by all sessions; calls are serialized."""

    def __init__(self, cfg: Config):
        self.cfg = cfg
        self.lock = threading.Lock()
        self._mt_cache: dict[str, str] = {}
        self.unheard = Counter()  # what recognition threw away, by reason (see Session._report)
        # One outline worker at a time, for every session together: a second outline model is another
        # ~2 GB of VRAM, and a reconnect or a second client used to start one.
        self.topic_slot = threading.Semaphore(1)
        self._topic_procs: set = set()
        self._procs_lock = threading.Lock()

        _preload_cuda_libs()  # also for --device cpu: the llama.cpp wheel is a CUDA build and will not load without them
        device = cfg.device
        if device in ("auto", "cuda"):
            import ctranslate2

            has_gpu = ctranslate2.get_cuda_device_count() > 0
            if device == "cuda" and not has_gpu:
                raise RuntimeError("--device cuda was requested but no CUDA device is visible")
            device = "cuda" if has_gpu else "cpu"
        self.device = device
        self.asr_name = cfg.asr_model or ("distil-large-v3.5" if device == "cuda" else "distil-small.en")
        compute = cfg.compute_type or ("int8_float16" if device == "cuda" else "int8")

        from faster_whisper import WhisperModel
        from faster_whisper.vad import VadOptions, get_speech_timestamps

        t0 = time.monotonic()
        # Weights already in 8 bits, if a copy has been made (asrcache.py): no conversion on the way in.
        converted = asrcache.ready(self.asr_name) if (device, compute) == ("cuda", asrcache.COMPUTE) else None
        self.asr = None
        if converted is not None:
            try:
                self.asr = WhisperModel(str(converted), device=device, compute_type=compute)
            except Exception as e:  # a damaged copy must not keep the engine from starting
                log.warning("the converted copy of %s does not load (%s); using the original", self.asr_name, e)
                asrcache.discard(self.asr_name)
                converted = None
        if self.asr is None:
            self.asr = _cached_first(lambda local: WhisperModel(self.asr_name, device=device, compute_type=compute, local_files_only=local))
        _trim_heap()  # here as well: what is allocated next would otherwise pin the freed pages
        self._vad = get_speech_timestamps
        self._vad_opts = VadOptions(threshold=0.5, min_speech_duration_ms=150, min_silence_duration_ms=200, speech_pad_ms=0)
        log.info("ASR %s on %s (%s%s) loaded in %.1fs", self.asr_name, device, compute, ", converted copy" if converted else "", time.monotonic() - t0)

        # One model after the other. Loading the translation model on a second thread meanwhile
        # saved 0.7 s, and once left the engine hanging right after "ASR ... loaded": two
        # libraries bringing up CUDA at the same moment.
        from huggingface_hub import hf_hub_download
        from llama_cpp import Llama

        t0 = time.monotonic()
        repo, file, self._mt_style = MT_MODELS[cfg.mt_model]
        file = file.format(q=cfg.mt_quant)
        self.mt_name = file.removesuffix(".gguf")
        path = _cached_first(lambda local: hf_hub_download(repo, file, local_files_only=local))
        gpu_layers = -1 if device == "cuda" else 0
        kv = PLAMO_KV if self._mt_style == "plamo" else None
        self.mt = Llama(model_path=path, n_ctx=1024, n_threads=cfg.mt_threads, n_gpu_layers=gpu_layers, kv_overrides=kv, verbose=False)
        self._mt_stop = plamo_stopper(self.mt) if self._mt_style == "plamo" else None
        log.info("MT %s on %s loaded in %.1fs", self.mt_name, device, time.monotonic() - t0)

        # First calls pay one-off allocation costs; do them before a client is waiting.
        self.transcribe(np.zeros(SR, dtype=np.float32), "")
        self.translate("Hello.")
        _trim_heap()

        if cfg.topics:
            threading.Thread(target=topics.prefetch, daemon=True).start()
        if converted is None and (device, compute) == ("cuda", asrcache.COMPUTE):
            try:
                asrcache.build_later(self.asr_name)  # the next start is faster
            except Exception as e:
                log.warning("could not start converting %s: %s", self.asr_name, e)

    def speech_spans(self, audio: np.ndarray) -> list[dict]:
        return self._vad(audio, self._vad_opts)

    def hold_topic_proc(self, proc, live: bool) -> None:
        """Remember (or forget) a running outline worker, so none is left behind on shutdown."""
        with self._procs_lock:
            if live:
                self._topic_procs.add(proc)
            else:
                self._topic_procs.discard(proc)

    def stop_topic_procs(self) -> None:
        """The server is going down: a worker of its own would hold the GPU until it is done."""
        with self._procs_lock:
            procs = list(self._topic_procs)
        for proc in procs:
            topics.stop(proc)

    def transcribe(self, audio: np.ndarray, prompt: str) -> list:
        """Returns faster-whisper Word objects (.word has a leading space, .start/.end in seconds)."""
        with self.lock:
            segments, _ = self.asr.transcribe(
                audio,
                language="en",
                beam_size=1,
                temperature=0.0,
                condition_on_previous_text=False,
                initial_prompt=prompt or None,
                word_timestamps=True,
            )
            words = []
            for seg in segments:
                log.debug("asr %.1fs nsp=%.2f lp=%.2f cr=%.1f %r", len(audio) / SR, seg.no_speech_prob, seg.avg_logprob, seg.compression_ratio, seg.text)
                if seg.avg_logprob < -1.0 or seg.compression_ratio > 2.4:  # Whisper's own "decoding failed" criteria
                    self.unheard["unsure"] += 1
                    continue
                words.extend(seg.words or [])
        if _text(words) == "you" or _norm(words) in HALLUCINATIONS:
            self.unheard["thanks"] += 1
            return []
        return words

    def _mt(self, text: str, repeat_penalty: float) -> tuple[str, bool]:
        """The translation, and whether it was cut off by the length limit."""
        if self._mt_style == "plamo":
            out = self.mt.create_completion(
                plamo_prompt(text),
                max_tokens=min(256, 24 + 2 * len(text)),
                temperature=0.0,
                repeat_penalty=repeat_penalty,
                stop=list(PLAMO_STOPS),
                stopping_criteria=self._mt_stop,
            )
            choice = out["choices"][0]
            return " ".join(choice["text"].split()), choice["finish_reason"] == "length"
        out = self.mt.create_chat_completion(
            messages=[
                {"role": "system", "content": "Translate to Japanese."},
                {"role": "user", "content": text},
            ],
            max_tokens=min(256, 24 + 2 * len(text)),
            temperature=0.0,
            repeat_penalty=repeat_penalty,
        )
        choice = out["choices"][0]
        return " ".join(choice["message"]["content"].split()), choice["finish_reason"] == "length"

    def translate(self, text: str) -> str:
        text = text.strip()
        if not text:
            return ""
        hit = self._mt_cache.get(text)  # one atomic read: another session may clear the cache
        if hit is not None:
            return hit
        # The slang table was made for LFM2. PLaMo knows the slang itself, and the rewording
        # only makes it stiffer ("gifted subs" -> "gift subscriptions" -> ギフト購読5回分).
        source = plain_english(text) if self._mt_style == "lfm" else text
        with self.lock:
            ja, cut = self._mt(source, 1.05)
            if cut:
                # It fell into repeating one piece until it ran out of room ("wait, wait, wait,
                # go back" can set that off). A firmer penalty gets it out; that is not the
                # default because it also discourages the particles Japanese has to repeat.
                ja, cut = self._mt(source, 1.15)
                if cut:
                    ja = MT_LOOP.sub(r"\1\1", ja)
        ja = MT_ARTIFACT.sub("", ja) or ja
        ja = game_words(text, ja)
        wrapped = MT_WRAP.match(ja)
        if wrapped and not any(q in text for q in QUOTES):
            ja = wrapped.group(1) + wrapped.group(2)
        if len(self._mt_cache) > 256:
            self._mt_cache.clear()
        self._mt_cache[text] = ja
        return ja


def plamo_prompt(text: str, target: str = "ja") -> str:
    """PLaMo 3 Translate's PFML3 prompt (one line; llama.cpp adds the BOS token itself)."""
    return (
        "<|plamo:tag|>dataset<|plamo:msg|>translation"
        f"<|plamo:tag|>input<|plamo:msg|>{text}"
        f"<|plamo:tag|>output<|plamo:key|>lang<|plamo:val|>{target}<|plamo:msg|>"
    )


def plamo_stopper(llm):
    """Stop at PLaMo's tag tokens. The text stop strings alone are not enough: special tokens
    come out of llama-cpp-python as empty text, so the model would run on into the next element."""
    from llama_cpp import StoppingCriteriaList

    ids = set()
    for tok in PLAMO_STOPS:
        t = llm.tokenize(tok.encode(), add_bos=False, special=True)
        if len(t) == 1:
            ids.add(t[0])
    return StoppingCriteriaList([lambda input_ids, logits: int(input_ids[-1]) in ids])


def _text(words) -> str:
    return "".join(w.word for w in words).strip()


def _norm(words) -> str:
    return re.sub(r"[^a-z0-9]+", "", _text(words).lower())


def _key(en: str) -> str:
    return re.sub(r"[^a-z0-9]+", "", en.lower())


def _ends_with(word: str, marks: tuple[str, ...]) -> bool:
    w = word.strip().lower().rstrip("\"')")
    if not w.endswith(marks) or w.endswith("..."):
        return False
    if not w.endswith("."):
        return True  # "?" and "!" are never an abbreviation
    bare = w.lstrip("\"'(“‘")
    return not (bare in ABBREV or ABBREV_DOTS.match(bare) or ABBREV_INITIAL.match(bare))


class Session:
    """One audio stream. feed() is called from the event loop; the work happens on its own thread."""

    def __init__(self, engine: Engine, emit):
        self.engine, self.cfg, self.emit = engine, engine.cfg, emit
        self._lock = threading.Lock()
        self._wake = threading.Event()
        self._pending: list[bytes] = []
        self._t_last = time.monotonic()
        self._reset_req = ""  # "" | "soft" | "new"
        self._closed = False
        self.seg_id = 0
        self.topic_id = 0
        # The client's own switch, on top of --no-topics. Off until the client asks for the
        # outline: the browser page and tools/feed.py show none, and each entry they never
        # asked for loaded a 1.5 GB model onto the GPU for nothing.
        self.topics_on = False
        self._topic_busy = False
        self._topic_proc = None  # the outline worker now running, so it can be ended with the session
        self._topic_buf: list[str] = []  # finalized English not yet covered by an outline entry
        self._topic_seg = 0  # segment id the next entry starts at
        self._t_final = time.monotonic()
        self._said = ("", 0.0)  # the last line shown, as _key() has it, and when
        self._t_drop = 0.0  # when feed() last complained about dropping audio
        # What happened to the sound in the last minute, for the log (see _report): when part
        # of a stream gets no subtitle, this says whether the audio arrived and what was done
        # with it.
        self._stats: Counter = Counter()
        self._unheard_seen: Counter = Counter()
        self._t_stats = time.monotonic()
        self._clear()
        threading.Thread(target=self._run, daemon=True).start()

    def _clear(self) -> None:
        self.buf = np.zeros(0, dtype=np.float32)
        self.prompt = ""
        self.prev_words: list = []
        self._checked = 0  # buffer length at the last VAD pass
        self._decoded = 0  # buffer length at the last Whisper pass
        self._recheck_at: int | None = None  # buffer length at which a just-ended sentence becomes committable
        self._last: tuple[int, list] | None = None  # (decoded length, words) of the last Whisper pass
        self._interim: tuple[str, str] | None = None  # (en, ja) currently on screen for seg_id
        self._mt_done = ("", 0.0)  # the English the interim's Japanese was made from, and when
        self._vad_scanned = 0  # buffer length the VAD has already been over
        self._vad_start: int | None = None  # where the speech in the buffer begins
        self._vad_end: int | None = None  # ...and where it last stopped
        self._vad_open = False  # the last span was still going on at the end of the audio

    def feed(self, pcm: bytes) -> None:
        with self._lock:
            self._pending.append(pcm)
            self._t_last = time.monotonic()
            # Should the thread ever stop taking the audio, the client keeps sending it anyway:
            # hold a few seconds of it, not the whole session.
            held, dropped = sum(len(c) for c in self._pending), 0
            while held > PENDING_MAX and len(self._pending) > 1:
                held -= len(self._pending[0])
                dropped += len(self._pending.pop(0))
            self._stats["audio_s"] += len(pcm) / (2 * SR)
            self._stats["behind_s"] += dropped / (2 * SR)
            say = dropped > 0 and time.monotonic() - self._t_drop > 5.0  # once in a while, not per frame
            if say:
                self._t_drop = time.monotonic()
        if say:
            log.warning("the pipeline thread is not keeping up: %.1fs of audio dropped", dropped / (2 * SR))
            # The client is told too: what was dropped gets no subtitle, and without a word
            # on the screen that looks like the speaker having said nothing.
            try:
                self.emit({"type": "behind", "dropped_s": round(dropped / (2 * SR), 1)})
            except Exception:
                log.exception("could not tell the client about it")
        self._wake.set()

    def reset(self, new_stream: bool = False) -> None:
        """Drop buffered audio. `new_stream` also forgets what the outline had collected and
        skips a segment id, so nothing from the old stream is continued into the new one."""
        with self._lock:
            self._pending.clear()
            # Both can arrive before the thread looks (the client sends a plain reset on connect):
            # a new stream stays a new stream until it has been handled.
            if new_stream or self._reset_req != "new":
                self._reset_req = "new" if new_stream else "soft"
        self._wake.set()

    def close(self) -> None:
        self._closed = True
        self._wake.set()
        with self._lock:
            proc = self._topic_proc
        if proc is not None:
            topics.stop(proc)  # nobody is left to read its answer, and it is holding the GPU

    def _run(self) -> None:
        while True:
            self._wake.wait()
            self._wake.clear()
            if self._closed:
                return
            try:
                self._tick()
            except Exception as e:  # keep the stream alive; one bad chunk must not kill subtitles
                log.exception("pipeline error")
                self._clear()
                try:
                    self.emit({"type": "error", "message": str(e)})
                except Exception:
                    # The report itself failed (a closed event loop, say). Losing the thread here
                    # is worse than losing the message: feed() would pile audio up for nobody.
                    log.exception("could not tell the client about it")

    def _drop_front(self, n: int) -> None:
        n = max(0, min(n, len(self.buf)))
        self.buf = self.buf[n:]
        self._checked = max(0, self._checked - n)
        self._decoded = max(0, self._decoded - n)
        if self._recheck_at is not None:
            self._recheck_at = max(0, self._recheck_at - n)
        self._last = None
        # What is left is short, so the VAD starts over on it: its window grid and its own state
        # both begin at the front of the buffer, and nothing has to be rebased.
        self._vad_scanned, self._vad_start, self._vad_end, self._vad_open = 0, None, None, False

    def _report(self) -> None:
        """Once a minute: how much sound came in and what became of it."""
        if time.monotonic() - self._t_stats < 60.0:
            return
        with self._lock:
            st, self._stats = self._stats, Counter()
        unheard = self.engine.unheard - self._unheard_seen  # shared by all sessions; this is the share since last time
        self._unheard_seen = self.engine.unheard.copy()
        secs = time.monotonic() - self._t_stats
        self._t_stats = time.monotonic()
        self.emit({"type": "stats", "summary": (
            f"last {secs:.0f} s: audio {st['audio_s']:.1f} s, decoded {st['decodes']} times, {st['lines']} lines; "
            f"not shown: unsure decodes {unheard['unsure']}, thank-you {unheard['thanks']}, no words {st['no_words']}, "
            f"repeats {st['echo']}, lyrics {st['lyrics']}, music {st['music_s']:.1f} s, behind {st['behind_s']:.1f} s")})

    def _tick(self) -> None:
        self._report()
        with self._lock:
            chunks, self._pending = self._pending, []
            t_end = self._t_last
            reset, self._reset_req = self._reset_req, ""
        if reset == "soft":
            # The audio source switched (or the app resumed): the sentence that was being spoken
            # is finished as far as this stream goes, so commit it instead of dropping it.
            self._flush_interim(t_end)
        if reset:
            self._clear()
        if reset == "new":
            self._topic_buf = []
            self.seg_id += 1  # the id of a half-shown interim is never reused for new content
            # An outline entry still being written is about the stream that was left: the
            # client throws it away, and until it is done the worker holds ~2 GB of the GPU.
            with self._lock:
                proc = self._topic_proc
            if proc is not None:
                topics.stop(proc)
        if chunks:
            pcm = np.frombuffer(b"".join(chunks), dtype="<i2").astype(np.float32) / 32768.0
            self.buf = np.concatenate([self.buf, pcm])
        n = len(self.buf)
        recheck = self._recheck_at is not None and n >= self._recheck_at
        if n - self._checked < TICK and not recheck:
            return
        self._checked = n

        edges = self._speech_edges(n)
        if edges is None:
            self._flush_interim(t_end)
            self._maybe_topic()
            self._drop_front(n - KEEP)
            self.prev_words = []
            return
        start, end = edges
        paused = (n - end) >= self.cfg.pause_s * SR
        ended = (n - end) >= KEEP  # the speaker has stopped, at least for a moment
        n_dec = min(n, end + KEEP)
        fresh = self._last is not None and self._last[0] == n_dec  # the last pass already heard all of it
        # Decode as soon as speech stops, not when the pause is confirmed: by then the words (and
        # their translation, via the interim line) are ready and the commit costs nothing.
        due = recheck or n - self._decoded >= self.cfg.step_s * SR or (ended and not fresh)
        if not paused and (end - start < 0.3 * SR or not due):
            return
        self._decoded = n
        self._recheck_at = None

        dur = n / SR
        if fresh:
            words = self._last[1]
        else:
            words = self.engine.transcribe(self.buf[:n_dec], self.prompt)
            self._stats["decodes"] += 1
            self._last = (n_dec, words)
        # A pause after words that do not end a sentence is usually the speaker drawing breath.
        # Closing there hands the translator half a clause, and the other half next (a third of
        # the lines in a real session were such pieces). Wait longer before giving up on it;
        # the unfinished line is on screen in the meantime. Not for ever, though: when Whisper
        # stops punctuating (it does on choppy audio) every pause looks like a breath, and what
        # has piled up by then is long enough to translate as it is.
        waiting = (
            paused
            and bool(words)
            and not _ends_with(words[-1].word, SENT_END)
            and (n - end) < self.cfg.open_pause_s * SR
            and dur < 0.6 * self.cfg.max_s
        )
        if waiting:
            paused = False
        k = self._pick_cut(words, dur, paused)

        if k:
            done, rest = words[:k], words[k:]
            if rest:
                cut = (done[-1].end + rest[0].start) / 2
            elif paused:
                cut = dur - KEEP / SR
            else:  # speech goes on, but Whisper has not put the newest sounds into words yet: keep them
                cut = min(done[-1].end, dur - KEEP / SR)
            self._finalize(_text(done), None, t_end - (dur - done[-1].end))
            self._drop_front(int(cut * SR))
            for w in rest:
                w.start, w.end = w.start - cut, w.end - cut
            words = rest
        elif paused:
            # Speech-like sound that Whisper found no words in.
            self._stats["no_words"] += 1
            self._flush_interim(t_end)
            self._drop_front(n - KEEP)
        elif dur > 1.5 * self.cfg.max_s:
            # Nothing has been committable for a long time (music or noise the VAD takes for
            # speech). Let the old audio go, or every pass re-decodes an ever longer buffer.
            self._stats["music_s"] += (n - int(2 * self.cfg.stable_s * SR)) / SR
            self._drop_front(n - int(2 * self.cfg.stable_s * SR))
            words = []
            self._interim = None  # the audio it was heard in is gone; it must not become a line later
        self.prev_words = words
        self._schedule_recheck(words, speaking=not ended)
        if ended and not paused:
            # ...and come back the moment the silence is long enough to close the segment.
            at = end - (n - len(self.buf)) + int((self.cfg.open_pause_s if waiting else self.cfg.pause_s) * SR)
            self._recheck_at = at if self._recheck_at is None else min(self._recheck_at, at)

        en = _text(words)
        if en and (self._interim is None or self._interim[0] != en):
            ja = self._interim_ja(en, ended)
            self._interim = (en, ja)
            if not self._noise(en, ja):
                self.emit({"type": "interim", "id": self.seg_id, "en": en, "ja": ja})

    def _speech_edges(self, n: int) -> tuple[int, int] | None:
        """Where the speech in the buffer starts and where it last stopped, or None if there is none.

        Only audio the VAD has not seen yet is given to it, plus an overlap: its own state needs a
        moment to warm up again, and a span can still change over the join (a silence shorter than
        min_silence_duration_ms merges two spans, speech shorter than min_speech_duration_ms only
        counts once it is long enough). The whole buffer used to be scanned every 0.25 s, which by
        the end of a long sentence meant 18 seconds of audio per tick.
        """
        if n > self._vad_scanned:
            base = max(0, self._vad_scanned - VAD_OVERLAP) // VAD_WINDOW * VAD_WINDOW  # same grid as a full pass
            chunk = self.buf[base:n]
            spans = self.engine.speech_spans(chunk)
            self._vad_scanned = n
            if spans:
                if self._vad_start is None:
                    self._vad_start = base + spans[0]["start"]
                # A span that was still open only ended "as far as we had heard": its end is
                # whatever this pass makes of it, while a span that had closed can only grow.
                end = base + spans[-1]["end"]
                self._vad_end = end if self._vad_open or self._vad_end is None else max(self._vad_end, end)
                self._vad_open = spans[-1]["end"] >= len(chunk)
            else:
                self._vad_open = False
        if self._vad_start is None:
            return None
        return self._vad_start, self._vad_end

    def _schedule_recheck(self, words, speaking: bool) -> None:
        """A sentence that has just ended can be committed once it has stayed put for stable_s:
        look again exactly then, instead of up to a whole step later."""
        dur = len(self.buf) / SR
        for i, w in enumerate(words):
            if w.end <= dur - self.cfg.stable_s or not _ends_with(w.word, SENT_END):
                continue
            if speaking and i == len(words) - 1:
                break  # Whisper puts a full stop after whatever came last; not a real ending yet
            at = int((w.end + self.cfg.stable_s) * SR)
            self._recheck_at = max(at, self._decoded + int(0.3 * SR))
            break

    def _pick_cut(self, words, dur: float, paused: bool) -> int:
        """How many leading words to commit now."""
        if paused:
            return len(words)
        settled = [i for i, w in enumerate(words) if w.end <= dur - self.cfg.stable_s]
        prev = _norm(self.prev_words)
        for i in reversed(settled):
            # The sentence must read the same in two consecutive decodes: Whisper revises
            # punctuation near the live edge as more audio arrives.
            if _ends_with(words[i].word, SENT_END) and prev.startswith(_norm(words[: i + 1])):
                return i + 1
        if dur > self.cfg.max_s and settled:
            soft = [i for i in settled if _ends_with(words[i].word, SENT_END + SOFT_END)]
            return (soft[-1] if soft else settled[-1]) + 1
        return 0

    def _interim_ja(self, en: str, ended: bool) -> str:
        """The Japanese for the sentence that is still being spoken.

        Translating it afresh at every decode step costs more the longer the sentence gets, and a
        line that is rewritten three times a second cannot be read anyway: the Japanese already on
        screen stays until the English has grown by a noticeable amount or it has gone stale. What
        is committed is never one of these older translations (see _flush_interim), so the line is
        translated in full once the speaker has stopped: the commit is then only a moment away and
        this is the translation it will use.
        """
        if not self.cfg.interim:
            return ""
        if self._interim is not None and not ended:
            was, at = self._mt_done
            if len(en) - len(was) < MT_GROW and time.monotonic() - at < MT_AGE:
                return self._interim[1]
        self._mt_done = (en, time.monotonic())
        return self.engine.translate(en)

    def _noise(self, en: str, ja: str) -> bool:
        """A short line that adds nothing: the same few words again, or sounds taken for lyrics."""
        if len(en.split()) > 3:
            return False
        said, at = self._said
        return ja.startswith("♪") or (said == _key(en) and time.monotonic() - at < ECHO_S)

    def _finalize(self, en: str, ja: str | None, t_speech_end: float) -> None:
        if ja is None:
            ja = self.engine.translate(en)
        if self._noise(en, ja):
            self._stats["lyrics" if ja.startswith("♪") else "echo"] += 1
            self._interim = None  # its id goes to whatever is said next
            return
        self._said = (_key(en), time.monotonic())
        lag = int((time.monotonic() - t_speech_end) * 1000)
        self.emit({"type": "final", "id": self.seg_id, "en": en, "ja": ja, "lag_ms": lag})
        self._stats["lines"] += 1
        self._note_topic(self.seg_id, en)
        self.seg_id += 1
        self.prompt = f"{self.prompt} {en}"[-200:].strip()
        self._interim = None

    def set_topics(self, on: bool) -> None:
        self.topics_on = on

    def _note_topic(self, seg_id: int, en: str) -> None:
        if not (self.cfg.topics and self.topics_on):
            self._topic_buf = []  # what was said while the outline is off is not summarized later
            return
        if not self._topic_buf:
            self._topic_seg = seg_id
        self._topic_buf.append(en)
        self._t_final = time.monotonic()
        self._maybe_topic()

    def _maybe_topic(self) -> None:
        if not (self.cfg.topics and self.topics_on) or self._topic_busy or not self._topic_buf:
            return
        chars = sum(len(t) for t in self._topic_buf)
        quiet = time.monotonic() - self._t_final > TOPIC_IDLE[1]
        if chars < TOPIC_CHARS and not (quiet and chars >= TOPIC_IDLE[0]):
            return
        if not self.engine.topic_slot.acquire(blocking=False):
            return  # another session's worker has the GPU; this text waits for the next entry
        text, seg, self._topic_buf = " ".join(self._topic_buf), self._topic_seg, []
        self._topic_busy = True
        threading.Thread(target=self._topic_job, args=(text, seg), daemon=True).start()

    def _topic_job(self, text: str, seg: int) -> None:
        """Runs beside the subtitle thread: the worker takes seconds and must not stall recognition."""
        try:
            res = topics.summarize(text, self.engine.device, self._hold_topic)
            if res and not self._closed:
                self.emit({"type": "topic", "id": self.topic_id, "seg": seg, **res})
                self.topic_id += 1
        finally:
            self._hold_topic(None)
            self._topic_busy = False
            self.engine.topic_slot.release()

    def _hold_topic(self, proc) -> None:
        """The worker's handle while it runs (None once it is done), so it can be ended early."""
        with self._lock:
            before, self._topic_proc = self._topic_proc, proc
        if before is not None:
            self.engine.hold_topic_proc(before, False)
        if proc is not None:
            self.engine.hold_topic_proc(proc, True)
            if self._closed:
                topics.stop(proc)  # the client left while the worker was starting up

    def _flush_interim(self, t_end: float) -> None:
        """The speech ended without a decode that confirmed it: keep what is on screen."""
        if self._interim is not None:
            en, ja = self._interim
            fresh = self.cfg.interim and self._mt_done[0] == en  # not a translation of an earlier, shorter line
            self._finalize(en, ja if fresh else None, t_end)
