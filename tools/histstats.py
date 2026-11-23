"""Count what a saved subtitle history (ドキュメント\\ライブ字幕\\*.txt) shows about quality:
pieces of sentences, filler, repeats. Run it on a session before and after a change.

    uv run tools/histstats.py "/mnt/c/Users/<name>/OneDrive/ドキュメント/ライブ字幕/2026-10-11_110437.txt"

Baseline, the session of 2026-10-11 11:04 (before the engine waited for unfinished sentences):
1022 lines, 35% pieces, 29% of one or two words, 33 bare thank-you/bye lines.
"""

from __future__ import annotations

import re
import statistics
import sys

ENTRY = re.compile(r"^\[(\d\d):(\d\d):(\d\d)\] (.*)$")
FILLER = {"thankyou", "thanks", "thankyousomuch", "thankyouverymuch", "bye", "byebye"}


def main() -> None:
    pairs = []  # (seconds, ja, en)
    lines = open(sys.argv[1], encoding="utf-8-sig").read().splitlines()
    for i, line in enumerate(lines):
        m = ENTRY.match(line)
        if m and i + 1 < len(lines) and lines[i + 1].startswith(" "):
            pairs.append((int(m[1]) * 3600 + int(m[2]) * 60 + int(m[3]), m[4], lines[i + 1].strip()))
    if not pairs:
        print("no subtitle lines with English found (the history must be saved with English shown)")
        return
    n = len(pairs)
    key = [re.sub(r"[^a-z0-9]+", "", en.lower()) for _, _, en in pairs]
    pieces = sum(1 for _, _, en in pairs if en[:1].islower() or not en.rstrip("\"')").endswith((".", "?", "!")))
    short = sum(1 for _, _, en in pairs if len(en.split()) <= 2)
    repeats = sum(1 for i in range(1, n) if key[i] == key[i - 1] and pairs[i][0] - pairs[i - 1][0] <= 8)
    quoted = sum(1 for _, ja, en in pairs if ja[:1] in "「『\"“" and not any(q in en for q in "\"“”"))
    minutes = max(1, pairs[-1][0] - pairs[0][0]) / 60
    print(f"{n} lines over {minutes:.0f} min ({n / minutes:.1f} a minute)")
    print(f"pieces of sentences (lowercase start or no end mark): {pieces} ({100 * pieces / n:.0f}%)")
    print(f"one or two words: {short} ({100 * short / n:.0f}%)")
    print(f"bare thank-you/bye lines: {sum(1 for k in key if k in FILLER)}")
    print(f"same line again within 8 s: {repeats}")
    print(f"translation wrapped in quotes the English does not have: {quoted}")
    print(f"English length in characters: median {statistics.median(len(en) for _, _, en in pairs):.0f}")


if __name__ == "__main__":
    main()
