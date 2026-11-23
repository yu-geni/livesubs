//! ライブ字幕: live Japanese subtitles for English audio. This is the Windows app; the
//! recognition and translation engine runs in WSL (server.py).
//!
//! It took the place of an app written in C# (WPF) and keeps that app's behaviour, its files
//! and its look. Comments that say "as in the C#" mean that app; its source is in this
//! repository's history (the first commit, under windows/).
#![windows_subsystem = "windows"]

mod app;
mod core;
mod install;
mod mainwin;
mod model;
mod samples;
mod selftest;
mod view;

use livesubs::{capture, engine, tabaudio};

use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // a bug that stops the program should at least leave its name in the log
    std::panic::set_hook(Box::new(|info| {
        core::log_init();
        core::log(&format!("panic: {info}"));
    }));
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let done = unsafe {
        match mode {
            "--selftest-overlay-shot" => selftest::overlay_shot(&args),
            "--selftest-overlay-load" => selftest::overlay_load(&args),
            "--selftest-main-shot" => selftest::main_shot(&args),
            "--selftest-main-load" => selftest::main_load(&args),
            // --selftest-gui <seconds> <picture.raw> <engine port> <pcm file> [onscreen|offscreen] [tab port] [main]
            "--selftest-gui" if args.len() >= 6 => app::run(Some(app::Test {
                secs: selftest::arg(&args, 2, 30), dump: args[3].clone(), port: selftest::arg(&args, 4, 18765), pcm: args[5].clone(),
                on_screen: args.get(6).map(|s| s == "onscreen").unwrap_or(false),
                tab_port: selftest::arg(&args, 7, 0),
                main: args.iter().any(|s| s == "main"),
            })),
            "--install-shortcut" => install::shortcut(),
            "--selftest-sources" => app::sources_report(args.get(2).map(|s| s.as_str()).unwrap_or("sources.txt")),
            "" => app::run(None),
            _ => Ok(()),
        }
    };
    if let Err(e) = done {
        core::log_init();
        core::log(&format!("{mode} failed: {e:?}"));
        std::process::exit(1);
    }
}
