//! Verification harness for the `capture` crate.
//!
//!   capture-test resample <report>              the C# --selftest-resample checks
//!   capture-test pacing <report>                the C# --selftest-pacing checks
//!   capture-test format <report>                mix formats that are accepted and refused
//!   capture-test panic <report>                 a panicking frame sink is survived
//!   capture-test apps <report>                  the source list (audio_apps)
//!   capture-test run <report> <seconds> [app] [zeros]
//!
//! `zeros` opens a render stream on the default endpoint that writes nothing but
//! zero samples, released with AUDCLNT_BUFFERFLAGS_SILENT, so that the packet path
//! can be exercised without a single audible sample.

#![allow(non_snake_case)]

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use livesubs::capture::{sound_sessions, Deaf, 
    audio_apps, frame_panic_scenario, mix_format_error, pacing_scenario, tone_gain_db, Capture,
    FRAME_SAMPLES,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let code = match cmd {
        "resample" => resample(&args),
        "pacing" => pacing(&args),
        "format" => format(&args),
        "panic" => panics(&args),
        "apps" => apps(&args),
        "sessions" => sessions(&args),
        "deaf" => deaf(&args),
        "run" => run(&args),
        _ => {
            eprintln!("usage: capture-test resample|pacing|format|panic|apps <report> | capture-test run <report> <seconds> [app] [zeros]");
            2
        }
    };
    std::process::exit(code);
}

struct Report(File);

impl Report {
    fn open(path: &str) -> Report {
        let p = std::path::Path::new(path);
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        Report(File::create(p).expect("create report"))
    }

    fn line(&mut self, s: &str) {
        println!("{}", s);
        let _ = writeln!(self.0, "{}", s);
        let _ = self.0.flush();
    }
}

/// The same checks as the C# `--selftest-resample`: how much of each tone survives
/// conversion to 16 kHz. Tones below ~6 kHz must pass; tones above 8 kHz would alias
/// and must be removed.
fn resample(a: &[String]) -> i32 {
    let mut w = Report::open(a.get(2).map(|s| s.as_str()).unwrap_or("selftest-resample.txt"));
    let mut ok = true;
    for rate in [44100u32, 48000, 96000] {
        for hz in [300.0f64, 1000.0, 3000.0, 5500.0, 9000.0, 12000.0, 15000.0, 20000.0] {
            let db = tone_gain_db(rate, hz);
            let pass = if hz <= 5500.0 {
                db.abs() <= 1.5
            } else {
                db <= -40.0
            };
            ok &= pass;
            w.line(&format!(
                "{} Hz in, tone {:.0} Hz: {:.1} dB {}",
                rate,
                hz,
                db,
                if pass { "ok" } else { "FAIL" }
            ));
        }
    }
    w.line(if ok { "PASS" } else { "FAIL" });
    if ok {
        0
    } else {
        1
    }
}

/// The C# `--selftest-pacing`.
fn pacing(a: &[String]) -> i32 {
    let mut w = Report::open(a.get(2).map(|s| s.as_str()).unwrap_or("selftest-pacing.txt"));
    let (l1, ok1) = pacing_scenario(true);
    w.line(&l1);
    let (l2, ok2) = pacing_scenario(false);
    w.line(&l2);
    w.line(if ok1 && ok2 { "PASS" } else { "FAIL" });
    if ok1 && ok2 {
        0
    } else {
        1
    }
}

/// Mix formats a driver may report. The real ones must be taken, and anything whose
/// frames are too small for its own channels must be refused: the packet reader trusts
/// `block_align` and would otherwise read past the packet.
fn format(a: &[String]) -> i32 {
    let mut w = Report::open(a.get(2).map(|s| s.as_str()).unwrap_or("selftest-format.txt"));
    // (channels, rate, bits, block_align, float, must be accepted)
    let cases: [(u16, u32, u16, u16, bool, bool); 12] = [
        (2, 48000, 32, 8, true, true),   // the usual shared-mode mix format
        (2, 44100, 16, 4, false, true),  // plain 16-bit stereo
        (2, 48000, 24, 6, false, true),  // packed 24-bit
        (8, 48000, 32, 32, true, true),  // 7.1 float
        (1, 16000, 16, 2, false, true),  // mono, the lowest rate the app takes
        (2, 48000, 32, 32, true, true),  // a frame larger than its samples need
        (2, 48000, 32, 4, true, false),  // two float channels in four bytes
        (6, 48000, 16, 4, false, false), // six channels in a two-channel frame
        (2, 48000, 24, 4, false, false), // 24-bit read out of 16-bit frames
        (0, 48000, 16, 4, false, false), // no channels
        (2, 100, 16, 4, false, false),   // 100 Hz
        (2, 48000, 8, 2, false, false),  // 8-bit, which the reader cannot do
    ];
    let mut ok = true;
    for (ch, rate, bits, align, float, want) in cases {
        let err = mix_format_error(ch, rate, bits, align, float);
        let pass = err.is_none() == want;
        ok &= pass;
        w.line(&format!(
            "{} ch, {} Hz, {} bit {}, blockAlign {}: {} {}",
            ch,
            rate,
            bits,
            if float { "float" } else { "int  " },
            align,
            match &err {
                None => "accepted".to_string(),
                Some(e) => format!("refused ({})", e),
            },
            if pass { "ok" } else { "FAIL" }
        ));
    }
    w.line(if ok { "PASS" } else { "FAIL" });
    if ok {
        0
    } else {
        1
    }
}

/// With panic = "unwind" a panicking frame sink must cost its own frame and nothing else.
fn panics(a: &[String]) -> i32 {
    let mut w = Report::open(a.get(2).map(|s| s.as_str()).unwrap_or("selftest-panic.txt"));
    let (line, ok) = frame_panic_scenario();
    w.line(&line);
    w.line(if ok { "PASS" } else { "FAIL" });
    if ok {
        0
    } else {
        1
    }
}

fn apps(a: &[String]) -> i32 {
    let mut w = Report::open(a.get(2).map(|s| s.as_str()).unwrap_or("capture-apps.txt"));
    let list = audio_apps();
    w.line(&format!("audio_apps(): {} entries", list.len()));
    for n in &list {
        w.line(&format!("  {}", n));
    }
    0
}

/// The sound sessions the source list is made from: a render stream of this process (writing
/// only silence) must show up as a session that is playing, and be gone when it stops.
fn sessions(a: &[String]) -> i32 {
    let mut w = Report::open(a.get(2).map(|s| s.as_str()).unwrap_or("capture-sessions.txt"));
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let me = std::process::id();
    let show = |w: &mut Report, when: &str, list: &[(u32, bool)]| {
        w.line(&format!("{}: {}", when, list.iter().map(|(p, on)| format!("{}{}", p, if *on { " (playing)" } else { "" })).collect::<Vec<_>>().join(", ")));
    };
    let before = sound_sessions();
    show(&mut w, "before", &before);
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let t = thread::spawn(move || silent_render(s2));
    thread::sleep(Duration::from_millis(1500));
    let during = sound_sessions();
    show(&mut w, "while this process renders silence", &during);
    stop.store(true, Ordering::SeqCst);
    let _ = t.join();
    thread::sleep(Duration::from_millis(500));
    let after = sound_sessions();
    show(&mut w, "after it stopped", &after);
    let ok = during.iter().any(|&(p, on)| p == me && on) && !after.iter().any(|&(p, on)| p == me && on);
    w.line(&format!("this process ({}) playing while it rendered and not after: {}", me, if ok { "PASS" } else { "FAIL" }));
    if ok { 0 } else { 1 }
}

/// When an app's own capture counts as hearing nothing of it (then everything the PC plays is
/// taken instead): a check every 2 s of (packets so far, playing by Windows' account).
fn deaf(a: &[String]) -> i32 {
    let mut w = Report::open(a.get(2).map(|s| s.as_str()).unwrap_or("capture-deaf.txt"));
    let cases: [(&str, &[(u64, bool)], Option<i64>); 5] = [
        ("playing and heard", &[(0, true), (10, true), (20, true), (30, true), (40, true), (50, true)], None),
        ("playing, never heard", &[(0, true), (0, true), (0, true), (0, true), (0, true), (0, true)], Some(6000)),
        ("playback just started, heard a moment later", &[(0, false), (0, true), (5, true), (15, true), (25, true), (35, true)], None),
        ("paused all along", &[(0, false), (0, false), (0, false), (0, false), (0, false), (0, false)], None),
        ("heard, then a long stretch playing with nothing", &[(10, true), (20, true), (20, true), (20, true), (20, true), (20, true)], Some(10000)),
    ];
    let mut ok = true;
    for (name, looks, want) in cases {
        let mut d = Deaf::new(0);
        let mut got = None;
        for (i, &(packets, playing)) in looks.iter().enumerate() {
            let now = 2000 * i as i64 + 2000;
            if d.step(now, packets, playing) && got.is_none() {
                got = Some(now - 2000);
            }
        }
        let pass = got == want;
        ok &= pass;
        w.line(&format!("{} {}: switch {}", if pass { "ok  " } else { "FAIL" }, name,
            got.map(|t| format!("at {} ms", t)).unwrap_or_else(|| "never".into())));
    }
    w.line(if ok { "PASS" } else { "FAIL" });
    if ok { 0 } else { 1 }
}

fn run(a: &[String]) -> i32 {
    let report = a.get(2).map(|s| s.as_str()).unwrap_or("capture-run.txt");
    let secs: u64 = a
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
        .max(1);
    let mut app = a.get(4).cloned().unwrap_or_default();
    let zeros = a.iter().any(|s| s == "zeros");
    // switch=<process name> calls set_target half way through the run
    let switch = a
        .iter()
        .find(|s| s.starts_with("switch="))
        .map(|s| s["switch=".len()..].to_string());
    if app == "zeros" || app.starts_with("switch=") {
        app = String::new();
    }
    let mut w = Report::open(report);

    let t0 = Instant::now();
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let frames = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let nonzero = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU64::new(0));
    let per_sec: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(vec![0; (secs + 3) as usize]));

    let note = |s: String| {
        let mut l = log.lock().unwrap();
        l.push(format!("{:>8.3}s {}", t0.elapsed().as_secs_f64(), s));
    };

    let render_stop = Arc::new(AtomicBool::new(false));
    let render = if zeros {
        let stop = render_stop.clone();
        Some(std::thread::spawn(move || silent_render(stop)))
    } else {
        None
    };

    let (lg, fr, by, nz, pk, ps) = (
        log.clone(),
        frames.clone(),
        bytes.clone(),
        nonzero.clone(),
        peak.clone(),
        per_sec.clone(),
    );
    let on_frame = Box::new(move |b: &[u8]| {
        fr.fetch_add(1, Ordering::SeqCst);
        by.fetch_add(b.len() as u64, Ordering::SeqCst);
        let mut hi = 0u64;
        let mut nzc = 0u64;
        let mut i = 0usize;
        while i + 1 < b.len() {
            let v = i16::from_le_bytes([b[i], b[i + 1]]);
            if v != 0 {
                nzc += 1;
                let m = v.unsigned_abs() as u64;
                if m > hi {
                    hi = m;
                }
            }
            i += 2;
        }
        if nzc > 0 {
            nz.fetch_add(nzc, Ordering::SeqCst);
        }
        if hi > 0 {
            peak_max(&pk, hi);
        }
        let sec = t0.elapsed().as_secs() as usize;
        let mut v = ps.lock().unwrap();
        if sec < v.len() {
            v[sec] += 1;
        }
        if b.len() != FRAME_SAMPLES * 2 {
            lg.lock()
                .unwrap()
                .push(format!("BAD FRAME SIZE {}", b.len()));
        }
    });
    let st = log.clone();
    let on_status = Box::new(move |s: &str| {
        st.lock().unwrap().push(format!(
            "{:>8.3}s status: {}",
            t0.elapsed().as_secs_f64(),
            if s.is_empty() { "capturing" } else { s }
        ));
    });
    let dg = log.clone();
    let diag = Box::new(move |s: &str| {
        dg.lock()
            .unwrap()
            .push(format!("{:>8.3}s {}", t0.elapsed().as_secs_f64(), s));
    });

    w.line(&format!(
        "capture-test run {} s, target {}{}",
        secs,
        if app.is_empty() {
            "(everything the PC plays)".to_string()
        } else {
            format!("\"{}\"", app)
        },
        if zeros {
            ", silent render stream open"
        } else {
            ""
        }
    ));

    let cap = Capture::start(&app, on_frame, on_status, diag);
    note("capture started".to_string());

    let mut next = Instant::now() + Duration::from_secs(2);
    let mut marks: Vec<String> = Vec::new();
    let mut switched = switch.is_none();
    while t0.elapsed() < Duration::from_millis(secs * 1000 + 150) {
        std::thread::sleep(Duration::from_millis(20));
        if !switched && t0.elapsed() >= Duration::from_millis(secs * 500) {
            switched = true;
            let to = switch.clone().unwrap_or_default();
            note(format!("set_target(\"{}\")", to));
            cap.set_target(&to);
        }
        if Instant::now() >= next {
            next += Duration::from_secs(2);
            let c = cap.counters();
            marks.push(format!(
                "{:>8.3}s frames {:5}  packets {:6}  input {:9}  out {:8}  pad {:8}  trimmed {:6}  silent {:6}  disc {:4}  reopens {:3}  rms {:.4}",
                t0.elapsed().as_secs_f64(),
                frames.load(Ordering::SeqCst),
                c.packets,
                c.input_frames,
                c.out_samples,
                c.pad_samples,
                c.trimmed_samples,
                c.silent_packets,
                c.discontinuity_packets,
                c.reopens,
                cap.last_rms()
            ));
        }
    }

    let fmt_note = cap.format_note();
    let c = cap.counters();
    let rms_last = cap.last_rms();
    let stop_at = Instant::now();
    cap.stop();
    let stop_ms = stop_at.elapsed().as_millis();
    render_stop.store(true, Ordering::SeqCst);
    if let Some(h) = render {
        let r = h.join();
        match r {
            Ok(Ok(n)) => w.line(&format!(
                "silent render stream: {} buffers of zero samples released as SILENT",
                n
            )),
            Ok(Err(e)) => w.line(&format!("silent render stream failed: {}", e)),
            Err(_) => w.line("silent render stream panicked"),
        }
    }

    let elapsed = t0.elapsed().as_secs_f64();
    let nframes = frames.load(Ordering::SeqCst);
    w.line(&format!("wall clock          : {:.0} ms", elapsed * 1000.0));
    w.line(&format!(
        "mix format          : {}",
        if fmt_note.is_empty() {
            "(not opened)".to_string()
        } else {
            fmt_note
        }
    ));
    w.line(&format!("packets             : {}", c.packets));
    w.line(&format!("  of which silent   : {}", c.silent_packets));
    w.line(&format!("  discontinuities   : {}", c.discontinuity_packets));
    w.line(&format!("input frames        : {}", c.input_frames));
    w.line(&format!("reopens             : {}", c.reopens));
    w.line(&format!(
        "16 kHz samples out  : {}  (expected ~{})",
        c.out_samples,
        secs * 16000
    ));
    w.line(&format!(
        "  of which padding  : {}{}",
        c.pad_samples,
        if c.out_samples > 0 {
            format!(
                "  ({:.1} %)",
                100.0 * c.pad_samples as f64 / c.out_samples as f64
            )
        } else {
            String::new()
        }
    ));
    w.line(&format!("  trimmed (latency) : {}", c.trimmed_samples));
    w.line(&format!(
        "frames emitted      : {}  ({:.2} per second, {} bytes each)",
        nframes,
        nframes as f64 / elapsed,
        if nframes > 0 {
            bytes.load(Ordering::SeqCst) / nframes
        } else {
            0
        }
    ));
    w.line(&format!(
        "non-zero samples    : {}  (peak {})",
        nonzero.load(Ordering::SeqCst),
        peak.load(Ordering::SeqCst)
    ));
    w.line(&format!("last_rms()          : {:.6}", rms_last));
    w.line(&format!("stop() took         : {} ms", stop_ms));
    {
        let v = per_sec.lock().unwrap();
        w.line(&format!(
            "frames per second   : {}",
            v.iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    w.line("");
    w.line("-- counters while running --");
    for m in &marks {
        w.line(m);
    }
    w.line("");
    w.line("-- status and diagnostics --");
    for s in log.lock().unwrap().iter() {
        w.line(s);
    }

    // same criterion as the C# --selftest-audio
    let ok = c.out_samples >= (secs as f64 * 16000.0 * 0.92) as u64;
    w.line("");
    w.line(&format!("RESULT: {}", if ok { "PASS" } else { "FAIL" }));
    if ok {
        0
    } else {
        1
    }
}

fn peak_max(cell: &AtomicU64, v: u64) {
    let mut cur = cell.load(Ordering::SeqCst);
    while v > cur {
        match cell.compare_exchange(cur, v, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return,
            Err(c) => cur = c,
        }
    }
}

// ------------------------------------------------- a render stream of zeros only

use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

/// Opens the default render endpoint and keeps releasing buffers of zero samples,
/// flagged AUDCLNT_BUFFERFLAGS_SILENT so the driver substitutes silence outright.
/// Nothing audible can come out of this; it only makes the mixer run so that the
/// loopback capture sees packets.
fn silent_render(stop: Arc<AtomicBool>) -> Result<u64, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let r = silent_render_inner(stop);
        CoUninitialize();
        r
    }
}

unsafe fn silent_render_inner(stop: Arc<AtomicBool>) -> Result<u64, String> {
    let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .map_err(|e| format!("CoCreateInstance {:?}", e))?;
    let dev = en
        .GetDefaultAudioEndpoint(eRender, eMultimedia)
        .map_err(|e| format!("GetDefaultAudioEndpoint {:?}", e))?;
    let client: IAudioClient = dev
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("Activate {:?}", e))?;
    let fmt = client
        .GetMixFormat()
        .map_err(|e| format!("GetMixFormat {:?}", e))?;
    let block = (*fmt).nBlockAlign as u32;
    client
        .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 10_000_000, 0, fmt, None)
        .map_err(|e| format!("Initialize {:?}", e))?;
    CoTaskMemFree(Some(fmt as *const core::ffi::c_void));
    let size = client
        .GetBufferSize()
        .map_err(|e| format!("GetBufferSize {:?}", e))?;
    let render: IAudioRenderClient = client
        .GetService()
        .map_err(|e| format!("GetService {:?}", e))?;
    client.Start().map_err(|e| format!("Start {:?}", e))?;
    let mut n = 0u64;
    while !stop.load(Ordering::SeqCst) {
        let pad = client.GetCurrentPadding().unwrap_or(size);
        let free = size.saturating_sub(pad);
        if free > 0 {
            if let Ok(p) = render.GetBuffer(free) {
                std::ptr::write_bytes(p, 0, (free * block) as usize);
                let _ = render.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
                n += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = client.Stop();
    Ok(n)
}
