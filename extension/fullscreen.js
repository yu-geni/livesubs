// Put into the tab whose sound is captured (by background.js): says when the page goes into or
// out of full screen, so that the browser window can follow it (see followFullscreen there).
if (!globalThis.liveSubsWatchesFullscreen) {
  globalThis.liveSubsWatchesFullscreen = true;
  document.addEventListener('fullscreenchange', () => {
    try {
      chrome.runtime.sendMessage({ to: 'background', fullscreen: !!document.fullscreenElement }).catch(() => {});
    } catch (e) {
      // the extension was reloaded or removed: this page's copy has no one left to tell
    }
  });
}
