// Holds the captured tab's audio: plays it back for the user and sends it to the
// LiveSubs app as mono 16 kHz signed 16-bit frames of 100 ms, one per WebSocket message.

const PORT = 8766;   // must match tabport in LiveSubs.ini (8766 unless set there)
const APP = 'ws://127.0.0.1:' + PORT + '/tab';
const RETRY_MS = 2000;
const REPLACED = 4001;   // the app's close code for "another tab has taken your place"

let media = null;    // the tab's MediaStream while capturing
let tabId = null;
let playback = null;
let capture = null;
let socket = null;
let retry = 0;

function tell(state) {
  chrome.runtime.sendMessage({ to: 'background', state }).catch(() => {});
}

function connect() {
  if (!media) return;
  const s = new WebSocket(APP);
  socket = s;
  s.onopen = () => tell('live');
  s.onclose = (e) => {
    if (socket !== s) return;
    socket = null;
    if (!media) return;
    if (e.code === REPLACED) {
      // a capture in another profile or browser took over: coming back would only start
      // the two taking the app from each other
      stop();
      tell('ended');
      return;
    }
    tell('waiting');   // the app is not running (yet); keep trying while the tab is captured
    retry = setTimeout(connect, RETRY_MS);
  };
}

function stop() {
  clearTimeout(retry);
  const s = socket;
  socket = null;
  if (s) s.close();
  if (media) media.getTracks().forEach((t) => t.stop());
  media = null;
  tabId = null;
  if (playback) playback.close();
  if (capture) capture.close();
  playback = capture = null;
}

async function start(m) {
  stop();
  media = await navigator.mediaDevices.getUserMedia({
    audio: { mandatory: { chromeMediaSource: 'tab', chromeMediaSourceId: m.streamId } },
    video: false,
  });
  tabId = m.tabId;
  media.getAudioTracks()[0].addEventListener('ended', () => {
    stop();
    tell('ended');
  });

  // Chrome stops playing a tab to the speakers once it is captured, so play it from here.
  playback = new AudioContext({ latencyHint: 'interactive' });
  playback.createMediaStreamSource(media).connect(playback.destination);

  // A second context at 16 kHz lets the browser do the resampling, low-pass included.
  capture = new AudioContext({ sampleRate: 16000 });
  await capture.audioWorklet.addModule('pcm-worklet.js');
  const frames = new AudioWorkletNode(capture, 'pcm-frames', {
    numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1],
  });
  frames.port.onmessage = (e) => {
    // A frame that cannot go out at once (the app is busy for a moment) is dropped rather
    // than queued: queued audio would arrive late, all at once, ahead of the live sound.
    if (socket && socket.readyState === WebSocket.OPEN && socket.bufferedAmount < 2 * 3200) socket.send(e.data);
  };
  // The worklet only runs while it leads to the destination; it writes silence there.
  const mute = capture.createGain();
  mute.gain.value = 0;
  capture.createMediaStreamSource(media).connect(frames);
  frames.connect(mute);
  mute.connect(capture.destination);

  await Promise.all([playback.resume(), capture.resume()]);
  connect();
}

chrome.runtime.onMessage.addListener((m, sender, reply) => {
  if (!m || m.to !== 'offscreen') return;
  if (m.type === 'status') reply({ tabId });
  else if (m.type === 'stop') {
    stop();
    reply({ ok: true });
  } else if (m.type === 'start') {
    start(m).then(
      () => reply({ ok: true }),
      (e) => {
        stop();
        reply({ ok: false, error: String((e && e.message) || e) });
      });
    return true;   // the reply comes later
  }
});
