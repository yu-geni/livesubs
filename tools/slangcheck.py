"""Show what the slang table in pipeline.py changes: each example translated as it was said
and after rewording, for a reader of Japanese to judge. Run after editing the table.

    uv run tools/slangcheck.py
"""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import pipeline  # noqa: E402

EXAMPLES = [
    "Let me pull up the patch notes real quick, they nerfed the shotgun again.",
    "They keep nerfing my favorite character.",
    "I hope they nerf that gun in the next patch.",
    "They buffed the sniper rifle, it's so OP now.",
    "Chat, should I go for the speedrun strat here or just play it safe?",
    "There are a couple of strats for this boss.",
    "GG, well played everyone, see you in the next match.",
    "He's cracked at this game, there's no way that wasn't aim assist.",
    "That play was goated.",
    "That guy is acting kind of sus.",
    "No cap, that was the best round I've ever played.",
    "I'm lowkey nervous about this fight.",
    "Have you ever met him IRL?",
    "Thanks for the five gifted subs, Mike, I really appreciate it.",
    "Thank you for the resub, welcome back.",
    "Save your ult for the next fight.",
    "I was asking if the stream is working.",
    "This entire stream is just gonna be me asking questions.",
    "Thanks for watching the stream, everyone.",
    "I've been streaming for six hours today.",
    "I usually stream on Tuesdays and Fridays.",
    "Should I do a poll?",
    "The poll says most of you want pizza.",
    "Thank you for the resubscription.",
    "I'll have the nachos with extra guac.",
    # must stay as they are
    "She's my favorite streamer.",
    "Op 4 has to hold the data site.",
    "The screen on my phone is cracked.",
    "I studied operations research at university.",
]


def main() -> None:
    engine = pipeline.Engine(pipeline.Config(topics=False))
    for en in EXAMPLES:
        plain = pipeline.plain_english(en)
        print(f"\n{en}")
        if plain == en:
            print(f"  (unchanged) {engine.translate(en)}")
            continue
        print(f"  as said   {engine._mt(en, 1.05)[0]}")
        print(f"  -> {plain}")
        print(f"  reworded  {engine.translate(en)}")
    for en in ("Wait, wait, wait, go back, I think we missed a treasure chest behind the waterfall.", "La la la la la la la la la la la la."):
        print(f"\n{en}\n  {engine.translate(en)}")


if __name__ == "__main__":
    main()
