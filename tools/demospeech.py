"""Speech for the picture in the README: a script of our own, read by a synthetic voice into a file.

    uv run --no-project --with piper-tts python -m piper.download_voices en_US-lessac-medium
    uv run --no-project --with piper-tts --with soxr --with numpy tools/demospeech.py en_US-lessac-medium.onnx speech.pcm

Nothing is played: the result is 16 kHz mono 16-bit PCM in a file, which the app can be fed
instead of the sound card. With LiveSubs.ini beside the exe pointing at this repository, this
makes docs/screenshot.png (the app draws its own window into a file; the window is never shown
and the screen is not captured):

    LiveSubs.exe --selftest-gui 112 <picture.raw> <engine port> <speech.pcm> offscreen 0 main
    uv run --no-project --with pillow --with numpy tools/rawpng.py docs/screenshot.png <picture.raw>
"""

from __future__ import annotations

import io
import sys
import wave

import numpy as np
import soxr
from piper import PiperVoice

SR = 16000
TEXT = """Alright, welcome back everyone. Thanks for hanging out with me tonight.
So before we jump into the next match, I want to talk about the patch that came out this morning.
They changed a lot more than I expected.
The biggest change is that the healer got a pretty big nerf.
Her healing is down by about twenty percent, and honestly, I think that was the right call.
She was way too strong in the last tournament.
On the other hand, the sniper got a small buff.
The reload is a little faster now, so you can take more risks in the middle of the game.
I tried it for about two hours before the stream, and it feels really good.
Now, some of you asked in chat about the new map.
I like the layout, but the starting points are a little unfair right now.
If you start on the north side, you can reach the high ground almost ten seconds earlier.
I hope they fix that in the next update.
Anyway, here is the plan for today.
We are going to play three ranked matches, and after that I will open the viewer games.
If you want to join, just type your name in chat, and my moderators will add you to the list.
Oh, and one more thing. Next Saturday we have the charity stream.
It starts at six in the evening, and all the donations go to the children's hospital.
Last year we raised over eight thousand dollars, and I would love to beat that this time.
Okay, the queue just popped. Let's go. Wish me luck."""


def main() -> None:
    voice = PiperVoice.load(sys.argv[1])
    rng = np.random.default_rng(7)  # the same pauses every time
    out = [np.zeros(SR, np.int16)]
    for line in TEXT.split("\n"):
        buf = io.BytesIO()
        with wave.open(buf, "wb") as w:
            voice.synthesize_wav(line, w)
        buf.seek(0)
        with wave.open(buf, "rb") as w:
            rate = w.getframerate()
            pcm = np.frombuffer(w.readframes(w.getnframes()), np.int16).astype(np.float32) / 32768
        out.append((soxr.resample(pcm, rate, SR) * 32767 * 0.8).astype(np.int16))
        out.append(np.zeros(int(SR * rng.uniform(0.5, 1.0)), np.int16))  # a breath between sentences
    pcm = np.concatenate(out)
    pcm.tofile(sys.argv[2])
    print(f"wrote {sys.argv[2]}: {len(pcm) / SR:.1f} s of speech")


if __name__ == "__main__":
    main()
