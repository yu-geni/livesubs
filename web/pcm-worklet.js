/*
 * pcm-worklet.js - AudioWorkletProcessor
 *
 * Downmixes every input channel to mono, resamples to 16000 Hz (linear
 * interpolation, continuous across render quanta), accumulates 1600 samples
 * (= 100 ms) and posts them to the main thread as a transferable
 * Int16Array buffer (signed 16-bit little-endian, the platform byte order
 * on every browser target).
 *
 * Message out: { type: 'pcm', rms: <0..1>, sampleCount: 1600, pcm: ArrayBuffer }
 * Message in:  { type: 'stop' } -> processor returns false and is released.
 */

const TARGET_RATE = 16000;
const FRAME_SAMPLES = 1600; // 100 ms @ 16 kHz -> 3200 bytes

class PCMDownsampler extends AudioWorkletProcessor {
  constructor(options) {
    super(options);

    // `sampleRate` is a global inside the AudioWorkletGlobalScope.
    const sr = typeof sampleRate === 'number' && sampleRate > 0 ? sampleRate : TARGET_RATE;
    this.inRate = sr;
    this.ratio = sr / TARGET_RATE;
    this.passthrough = Math.abs(this.ratio - 1) < 1e-9;

    this.acc = new Float32Array(FRAME_SAMPLES);
    this.accLen = 0;

    this.mono = new Float32Array(0);
    this.pos = 0; // read cursor relative to the start of the current block, always >= -1
    this.prev = 0; // last sample of the previous block (needed when pos < 0)
    this.stopped = false;

    this.port.onmessage = (e) => {
      const d = e && e.data;
      if (d && d.type === 'stop') this.stopped = true;
    };
  }

  _push(v) {
    this.acc[this.accLen++] = v;
    if (this.accLen >= FRAME_SAMPLES) this._flush();
  }

  _flush() {
    const n = this.accLen;
    if (n === 0) return;
    this.accLen = 0;
    const out = new Int16Array(n);
    const acc = this.acc;
    let sumSq = 0;
    for (let i = 0; i < n; i++) {
      let s = acc[i];
      if (!(s === s)) s = 0; // NaN guard
      if (s > 1) s = 1;
      else if (s < -1) s = -1;
      sumSq += s * s;
      out[i] = Math.round(s < 0 ? s * 32768 : s * 32767);
    }
    const buf = out.buffer;
    this.port.postMessage(
      { type: 'pcm', rms: Math.sqrt(sumSq / n), sampleCount: n, pcm: buf },
      [buf]
    );
  }

  process(inputs) {
    if (this.stopped) return false;

    const input = inputs[0];
    if (!input || input.length === 0) return true;
    const first = input[0];
    const frames = first ? first.length : 0;
    if (frames === 0) return true;

    if (this.mono.length !== frames) this.mono = new Float32Array(frames);
    const mono = this.mono;

    // ---- downmix to mono -------------------------------------------------
    let used = 0;
    mono.fill(0);
    for (let c = 0; c < input.length; c++) {
      const ch = input[c];
      if (!ch || ch.length !== frames) continue;
      used++;
      for (let i = 0; i < frames; i++) mono[i] += ch[i];
    }
    if (used === 0) return true;
    if (used > 1) {
      const inv = 1 / used;
      for (let i = 0; i < frames; i++) mono[i] *= inv;
    }

    // ---- resample / accumulate ------------------------------------------
    if (this.passthrough) {
      for (let i = 0; i < frames; i++) this._push(mono[i]);
      this.prev = mono[frames - 1];
      return true;
    }

    const ratio = this.ratio;
    const prev = this.prev;
    let pos = this.pos;
    // `pos + 1 < frames` guarantees both interpolation neighbours exist in
    // this block (or that the left one is `prev`, for pos in [-1, 0)).
    while (pos + 1 < frames) {
      const i0 = Math.floor(pos);
      const frac = pos - i0;
      const s0 = i0 < 0 ? prev : mono[i0];
      const s1 = mono[i0 + 1];
      this._push(s0 + (s1 - s0) * frac);
      pos += ratio;
    }
    this.pos = pos - frames; // >= -1 by the loop condition
    this.prev = mono[frames - 1];
    return true;
  }
}

registerProcessor('pcm-downsampler', PCMDownsampler);
