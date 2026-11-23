"""Translate the same English sentences with several small models and compare speed and output.

    uv run tools/mtcompare.py [--models lfm2,plamo08,plamo2,plamo8] [--out FILE.json]

Each model runs in a process of its own so that load time and GPU memory can be told apart.
The sentences are the kind of thing a live stream produces: a recognised game briefing, chat
with viewers, slang, product news, numbers and names.

Models that are not in the Hugging Face cache yet are downloaded (0.2 to 1.1 GB each).

What it showed on 2026-10-11 (RTX 5070 Ti, Q4_K_M unless noted, my own reading of the output):
  lfm2    median 85 ms, +0.65 GB GPU   about 10 of 48 wrong, mostly game slang; terse and readable
  lfm2q8  median 91 ms, +0.8 GB        35 of 48 worded differently, not better on balance
  cat08   median 114 ms, +0.8 GB       left a fifth of the sentences in English: unusable here
  cat14   median 118 ms, +1.4 GB       natural Japanese, but about 15 wrong, numbers and facts among them
  hy18    median 258 ms, +1.7 GB       about 9 wrong, long-winded, one sentence took 4.6 s
Nothing in this size class beat the 350M model, so it stayed; its slang mistakes are handled by
the table in pipeline.py instead (tools/slangcheck.py).

PLaMo 3 Translate (PFN, released 2026-11-05) on 2026-11-07, same GPU and sentences:
  lfm2     median 61 ms,  +0.66 GB   about 12 of 48 wrong (subs, nerf, chest, GG, "3 PM Pacific" as Japan time)
  plamo08  median 67 ms,  +0.97 GB   about 4 wrong; casual and natural (one run had two 4 s stalls, a rerun none)
  plamo2   median 95 ms,  +2.1 GB    about 3 wrong ("follow button" became 下のボタン, "cracked" キレてる)
  plamo8   median 198 ms, +5.4 GB    about 2 wrong, but "he's cracked" came out as 下手, the opposite
All three read like a streamer talking, where LFM2 is stiff. plamo-2b became the default.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

# key: (repo, file, how the model is asked)
MODELS = {
    "lfm2": ("LiquidAI/LFM2-350M-ENJP-MT-GGUF", "LFM2-350M-ENJP-MT-Q4_K_M.gguf", "lfm"),
    "lfm2q8": ("LiquidAI/LFM2-350M-ENJP-MT-GGUF", "LFM2-350M-ENJP-MT-Q8_0.gguf", "lfm"),
    "cat08": ("mmnga-o/CAT-Translate-0.8b-gguf", "CAT-Translate-0.8b-Q4_K_M.gguf", "cat"),
    "cat14": ("mmnga-o/CAT-Translate-1.4b-gguf", "CAT-Translate-1.4b-Q4_K_M.gguf", "cat"),
    "hy18": ("tencent/HY-MT1.5-1.8B-GGUF", "HY-MT1.5-1.8B-Q4_K_M.gguf", "hy"),
    "plamo08": ("pfnet/plamo-3-translate-0.8b-GGUF", "plamo-3-translate-0.8b-Q4_K_M.gguf", "plamo"),
    "plamo2": ("pfnet/plamo-3-translate-2b-GGUF", "plamo-3-translate-2b-Q4_K_M.gguf", "plamo"),
    "plamo8": ("pfnet/plamo-3-translate-8b-GGUF", "plamo-3-translate-8b-Q4_K_M.gguf", "plamo"),
}

SENTENCES = [
    # recognised from a game stream (mistakes of the recogniser left in)
    "Whoever has control of this area by the end of the AO will get four points.",
    "Now, there is an ammo depot, specifically a truck filled with explosives at the ammo depot.",
    "If that truck goes up, every building around it goes with it.",
    "Now let me quickly look at the point values again to remind myself of what is going on here.",
    "So breaking everything down, here are the four different methods that either side can score points today.",
    "I think this is the blue for spawn and this is the op for spawn, but it could be ROM.",
    "but if Op 4 is able to hold the data site before it's taken, then that's worth two points.",
    "And then the supremacy bonus of either side is able to knock each other out to five men or less.",
    "Otherwise, this is a pretty nice map.",
    "Sorry, I just had a random thought occur to me. I'll talk about it later.",
    # talking to the viewers
    "Chat, should I go for the speedrun strat here or just play it safe?",
    "Thanks for the five gifted subs, Mike, I really appreciate it.",
    "Oh my god, that was so close, I had like one HP left.",
    "Hold on, my mic was muted this whole time, wasn't it?",
    "If you're new here, make sure to hit the follow button, it really helps the channel.",
    "Can you guys hear the game audio, or is it just my voice?",
    "I'll be back in five minutes, I need to grab some water.",
    "Shout out to everyone watching from Japan, thanks for staying up so late.",
    "GG, well played everyone, see you in the next match.",
    # games
    "Let me pull up the patch notes real quick, they nerfed the shotgun again.",
    "I'm not gonna lie, this boss is way harder than I expected.",
    "I don't think the developers intended for you to skip that entire section.",
    "He's cracked at this game, there's no way that wasn't aim assist.",
    "Wait, wait, wait, go back, I think we missed a chest behind the waterfall.",
    "The economy in this game is completely broken right now.",
    "It turns out the bug only happens if you open the inventory while reloading.",
    "We need to rotate to the next zone before the circle closes.",
    "That last level was a piece of cake compared to this one.",
    "So basically what happened is the server crashed right as we were about to win.",
    # tech and news
    "We're going live with the new update on Tuesday at 3 PM Pacific.",
    "The frame rate drops to about forty whenever there's a lot of smoke on screen.",
    "Nvidia announced that the new graphics cards will ship in the first quarter of next year.",
    "Let's take a look at the benchmark results before we draw any conclusions.",
    "The model was trained on about two hundred thousand hours of audio.",
    "Apple is expected to unveil the new iPhone at its event in September.",
    "The interest rate was raised by a quarter of a percentage point.",
    "Elon Musk said on Friday that Tesla would delay the launch of its robotaxi.",
    "They said the patch would fix the memory leak, but it's actually worse now.",
    # everyday talk
    "You can't just buy your way to the top, you still have to put in the hours.",
    "That's what I'm talking about! Let's go!",
    "To be fair, the first half of the tournament was a lot more exciting.",
    "She said she'd rather lose with her own strategy than win by copying someone else.",
    "This is by far the best pizza I've had since I moved to Tokyo.",
    "Don't quote me on this, but I heard the sequel got cancelled.",
    "It's not that I don't like it, it's just not what I expected.",
    "I literally have no idea what I'm doing, but it's working somehow.",
    # an unfinished sentence, as the interim line shows them
    "what you might call it day Z run on and",
    "So the reason I wanted to bring this up today is that",
]


def messages(style: str, text: str) -> list[dict]:
    if style == "lfm":
        return [{"role": "system", "content": "Translate to Japanese."}, {"role": "user", "content": text}]
    if style == "cat":
        return [{"role": "user", "content": f"Translate the following English text into Japanese.\n\n{text}"}]
    if style == "hy":
        return [{"role": "user", "content": f"Translate the following segment into Japanese, without additional explanation.\n\n{text}"}]
    raise ValueError(style)


def gpu_used_mb() -> int:
    try:
        out = subprocess.run(["nvidia-smi", "--query-gpu=memory.used", "--format=csv,noheader,nounits"], capture_output=True, text=True, timeout=10)
        return int(out.stdout.split()[0])
    except Exception:
        return -1


def worker(key: str) -> None:
    from huggingface_hub import hf_hub_download

    from pipeline import PLAMO_KV, _preload_cuda_libs, plamo_prompt, plamo_stopper

    repo, name, style = MODELS[key]
    path = hf_hub_download(repo, name)
    _preload_cuda_libs()
    from llama_cpp import Llama

    before = gpu_used_mb()
    t0 = time.monotonic()
    llm = Llama(model_path=path, n_ctx=1024, n_gpu_layers=-1, kv_overrides=PLAMO_KV if style == "plamo" else None, verbose=False)

    stopper = plamo_stopper(llm) if style == "plamo" else None

    def run(text: str) -> tuple[str, int]:
        if style == "plamo":
            out = llm.create_completion(plamo_prompt(text), max_tokens=min(256, 24 + 2 * len(text)), temperature=0.0, repeat_penalty=1.05,
                                        stop=["<|plamo:tag|>", "<|plamo:bos|>"], stopping_criteria=stopper)
            return " ".join(out["choices"][0]["text"].split()), out["usage"]["completion_tokens"]
        out = llm.create_chat_completion(messages=messages(style, text), max_tokens=min(256, 24 + 2 * len(text)), temperature=0.0, repeat_penalty=1.05)
        return " ".join(out["choices"][0]["message"]["content"].split()), out["usage"]["completion_tokens"]

    run("Hello.")
    load_s = time.monotonic() - t0
    vram = gpu_used_mb() - before
    rows = []
    for s in SENTENCES:
        t = time.monotonic()
        ja, tokens = run(s)
        rows.append({"en": s, "ja": ja, "ms": round((time.monotonic() - t) * 1000), "tokens": tokens})
    print(json.dumps({"key": key, "file": name, "size_gb": round(os.path.getsize(path) / 1e9, 2), "load_s": round(load_s, 1), "vram_mb": vram, "rows": rows}, ensure_ascii=False))


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--models", default="lfm2,plamo08,plamo2,plamo8")
    p.add_argument("--out", help="also write everything as JSON")
    p.add_argument("--worker", help=argparse.SUPPRESS)
    args = p.parse_args()
    if args.worker:
        worker(args.worker)
        return

    results = []
    for key in args.models.split(","):
        r = subprocess.run([sys.executable, __file__, "--worker", key], capture_output=True, text=True)
        if r.returncode != 0:
            print(f"{key}: failed\n{r.stderr[-1500:]}", file=sys.stderr)
            continue
        results.append(json.loads(r.stdout.strip().splitlines()[-1]))
    if args.out:
        with open(args.out, "w", encoding="utf-8") as f:
            json.dump(results, f, ensure_ascii=False, indent=1)

    for r in results:
        ms = [row["ms"] for row in r["rows"]]
        rate = sum(row["tokens"] for row in r["rows"]) / (sum(ms) / 1000)
        print(f"{r['key']:6} {r['file']:34} {r['size_gb']:.2f} GB  load {r['load_s']:.1f} s  GPU +{r['vram_mb']} MB  "
              f"per sentence: median {statistics.median(ms):.0f} ms, worst {max(ms)} ms  ({rate:.0f} tokens/s)")
    for i, s in enumerate(SENTENCES):
        print(f"\n[{i + 1}] {s}")
        for r in results:
            print(f"  {r['key']:6} {r['rows'][i]['ja']}")


if __name__ == "__main__":
    main()
