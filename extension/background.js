// The toolbar button turns capture of the clicked tab on and off. The audio itself is
// handled in the offscreen document (a service worker cannot hold a media stream); this
// worker only starts and stops it and shows the state on the button.

const IDLE_TITLE = 'このタブの音声を字幕にする';

async function hasOffscreen() {
  const found = await chrome.runtime.getContexts({ contextTypes: ['OFFSCREEN_DOCUMENT'] });
  return found.length > 0;
}

function ask(message) {
  return chrome.runtime.sendMessage(Object.assign({ to: 'offscreen' }, message)).catch(() => null);
}

// The offscreen document outlives this worker, so it is the one that knows what is captured.
async function capturedTab() {
  if (!(await hasOffscreen())) return null;
  const r = await ask({ type: 'status' });
  return r && r.tabId != null ? r.tabId : null;
}

// The button's look is set for the captured tab only: on every other tab it keeps saying what a
// press does there (start capturing that tab), not that the capture of another one is on.
async function show(state, detail, tabId) {
  const looks = {
    off: ['', '#666666', IDLE_TITLE],
    live: ['ON', '#1a7f37', '字幕にしています。このタブでもう一度押すと止まります。'],
    waiting: ['…', '#b26a00', 'ライブ字幕アプリにつながっていません。アプリを起動すると自動でつながります。'],
    error: ['×', '#c62828', detail || 'このタブの音声は取得できませんでした。'],
  }[state];
  const shown = (await chrome.storage.session.get('badgeTab')).badgeTab;
  if (shown != null && shown !== tabId) {        // the tab that had a look of its own goes back to idle
    await chrome.action.setBadgeText({ tabId: shown, text: '' }).catch(() => {});
    await chrome.action.setTitle({ tabId: shown, title: IDLE_TITLE }).catch(() => {});
  }
  if (state === 'off' || tabId == null) {
    await chrome.storage.session.remove('badgeTab');
    await chrome.action.setBadgeText({ text: '' });   // and the look every tab starts from
    await chrome.action.setTitle({ title: IDLE_TITLE });
    return;
  }
  await chrome.storage.session.set({ badgeTab: tabId });
  await chrome.action.setBadgeText({ tabId, text: looks[0] }).catch(() => {});
  await chrome.action.setBadgeBackgroundColor({ tabId, color: looks[1] }).catch(() => {});
  await chrome.action.setTitle({ tabId, title: looks[2] }).catch(() => {});
}

async function stop() {
  const tabId = await capturedTab();
  if (await hasOffscreen()) {
    await ask({ type: 'stop' });
    await chrome.offscreen.closeDocument().catch(() => {});
  }
  // a window made full screen with the captured page is put back now, whatever the page does
  if (tabId != null) {
    const tab = await chrome.tabs.get(tabId).catch(() => null);
    if (tab) await restoreWindow(tab.windowId);
  }
  await show('off');
}

async function start(tab) {
  // Chrome hands out a stream for its own pages (chrome://...) as well, but it only ever
  // carries silence. activeTab makes tab.url readable for ordinary pages and leaves it
  // empty for those, which is the one way to tell them apart.
  if (!/^(https?|file):/i.test(tab.url || '')) {
    await show('error', 'このタブの音声は取得できません。', tab.id);
    return;
  }
  let streamId;
  try {
    streamId = await chrome.tabCapture.getMediaStreamId({ targetTabId: tab.id });
  } catch (e) {
    await show('error', 'このタブの音声は取得できません。', tab.id);
    return;
  }
  if (!(await hasOffscreen())) {
    await chrome.offscreen.createDocument({
      url: 'offscreen.html',
      reasons: ['USER_MEDIA'],
      justification: 'タブの音声をライブ字幕アプリへ送るため',
    });
  }
  await show('waiting', undefined, tab.id);
  const r = await ask({ type: 'start', streamId, tabId: tab.id });
  if (!r || !r.ok) {
    await chrome.offscreen.closeDocument().catch(() => {});
    await show('error', r && r.error ? 'このタブの音声は取得できませんでした: ' + r.error : undefined, tab.id);
    return;
  }
  await watchFullscreen(tab.id);
}

// While a tab's sound is captured, Chrome keeps a page's full screen inside the tab: the video
// fills the tab, not the screen. fullscreen.js, put into the captured page, says when the page
// goes full screen; the window is then made full screen with it, and put back as it was after.
async function watchFullscreen(tabId) {
  await chrome.scripting.executeScript({ target: { tabId }, files: ['fullscreen.js'] }).catch(() => {});
}

async function followFullscreen(tab, on) {
  const key = 'window' + tab.windowId;   // how the window was before it was made full screen
  if (on) {
    if ((await capturedTab()) !== tab.id) return;   // not captured: Chrome's own full screen is the real one
    const ours = (await chrome.storage.session.get(key))[key];
    if (!ours) {
      // a window that is full screen and not by us is the user's own F11: leave it alone
      const win = await chrome.windows.get(tab.windowId);
      if (win.state === 'fullscreen') return;
      await chrome.storage.session.set({ [key]: win.state });
    }
    await chrome.windows.update(tab.windowId, { state: 'fullscreen' });
  } else {
    await restoreWindow(tab.windowId);
  }
}

// Puts a window that was made full screen here back as it was; nothing if it was not.
async function restoreWindow(windowId) {
  const key = 'window' + windowId;
  const was = (await chrome.storage.session.get(key))[key];
  if (!was) return;
  await chrome.storage.session.remove(key);
  await chrome.windows.update(windowId, { state: was }).catch(() => {});
}

async function toggle(tab) {
  const current = await capturedTab();
  await stop();
  if (current !== tab.id) await start(tab);   // a second press on the captured tab only stops
}

// Two quick presses must not interleave: the second one would otherwise look at a capture
// that is only half started and tear it down again. One thing at a time, and that goes for
// what the offscreen document reports as well: its "connected" can arrive after the press
// that stops the capture, and must not put the badge back on then.
let queue = Promise.resolve();
function inTurn(job) {
  queue = queue.then(job).catch(() => {});
}

chrome.action.onClicked.addListener((tab) => inTurn(() => toggle(tab)));

chrome.runtime.onMessage.addListener((m, sender) => {
  if (!m || m.to !== 'background') return;
  if (typeof m.fullscreen === 'boolean') {       // from fullscreen.js in the captured page
    if (sender.tab) inTurn(() => followFullscreen(sender.tab, m.fullscreen));
    return;
  }
  inTurn(async () => {
    if (!(await hasOffscreen())) return;       // stopped in the meantime
    if (m.state === 'ended') await stop();     // the captured tab was closed
    else await show(m.state, undefined, await capturedTab());
  });
});

// A page loaded anew in the captured tab has lost its fullscreen.js: it gets it back (where
// Chrome still lets the extension into that page).
chrome.tabs.onUpdated.addListener((tabId, info) => {
  if (info.status !== 'complete') return;
  inTurn(async () => { if ((await capturedTab()) === tabId) await watchFullscreen(tabId); });
});

// A window closed while it was full screen leaves nothing behind to put back.
chrome.windows.onRemoved.addListener((windowId) => {
  chrome.storage.session.remove('window' + windowId).catch(() => {});
});

// After a browser restart or an extension reload nothing is being captured any more.
chrome.runtime.onStartup.addListener(() => show('off'));
chrome.runtime.onInstalled.addListener(() => show('off'));
