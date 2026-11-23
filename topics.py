"""Topic outline: every minute or two of speech gets a Japanese heading and a short summary.

Headings need a far more capable model than the subtitles do, but not in real time. So the model
runs in a short-lived child process: it uses the GPU for a few seconds per entry instead of
holding ~2 GB of VRAM for the whole session.
"""

from __future__ import annotations

import contextlib
import json
import logging
import os
import re
import subprocess
import sys

# LFM2-2.6B wrote outlines as good as Qwen3-4B-Instruct-2507 (or more concrete) on saved transcripts,
# in half the time, from a 1.5 GB file instead of 2.4 GB. The 1.2B models were too weak: English or
# Chinese headings and one-word summaries.
REPO = "LiquidAI/LFM2-2.6B-GGUF"
FILE = "LFM2-2.6B-Q4_K_M.gguf"
SYSTEM = (
    "あなたは英語のライブ配信の書き起こしを読んで、日本語の目次を作る編集者です。"
    "書き起こしは音声認識なので誤字や聞き間違いがあります。書かれていないことは足しません。"
    "この部分で配信者が話した内容を、次の2行だけで答えます。\n"
    "見出し: （話題を表す日本語20文字以内の見出し）\n"
    "要約: （具体的な内容を日本語1〜2文、80文字以内で。固有名詞や数字を入れる）"
)
log = logging.getLogger("rvt")


def prefetch() -> None:
    """Download the model ahead of the first entry (a no-op once it is cached)."""
    try:
        _model_path()
    except Exception as e:
        log.warning("topic model download failed: %s", e)


def _model_path() -> str:
    from huggingface_hub import hf_hub_download

    from pipeline import _cached_first

    return _cached_first(lambda local: hf_hub_download(REPO, FILE, local_files_only=local))


def summarize(text: str, device: str, hold=None) -> dict | None:
    """{"title", "summary"} for a stretch of English transcript, or None if the worker failed.

    `hold(proc)` is handed the child as soon as it exists, and None when it is gone: the caller
    keeps the handle so the worker can be ended with the session instead of holding the GPU on.
    """
    proc = None
    try:
        proc = subprocess.Popen(
            [sys.executable, __file__, device],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, encoding="utf-8",
        )
        if hold is not None:
            hold(proc)
        try:
            out, err = proc.communicate(text, timeout=600)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.communicate()
            log.warning("topic worker timed out")
            return None
        if proc.returncode < 0:
            log.info("topic worker ended early (signal %d)", -proc.returncode)
            return None
        if proc.returncode != 0:
            log.warning("topic worker failed: %s", err.strip()[-300:])
            return None
        res = json.loads(out.strip().splitlines()[-1])
        return res if res.get("title") else None
    except Exception as e:
        log.warning("topic worker failed: %s", e)
        return None
    finally:
        if proc is not None and proc.poll() is None:
            stop(proc)  # something threw on the way out: do not leave the model on the GPU
            with contextlib.suppress(Exception):
                proc.wait(timeout=10)


def stop(proc) -> None:
    """End a worker: nothing is waiting for its answer any more."""
    try:
        if proc.poll() is None:
            proc.terminate()
    except Exception as e:  # already reaped, or gone
        log.debug("topic worker could not be stopped: %s", e)


def _parse(answer: str) -> dict:
    """The model is asked for "見出し: ..." and "要約: ..." lines, but LFM2 often drops the labels or
    writes the heading as Markdown ("### ..."). Then the first line is the heading, the second the summary."""
    answer = re.sub(r"[#*]+", "", answer)
    found = dict(re.findall(r"^\s*(見出し|要約)\s*[:：]\s*(.+?)\s*$", answer, flags=re.MULTILINE))
    lines = [ln.strip() for ln in answer.splitlines() if ln.strip()]
    title = found.get("見出し") or (lines[0] if lines else "")
    summary = found.get("要約") or (lines[1] if len(lines) > 1 and not found else "")
    return {"title": title.strip("「」\"'。 ")[:40], "summary": summary.strip()[:160]}


def _drop_cache(path: str) -> None:
    """Tell the kernel it may forget the model file. It is read again in a minute or two, but WSL
    hands page cache back to Windows only reluctantly, so a 1.5 GB file left there shows up as
    memory the whole PC has lost. Reading it again costs about a second and a half."""
    try:
        fd = os.open(path, os.O_RDONLY)
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)
    except (OSError, AttributeError):
        pass


def main() -> None:
    sys.stdin.reconfigure(encoding="utf-8")
    sys.stdout.reconfigure(encoding="utf-8")
    text = sys.stdin.read()
    from pipeline import _preload_cuda_libs

    _preload_cuda_libs()
    from llama_cpp import Llama

    gpu_layers = -1 if sys.argv[1:] == ["cuda"] else 0
    path = _model_path()
    try:
        llm = Llama(model_path=path, n_ctx=4096, n_gpu_layers=gpu_layers, verbose=False)
        out = llm.create_chat_completion(
            messages=[{"role": "system", "content": SYSTEM}, {"role": "user", "content": f"書き起こし:\n{text[-6000:]}"}],
            max_tokens=220,
            temperature=0.0,
        )
        del llm  # unmap the file first: pages still mapped would stay cached
    finally:
        _drop_cache(path)
    print(json.dumps(_parse(out["choices"][0]["message"]["content"]), ensure_ascii=False))


if __name__ == "__main__":
    main()
