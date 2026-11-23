//! The app itself: the overlay window and what the mouse does to it, the menu, the icon in the
//! notification area, and the wiring between sound, engine and picture.
//!
//! One thread owns every window and all of `App`. The sound and engine threads never touch it:
//! they put an event in a queue and post a message.
use std::{
    cell::RefCell,
    collections::VecDeque,
    mem::size_of,
    sync::{atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering::Relaxed}, Arc, Mutex},
    time::{Duration, Instant},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::{BeginPaint, ClientToScreen, DeleteDC, DeleteObject, EndPaint, InvalidateRect, ScreenToClient, HGDIOBJ, PAINTSTRUCT},
        Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW},
        System::{LibraryLoader::GetModuleHandleW, ProcessStatus::K32EmptyWorkingSet, Threading::*},
        UI::{HiDpi::{GetDpiForSystem, GetDpiForWindow},
             Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_CONTROL},
             Shell::*, WindowsAndMessaging::*},
    },
};

use crate::{
    capture::Capture,
    core::{self, history, AppConfig, Settings},
    engine::{self, Conn, EngineLink},
    mainwin::{self, Act, MainWin, Pick},
    model::Model,
    selftest,
    tabaudio::TabAudio,
    view::{self, Look, View},
};

const HINT: &str = "字幕はここに表示されます（ドラッグで移動・右クリックで設定）";
const WM_APP_EVENT: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_MOUSE_LEFT: u32 = 0x02A3; // WM_MOUSELEAVE, which lives in a feature this app does not pull in
const T_POINTER: usize = 1; // while click-through: is the pointer over the overlay?
const T_SAVE: usize = 2; // settings changed a moment ago
const T_TRIM: usize = 3; // once, after start-up
const T_TAB_RETRY: usize = 4; // the extension's port was taken
const T_TEST_END: usize = 5;
const T_PULSE: usize = 6; // the level meter, only while the normal window is on show
const T_REPAINT: usize = 7; // the normal window could not be painted: the app was busy
const T_LATE: usize = 8; // work a message found the app too busy to do
const T_BEHIND: usize = 9; // the "not keeping up" notice has been up long enough
const BEHIND_MS: u32 = 8000;
const BEHIND: &str = "処理が追いついていません（音声の一部を字幕にできませんでした）";
const ICON: &[u8] = include_bytes!("../app.ico");

// What a message could not do because the app was borrowed further up the stack. It is kept here
// rather than in `App`, which is exactly what could not be reached, and done from T_LATE.
static LATE: AtomicU32 = AtomicU32::new(0);
const L_EVENT: u32 = 1; // the engine and the sound threads have something to show
const L_SAVE: u32 = 2; // the settings changed
const L_POINTER: u32 = 4; // where the pointer is, while click-through
const L_SHOW_MAIN: u32 = 8; // the tray icon was double clicked: the normal window was asked for
const L_TRAY: u32 = 16; // Explorer restarted: the icon in the notification area has to go back

/// How often the timer that ends a self-test found the app busy (see T_TEST_END).
static TEST_END_TRIES: AtomicU32 = AtomicU32::new(0);

/// --selftest-gui: the whole app, fed from a PCM file instead of the sound card. With "live"
/// for the file it takes the sound card after all, to try the start and the end of the real thing.
pub struct Test { pub secs: u64, pub dump: String, pub port: u16, pub pcm: String, pub on_screen: bool, pub tab_port: u16, pub main: bool }

enum Ev { Engine(engine::Event), AudioStatus(String), TabActive(bool) }

/// What the sound threads and the window thread share.
struct Shared {
    hwnd: AtomicIsize,
    events: Mutex<VecDeque<Ev>>,
    tab_on: AtomicBool,
    paused: AtomicBool,
    pause_flush: AtomicI32, // frames of silence still to send after pausing: they close the open sentence
    quitting: AtomicBool,
}

impl Shared {
    fn post(&self, ev: Ev) {
        self.events.lock().unwrap().push_back(ev);
        let hwnd = HWND(self.hwnd.load(Relaxed) as *mut _);
        if !hwnd.0.is_null() { unsafe { let _ = PostMessageW(hwnd, WM_APP_EVENT, WPARAM(0), LPARAM(0)); } }
    }

    fn frame(&self, eng: &EngineLink, frame: &[u8]) {
        if !self.paused.load(Relaxed) {
            eng.send_frame(frame);
        } else if self.pause_flush.load(Relaxed) > 0 {
            self.pause_flush.fetch_sub(1, Relaxed);
            eng.send_frame(&vec![0u8; frame.len()]);
        }
    }
}

#[derive(Clone, PartialEq)]
pub enum Cmd { Pause, Source(String), JaSize(i32), EnSize(i32), Mode(usize), Bg(i32), ClickThrough, Topics, SaveHistory,
               OpenHistory, OpenExtension, NewStream, ResetPosition, AlwaysOnTop, ShowMain, ShowOverlay, Quit }

struct App {
    hwnd: HWND,
    s: Settings,
    model: Model,
    view: View,
    main: Option<MainWin>,
    main_on: bool, // the normal window is the view on show
    level: f32,
    hinst: HINSTANCE,
    shared: Arc<Shared>,
    engine: Option<Arc<EngineLink>>,
    capture: Option<Capture>,
    tab: Option<TabAudio>,
    tab_port: u16,
    tab_failed: bool,
    conn: Conn,
    conn_text: String,
    audio_text: String,
    err_text: String,
    behind_until: Option<Instant>, // the engine dropped sound a moment ago: said on the screen until then
    sys_scale: f64,
    alpha: u8,
    pill: HWND,
    pill_view: Option<View>,
    pill_shown: bool,
    away: u32,
    tray_on: bool,
    main_min: bool, // the normal window is minimized: nothing of it is drawn
    // made once and kept: the notification area and the normal window are given them again and
    // again (an icon per Explorer restart otherwise), and they are destroyed at the end
    icon_big: Option<HICON>,
    icon_small: Option<HICON>,
    test: Option<Test>,
}

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

thread_local! { static APP: RefCell<Option<App>> = const { RefCell::new(None) }; }

/// Runs `f` on the app. None while the app is busy further up the stack (a call that sent
/// this window a message) or does not exist yet.
fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|c| c.try_borrow_mut().ok().and_then(|mut o| o.as_mut().map(f)))
}

fn wide(s: &str) -> HSTRING { HSTRING::from(s) }

impl App {
    /// The engine could not keep up a moment ago and let some of the sound go (see "behind").
    fn behind(&self) -> bool { self.behind_until.map_or(false, |t| Instant::now() < t) }

    fn status(&self) -> String {
        if self.conn != Conn::Live { return self.conn_text.clone(); }
        if !self.audio_text.is_empty() && !self.shared.tab_on.load(Relaxed) { return self.audio_text.clone(); }
        if !self.err_text.is_empty() { return self.err_text.clone(); }
        if self.shared.paused.load(Relaxed) { return "一時停止中".into(); }
        if self.behind() { return BEHIND.into(); }
        String::new()
    }

    unsafe fn redraw(&mut self) {
        if self.main_on { return self.redraw_main(); } // the overlay is hidden: nothing of it is drawn
        let status = self.status();
        let entries = selftest::entries(&self.model);
        let hint = if entries.is_empty() && status.is_empty() && !self.s.seen_subtitle { HINT } else { "" };
        let look = Look { ja_size: self.s.ja_size, en_size: self.s.en_size, show_ja: self.s.show_japanese, show_en: self.s.show_english,
                          bg_percent: self.s.bg_percent, grip: !self.s.click_through };
        let done = self.view.compose(&entries, &status, hint, &look).and_then(|_| self.view.present(self.hwnd, self.alpha));
        if let Err(e) = done { core::log(&format!("draw failed: {e:?}")); }
    }

    // ---- the normal window

    /// The sidebar says more than the overlay's one line: every state has a text and a colour.
    fn main_status(&self) -> mainwin::Status {
        let tab_on = self.shared.tab_on.load(Relaxed);
        let paused = self.shared.paused.load(Relaxed);
        let (text, dot) = if self.conn == Conn::StartingEngine {
            ("エンジン起動中".to_string(), mainwin::WARN)
        } else if self.conn != Conn::Live {
            (if self.conn_text.is_empty() { "接続中".to_string() } else { self.conn_text.clone() }, mainwin::WARN)
        } else if !self.err_text.is_empty() {
            (self.err_text.clone(), mainwin::BAD)
        } else if !self.audio_text.is_empty() && !tab_on {
            (self.audio_text.clone(), mainwin::WARN)
        } else if paused {
            ("一時停止中".to_string(), mainwin::WARN)
        } else if self.behind() {
            (BEHIND.to_string(), mainwin::WARN)
        } else {
            ((if tab_on { "受信中（Chrome のタブ）" } else { "受信中" }).to_string(), mainwin::GOOD)
        };
        mainwin::Status {
            text, dot, level: self.level, paused,
            lag: if self.model.last_lag_ms >= 0 { format!("遅延 {:.1} 秒", self.model.last_lag_ms as f64 / 1000.0) } else { String::new() },
            models: self.model.models.clone(),
            tab: if tab_on { 1 } else if self.tab_failed { -1 } else { 0 },
        }
    }

    /// A minimized window has nothing to show: it is not drawn until it comes back.
    unsafe fn main_iconic(&self) -> bool {
        self.main.as_ref().map(|m| !m.hwnd.0.is_null() && IsIconic(m.hwnd).as_bool()).unwrap_or(false)
    }

    unsafe fn redraw_main(&mut self) {
        if self.main_iconic() { return; }
        let st = self.main_status();
        let (model, s) = (&self.model, &self.s);
        let Some(m) = self.main.as_mut() else { return };
        let dirty = match m.compose(model, s, &st) {
            Ok(d) => d,
            Err(e) => { core::log(&format!("main draw failed: {e:?}")); return; }
        };
        let hwnd = m.hwnd;
        if !hwnd.0.is_null() {
            let _ = InvalidateRect(hwnd, Some(&dirty.rect), false);
            if let Some(head) = &dirty.head { let _ = InvalidateRect(hwnd, Some(head), false); }
        }
    }

    /// The normal window was moved, resized, minimized or restored.
    unsafe fn main_sized(&mut self, iconic: bool) {
        let Some(m) = &self.main else { return };
        let hwnd = m.hwnd;
        if iconic {
            let _ = KillTimer(hwnd, T_PULSE); // nothing moves while it is minimized, not even the meter
            self.main_min = true;
            return;
        }
        if self.main_min {
            self.main_min = false;
            if let Some(m) = &mut self.main { m.key_dirty(); } // back on the screen: all of it, once
            if self.main_on && self.test.is_none() { SetTimer(hwnd, T_PULSE, 120, None); }
        }
        self.sync_main();
        self.redraw_main();
    }

    /// Which view is on show. The overlay window stays alive either way: it carries the icon in
    /// the notification area.
    unsafe fn show_view(&mut self, to_main: bool) {
        self.s.overlay_mode = !to_main;
        if to_main {
            if self.main.is_none() { self.make_main(); }
            let Some(m) = &self.main else {
                // without a normal window there would be nothing but the icon in the notification area
                core::log("the normal window could not be made: the overlay is shown instead");
                self.show_view(false);
                return;
            };
            let (hwnd, topmost) = (m.hwnd, if self.s.always_on_top { HWND_TOPMOST } else { HWND_NOTOPMOST });
            let _ = SetWindowPos(hwnd, topmost, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            let how = if IsIconic(hwnd).as_bool() { SW_RESTORE } else if self.test.is_some() { SW_SHOWNOACTIVATE } else { SW_SHOW };
            let _ = ShowWindow(hwnd, how);
            if self.test.is_none() { let _ = SetForegroundWindow(hwnd); }
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            self.main_on = true;
            if let Err(e) = self.view.release() { core::log(&format!("overlay release failed: {e:?}")); }
            self.sync_main();
            self.redraw_main();
            if self.test.is_none() { SetTimer(hwnd, T_PULSE, 120, None); }
        } else {
            if let Some(m) = &self.main {
                let _ = KillTimer(m.hwnd, T_PULSE);
                let _ = ShowWindow(m.hwnd, SW_HIDE);
            }
            self.main_on = false;
            if let Some(m) = &mut self.main { if let Err(e) = m.release() { core::log(&format!("main release failed: {e:?}")); } }
            self.sync_geometry(); // gives the overlay its picture back
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            self.redraw();
        }
        self.apply_click_through();
        self.save_soon();
    }

    /// The window itself, where it was left last time (unless that is no longer on any screen).
    unsafe fn make_main(&mut self) {
        let k = self.sys_scale;
        let (mut w, mut h) = (self.s.main_width.max(mainwin::MIN_W as f64), self.s.main_height.max(mainwin::MIN_H as f64));
        let (vl, vt) = (GetSystemMetrics(SM_XVIRTUALSCREEN) as f64 / k, GetSystemMetrics(SM_YVIRTUALSCREEN) as f64 / k);
        let (vr, vb) = (vl + GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64 / k, vt + GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64 / k);
        let (mut l, mut t) = (self.s.main_left, self.s.main_top);
        let seen = !l.is_nan() && !t.is_nan() && (l + w).min(vr) - l.max(vl) >= 200.0 && (t + h).min(vb) - t.max(vt) >= 100.0;
        if !seen {
            let mut wa = RECT::default();
            let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut wa as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
            let (al, at) = (wa.left as f64 / k, wa.top as f64 / k);
            let (aw, ah) = ((wa.right - wa.left) as f64 / k, (wa.bottom - wa.top) as f64 / k);
            w = w.min(aw);
            h = h.min(ah);
            l = al + (aw - w) / 2.0;
            t = at + (ah - h) / 2.0;
            (self.s.main_left, self.s.main_top, self.s.main_width, self.s.main_height) = (l, t, w, h);
        }
        let px = |v: f64| (v * k).round() as i32;
        // in a test it is never shown, and never in the taskbar either
        let ex = if self.test.is_some() { WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE } else { WINDOW_EX_STYLE(0) };
        let made = CreateWindowExW(ex, w!("LiveSubsMain"), w!("ライブ字幕"), WS_OVERLAPPEDWINDOW,
                                   px(l), px(t), px(w), px(h), None, None, self.hinst, None);
        let hwnd = match made { Ok(h) => h, Err(e) => { core::log(&format!("main window failed: {e:?}")); return; } };
        for (which, ico) in [(ICON_BIG, self.icon_big), (ICON_SMALL, self.icon_small)] {
            if let Some(i) = ico { SendMessageW(hwnd, WM_SETICON, WPARAM(which as usize), LPARAM(i.0 as isize)); }
        }
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        match MainWin::new(hwnd, r.right.max(1), r.bottom.max(1), scale, self.view.factory(), self.view.dwrite()) {
            Ok(mut m) => { m.src_label = source_label(&self.s.capture_app); self.main = Some(m); }
            Err(e) => { core::log(&format!("main window picture failed: {e:?}")); let _ = DestroyWindow(hwnd); }
        }
    }

    /// The window was moved or resized: follow with the picture and remember the place.
    unsafe fn sync_main(&mut self) {
        let Some(m) = &mut self.main else { return };
        let hwnd = m.hwnd;
        let mut c = RECT::default();
        let _ = GetClientRect(hwnd, &mut c);
        if c.right > 0 && c.bottom > 0 && (c.right != m.w || c.bottom != m.h) {
            if let Err(e) = m.resize(c.right, c.bottom) { core::log(&format!("main resize failed: {e:?}")); }
        }
        if self.test.is_some() || IsIconic(hwnd).as_bool() || IsZoomed(hwnd).as_bool() { return; }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() { return; }
        let k = self.sys_scale;
        let (l, t) = (r.left as f64 / k, r.top as f64 / k);
        let (w, h) = ((r.right - r.left) as f64 / k, (r.bottom - r.top) as f64 / k);
        if w <= 0.0 || h <= 0.0 { return; }
        if (l, t, w, h) != (self.s.main_left, self.s.main_top, self.s.main_width, self.s.main_height) {
            (self.s.main_left, self.s.main_top, self.s.main_width, self.s.main_height) = (l, t, w, h);
            self.save_soon();
        }
    }

    /// The input level, 8 times a second while the normal window is the view on show. Only the
    /// meter is redrawn, and only when it has moved enough to see.
    unsafe fn pulse(&mut self) {
        if !self.main_on || self.main_iconic() { return; }
        let rms = if self.shared.tab_on.load(Relaxed) { self.tab.as_ref().map(|t| t.last_rms()).unwrap_or(0.0) }
                  else { self.capture.as_ref().map(|c| c.last_rms()).unwrap_or(0.0) };
        let live = if self.shared.paused.load(Relaxed) { 0.0 } else { (rms * 4.0).min(1.0) };
        let mut smooth = self.level * 0.6 + live * 0.4;
        if smooth < 0.005 { smooth = 0.0; }
        if (smooth - self.level).abs() < 0.02 && !(smooth == 0.0 && self.level != 0.0) { return; }
        self.level = smooth;
        let Some(m) = &mut self.main else { return };
        match m.draw_meter(smooth) {
            Ok(r) => { let hwnd = m.hwnd; if !hwnd.0.is_null() { let _ = InvalidateRect(hwnd, Some(&r), false); } }
            Err(e) => core::log(&format!("meter draw failed: {e:?}")),
        }
    }

    /// A click in the normal window came to something. A drop-down list is not opened here: it
    /// is handed back, for the caller to show once this borrow of the app has ended.
    unsafe fn act(&mut self, a: Act) -> Option<(Pick, RECT)> {
        match a {
            Act::None => {}
            Act::Redraw => self.redraw_main(),
            Act::Cmd(c) => self.run(c),
            Act::Menu(pick, r) => return Some((pick, r)),
        }
        None
    }

    /// The entries of one drop-down list: the menu and what each entry does. The list of running
    /// apps is made here, so that it is as fresh as the moment the list was opened.
    unsafe fn menu_for(&self, pick: Pick) -> Option<(HMENU, Vec<Cmd>)> {
        let menu = CreatePopupMenu().ok()?;
        let mut cmds: Vec<Cmd> = Vec::new();
        let mut item = |text: &str, on: bool, cmd: Cmd| {
            cmds.push(cmd);
            let _ = AppendMenuW(menu, if on { MF_STRING | MF_CHECKED } else { MF_STRING }, cmds.len(), &wide(text));
        };
        match pick {
            Pick::Lang => {
                for (i, name) in ["日本語のみ", "日本語＋英語", "英語のみ"].iter().enumerate() { item(name, self.s.text_mode() == i, Cmd::Mode(i)); }
            }
            Pick::Source => {
                let current = self.s.capture_app.as_str();
                item("PC の音すべて", current.is_empty(), Cmd::Source(String::new()));
                let apps = running_apps();
                for (name, label) in &apps { item(label, name.eq_ignore_ascii_case(current), Cmd::Source(name.clone())); }
                if !current.is_empty() && !apps.iter().any(|(n, _)| n.eq_ignore_ascii_case(current)) {
                    item(&format!("{current}（起動していません）"), true, Cmd::Source(current.to_string()));
                }
            }
        }
        Some((menu, cmds))
    }

    /// The window was moved or resized (by the mouse or by us): follow with the picture and
    /// remember the place.
    unsafe fn sync_geometry(&mut self) {
        let mut r = RECT::default();
        if GetWindowRect(self.hwnd, &mut r).is_err() { return; }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 { return; }
        let resized = !self.main_on && (w != self.view.w || h != self.view.h); // hidden, it keeps no picture
        if resized {
            if let Err(e) = self.view.resize(w, h) { core::log(&format!("resize failed: {e:?}")); }
            self.redraw();
        }
        if self.test.is_some() { return; }
        let k = self.sys_scale;
        let (l, t, wd, hd) = (r.left as f64 / k, r.top as f64 / k, w as f64 / k, h as f64 / k);
        if (l, t, wd, hd) != (self.s.left, self.s.top, self.s.width, self.s.height) {
            (self.s.left, self.s.top, self.s.width, self.s.height) = (l, t, wd, hd);
            self.save_soon();
        }
        if self.pill_shown { self.place_pill(); }
    }

    unsafe fn save_soon(&mut self) {
        if self.test.is_none() { SetTimer(self.hwnd, T_SAVE, 800, None); }
    }

    /// What a message found the app too busy to do, done now that it is free again.
    unsafe fn catch_up(&mut self, what: u32) {
        if what & L_POINTER != 0 { self.pointer_tick(); }
        if what & L_SAVE != 0 { let _ = KillTimer(self.hwnd, T_SAVE); self.s.save(); }
        if what & L_EVENT != 0 { self.drain(); }
        if what & L_SHOW_MAIN != 0 { self.show_view(true); }
        if what & L_TRAY != 0 { self.tray(true); }
    }

    unsafe fn hit_test(&self, lp: LPARAM) -> u32 {
        let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
        let mut r = RECT::default();
        let _ = GetWindowRect(self.hwnd, &mut r);
        let band = self.view.px(view::EDGE_BAND) as i32;
        let grip = self.view.px(21.0) as i32;
        let (l, rt, t, b) = (x < r.left + band, x >= r.right - band, y < r.top + band, y >= r.bottom - band);
        if x >= r.right - grip && y >= r.bottom - grip { return HTBOTTOMRIGHT; }
        if t { return if l { HTTOPLEFT } else if rt { HTTOPRIGHT } else { HTTOP }; }
        if b { return if l { HTBOTTOMLEFT } else if rt { HTBOTTOMRIGHT } else { HTBOTTOM }; }
        if l { HTLEFT } else if rt { HTRIGHT } else { HTCAPTION } // anywhere else drags the window
    }

    unsafe fn apply_click_through(&mut self) {
        let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
        let t = WS_EX_TRANSPARENT.0 as isize;
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, if self.s.click_through { ex | t } else { ex & !t });
        if self.s.click_through && !self.main_on && self.test.is_none() {
            SetTimer(self.hwnd, T_POINTER, 250, None);
        } else {
            let _ = KillTimer(self.hwnd, T_POINTER);
            self.hide_pill();
        }
    }

    // ---- the way back out of click-through: a small button that appears under the pointer's window

    unsafe fn place_pill(&mut self) {
        let Some(v) = &self.pill_view else { return };
        let mut r = RECT::default();
        let _ = GetWindowRect(self.hwnd, &mut r);
        let m = self.view.px(10.0) as i32;
        let _ = SetWindowPos(self.pill, HWND_TOPMOST, r.right - v.w - m, r.top + m, v.w, v.h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }

    unsafe fn show_pill(&mut self) {
        if self.pill_shown { return; }
        if self.pill_view.is_none() {
            let made = View::new(8, 8, self.view.scale).and_then(|mut v| v.pill("クリック透過を解除").map(|_| v));
            match made {
                Ok(v) => { let _ = v.present(self.pill, self.alpha); self.pill_view = Some(v); }
                Err(e) => { core::log(&format!("unlock button failed: {e:?}")); return; }
            }
        }
        self.pill_shown = true;
        self.place_pill();
    }

    unsafe fn hide_pill(&mut self) {
        self.away = 0;
        if self.pill_shown { let _ = ShowWindow(self.pill, SW_HIDE); }
        self.pill_shown = false;
    }

    unsafe fn pointer_tick(&mut self) {
        let mut p = POINT::default();
        if GetCursorPos(&mut p).is_err() { return; }
        let inside = |h: HWND, grow: i32| {
            let mut r = RECT::default();
            GetWindowRect(h, &mut r).is_ok() && p.x >= r.left - grow && p.y >= r.top - grow && p.x < r.right + grow && p.y < r.bottom + grow
        };
        if inside(self.hwnd, 0) || (self.pill_shown && inside(self.pill, self.view.px(6.0) as i32)) {
            self.away = 0;
            self.show_pill();
        } else {
            self.away += 1;
            if self.away >= 4 { self.hide_pill(); } // a second after the pointer has left
        }
    }

    // ---- placement

    unsafe fn reset_position(&mut self) {
        let mut wa = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut wa as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let k = self.sys_scale;
        let (l, t, r, b) = (wa.left as f64 / k, wa.top as f64 / k, wa.right as f64 / k, wa.bottom as f64 / k);
        self.s.width = 900.0;
        self.s.height = 170.0;
        self.s.left = l + ((r - l - 900.0) / 2.0).max(0.0);
        self.s.top = t.max(b - 170.0 - 60.0);
    }

    unsafe fn move_to_settings(&mut self) {
        let k = self.sys_scale;
        let px = |v: f64| (v * k).round() as i32;
        let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, px(self.s.left), px(self.s.top), px(self.s.width), px(self.s.height), SWP_NOACTIVATE);
        self.sync_geometry();
    }

    // ---- menu

    unsafe fn build_menu(&self, for_tray: bool) -> Result<(HMENU, Vec<Cmd>)> {
        let mut cmds: Vec<Cmd> = Vec::new();
        let menu = CreatePopupMenu()?;
        let mut item = |m: HMENU, text: &str, checked: Option<bool>, cmd: Cmd| {
            cmds.push(cmd);
            let flags = if checked == Some(true) { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(m, flags, cmds.len(), &wide(text));
        };
        let sub = |m: HMENU, text: &str| -> HMENU {
            let s = CreatePopupMenu().unwrap_or_default();
            let _ = AppendMenuW(m, MF_POPUP, s.0 as usize, &wide(text));
            s
        };
        let sep = |m: HMENU| { let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null()); };

        item(menu, "通常表示に戻す", None, Cmd::ShowMain);
        sep(menu);
        if for_tray { item(menu, "透過モード", None, Cmd::ShowOverlay); }
        item(menu, "一時停止", Some(self.shared.paused.load(Relaxed)), Cmd::Pause);
        sep(menu);
        let src = sub(menu, "音声の取得元");
        let current = self.s.capture_app.as_str();
        item(src, "PC の音すべて", Some(current.is_empty()), Cmd::Source(String::new()));
        let apps = running_apps();
        for (name, label) in &apps { item(src, label, Some(name.eq_ignore_ascii_case(current)), Cmd::Source(name.clone())); }
        if !current.is_empty() && !apps.iter().any(|(n, _)| n.eq_ignore_ascii_case(current)) {
            item(src, &format!("{current}（起動していません）"), Some(true), Cmd::Source(current.to_string()));
        }
        let ja = sub(menu, "文字サイズ（日本語）");
        for v in core::JA_PRESETS { item(ja, &format!("{v} px"), Some(self.s.ja_size == v), Cmd::JaSize(v)); }
        let en = sub(menu, "文字サイズ（英語）");
        for v in core::EN_PRESETS { item(en, &format!("{v} px"), Some(self.s.en_size == v), Cmd::EnSize(v)); }
        let lang = sub(menu, "表示する言語");
        for (i, name) in ["日本語のみ", "日本語＋英語", "英語のみ"].iter().enumerate() { item(lang, name, Some(self.s.text_mode() == i), Cmd::Mode(i)); }
        let bg = sub(menu, "背景の濃さ");
        for v in core::BG_PRESETS { item(bg, &format!("{v} %"), Some(self.s.bg_percent == v), Cmd::Bg(v)); }
        item(menu, "クリック透過", Some(self.s.click_through), Cmd::ClickThrough);
        sep(menu);
        item(menu, "目次を作る", Some(self.s.topics), Cmd::Topics);
        item(menu, "履歴をファイルに保存", Some(self.s.save_history), Cmd::SaveHistory);
        item(menu, "保存先を開く", None, Cmd::OpenHistory);
        sep(menu);
        item(menu, "配信を切り替え（字幕をクリア）", None, Cmd::NewStream);
        item(menu, "位置をリセット", None, Cmd::ResetPosition);
        item(menu, "終了", None, Cmd::Quit);
        Ok((menu, cmds))
    }

    unsafe fn run(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Pause => {
                let paused = !self.shared.paused.load(Relaxed);
                if paused { self.shared.pause_flush.store(15, Relaxed); } // about 1.5 s of silence
                self.shared.paused.store(paused, Relaxed);
                if !paused { if let Some(e) = &self.engine { e.send_reset(false); } }
                core::log(if paused { "paused" } else { "resumed" });
            }
            Cmd::Source(app) => {
                if let Some(c) = &self.capture { c.set_target(&app); }
                self.s.capture_app = app;
                // the sidebar shows the choice too, whichever menu it was made in
                let label = source_label(&self.s.capture_app);
                if let Some(m) = &mut self.main { m.src_label = label; m.key_dirty(); }
            }
            Cmd::AlwaysOnTop => {
                self.s.always_on_top = !self.s.always_on_top;
                if let Some(m) = &self.main {
                    let z = if self.s.always_on_top { HWND_TOPMOST } else { HWND_NOTOPMOST };
                    let _ = SetWindowPos(m.hwnd, z, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
            }
            Cmd::OpenExtension => {
                let dir = core::exe_dir().join("extension");
                let _ = std::fs::create_dir_all(&dir);
                ShellExecuteW(None, w!("open"), &wide(&dir.to_string_lossy()), None, None, SW_SHOWNORMAL);
            }
            Cmd::ShowMain => { self.show_view(true); return; }
            Cmd::ShowOverlay => { self.show_view(false); return; }
            Cmd::JaSize(v) => self.s.ja_size = v,
            Cmd::EnSize(v) => self.s.en_size = v,
            Cmd::Mode(m) => self.s.set_text_mode(m),
            Cmd::Bg(v) => self.s.bg_percent = v,
            Cmd::ClickThrough => { self.s.click_through = !self.s.click_through; self.apply_click_through(); }
            Cmd::Topics => { self.s.topics = !self.s.topics; if let Some(e) = &self.engine { e.set_topics(self.s.topics); } }
            Cmd::SaveHistory => self.s.save_history = !self.s.save_history,
            Cmd::OpenHistory => {
                let dir = history::folder();
                let _ = std::fs::create_dir_all(&dir);
                ShellExecuteW(None, w!("open"), &wide(&dir.to_string_lossy()), None, None, SW_SHOWNORMAL);
            }
            Cmd::NewStream => {
                self.model.clear();
                history::new_file();
                if let Some(e) = &self.engine { e.send_reset(true); }
                if let Some(m) = &mut self.main { m.show_newest(); }
                core::log("new stream");
            }
            Cmd::ResetPosition => { self.reset_position(); self.move_to_settings(); }
            Cmd::Quit => { let _ = PostMessageW(self.hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)); return; }
        }
        self.save_soon();
        self.redraw();
    }

    // ---- what the other threads report

    unsafe fn drain(&mut self) {
        loop {
            let Some(ev) = self.shared.events.lock().unwrap().pop_front() else { break };
            match ev {
                Ev::Engine(engine::Event::State(conn, text)) => { self.conn = conn; self.conn_text = text; }
                Ev::Engine(engine::Event::Message(m)) => self.message(m),
                Ev::Engine(engine::Event::Raw(_)) => {}
                Ev::AudioStatus(t) => self.audio_text = t,
                Ev::TabActive(on) => core::log(if on { "audio now comes from the Chrome extension" } else { "audio is back on the loopback" }),
            }
        }
        if self.model.ever_had_subtitle && !self.s.seen_subtitle {
            self.s.seen_subtitle = true;
            self.save_soon();
        }
        self.redraw();
    }

    fn message(&mut self, m: engine::SubMsg) {
        let keep = self.s.save_history && self.test.is_none();
        match m.kind.as_str() {
            "ready" => {
                self.err_text.clear();
                self.model.on_ready(&m.asr, &m.mt, &m.device);
                core::log(&format!("ready: asr={} mt={} device={}", m.asr, m.mt, m.device));
            }
            "interim" => { self.err_text.clear(); self.model.on_interim(m.id, &m.ja, &m.en); }
            "final" => {
                self.err_text.clear();
                if self.model.on_final(m.id, &m.ja, &m.en, m.lag_ms) && keep && !m.ja.is_empty() { history::line(&m.ja, &m.en); }
            }
            "topic" => { if self.model.on_topic(m.id, m.seg, &m.title, &m.summary) && keep { history::topic(&m.title, &m.summary); } }
            "error" => { self.err_text = format!("サーバーエラー: {}", m.error); core::log(&format!("server error: {}", m.error)); }
            // once a minute: how much sound reached the engine and what became of it
            "stats" => core::log(&format!("engine: {}", m.summary)),
            // The engine fell so far behind that it dropped sound (the GPU is busy with something
            // else, usually): that stretch gets no subtitle, so the screen says why. The engine
            // sends this at most every five seconds while it lasts; T_BEHIND takes it down again.
            "behind" => {
                self.behind_until = Some(Instant::now() + Duration::from_millis(BEHIND_MS as u64));
                unsafe { SetTimer(self.hwnd, T_BEHIND, BEHIND_MS + 100, None); }
                core::log("engine: fell behind and dropped sound");
            }
            _ => {}
        }
    }

    unsafe fn start_tab(&mut self) {
        let Some(eng) = self.engine.clone() else { return };
        let (frames, active) = (self.shared.clone(), self.shared.clone());
        let (e1, e2) = (eng.clone(), eng);
        self.tab = TabAudio::start(self.tab_port, 2000,
            Box::new(move |f| frames.frame(&e1, f)),
            Box::new(move |on| { active.tab_on.store(on, Relaxed); e2.send_reset(false); active.post(Ev::TabActive(on)); }),
            Box::new(|t| core::log(&format!("tab: {t}"))));
        self.tab_failed = self.tab.is_none();
        if self.tab_failed { SetTimer(self.hwnd, T_TAB_RETRY, 60000, None); } else { let _ = KillTimer(self.hwnd, T_TAB_RETRY); }
    }

    unsafe fn tray(&mut self, add: bool) {
        if self.test.as_ref().map(|t| t.pcm != "live").unwrap_or(false) { return; }
        let mut nid = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: self.hwnd, uID: 1, ..Default::default() };
        if add {
            nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            nid.uCallbackMessage = WM_APP_TRAY;
            nid.hIcon = self.icon_small.unwrap_or_default();
            for (i, c) in "ライブ字幕".encode_utf16().enumerate() { nid.szTip[i] = c; }
            self.tray_on = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
            if !self.tray_on { core::log("tray icon could not be added"); }
        } else if self.tray_on {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            self.tray_on = false;
        }
    }

    /// --selftest-gui is over: say what it cost and save the picture.
    unsafe fn finish_test(&mut self) {
        let Some(t) = &self.test else { return };
        let u = selftest::usage();
        core::log(&format!("selftest-gui: {} lines, {} topics, status '{}', working set {:.0} MB, private {:.0} MB, cpu {:.1} s over {} s{}",
            self.model.items.len(), self.model.topics.len(), self.status(), u.working_set_mb, u.private_mb, u.cpu_s, t.secs,
            if t.pcm == "live" { format!(", sound card: {}", self.capture.as_ref().map(|c| { let n = c.counters(); format!("{} frames out, {} packets in, {}", n.out_samples / 1600, n.packets, c.format_note()) }).unwrap_or_default()) }
            else if t.tab_port > 0 { format!(", through the extension's port: {}", if self.shared.tab_on.load(Relaxed) { "active" } else { "NOT active" }) } else { String::new() }));
        let (main, path) = (t.main, t.dump.clone());
        let done = match (main, &self.main) {
            (true, Some(m)) => selftest::dump_px(m.w, m.h, m.pixels(), &path),
            _ => selftest::dump(&self.view, &path),
        };
        if let Err(e) = done { core::log(&format!("selftest-gui: no picture: {e}")); }
    }

    /// The same idea as `check_handling`, for the normal window: it is asked straight out what a
    /// click, a drag or a wheel turn at given coordinates comes to. No mouse, nothing on show.
    unsafe fn check_main_handling(&mut self) {
        let mut failed: Vec<&str> = Vec::new();
        let mut n = 0;
        macro_rules! check { ($name:expr, $ok:expr) => {{ n += 1; if !$ok { failed.push($name); } }}; }
        // a click that is only looked at, never run: 保存先を開く and the extension note open a folder
        macro_rules! what { ($label:expr) => {{
            let spot = self.main.as_ref().and_then(|m| m.spot($label, 0.5));
            match spot { None => None, Some((x, y)) => {
                let model = &self.model;
                let m = self.main.as_mut().unwrap();
                m.click(x, y, true, model);
                let a = m.click(x, y, false, model);
                match a { Act::Cmd(c) => Some(c), _ => None }
            } }
        }}; }
        macro_rules! press { ($label:expr) => {{ match what!($label) { Some(c) => { self.run(c.clone()); Some(c) } None => None } }}; }

        check!("the window has a picture", self.main.is_some());
        if self.main.is_none() {
            core::log("selftest-gui main: the window was never made");
            return;
        }
        check!("一時停止 pauses", press!("一時停止") == Some(Cmd::Pause) && self.shared.paused.load(Relaxed));
        check!("and then says 再開", self.main.as_ref().unwrap().spot("再開", 0.5).is_some());
        check!("再開 resumes", press!("再開") == Some(Cmd::Pause) && !self.shared.paused.load(Relaxed));
        check!("透過モード asks for the overlay", what!("透過モード") == Some(Cmd::ShowOverlay));
        let before = self.model.items.len();
        check!("配信を切り替え clears the subtitles",
               press!("配信を切り替え（字幕をクリア）") == Some(Cmd::NewStream) && self.model.items.is_empty() && before > 0);
        let top = self.s.always_on_top;
        check!("常に最前面", press!("常に最前面") == Some(Cmd::AlwaysOnTop) && self.s.always_on_top != top);
        self.run(Cmd::AlwaysOnTop);
        let topics = self.s.topics;
        check!("目次を作る", press!("目次を作る") == Some(Cmd::Topics) && self.s.topics != topics);
        self.run(Cmd::Topics);
        let hist = self.s.save_history;
        check!("履歴をファイルに保存", press!("履歴をファイルに保存") == Some(Cmd::SaveHistory) && self.s.save_history != hist);
        self.run(Cmd::SaveHistory);
        check!("保存先を開く", what!("保存先を開く") == Some(Cmd::OpenHistory));
        check!("the Chrome note opens the extension's folder", what!("Chrome のタブを 1 つだけ選ぶには（拡張機能）") == Some(Cmd::OpenExtension));
        // the two lists: what a click opens, and what the entries of that list do
        let model = &self.model;
        let m = self.main.as_mut().unwrap();
        let lang = m.spot("日本語＋英語", 0.5).or_else(|| m.spot("日本語のみ", 0.5)).or_else(|| m.spot("英語のみ", 0.5));
        let opened = lang.map(|(x, y)| { m.click(x, y, true, model); m.click(x, y, false, model) });
        check!("表示する言語 opens its list", matches!(opened, Some(Act::Menu(Pick::Lang, _))));
        let src = m.spot("PC の音すべて", 0.5);
        let opened = src.map(|(x, y)| { m.click(x, y, true, model); m.click(x, y, false, model) });
        check!("音声の取得元 opens its list", matches!(opened, Some(Act::Menu(Pick::Source, _))));
        check!("the language list has the three choices",
               self.menu_for(Pick::Lang).map(|(h, c)| { let k = GetMenuItemCount(h); let _ = DestroyMenu(h); k == 3 && c[0] == Cmd::Mode(0) && c[2] == Cmd::Mode(2) }).unwrap_or(false));
        check!("the source list starts with every sound",
               self.menu_for(Pick::Source).map(|(h, c)| { let k = GetMenuItemCount(h); let _ = DestroyMenu(h); k >= 1 && c[0] == Cmd::Source(String::new()) }).unwrap_or(false));
        // the list is opened from outside the borrow (`probe_pick_path`, run before these checks)
        let path = PICK_PATH.load(Relaxed);
        check!("a click on a list hands the request out of the borrow", path & P_ASKED != 0);
        check!("the list is built where TrackPopupMenu would be called", path & P_REACHED != 0);
        check!("and the app is free at that point, for the messages dispatched while it is open", path & P_FREE != 0);
        // the icon in the notification area: the menu on the right button, the window on a double click
        check!("the tray's right button asks for the menu", tray_click(WM_RBUTTONUP) == Tray::Menu);
        check!("a single left click on the tray does nothing", tray_click(WM_LBUTTONUP) == Tray::Nothing);
        check!("a double click on the tray shows the normal window", tray_click(WM_LBUTTONDBLCLK) == Tray::Main);
        // and the two of them that the app may be too busy for: kept, not dropped. The app is
        // held by these checks, so the handler finds it busy exactly as it would then.
        LATE.store(0, Relaxed);
        wnd_proc(self.hwnd, WM_APP_TRAY, WPARAM(0), LPARAM(WM_LBUTTONDBLCLK as isize));
        check!("a double click on the tray while the app is busy is kept", LATE.load(Relaxed) & L_SHOW_MAIN != 0);
        LATE.store(0, Relaxed);
        let taskbar = TASKBAR_CREATED.load(Relaxed);
        wnd_proc(self.hwnd, taskbar, WPARAM(0), LPARAM(0));
        check!("and so is putting the tray icon back after Explorer restarted",
               taskbar != 0 && LATE.load(Relaxed) & L_TRAY != 0);
        LATE.store(0, Relaxed);
        let _ = KillTimer(self.hwnd, T_LATE); // nothing of the two is really wanted
        // the source, changed from the tray or the overlay's menu, reaches the sidebar
        let src_was = self.main.as_ref().unwrap().src_label.clone();
        self.run(Cmd::Source("chrome".into()));
        check!("a source chosen elsewhere reaches the sidebar's label", {
            let now = &self.main.as_ref().unwrap().src_label;
            now != &src_was && now.contains("chrome")
        });
        self.run(Cmd::Source(String::new()));
        check!("and PC の音すべて empties it again", self.main.as_ref().unwrap().src_label.is_empty());
        // sliders: a click a quarter along sets a quarter of the range
        let quarter = |lo: i32, hi: i32| lo + (((hi - lo) as f32) * 0.25).round() as i32;
        macro_rules! slide { ($label:expr, $frac:expr) => {{
            let spot = self.main.as_ref().and_then(|m| m.spot($label, $frac));
            match spot { None => None, Some((x, y)) => {
                let model = &self.model;
                let a = self.main.as_mut().unwrap().click(x, y, true, model);
                self.main.as_mut().unwrap().click(x, y, false, model);
                match a { Act::Cmd(c) => { self.run(c.clone()); Some(c) } _ => None }
            } }
        }}; }
        // a pixel is coarser than a step of the range, so the value may land one beside
        check!("文字サイズ（日本語）slider", matches!(slide!("文字サイズ（日本語）", 0.25), Some(Cmd::JaSize(v)) if (v - quarter(16, 80)).abs() <= 1) && (self.s.ja_size - quarter(16, 80)).abs() <= 1);
        check!("文字サイズ（英語）slider", matches!(slide!("文字サイズ（英語）", 0.25), Some(Cmd::EnSize(v)) if (v - quarter(10, 60)).abs() <= 1) && (self.s.en_size - quarter(10, 60)).abs() <= 1);
        // a drag holds the mouse: it goes on past the window's edge and ends on the button
        check!("a slider drag outlives the pointer leaving the window", {
            let spot = self.main.as_ref().and_then(|m| m.spot("文字サイズ（日本語）", 0.5));
            match spot { None => false, Some((x, y)) => {
                let model = &self.model;
                let m = self.main.as_mut().unwrap();
                m.click(x, y, true, model);
                let held = m.dragging();
                m.leave(); // the pointer crossed the edge, with the button still down
                let still = m.dragging();
                let follows = matches!(m.mouse_move(x - 60, y, model), Act::Cmd(Cmd::JaSize(_)));
                m.click(x - 60, y, false, model);
                held && still && follows && !m.dragging()
            } }
        });
        check!("and a drag ends when the mouse is taken away", {
            let spot = self.main.as_ref().and_then(|m| m.spot("文字サイズ（日本語）", 0.5));
            match spot { None => false, Some((x, y)) => {
                let model = &self.model;
                let m = self.main.as_mut().unwrap();
                m.click(x, y, true, model);
                let held = m.dragging();
                m.lost_capture();
                held && !m.dragging()
            } }
        });
        let was = self.main.as_ref().unwrap().expanded;
        check!("透過モードの設定 opens", { let _ = what!("透過モードの設定"); self.redraw_main(); self.main.as_ref().unwrap().expanded != was });
        self.main.as_mut().unwrap().settings_to_end(); // what it folds out may need scrolling to
        self.redraw_main();
        check!("背景の濃さ slider", matches!(slide!("背景の濃さ", 0.25), Some(Cmd::Bg(v)) if (v - 25).abs() <= 1) && (self.s.bg_percent - 25).abs() <= 1);
        let ct = self.s.click_through;
        check!("クリック透過", press!("クリック透過") == Some(Cmd::ClickThrough) && self.s.click_through != ct);
        self.run(Cmd::ClickThrough);
        // the 設定 panel's scroll bar is dragged like the transcript's
        check!("the 設定 scroll bar drags", {
            let spot = self.main.as_ref().and_then(|m| m.settings_bar_spot(0.9));
            match spot { None => false, Some((x, y)) => {
                let model = &self.model;
                let m = self.main.as_mut().unwrap();
                m.click(x, y, true, model);
                let (held, down) = (m.dragging(), m.settings_off());
                m.mouse_move(x, (y as f32 - (m.h as f32) * 0.5) as i32, model);
                let up = m.settings_off();
                m.click(x, y, false, model);
                held && down > 0.0 && up < down && !m.dragging()
            } }
        });
        // and it lands under the pointer: the drawing takes the thumb's own height off the
        // travel, so the press has to take it off too, or the thumb runs away from the pointer
        check!("the 設定 thumb stays where the pointer puts it", {
            let spot = self.main.as_ref().and_then(|m| m.settings_bar_spot(0.9));
            match spot { None => false, Some((x, y)) => {
                let model = &self.model;
                let m = self.main.as_mut().unwrap();
                m.click(x, y, true, model);
                m.click(x, y, false, model);
                match m.settings_thumb() {
                    None => false,
                    Some((ty, th)) => {
                        // pressing on the middle of the thumb must leave it where it is
                        let middle = (ty + th / 2.0) as i32;
                        let was = m.settings_off();
                        m.click(x, middle, true, model);
                        let now = m.settings_off();
                        m.click(x, middle, false, model);
                        (now - was).abs() <= 1.0
                    }
                }
            } }
        });
        self.main.as_mut().unwrap().settings_to_end();
        self.redraw_main();
        // the transcript: following, the wheel, the jump button, a 目次 entry
        self.model.on_ready("asr", "mt", "cuda"); // a connection of its own, after the stream was switched
        for (i, (ja, en)) in crate::samples::SAMPLES.iter().enumerate() { self.model.on_final(i as i32, ja, en, 1200); }
        self.model.on_topic(0, 3, "テスト話題", "この話題の説明");
        self.redraw_main();
        check!("a fresh transcript follows the newest line", self.main.as_ref().unwrap().following());
        let (model, s) = (&self.model, &self.s);
        let m = self.main.as_mut().unwrap();
        let (tx, ty) = m.transcript_spot();
        m.wheel(tx, ty, 120, false, model, s);
        check!("the wheel up stops following", !self.main.as_ref().unwrap().following());
        check!("and the ↓ 最新へ button is shown", self.main.as_ref().unwrap().spot("↓ 最新へ", 0.5).is_some());
        let spot = self.main.as_ref().unwrap().spot("↓ 最新へ", 0.5);
        if let Some((x, y)) = spot {
            let model = &self.model;
            let m = self.main.as_mut().unwrap();
            m.click(x, y, true, model);
            m.click(x, y, false, model);
        }
        check!("↓ 最新へ follows again", self.main.as_ref().unwrap().following());
        self.redraw_main();
        let spot = self.main.as_ref().unwrap().topic_spot(0);
        let target = self.model.topics.first().and_then(|t| self.model.topic_target(t));
        if let Some((x, y)) = spot {
            let model = &self.model;
            let m = self.main.as_mut().unwrap();
            m.click(x, y, true, model);
        }
        check!("a 目次 entry jumps to its line",
               spot.is_some() && target == Some(self.main.as_ref().unwrap().top_row()) && !self.main.as_ref().unwrap().following());
        check!("Ctrl and the wheel change the size", {
            let (model, s) = (&self.model, &self.s);
            let (tx, ty) = self.main.as_ref().unwrap().transcript_spot();
            let want = (s.ja_size + 2).clamp(16, 80);
            self.main.as_mut().unwrap().wheel(tx, ty, 120, true, model, s) == Act::Cmd(Cmd::JaSize(want))
        });
        // the size of the window
        let hwnd = self.main.as_ref().unwrap().hwnd;
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let _ = SetWindowPos(hwnd, None, r.left, r.top, 900, 520, SWP_NOZORDER | SWP_NOACTIVATE);
        self.sync_main();
        self.redraw_main();
        let m = self.main.as_ref().unwrap();
        let mut c = RECT::default();
        let _ = GetClientRect(hwnd, &mut c);
        check!("resizing lays the window out again",
               m.w == c.right && m.h == c.bottom && m.sidebar_left() == c.right - m.px(300.0) as i32);
        check!("5000 lines are kept", crate::model::MAX_ITEMS == 5000);
        // a new 遅延 text comes with every sentence: it must not lay the sidebar out again
        self.redraw_main();
        let (laid, side) = { let m = self.main.as_ref().unwrap(); (m.rows_built, m.sidebar_left()) };
        self.model.last_lag_ms = 2345;
        check!("a new 遅延 text redraws the status block alone", {
            let st = self.main_status();
            let (model, s) = (&self.model, &self.s);
            let m = self.main.as_mut().unwrap();
            match m.compose(model, s, &st) {
                Ok(d) => d.head.map(|h| h.left == side && h.bottom < m.h / 2).unwrap_or(false)
                         && d.rect.right <= side + 1 && m.rows_built - laid == 3, // the three texts of the block
                Err(_) => false,
            }
        });
        // The divider between transcript and sidebar runs down the status block's own left
        // edge: the block drawn alone must leave that column as a full redraw leaves it.
        check!("and the divider beside it is the one a full redraw draws", {
            let col = |m: &MainWin| {
                let (w, x) = (m.w, m.sidebar_left());
                let px = m.pixels();
                (0..m.h).map(|y| {
                    let i = ((y * w + x) * 4) as usize;
                    [px[i], px[i + 1], px[i + 2], px[i + 3]]
                }).collect::<Vec<[u8; 4]>>()
            };
            let head_only = col(self.main.as_ref().unwrap());
            let st = self.main_status();
            let drawn = {
                let (model, s) = (&self.model, &self.s);
                let m = self.main.as_mut().unwrap();
                m.key_dirty(); // the same status, this time drawn in full
                m.compose(model, s, &st).is_ok()
            };
            drawn && col(self.main.as_ref().unwrap()) == head_only
        });
        // a row taller than the view: the wheel has to reach its bottom all the same
        self.model.on_final(4242, &"長い行です。".repeat(100), "a very tall row", 1200);
        self.main.as_mut().unwrap().show_newest();
        self.redraw_main();
        check!("the last row, taller than the view, can be scrolled to its end", {
            let (model, s) = (&self.model, &self.s);
            let m = self.main.as_mut().unwrap();
            let (tx, ty) = m.transcript_spot();
            m.wheel(tx, ty, 360, false, model, s); // up, into the middle of the tall row
            let away = !m.following() && m.top_row() + 1 == model.items.len();
            for _ in 0..200 { if m.following() { break; } m.wheel(tx, ty, -1200, false, model, s); }
            away && m.following()
        });
        // the transcript's bar is dragged the way the 設定 one is: a press half a thumb above
        // the end of the bar is a thumb at the very bottom, which is the newest line
        check!("a press at the end of the transcript bar follows the newest line", {
            let (model, s) = (&self.model, &self.s);
            let m = self.main.as_mut().unwrap();
            let (tx, ty) = m.transcript_spot();
            m.wheel(tx, ty, 1200, false, model, s); // up, away from the newest line
            let away = !m.following();
            match m.transcript_thumb() {
                None => false,
                Some((_, th)) => {
                    let (x, y) = (m.sidebar_left() - 4, (m.h as f32 - th / 2.0) as i32);
                    m.click(x, y, true, model);
                    let back = m.following();
                    m.click(x, y, false, model);
                    away && back
                }
            }
        });
        // minimized: nothing is composed until it is back
        let _ = ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
        self.main_sized(true);
        let laid = self.main.as_ref().unwrap().rows_built;
        self.model.on_final(4243, "最小化のあいだに来た行", "a line that arrived while minimized", 1200);
        self.redraw_main();
        self.pulse();
        check!("a minimized window is not composed", IsIconic(hwnd).as_bool() && self.main.as_ref().unwrap().rows_built == laid);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        self.main_sized(false);
        check!("and is drawn again when it comes back", !IsIconic(hwnd).as_bool() && self.main.as_ref().unwrap().rows_built > laid);
        // settings.ini: written beside and renamed over, so an interrupted save cannot empty it
        let dir = std::env::temp_dir().join("livesubs-selftest-ini");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let (good, blocked) = (dir.join("settings.ini"), dir.join("blocked.ini"));
        let _ = std::fs::write(&good, "old");
        let _ = std::fs::create_dir_all(&blocked); // a rename over it cannot work
        check!("a save that cannot finish leaves what was there", {
            let failed = self.s.save_to(&blocked).is_err();
            failed && blocked.is_dir() && !dir.join("blocked.ini.new").exists()
                && std::fs::read_to_string(&good).unwrap_or_default() == "old"
        });
        check!("a saved file reads back the same", {
            let saved = self.s.save_to(&good).is_ok();
            let back = Settings::load_from(&good);
            saved && !dir.join("settings.ini.new").exists()
                && (back.ja_size, back.en_size, back.bg_percent, back.text_mode(), back.capture_app.clone())
                   == (self.s.ja_size, self.s.en_size, self.s.bg_percent, self.s.text_mode(), self.s.capture_app.clone())
        });
        let _ = std::fs::remove_dir_all(&dir);
        check!("the picture is copied to the window", {
            let m = self.main.as_ref().unwrap();
            let (w, h) = (m.w, m.h);
            match crate::mainwin::surface(w, h) {
                Ok((dc, bmp, bits)) => {
                    m.blit(dc, &RECT { left: 0, top: 0, right: w, bottom: h });
                    let got = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
                    let same = got == m.pixels();
                    let _ = DeleteDC(dc);
                    let _ = DeleteObject(HGDIOBJ(bmp.0));
                    same
                }
                Err(_) => false,
            }
        });
        // and back to the overlay, and here again
        self.show_view(false);
        check!("the overlay comes back", !self.main_on && IsWindowVisible(self.hwnd).as_bool() && !IsWindowVisible(hwnd).as_bool() && self.s.overlay_mode);
        check!("with its picture, while the hidden window keeps none", self.view.w > 1 && self.main.as_ref().map(|m| m.w == 1 && m.h == 1).unwrap_or(false));
        self.show_view(true);
        check!("and the normal window after it", self.main_on && !self.s.overlay_mode);
        check!("with its picture, while the hidden overlay keeps none", self.view.w == 1 && self.main.as_ref().map(|m| m.w > 1).unwrap_or(false));
        core::log(&format!("selftest-gui main: {} of {n} checks passed{}", n - failed.len(),
            if failed.is_empty() { String::new() } else { format!("; FAILED: {}", failed.join(" | ")) }));
    }

    /// What the mouse and the menu do, tried without a mouse: the window is asked directly.
    unsafe fn check_handling(&mut self) {
        let mut failed: Vec<&str> = Vec::new();
        let mut n = 0;
        let mut check = |name: &'static str, ok: bool| { n += 1; if !ok { failed.push(name); } };
        let mut r = RECT::default();
        let _ = GetWindowRect(self.hwnd, &mut r);
        let at = |x: i32, y: i32| LPARAM((((y as u16) as isize) << 16) | (x as u16) as isize);
        let (mx, my) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        check("the middle drags", self.hit_test(at(mx, my)) == HTCAPTION);
        check("left edge resizes", self.hit_test(at(r.left + 2, my)) == HTLEFT);
        check("top edge resizes", self.hit_test(at(mx, r.top + 1)) == HTTOP);
        check("top right corner resizes", self.hit_test(at(r.right - 2, r.top + 2)) == HTTOPRIGHT);
        check("the grip resizes", self.hit_test(at(r.right - 12, r.bottom - 12)) == HTBOTTOMRIGHT);
        match self.build_menu(false) {
            Ok((menu, cmds)) => { check("menu has its entries", cmds.len() >= 31 && GetMenuItemCount(menu) == 18); let _ = DestroyMenu(menu); }
            Err(_) => check("menu can be built", false),
        }
        match self.build_menu(true) {
            Ok((menu, cmds)) => { check("the tray menu leads to both views", GetMenuItemCount(menu) == 19 && cmds.contains(&Cmd::ShowMain) && cmds.contains(&Cmd::ShowOverlay)); let _ = DestroyMenu(menu); }
            Err(_) => check("the tray menu can be built", false),
        }
        check("icon for the notification area", self.icon_small.is_some() && self.icon_big.is_some());
        let through = |h: HWND| GetWindowLongPtrW(h, GWL_EXSTYLE) & WS_EX_TRANSPARENT.0 as isize != 0;
        self.run(Cmd::ClickThrough);
        check("click-through off: the window takes the mouse", !self.s.click_through && !through(self.hwnd));
        self.run(Cmd::ClickThrough);
        check("click-through on: the mouse goes through", self.s.click_through && through(self.hwnd));
        self.show_pill();
        let mut p = RECT::default();
        let _ = GetWindowRect(self.pill, &mut p);
        check("the way back is shown inside the top right corner",
              self.pill_shown && IsWindowVisible(self.pill).as_bool() && p.right > p.left + 50 && p.right <= r.right && p.left >= r.left && p.top >= r.top);
        self.hide_pill();
        check("and hidden again", !self.pill_shown && !IsWindowVisible(self.pill).as_bool());
        self.run(Cmd::Mode(0));
        check("Japanese only", self.s.text_mode() == 0 && !self.s.show_english);
        self.run(Cmd::Mode(2));
        check("English only", self.s.text_mode() == 2 && !self.s.show_japanese);
        self.run(Cmd::Mode(1));
        self.run(Cmd::JaSize(28));
        check("another size is drawn", self.s.ja_size == 28 && (self.model.items.is_empty() || self.view.last_shown > 0));
        self.run(Cmd::Pause);
        // a word about the sound device goes before the pause in the status line, as it always did
        check("paused", self.shared.paused.load(Relaxed) && self.shared.pause_flush.load(Relaxed) == 15
              && (self.conn != Conn::Live || !self.audio_text.is_empty() || self.status() == "一時停止中"));
        self.run(Cmd::Pause);
        check("resumed", !self.shared.paused.load(Relaxed));
        let lines = self.model.items.len();
        self.run(Cmd::NewStream);
        check("a new stream starts empty", self.model.items.is_empty() && (lines == 0 || self.view.last_shown == 0));
        let _ = SetWindowPos(self.hwnd, None, r.left, r.top, 700, 400, SWP_NOZORDER | SWP_NOACTIVATE);
        self.sync_geometry();
        check("the picture follows the window's size", self.view.w == 700 && self.view.h == 400);
        core::log(&format!("selftest-gui handling: {} of {n} checks passed{}", n - failed.len(),
            if failed.is_empty() { String::new() } else { format!("; FAILED: {}", failed.join(" | ")) }));
    }

    unsafe fn shutdown(&mut self) {
        let _ = KillTimer(self.hwnd, T_SAVE);
        if let Some(m) = self.main.take() {
            let hwnd = m.hwnd;
            drop(m);
            let _ = DestroyWindow(hwnd);
        }
        if self.test.is_none() { self.s.save(); }
        self.tray(false);
        if let Some(c) = self.capture.take() { c.stop(); }
        if let Some(t) = self.tab.take() { t.stop(); }
        self.shared.quitting.store(true, Relaxed);
        if let Some(mut e) = self.engine.take() {
            // The sound sources' closures are gone with them, which leaves this the last holder
            // (the self-test's feeder needs a moment to notice).
            let mut stopped = false;
            for _ in 0..50 {
                match Arc::try_unwrap(e) {
                    Ok(link) => { link.stop(); stopped = true; break; }
                    Err(still) => { e = still; std::thread::sleep(Duration::from_millis(10)); }
                }
            }
            if !stopped { core::log("the engine link is still shared; its socket closes with the last holder"); }
        }
        for ico in [self.icon_big.take(), self.icon_small.take()].into_iter().flatten() { let _ = DestroyIcon(ico); }
        core::log("quitting");
    }
}

/// The icon in the size asked for, from the .ico that is built into the exe.
unsafe fn icon(size: i32) -> Option<HICON> {
    let count = u16::from_le_bytes([ICON[4], ICON[5]]) as usize;
    let mut best: Option<(i32, usize, usize)> = None;
    for i in 0..count {
        let e = &ICON[6 + 16 * i..22 + 16 * i];
        let w = if e[0] == 0 { 256 } else { e[0] as i32 };
        let len = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
        let at = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        // the smallest one that is big enough, else the biggest there is
        let better = match best { None => true, Some((bw, _, _)) => if bw >= size { w >= size && w < bw } else { w > bw } };
        if better { best = Some((w, at, len)); }
    }
    let (_, at, len) = best?;
    CreateIconFromResourceEx(&ICON[at..at + len], true, 0x0003_0000, size, size, LR_DEFAULTCOLOR).ok()
}

/// What can be picked as the source of the sound: programs with a window of their own, Store
/// apps (their windows belong to ApplicationFrameHost; the app is the process behind a child
/// window), and anything with a sound session, such as a player in the notification area.
/// (process name, label), the ones playing now first and marked so.
unsafe fn running_apps() -> Vec<(String, String)> {
    unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> BOOL {
        let found = &mut *(lp.0 as *mut Vec<(HWND, u32)>);
        if IsWindowVisible(h).as_bool() && GetWindow(h, GW_OWNER).map(|o| o.0.is_null()).unwrap_or(true) && GetWindowTextLengthW(h) > 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            if pid != 0 { found.push((h, pid)); }
        }
        TRUE
    }
    unsafe extern "system" fn behind(h: HWND, lp: LPARAM) -> BOOL {
        let (host, app) = &mut *(lp.0 as *mut (u32, u32));
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid != 0 && pid != *host { *app = pid; return FALSE; }
        TRUE
    }
    let mut wins: Vec<(HWND, u32)> = Vec::new();
    let _ = EnumWindows(Some(each), LPARAM(&mut wins as *mut _ as isize));
    let mut pids: Vec<(u32, bool)> = Vec::new(); // (process, playing now)
    let mut add = |pid: u32, playing: bool| match pids.iter_mut().find(|(p, _)| *p == pid) {
        Some(e) => e.1 |= playing,
        None => pids.push((pid, playing)),
    };
    for (h, pid) in &wins {
        if process_name(*pid).map(|(n, _)| n.eq_ignore_ascii_case("ApplicationFrameHost")).unwrap_or(false) {
            let mut pair = (*pid, 0u32);
            let _ = EnumChildWindows(*h, Some(behind), LPARAM(&mut pair as *mut _ as isize));
            if pair.1 != 0 { add(pair.1, false); }
        } else {
            add(*pid, false);
        }
    }
    for (pid, playing) in crate::capture::sound_sessions() { add(pid, playing); }
    let me = GetCurrentProcessId();
    let own = process_name(me).map(|(n, _)| n).unwrap_or_default(); // this app, in any copy: its capture shows as a sound session
    let mut apps: Vec<(String, String, bool)> = Vec::new();
    for (pid, playing) in pids {
        if pid == me { continue; }
        let Some((name, path)) = process_name(pid) else { continue };
        if name.is_empty() || name.eq_ignore_ascii_case(&own)
            || ["explorer", "ApplicationFrameHost", "TextInputHost", "SystemSettings"].contains(&name.as_str()) { continue; }
        if let Some(a) = apps.iter_mut().find(|(n, _, _)| n.eq_ignore_ascii_case(&name)) { a.2 |= playing; continue; }
        let label = match description(&path) { Some(d) if !d.is_empty() => format!("{d}（{name}）"), _ => name.clone() };
        apps.push((name, label, playing));
    }
    apps.sort_by_key(|(n, _, playing)| (!*playing, n.to_lowercase()));
    apps.into_iter().map(|(n, l, playing)| (n, if playing { format!("{l}（再生中）") } else { l })).collect()
}

/// (name without ".exe", full path) of a running process.
unsafe fn process_name(pid: u32) -> Option<(String, String)> {
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
    let _ = CloseHandle(h);
    if !ok { return None; }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let file = path.rsplit('\\').next().unwrap_or("");
    let name = file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file).to_string();
    Some((name, path))
}

/// The "file description" a program carries ("Google Chrome" for chrome.exe).
unsafe fn description(path: &str) -> Option<String> {
    let p = wide(path);
    let size = GetFileVersionInfoSizeW(&p, None);
    if size == 0 { return None; }
    let mut data = vec![0u8; size as usize];
    GetFileVersionInfoW(&p, 0, size, data.as_mut_ptr() as *mut _).ok()?;
    let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut len = 0u32;
    if !VerQueryValueW(data.as_ptr() as *const _, w!("\\VarFileInfo\\Translation"), &mut ptr, &mut len).as_bool() || len < 4 { return None; }
    let t = std::slice::from_raw_parts(ptr as *const u16, 2);
    let key = wide(&format!("\\StringFileInfo\\{:04x}{:04x}\\FileDescription", t[0], t[1]));
    if !VerQueryValueW(data.as_ptr() as *const _, &key, &mut ptr, &mut len).as_bool() || len == 0 { return None; }
    let s = std::slice::from_raw_parts(ptr as *const u16, len as usize);
    Some(String::from_utf16_lossy(s).trim_end_matches('\0').trim().to_string())
}

/// The label the 音声の取得元 list shows for a saved choice, without opening the list.
unsafe fn source_label(app: &str) -> String {
    if app.is_empty() { return String::new(); }
    match running_apps().into_iter().find(|(n, _)| n.eq_ignore_ascii_case(app)) {
        Some((_, label)) => label.trim_end_matches("（再生中）").to_string(), // that changes; the label stays
        None => format!("{app}（起動していません）"),
    }
}

/// --selftest-sources <report>: the list the source menus offer, written to a file.
pub unsafe fn sources_report(path: &str) -> Result<()> {
    let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
    let text: String = running_apps().iter().map(|(n, l)| format!("{n}\t{l}\n")).collect();
    std::fs::write(path, text).map_err(|_| Error::from_win32())
}

unsafe fn show_menu(hwnd: HWND, for_tray: bool) {
    let Some(Ok((menu, cmds))) = with(|a| a.build_menu(for_tray)) else { return };
    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    let _ = SetForegroundWindow(hwnd); // or the menu stays open when the user clicks elsewhere
    let picked = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, p.x, p.y, 0, hwnd, None).0 as usize;
    let _ = DestroyMenu(menu);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    if picked > 0 && picked <= cmds.len() {
        let cmd = cmds[picked - 1].clone();
        with(|a| a.run(cmd));
    }
}

/// A drop-down list of the normal window, shown as a real menu. Built in a borrow of its own and
/// shown with none held: TrackPopupMenu runs a message loop, and every other message of this
/// thread is dispatched through it while the list is open.
unsafe fn show_pick_menu(hwnd: HWND, pick: Pick, at: RECT) {
    let Some((testing, built)) = with(|a| (a.test.is_some(), a.menu_for(pick))) else { return };
    let Some((menu, cmds)) = built else { return };
    let mut p = POINT { x: at.left, y: at.bottom };
    let _ = ClientToScreen(hwnd, &mut p);
    // Below is where TrackPopupMenu runs its own message loop and the app has to be free. A
    // test may put nothing on the screen, so the list is thrown away again and whether the
    // app is free at this very point - which is what the check is about - is recorded here.
    if testing {
        PICK_PATH.fetch_or(P_REACHED | if with(|_| ()).is_some() { P_FREE } else { 0 }, Relaxed);
        let _ = DestroyMenu(menu);
        return;
    }
    let _ = SetForegroundWindow(hwnd);
    let picked = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTBUTTON | TPM_NONOTIFY, p.x, p.y, 0, hwnd, None).0 as usize;
    let _ = DestroyMenu(menu);
    if picked > 0 && picked <= cmds.len() {
        let cmd = cmds[picked - 1].clone();
        with(|a| a.run(cmd));
    }
}

/// What a click on the icon in the notification area comes to. As in the C# app: the menu on the
/// right button and the normal window on a double click. A single left click does nothing, or it
/// would open the menu under the second half of that double click.
#[derive(PartialEq)]
enum Tray { Nothing, Menu, Main }

fn tray_click(what: u32) -> Tray {
    match what {
        WM_RBUTTONUP => Tray::Menu,
        WM_LBUTTONDBLCLK => Tray::Main,
        _ => Tray::Nothing,
    }
}

/// Work that a message could not do because the app was busy: kept, and done from a timer once
/// it is free. A timer rather than another message, which would spin the thread meanwhile.
unsafe fn later(hwnd: HWND, what: u32) {
    LATE.fetch_or(what, Relaxed);
    SetTimer(hwnd, T_LATE, 30, None);
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => return LRESULT(with(|a| a.hit_test(lp)).unwrap_or(HTCAPTION) as isize),
        WM_NCRBUTTONDOWN => return LRESULT(0),
        WM_NCRBUTTONUP | WM_RBUTTONUP => { show_menu(hwnd, false); return LRESULT(0); }
        WM_GETMINMAXINFO => {
            let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
            let info = &mut *(lp.0 as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT { x: (view::MIN_W * scale) as i32, y: (view::MIN_H * scale) as i32 };
            return LRESULT(0);
        }
        WM_SIZE | WM_MOVE => { with(|a| a.sync_geometry()); }
        WM_DPICHANGED => {
            let r = *(lp.0 as *const RECT);
            // the way back out of click-through is drawn at the old scale: away with it, or it
            // keeps its old size and place until the pointer has left for a second
            with(|a| { a.view.set_scale((wp.0 & 0xFFFF) as f32 / 96.0); a.hide_pill(); a.pill_view = None; });
            let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            with(|a| { a.sync_geometry(); a.redraw(); });
            return LRESULT(0);
        }
        WM_TIMER => {
            if wp.0 == T_LATE {
                let _ = KillTimer(hwnd, T_LATE);
                let what = LATE.swap(0, Relaxed);
                if with(|a| a.catch_up(what)).is_none() { later(hwnd, what); }
                return LRESULT(0);
            }
            if wp.0 == T_TEST_END {
                // Busy here would mean the checks are skipped and the test ends as if it had
                // passed: ask again in a moment, and leave a failure in the log if the app
                // never gets free.
                if with(|_| ()).is_none() {
                    if TEST_END_TRIES.fetch_add(1, Relaxed) < 100 {
                        SetTimer(hwnd, T_TEST_END, 30, None);
                        return LRESULT(0);
                    }
                    let _ = KillTimer(hwnd, T_TEST_END);
                    core::log("selftest-gui: FAILED - the app stayed busy at the end of the test; no checks were run");
                    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                    return LRESULT(0);
                }
                let _ = KillTimer(hwnd, T_TEST_END);
                let main = match with(|a| { a.finish_test(); a.test.as_ref().map(|t| t.main).unwrap_or(false) }) {
                    Some(m) => m,
                    None => {
                        core::log("selftest-gui: FAILED - the app was busy at the end of the test; no checks were run");
                        let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                        return LRESULT(0);
                    }
                };
                if main { probe_pick_path(); } // needs the app free, so it is run from here
                if with(|a| if main { a.check_main_handling() } else { a.check_handling() }).is_none() {
                    core::log("selftest-gui: FAILED - the app was busy; the checks were not run");
                }
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                return LRESULT(0);
            }
            let ran = with(|a| match wp.0 {
                T_POINTER => a.pointer_tick(),
                T_SAVE => { let _ = KillTimer(hwnd, T_SAVE); a.s.save(); }
                T_TRIM => { let _ = KillTimer(hwnd, T_TRIM); let _ = K32EmptyWorkingSet(GetCurrentProcess()); }
                T_TAB_RETRY => a.start_tab(),
                T_BEHIND => { let _ = KillTimer(hwnd, T_BEHIND); a.redraw(); }
                _ => {}
            });
            // the app was busy: these two are kept rather than lost until the timer comes round again
            if ran.is_none() {
                match wp.0 { T_POINTER => later(hwnd, L_POINTER), T_SAVE => later(hwnd, L_SAVE), _ => {} }
            }
            return LRESULT(0);
        }
        WM_APP_EVENT => { if with(|a| a.drain()).is_none() { later(hwnd, L_EVENT); } return LRESULT(0); }
        WM_APP_TRAY => {
            match tray_click((lp.0 & 0xFFFF) as u32) {
                Tray::Menu => show_menu(hwnd, true),
                // the app busy: kept rather than lost, as the timers above are
                Tray::Main => { if with(|a| a.show_view(true)).is_none() { later(hwnd, L_SHOW_MAIN); } }
                Tray::Nothing => {}
            }
            return LRESULT(0);
        }
        WM_CLOSE => { with(|a| a.shutdown()); let _ = DestroyWindow(hwnd); return LRESULT(0); }
        WM_DESTROY => { PostQuitMessage(0); return LRESULT(0); }
        // Explorer restarted: the icon is gone, and it has to go back even if the app is busy
        m if m != 0 && m == TASKBAR_CREATED.load(Relaxed) => { if with(|a| a.tray(true)).is_none() { later(hwnd, L_TRAY); } }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe extern "system" fn pill_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR => { SetCursor(LoadCursorW(None, IDC_HAND).unwrap_or_default()); LRESULT(1) }
        WM_LBUTTONUP => {
            with(|a| { a.s.click_through = false; a.apply_click_through(); a.save_soon(); a.redraw(); });
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// A left button message of the normal window: the click is handled in a borrow of its own and
/// the drop-down list it may ask for is named back, to be shown once that borrow has ended.
unsafe fn main_click(hwnd: HWND, x: i32, y: i32, down: bool) -> Option<(Pick, RECT)> {
    with(|a| {
        let act = { let model = &a.model; a.main.as_mut().map(|m| m.click(x, y, down, model)).unwrap_or(Act::None) };
        let hold = a.main.as_ref().map(|m| m.dragging()).unwrap_or(false);
        let pick = a.act(act);
        if down && hold { SetCapture(hwnd); } else if !down { let _ = ReleaseCapture(); }
        pick
    }).flatten()
}

/// What `check_main_handling` found about the drop-down path, run where nothing holds the app.
static PICK_PATH: AtomicU32 = AtomicU32::new(0);
const P_ASKED: u32 = 1; // the click handed the request out of its borrow
const P_REACHED: u32 = 2; // the list was built where TrackPopupMenu would be called
const P_FREE: u32 = 4; // and the app was free of every borrow at that point

/// The 表示する言語 list opened the way a mouse does it, with the app held by nobody: the click
/// is handled in a borrow of its own and the list is then shown by `show_pick_menu`, which is
/// where the state above is recorded. Nothing is put on the screen.
unsafe fn probe_pick_path() {
    let spot = with(|a| a.main.as_ref().and_then(|m| {
        m.spot("日本語＋英語", 0.5).or_else(|| m.spot("日本語のみ", 0.5)).or_else(|| m.spot("英語のみ", 0.5))
    })).flatten();
    let Some((hwnd, (x, y))) = with(|a| a.main.as_ref().map(|m| m.hwnd)).flatten().zip(spot) else { return };
    main_click(hwnd, x, y, true);
    let Some((pick, r)) = main_click(hwnd, x, y, false) else { return };
    PICK_PATH.fetch_or(P_ASKED, Relaxed);
    show_pick_menu(hwnd, pick, r);
}

/// The normal window. Every message is one question to the window's own code; what comes back is
/// a command, which the controller runs.
unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let at = |l: LPARAM| (((l.0 & 0xFFFF) as i16) as i32, (((l.0 >> 16) & 0xFFFF) as i16) as i32);
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            // None: the app is busy further up the stack. Asking for the paint again at once
            // would spin the thread for as long as it stays busy, so it is asked for in a moment.
            let busy = with(|a| { if let Some(m) = &a.main { m.blit(hdc, &ps.rcPaint) } }).is_none();
            let _ = EndPaint(hwnd, &ps);
            if busy { SetTimer(hwnd, T_REPAINT, 50, None); }
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == T_REPAINT => {
            let _ = KillTimer(hwnd, T_REPAINT);
            let _ = InvalidateRect(hwnd, None, false);
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_SIZE | WM_MOVE => { with(|a| a.main_sized(msg == WM_SIZE && wp.0 as u32 == SIZE_MINIMIZED)); }
        WM_GETMINMAXINFO => {
            let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
            let info = &mut *(lp.0 as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT { x: (mainwin::MIN_W * scale) as i32, y: (mainwin::MIN_H * scale) as i32 };
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            let r = *(lp.0 as *const RECT);
            with(|a| if let Some(m) = &mut a.main { m.set_scale((wp.0 & 0xFFFF) as f32 / 96.0) });
            let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            with(|a| { a.sync_main(); a.redraw_main(); });
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            let (x, y) = at(lp);
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            let _ = TrackMouseEvent(&mut tme);
            with(|a| {
                let act = { let model = &a.model; a.main.as_mut().map(|m| m.mouse_move(x, y, model)).unwrap_or(Act::None) };
                a.act(act);
            });
            return LRESULT(0);
        }
        WM_MOUSE_LEFT => { with(|a| { let act = a.main.as_mut().map(|m| m.leave()).unwrap_or(Act::None); a.act(act); }); return LRESULT(0); }
        // the mouse was taken away (Alt+Tab, another window): a drag that was holding it is over
        WM_CAPTURECHANGED => { with(|a| { let act = a.main.as_mut().map(|m| m.lost_capture()).unwrap_or(Act::None); a.act(act); }); return LRESULT(0); }
        WM_LBUTTONDOWN | WM_LBUTTONUP => {
            let (x, y) = at(lp);
            // the list is opened after the click's borrow has ended, so that everything else
            // goes on being handled while it is on the screen
            if let Some((pick, r)) = main_click(hwnd, x, y, msg == WM_LBUTTONDOWN) { show_pick_menu(hwnd, pick, r); }
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            let mut p = POINT { x: ((lp.0 & 0xFFFF) as i16) as i32, y: (((lp.0 >> 16) & 0xFFFF) as i16) as i32 };
            let _ = ScreenToClient(hwnd, &mut p);
            let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            with(|a| {
                let act = { let (model, s) = (&a.model, &a.s); a.main.as_mut().map(|m| m.wheel(p.x, p.y, delta, ctrl, model, s)).unwrap_or(Act::None) };
                a.act(act);
            });
            return LRESULT(0);
        }
        WM_SETCURSOR => {
            if (lp.0 & 0xFFFF) as u32 == HTCLIENT as u32 && with(|a| a.main.as_ref().map(|m| m.is_hot()).unwrap_or(false)).unwrap_or(false) {
                SetCursor(LoadCursorW(None, IDC_HAND).unwrap_or_default());
                return LRESULT(1);
            }
        }
        WM_TIMER if wp.0 == T_PULSE => { with(|a| a.pulse()); return LRESULT(0); }
        // Closing the normal window ends the app, as in the C# window.
        WM_CLOSE => {
            if let Some(over) = with(|a| { a.sync_main(); a.hwnd }) {
                let _ = PostMessageW(over, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// Streams a PCM file at the pace of real time, then silence, as the sound card would.
/// With `tab_port`, the frames go the way the Chrome extension sends them: through the socket.
fn feed(shared: Arc<Shared>, eng: Arc<EngineLink>, path: String, secs: u64, tab_port: u16) {
    std::thread::spawn(move || {
        let data = std::fs::read(&path).unwrap_or_default();
        let until = Instant::now() + Duration::from_secs(secs);
        while !eng.is_live() && Instant::now() < until { std::thread::sleep(Duration::from_millis(100)); }
        let mut tab = if tab_port == 0 { None } else {
            let request = tungstenite::ClientRequestBuilder::new(format!("ws://127.0.0.1:{tab_port}/tab").parse().unwrap())
                .with_header("Origin", crate::tabaudio::EXTENSION_ORIGIN);
            let made = std::net::TcpStream::connect(("127.0.0.1", tab_port)).ok().and_then(|s| tungstenite::client(request, s).ok());
            if made.is_none() { core::log("selftest-gui: could not connect as the extension"); }
            made.map(|(ws, _)| ws)
        };
        let start = Instant::now();
        let mut n = 0usize;
        while Instant::now() < until && !shared.quitting.load(Relaxed) {
            let at = n * 3200;
            let mut frame = [0u8; 3200];
            if at + 3200 <= data.len() { frame.copy_from_slice(&data[at..at + 3200]); }
            match &mut tab {
                Some(ws) => { let _ = ws.send(tungstenite::Message::Binary(frame.to_vec().into())); }
                None if tab_port == 0 => shared.frame(&eng, &frame),
                None => {}
            }
            n += 1;
            let due = start + Duration::from_millis(100 * n as u64);
            if let Some(wait) = due.checked_duration_since(Instant::now()) { std::thread::sleep(wait); }
        }
    });
}

pub unsafe fn run(test: Option<Test>) -> Result<()> {
    core::log_init();
    // the sound sessions of the source list are asked of COM on this thread
    let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
    // The same name as the C# app's: the two share the settings, the engine and the extension's port.
    let name = match &test { None => "Local\\LiveSubs.SingleInstance.v1".to_string(), Some(t) => format!("Local\\LiveSubs.SelfTest.{}", t.port) };
    let _mutex = CreateMutexW(None, true, &wide(&name))?;
    if GetLastError() == ERROR_ALREADY_EXISTS {
        core::log("another instance is already running; exiting");
        return Ok(());
    }
    let cfg = AppConfig::load();
    let mut s = if test.is_some() { Settings::default() } else { Settings::load() };
    let sys_scale = GetDpiForSystem() as f64 / 96.0;
    if let Some(t) = &test {
        let at = if t.on_screen && !t.main { 60.0 } else { -30000.0 / sys_scale };
        (s.ja_size, s.en_size, s.show_english, s.bg_percent, s.left, s.top, s.width, s.height) = (20, 16, true, 75, at, at, 475.0, 802.0);
        s.click_through = true;
        // the normal window is never shown in a test: off every screen, and not in the taskbar
        (s.main_left, s.main_top, s.main_width, s.main_height) = (-30000.0 / sys_scale, -30000.0 / sys_scale, 1100.0, 700.0);
    }
    let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
    let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
    // the two sizes of the icon, made once here and destroyed when the app ends
    let (icon_big, icon_small) = (icon(GetSystemMetrics(SM_CXICON)), icon(GetSystemMetrics(SM_CXSMICON)));
    RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wnd_proc), hInstance: hinst, lpszClassName: w!("LiveSubsOverlay"), hCursor: arrow, ..Default::default() });
    RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(pill_proc), hInstance: hinst, lpszClassName: w!("LiveSubsUnlock"), ..Default::default() });
    RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(main_proc), hInstance: hinst, lpszClassName: w!("LiveSubsMain"), hCursor: arrow,
                                hIcon: icon_big.unwrap_or_default(), ..Default::default() });
    let ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | if test.is_some() { WS_EX_NOACTIVATE } else { WINDOW_EX_STYLE(0) };
    let hwnd = CreateWindowExW(ex, w!("LiveSubsOverlay"), w!("ライブ字幕"), WS_POPUP, 0, 0, 100, 100, None, None, hinst, None)?;
    let pill = CreateWindowExW(WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, w!("LiveSubsUnlock"), w!(""), WS_POPUP, 0, 0, 10, 10, None, None, hinst, None)?;
    let shared = Arc::new(Shared { hwnd: AtomicIsize::new(hwnd.0 as isize), events: Mutex::new(VecDeque::new()), tab_on: AtomicBool::new(false),
                                   paused: AtomicBool::new(false), pause_flush: AtomicI32::new(0),
                                   quitting: AtomicBool::new(false) });
    let view = View::new(100, 100, GetDpiForWindow(hwnd) as f32 / 96.0)?;
    let alpha = if test.as_ref().map(|t| t.on_screen).unwrap_or(false) { 3 } else { 255 };
    let mut app = App {
        hwnd, s, model: Model::new(), view, main: None, main_on: false, level: 0.0, hinst,
        shared: shared.clone(), engine: None, capture: None, tab: None, tab_port: cfg.tab_port, tab_failed: false,
        conn: Conn::Idle, conn_text: "接続中…".into(), audio_text: String::new(), err_text: String::new(), behind_until: None, sys_scale, alpha,
        pill, pill_view: None, pill_shown: false, away: 0, tray_on: false, main_min: false, icon_big, icon_small, test,
    };

    // where it was last time, unless that is no longer on any screen
    let k = sys_scale;
    let (vl, vt) = (GetSystemMetrics(SM_XVIRTUALSCREEN) as f64 / k, GetSystemMetrics(SM_YVIRTUALSCREEN) as f64 / k);
    let (vr, vb) = (vl + GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64 / k, vt + GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64 / k);
    let seen = !app.s.left.is_nan() && !app.s.top.is_nan()
        && (app.s.left + app.s.width).min(vr) - app.s.left.max(vl) >= 160.0 && (app.s.top + app.s.height).min(vb) - app.s.top.max(vt) >= 40.0;
    if !seen && app.test.is_none() { app.reset_position(); }
    app.move_to_settings();
    // a window on another screen may have another scale than the one it was made at
    app.view.set_scale(GetDpiForWindow(hwnd) as f32 / 96.0);

    let port = app.test.as_ref().map(|t| t.port).unwrap_or(cfg.port);
    let events = shared.clone();
    let eng = Arc::new(EngineLink::start(
        engine::Config { distro: cfg.distro.clone(), dir: cfg.dir.clone(), port, want_raw: false },
        Box::new(move |ev| events.post(Ev::Engine(ev))),
        Box::new(|t| core::log(&format!("ws: {t}")))));
    eng.set_topics(app.s.topics);
    app.engine = Some(eng.clone());
    let test = app.test.as_ref().map(|t| (t.pcm.clone(), t.secs, t.tab_port));
    if let Some((_, secs, tab_port)) = &test {
        if *tab_port > 0 { app.tab_port = *tab_port; }
        SetTimer(hwnd, T_TEST_END, (*secs * 1000) as u32, None);
    }
    match test {
        Some((pcm, secs, tab_port)) if pcm != "live" => {
            if tab_port > 0 { app.start_tab(); }
            feed(shared.clone(), eng.clone(), pcm, secs, tab_port);
        }
        _ => {
            let (frames, status, e) = (shared.clone(), shared.clone(), eng.clone());
            app.capture = Some(Capture::start(&app.s.capture_app,
                Box::new(move |f| if !frames.tab_on.load(Relaxed) { frames.frame(&e, f) }),
                Box::new(move |t| status.post(Ev::AudioStatus(t.to_string()))),
                Box::new(|t| core::log(&format!("audio: {t}")))));
            app.start_tab();
        }
    }
    drop(eng);
    core::log(&format!("controller started: port {port}, distro {}, dir {}", cfg.distro, cfg.dir));

    app.apply_click_through();
    app.redraw();
    app.tray(true);
    // the overlay window is there either way: it carries the icon in the notification area
    let start_main = match &app.test { Some(t) => t.main, None => !app.s.overlay_mode };
    if start_main { app.show_view(true); } else { let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE); }
    SetTimer(hwnd, T_TRIM, 30000, None);
    TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Relaxed);
    APP.with(|c| *c.borrow_mut() = Some(app));

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    APP.with(|c| *c.borrow_mut() = None);
    Ok(())
}
