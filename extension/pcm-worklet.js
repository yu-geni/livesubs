// Runs in a 16 kHz AudioContext: mixes the channels to mono and posts 100 ms of signed
// 16-bit samples at a time (an ArrayBuffer of 3200 bytes). While nothing is playing it
// keeps posting silence, because the app needs the audio in real time.

const FRAME = 1600;

class PcmFrames extends AudioWorkletProcessor {
  constructor() {
    super();
    this.frame = new Int16Array(FRAME);
    this.filled = 0;
  }

  process(inputs) {
    const input = inputs[0];
    const channels = input ? input.length : 0;
    const length = channels > 0 ? input[0].length : 128;
    for (let i = 0; i < length; i++) {
      let s = 0;
      for (let c = 0; c < channels; c++) s += input[c][i];
      if (channels > 1) s /= channels;
      if (s > 1) s = 1;
      else if (s < -1) s = -1;
      this.frame[this.filled++] = s < 0 ? s * 32768 : s * 32767;
      if (this.filled === FRAME) {
        const out = this.frame.buffer;
        this.port.postMessage(out, [out]);
        this.frame = new Int16Array(FRAME);
        this.filled = 0;
      }
    }
    return true;
  }
}

registerProcessor('pcm-frames', PcmFrames);
