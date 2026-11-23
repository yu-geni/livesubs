//! Where things live, the log, the two ini files and the history file. The files are the ones
//! the C# app uses, in the same format, so that either app can be started on the other's data.
use std::{collections::HashMap, fs, io::Write, path::{Path, PathBuf}, sync::{Mutex, OnceLock}};
use windows::Win32::{System::{Com::CoTaskMemFree, SystemInformation::GetLocalTime}, UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath, KF_FLAG_DEFAULT}};

pub fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_else(|| PathBuf::from("."))
}

/// Settings and the log live in %APPDATA%\LiveSubs; that can be unwritable (policy, folder
/// protection), so fall back rather than lose both.
pub fn app_data() -> &'static (PathBuf, String) {
    static DIR: OnceLock<(PathBuf, String)> = OnceLock::new();
    DIR.get_or_init(|| {
        let mut tried = Vec::new();
        let bases = [std::env::var_os("APPDATA").map(PathBuf::from), std::env::var_os("LOCALAPPDATA").map(PathBuf::from), Some(exe_dir())];
        for base in bases.into_iter().flatten() {
            let d = base.join("LiveSubs");
            let probe = d.join(".write-probe");
            match fs::create_dir_all(&d).and_then(|_| fs::write(&probe, "ok")) {
                Ok(()) => {
                    let _ = fs::remove_file(&probe);
                    let note = if tried.is_empty() { format!("using {}", d.display()) } else { format!("using {}; rejected: {}", d.display(), tried.join(" | ")) };
                    return (d, note);
                }
                Err(e) => tried.push(format!("{} -> {e}", d.display())),
            }
        }
        let d = std::env::temp_dir();
        let note = format!("falling back to {}; rejected: {}", d.display(), tried.join(" | "));
        (d, note)
    })
}

pub struct Now { pub date: String, pub time: String, pub ms: u16, pub hm: String, pub stamp: String }

pub fn now() -> Now {
    let t = unsafe { GetLocalTime() };
    Now {
        date: format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay),
        time: format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond),
        ms: t.wMilliseconds,
        hm: format!("{:02}:{:02}", t.wHour, t.wMinute),
        stamp: format!("{:04}-{:02}-{:02}_{:02}{:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond),
    }
}

static LOG: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn log_init() {
    let path = app_data().0.join("log.txt");
    if fs::metadata(&path).map(|m| m.len() > 1024 * 1024).unwrap_or(false) {
        let _ = fs::remove_file(&path);
    }
    *LOG.lock().unwrap() = Some(path);
    log(&format!("--- LiveSubs (Rust) start; {}", app_data().1));
}

pub fn log(msg: &str) {
    let guard = LOG.lock().unwrap();
    if let Some(path) = guard.as_ref() {
        let n = now();
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = write!(f, "{} {}.{:03} {}\r\n", n.date, n.time, n.ms, msg);
        }
    }
}

pub fn read_ini(path: &Path) -> HashMap<String, String> {
    let mut kv = HashMap::new();
    let Ok(text) = fs::read_to_string(path) else { return kv };
    for raw in text.trim_start_matches('\u{feff}').lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(['#', ';', '[']) { continue; }
        if let Some((k, v)) = line.split_once('=') {
            if !k.trim().is_empty() { kv.insert(k.trim().to_lowercase(), v.trim().to_string()); }
        }
    }
    kv
}

fn truthy(s: &str, fallback: bool) -> bool {
    match s.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => fallback,
    }
}

/// LiveSubs.ini beside the exe, written by the build script: where the engine is.
pub struct AppConfig { pub distro: String, pub dir: String, pub port: u16, pub tab_port: u16 }

impl AppConfig {
    pub fn load() -> AppConfig {
        let kv = read_ini(&exe_dir().join("LiveSubs.ini"));
        let text = |k: &str, d: &str| kv.get(k).filter(|v| !v.is_empty()).cloned().unwrap_or_else(|| d.to_string());
        let num = |k: &str, d: u16| kv.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        AppConfig { distro: text("distro", "Ubuntu"), dir: text("dir", ""), port: num("port", 8765), tab_port: num("tabport", 8766) }
    }
}

pub const JA_PRESETS: [i32; 8] = [20, 24, 28, 34, 40, 48, 56, 64];
pub const EN_PRESETS: [i32; 6] = [11, 13, 16, 19, 22, 26];
pub const BG_PRESETS: [i32; 5] = [0, 25, 50, 75, 100];

/// settings.ini. Positions and sizes are in the units the C# app uses (pixels at 100% scaling).
#[derive(Clone)]
pub struct Settings {
    pub ja_size: i32, pub en_size: i32, pub show_english: bool, pub show_japanese: bool, pub bg_percent: i32,
    pub click_through: bool, pub left: f64, pub top: f64, pub width: f64, pub height: f64, pub seen_subtitle: bool,
    pub always_on_top: bool, pub overlay_mode: bool, pub main_left: f64, pub main_top: f64, pub main_width: f64,
    pub main_height: f64, pub topics: bool, pub save_history: bool, pub capture_app: String,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            ja_size: 34, en_size: 16, show_english: false, show_japanese: true, bg_percent: 55, click_through: false,
            left: f64::NAN, top: f64::NAN, width: 900.0, height: 170.0, seen_subtitle: false, always_on_top: false,
            overlay_mode: false, main_left: f64::NAN, main_top: f64::NAN, main_width: 1100.0, main_height: 700.0,
            topics: true, save_history: true, capture_app: String::new(),
        }
    }
}

fn num(v: f64) -> String {
    if v.is_nan() { return "NaN".into(); }
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

impl Settings {
    fn path() -> PathBuf { app_data().0.join("settings.ini") }

    pub fn load() -> Settings { Settings::load_from(&Settings::path()) }

    pub fn load_from(path: &Path) -> Settings {
        let mut s = Settings::default();
        let kv = read_ini(path);
        let int = |k: &str, d: i32, lo: i32, hi: i32| kv.get(k).and_then(|v| v.parse::<i32>().ok()).unwrap_or(d).clamp(lo, hi);
        let dbl = |k: &str, d: f64| kv.get(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(d);
        let flag = |k: &str, d: bool| kv.get(k).map(|v| truthy(v, d)).unwrap_or(d);
        s.ja_size = int("jasize", s.ja_size, 10, 140);
        s.en_size = int("ensize", s.en_size, 8, 90);
        s.show_english = flag("showenglish", s.show_english);
        s.show_japanese = flag("showjapanese", s.show_japanese);
        if !s.show_japanese && !s.show_english { s.show_japanese = true; } // never nothing at all
        s.bg_percent = int("bgpercent", s.bg_percent, 0, 100);
        s.click_through = flag("clickthrough", s.click_through);
        s.left = dbl("left", s.left);
        s.top = dbl("top", s.top);
        s.width = dbl("width", s.width);
        s.height = dbl("height", s.height);
        s.seen_subtitle = flag("seensubtitle", s.seen_subtitle);
        s.always_on_top = flag("alwaysontop", s.always_on_top);
        s.overlay_mode = flag("overlaymode", s.overlay_mode);
        s.main_left = dbl("mainleft", s.main_left);
        s.main_top = dbl("maintop", s.main_top);
        s.main_width = dbl("mainwidth", s.main_width);
        s.main_height = dbl("mainheight", s.main_height);
        s.topics = flag("topics", s.topics);
        s.save_history = flag("savehistory", s.save_history);
        s.capture_app = kv.get("captureapp").cloned().unwrap_or_default();
        if !(640.0..=8000.0).contains(&s.main_width) { s.main_width = 1100.0; }
        if !(420.0..=4000.0).contains(&s.main_height) { s.main_height = 700.0; }
        if !(240.0..=8000.0).contains(&s.width) { s.width = 900.0; }
        if !(70.0..=4000.0).contains(&s.height) { s.height = 170.0; }
        s
    }

    pub fn save(&self) {
        if let Err(e) = self.save_to(&Settings::path()) { log(&format!("settings save failed: {e}")); }
    }

    /// Written to a file of its own beside the real one and renamed over it: a crash between the
    /// two leaves the settings as they were, never an empty file.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let b = |v: bool| if v { "1" } else { "0" };
        let text = format!(
            "\u{feff}jaSize={}\r\nenSize={}\r\nshowEnglish={}\r\nshowJapanese={}\r\nbgPercent={}\r\nclickThrough={}\r\nleft={}\r\ntop={}\r\nwidth={}\r\nheight={}\r\nseenSubtitle={}\r\nalwaysOnTop={}\r\noverlayMode={}\r\nmainLeft={}\r\nmainTop={}\r\nmainWidth={}\r\nmainHeight={}\r\ntopics={}\r\nsaveHistory={}\r\ncaptureApp={}\r\n",
            self.ja_size, self.en_size, b(self.show_english), b(self.show_japanese), self.bg_percent, b(self.click_through),
            num(self.left), num(self.top), num(self.width), num(self.height), b(self.seen_subtitle), b(self.always_on_top),
            b(self.overlay_mode), num(self.main_left), num(self.main_top), num(self.main_width), num(self.main_height),
            b(self.topics), b(self.save_history), self.capture_app);
        let tmp = path.with_extension("ini.new");
        match fs::write(&tmp, text).and_then(|()| fs::rename(&tmp, path)) {
            Ok(()) => Ok(()),
            Err(e) => { let _ = fs::remove_file(&tmp); Err(e) }
        }
    }

    /// 0 = Japanese only, 1 = both, 2 = English only.
    pub fn text_mode(&self) -> usize { if !self.show_japanese { 2 } else if self.show_english { 1 } else { 0 } }
    pub fn set_text_mode(&mut self, m: usize) { self.show_japanese = m != 2; self.show_english = m != 0; }
}

/// Every confirmed subtitle is appended to a text file in Documents\ライブ字幕, one file per start
/// (and per stream the user switches to).
pub mod history {
    use super::*;
    static FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

    pub fn folder() -> PathBuf {
        let docs = unsafe {
            SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DEFAULT, None).ok().map(|p| {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                PathBuf::from(s)
            })
        };
        docs.unwrap_or_else(|| PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join("Documents")).join("ライブ字幕")
    }

    pub fn line(ja: &str, en: &str) {
        let t = format!("[{}] ", now().time);
        let mut text = format!("{t}{ja}\r\n");
        if !en.is_empty() { text += &format!("{}{en}\r\n", " ".repeat(t.len())); }
        append(&text);
    }

    pub fn topic(title: &str, summary: &str) {
        let mut text = format!("\r\n■ {} {title}", now().hm);
        if !summary.is_empty() { text += &format!(" ― {summary}"); }
        append(&(text + "\r\n\r\n"));
    }

    /// The next line starts a new file (the user switched to another stream).
    pub fn new_file() { *FILE.lock().unwrap() = None; }

    fn append(text: &str) {
        let mut file = FILE.lock().unwrap();
        let fresh = file.is_none();
        if fresh {
            let dir = folder();
            let _ = fs::create_dir_all(&dir);
            *file = Some(dir.join(format!("{}.txt", now().stamp)));
        }
        let path = file.as_ref().unwrap();
        let done = fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| {
            if fresh { f.write_all("\u{feff}".as_bytes())?; }
            f.write_all(text.as_bytes())
        });
        if let Err(e) = done { log(&format!("history write failed: {e}")); }
    }
}
