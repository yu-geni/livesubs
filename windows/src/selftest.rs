//! Checks that run without the engine and without sound: what the overlay looks like, and
//! what it costs while subtitles stream through it.
use std::{mem::size_of, time::Instant};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::{Direct2D::*, DirectWrite::*, Gdi::{BitBlt, DeleteDC, DeleteObject, HGDIOBJ, SRCCOPY}},
        System::{LibraryLoader::GetModuleHandleW, ProcessStatus::*, Threading::*},
        UI::{HiDpi::GetDpiForSystem, WindowsAndMessaging::*},
    },
};

use crate::{mainwin::{self, MainWin}, model::Model, samples::SAMPLES, view::{Entry, Look, View}};

pub fn arg<T: std::str::FromStr>(a: &[String], i: usize, fallback: T) -> T {
    a.get(i).and_then(|s| s.parse().ok()).unwrap_or(fallback)
}

pub struct Usage { pub working_set_mb: f64, pub private_mb: f64, pub cpu_s: f64 }

pub fn usage() -> Usage {
    unsafe {
        let me = GetCurrentProcess();
        let mut pmc = PROCESS_MEMORY_COUNTERS_EX::default();
        let _ = K32GetProcessMemoryInfo(me, &mut pmc as *mut _ as *mut PROCESS_MEMORY_COUNTERS, size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32);
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let _ = GetProcessTimes(me, &mut c, &mut e, &mut k, &mut u);
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
        Usage { working_set_mb: pmc.WorkingSetSize as f64 / 1048576.0, private_mb: pmc.PrivateUsage as f64 / 1048576.0, cpu_s: t(k) + t(u) }
    }
}

pub fn entries(model: &Model) -> Vec<Entry<'_>> {
    let from = model.items.len().saturating_sub(200);
    model.items[from..].iter().map(|i| Entry { uid: i.uid, ja: &i.ja, en: &i.en, dim: i.dim }).collect()
}

/// The picture a view has composed, for looking at: "LSRAW", width, height, premultiplied BGRA.
pub fn dump(view: &View, path: &str) -> std::io::Result<()> {
    dump_px(view.w, view.h, view.pixels(), path)
}

pub fn dump_px(w: i32, h: i32, px: &[u8], path: &str) -> std::io::Result<()> {
    let mut out = b"LSRAW".to_vec();
    out.extend_from_slice(&(w as u32).to_le_bytes());
    out.extend_from_slice(&(h as u32).to_le_bytes());
    out.extend_from_slice(px);
    std::fs::write(path, out)
}

/// The sample transcript the picture tests are taken of: a few confirmed lines, two 目次 entries
/// and one line still being spoken.
fn sample_model(topics: bool) -> Model {
    let mut model = Model::new();
    model.on_ready("distil-large-v3.5", "LFM2 350M ENJP MT Q4_K_M", "cuda");
    for (i, (ja, en)) in SAMPLES.iter().enumerate() { model.on_final(i as i32, ja, en, 1200); }
    // the clock does not move while a picture is taken: spread the lines over three minutes, so
    // the stamps are the ones the user would see
    for (i, it) in model.items.iter_mut().enumerate() {
        it.time = format!("16:{:02}", 7 + i / 5);
        it.show_time = i % 5 == 0;
    }
    let (ja, en) = SAMPLES[2];
    let cut = |s: &str| s.chars().take(s.chars().count() / 2).collect::<String>();
    model.on_interim(SAMPLES.len() as i32, &cut(ja), &cut(en));
    if topics {
        model.on_topic(0, 0, "今日の試合の得点方法", "デポ、幼稚園、覇権ボーナスの 3 つの目標と、それぞれの点数を順に説明。");
        model.on_topic(1, 7, "弾薬庫と爆発物トラック", "弾薬庫のトラックが爆発すると周囲の建物はすべて吹き飛ぶ。");
    }
    model
}

fn sample_status(kind: &str) -> crate::mainwin::Status {
    let (text, dot) = match kind {
        "warn" => ("音声が取得できません（再生中のアプリがありません）".to_string(), crate::mainwin::WARN),
        "error" => ("サーバーエラー: model failed to load".to_string(), crate::mainwin::BAD),
        _ => ("受信中".to_string(), crate::mainwin::GOOD),
    };
    crate::mainwin::Status {
        text, dot, level: 0.42, lag: "遅延 1.2 秒".into(),
        models: "distil-large-v3.5 / LFM2 350M ENJP MT Q4_K_M (cuda)".into(),
        tab: if kind == "tab" { 1 } else { 0 }, paused: false,
    }
}

/// --selftest-main-shot <out.raw> [width] [height] [scale] [mode 0|1|2] [variant]
/// No window: the normal window's picture for a fixed transcript, written to a file. The
/// variants are "", "scrolled", "expanded", "warn", "error", "empty", "tab".
pub unsafe fn main_shot(a: &[String]) -> Result<()> {
    let out = a.get(2).cloned().unwrap_or_else(|| "main.raw".into());
    let scale: f32 = arg(a, 5, 2.5);
    let (w, h) = ((arg(a, 3, 1100.0f32) * scale).round() as i32, (arg(a, 4, 700.0f32) * scale).round() as i32);
    let mode: usize = arg(a, 6, 1);
    let variant = a.get(7).cloned().unwrap_or_default();
    let mut s = crate::core::Settings::default();
    s.set_text_mode(mode);
    (s.ja_size, s.en_size, s.topics, s.save_history) = (20, 14, true, true);
    let model = sample_model(variant != "empty");
    let st = sample_status(&variant);
    let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
    let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let mut win = MainWin::new(HWND::default(), w, h, scale, &d2d, &dw)?;
    if variant == "expanded" { win.expanded = true; }
    win.compose(&model, &s, &st)?;
    if variant == "scrolled" {
        let (x, y) = win.transcript_spot();
        win.wheel(x, y, 240, false, &model, &s);
        win.compose(&model, &s, &st)?;
    }
    if variant == "expanded" {
        win.settings_to_end();
        win.compose(&model, &s, &st)?;
    }
    let _ = dump_px(win.w, win.h, win.pixels(), &out);
    Ok(())
}

/// --selftest-overlay-shot <out.raw> [jaSize] [enSize] [width] [height] [scale] [mode 0|1|2] [bg%] [status]
/// No window: the overlay's picture for a fixed set of subtitles, written to a file.
pub unsafe fn overlay_shot(a: &[String]) -> Result<()> {
    let out = a.get(2).cloned().unwrap_or_else(|| "overlay.raw".into());
    let scale: f32 = arg(a, 7, 2.5);
    let (w, h) = (arg(a, 5, 475.0f32) * scale, arg(a, 6, 802.0f32) * scale);
    let mode: usize = arg(a, 8, 1);
    let look = Look { ja_size: arg(a, 3, 20), en_size: arg(a, 4, 16), show_ja: mode != 2, show_en: mode != 0, bg_percent: arg(a, 9, 75), grip: true };
    let mut model = Model::new();
    model.on_ready("asr", "mt", "cuda");
    for (i, (ja, en)) in SAMPLES.iter().take(9).enumerate() { model.on_final(i as i32, ja, en, 1200); }
    let (ja, en) = SAMPLES[9];
    let cut = |s: &str| s.chars().take(s.chars().count() / 2).collect::<String>();
    model.on_interim(9, &cut(ja), &cut(en));
    let mut view = View::new(w.round() as i32, h.round() as i32, scale)?;
    view.compose(&entries(&model), a.get(10).map(|s| s.as_str()).unwrap_or(""), "", &look)?;
    let _ = dump(&view, &out);
    Ok(())
}

unsafe extern "system" fn plain_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

/// --selftest-overlay-load <report> [seconds] [width] [height] [onscreen|offscreen]: what the
/// overlay costs while subtitles stream through it, the same stream as the C# app's test of
/// that name. A sentence grows as dimmed text every 400 ms and then becomes final. On the
/// screen the window is shown at 1% opacity and click-through, so that it is really presented
/// but neither seen nor in the way; the first five seconds are warm-up.
pub unsafe fn overlay_load(a: &[String]) -> Result<()> {
    let report = a.get(2).cloned().unwrap_or_else(|| "overlay-load.txt".into());
    let secs: f64 = arg(a, 3, 60.0);
    let scale = GetDpiForSystem() as f32 / 96.0;
    let (w, h) = ((arg(a, 4, 475.0f32) * scale).round() as i32, (arg(a, 5, 802.0f32) * scale).round() as i32);
    let on_screen = a.get(6).map(|s| s.starts_with("onscreen")).unwrap_or(false);
    let at = if on_screen { (60.0 * scale) as i32 } else { -30000 };
    let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
    let class = w!("LiveSubsLoadTest");
    RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(plain_proc), hInstance: hinst, lpszClassName: class, ..Default::default() });
    let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
    let hwnd = CreateWindowExW(ex, class, w!(""), WS_POPUP, at, at, w, h, None, None, hinst, None)?;
    let mut view = View::new(w, h, scale)?;
    let look = Look { ja_size: 20, en_size: 16, show_ja: true, show_en: true, bg_percent: 75, grip: false };
    let mut model = Model::new();
    model.on_ready("asr", "mt", "cuda");
    view.compose(&[], "", "", &look)?;
    view.present(hwnd, 3)?;
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    SetTimer(hwnd, 1, 400, None);

    const STEPS: usize = 7; // 7 x 400 ms: a sentence every 2.8 s
    let (clock, mut cpu0, mut t0) = (Instant::now(), 0.0, 0.0);
    let (mut id, mut step, mut updates, mut finals, mut work_ms) = (0usize, 0usize, 0u32, 0u32, 0.0f64);
    let mut timeline = String::new();
    let mut next_sample = 30.0;
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        if msg.message != WM_TIMER {
            DispatchMessageW(&msg);
            continue;
        }
        let began = Instant::now();
        let (ja, en) = SAMPLES[id % SAMPLES.len()];
        step += 1;
        if step < STEPS {
            let cut = |s: &str| s.chars().take(s.chars().count() * step / STEPS).collect::<String>();
            model.on_interim(id as i32, &cut(ja), &cut(en));
        } else {
            model.on_final(id as i32, ja, en, 1200);
            id += 1;
            step = 0;
            finals += 1;
        }
        view.compose(&entries(&model), "", "", &look)?;
        view.present(hwnd, 3)?;
        updates += 1;
        work_ms += began.elapsed().as_secs_f64() * 1000.0;
        let now = clock.elapsed().as_secs_f64();
        if t0 == 0.0 && now >= 5.0 { cpu0 = usage().cpu_s; t0 = now; }
        if now >= next_sample {
            let u = usage();
            timeline += &format!("at {next_sample:4.0} s: working set {:.0} MB, private {:.0} MB, cpu {:.1} s\n", u.working_set_mb, u.private_mb, u.cpu_s);
            next_sample += 30.0;
        }
        if now >= secs { break; }
    }
    let span = clock.elapsed().as_secs_f64() - t0;
    let u = usage();
    let text = format!(
        "mode: {}, drawn on the CPU (Rust), scale {scale:.2}\n{timeline}overlay {w} x {h} px, {updates} updates, {finals} sentences, {} shown at the end\ncpu: {:.1}% of a core over {span:.0} s; {:.1} ms per update\nsubtitles outlined: {}\nat end : working set {:.0} MB, private {:.0} MB\n",
        if on_screen { "onscreen" } else { "offscreen" }, view.last_shown, (u.cpu_s - cpu0) / span * 100.0, work_ms / updates.max(1) as f64, view.blocks_drawn, u.working_set_mb, u.private_mb);
    let _ = std::fs::write(&report, text);
    let _ = DestroyWindow(hwnd);
    Ok(())
}

/// --selftest-main-load <report> [seconds] [width] [height]: what the normal window costs while
/// subtitles stream through it, the same stream as the overlay's test of that name. The window is
/// never shown; the picture is copied to a bitmap of the same size instead, so the copy costs what
/// it costs on a screen. The transcript starts with 5000 lines, to show that the count is free.
pub unsafe fn main_load(a: &[String]) -> Result<()> {
    let report = a.get(2).cloned().unwrap_or_else(|| "main-load.txt".into());
    let secs: f64 = arg(a, 3, 60.0);
    let scale = GetDpiForSystem() as f32 / 96.0;
    let (w, h) = ((arg(a, 4, 1100.0f32) * scale).round() as i32, (arg(a, 5, 700.0f32) * scale).round() as i32);
    let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
    let class = w!("LiveSubsLoadMain");
    RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(plain_proc), hInstance: hinst, lpszClassName: class, ..Default::default() });
    let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, class, w!(""), WS_POPUP, -30000, -30000, w, h, None, None, hinst, None)?;
    let (screen, bmp, _) = mainwin::surface(w, h)?; // stands in for the screen
    let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
    let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let mut win = MainWin::new(hwnd, w, h, scale, &d2d, &dw)?;
    let mut s = crate::core::Settings::default();
    (s.ja_size, s.en_size, s.show_english) = (20, 14, true);
    let mut st = sample_status("");
    let mut model = Model::new();
    model.on_ready("asr", "mt", "cuda");
    let preload = crate::model::MAX_ITEMS;
    for i in 0..preload { let (ja, en) = SAMPLES[i % SAMPLES.len()]; model.on_final(i as i32, ja, en, 1200); }
    model.on_topic(0, 0, "はじめに", "積み上げた字幕の上で、新しい行が流れ込むあいだの負荷を測ります。");
    win.compose(&model, &s, &st)?;
    SetTimer(hwnd, 1, 400, None);

    const STEPS: usize = 7; // 7 x 400 ms: a sentence every 2.8 s
    let (clock, mut cpu0, mut t0) = (Instant::now(), 0.0, 0.0);
    let (mut id, mut step, mut updates, mut finals, mut work_ms) = (preload, 0usize, 0u32, 0u32, 0.0f64);
    let mut timeline = String::new();
    let mut next_sample = 30.0;
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        if msg.message != WM_TIMER {
            DispatchMessageW(&msg);
            continue;
        }
        let began = Instant::now();
        let (ja, en) = SAMPLES[id % SAMPLES.len()];
        step += 1;
        if step < STEPS {
            let cut = |s: &str| s.chars().take(s.chars().count() * step / STEPS).collect::<String>();
            model.on_interim(id as i32, &cut(ja), &cut(en));
        } else {
            model.on_final(id as i32, ja, en, 1200);
            id += 1;
            step = 0;
            finals += 1;
            // every sentence comes with a lag of its own, as it does in the app
            st.lag = format!("遅延 {:.1} 秒", 1.0 + (finals % 9) as f64 / 10.0);
        }
        let d = win.compose(&model, &s, &st)?;
        // only what changed is copied, the way WM_PAINT does it
        let copy = |r: RECT| { let _ = BitBlt(screen, r.left, r.top, r.right - r.left, r.bottom - r.top, win.dc(), r.left, r.top, SRCCOPY); };
        copy(d.rect);
        if let Some(head) = d.head { copy(head); }
        updates += 1;
        work_ms += began.elapsed().as_secs_f64() * 1000.0;
        let now = clock.elapsed().as_secs_f64();
        if t0 == 0.0 && now >= 5.0 { cpu0 = usage().cpu_s; t0 = now; }
        if now >= next_sample {
            let u = usage();
            timeline += &format!("at {next_sample:4.0} s: working set {:.0} MB, private {:.0} MB, cpu {:.1} s\n", u.working_set_mb, u.private_mb, u.cpu_s);
            next_sample += 30.0;
        }
        if now >= secs { break; }
    }
    let span = clock.elapsed().as_secs_f64() - t0;
    let u = usage();
    let text = format!(
        "normal window, drawn on the CPU (Rust), scale {scale:.2}\n{timeline}window {w} x {h} px, {updates} updates, {finals} sentences, {} lines in the transcript, {} rows on show\ncpu: {:.1}% of a core over {span:.0} s; {:.1} ms per update\ntexts laid out: {}\nat end : working set {:.0} MB, private {:.0} MB\n",
        model.items.len(), win.shown, (u.cpu_s - cpu0) / span * 100.0, work_ms / updates.max(1) as f64, win.rows_built, u.working_set_mb, u.private_mb);
    let _ = std::fs::write(&report, text);
    drop(win);
    let _ = DeleteDC(screen);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DestroyWindow(hwnd);
    Ok(())
}
