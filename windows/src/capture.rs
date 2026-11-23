//! WASAPI shared-mode loopback of the default render endpoint, downmixed to
//! mono 16 kHz and handed out as 1600-sample (3200-byte) s16le frames.
//!
//! Loopback delivers nothing while the PC is silent, so frames are released
//! against a monotonic clock and short-fed frames are zero-padded: the server
//! needs real-time audio to detect end-of-sentence from trailing silence.
//!
//! A port of `windows/Capture.cs` (`LoopbackCapture`) with no async runtime:
//! one capture thread and one pacer thread, exactly as in the C#.

#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::core::{w, Interface as _, GUID, PCWSTR, PROPVARIANT};
use windows::Win32::Foundation::{
    CloseHandle, BOOL, FALSE, FILETIME, HANDLE, HWND, LPARAM, TRUE, WAIT_OBJECT_0,
};
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AudioSessionStateActive,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IAgileObject,
    IAgileObject_Impl, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, GetProcessTimes, OpenProcess, WaitForSingleObject,
    PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, GW_OWNER,
};

pub const RATE: u32 = 16000;
pub const FRAME_SAMPLES: usize = 1600;
/// How long an app may play without a packet reaching its own capture before everything the
/// PC plays is taken instead.
const DEAF_AFTER_MS: i64 = 6000;
/// Shown while that switch is made (cleared once the stream runs, like the other lines).
const NOT_ON_ITS_OWN: &str = "このアプリの音は単体では取れないため、PC の音すべてを使います…";

const SYNCHRONIZE: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(0x0010_0000);

/// Latency control. Audio that arrives late (a stalled capture thread, a device clock
/// that runs a little fast) would otherwise wait in the queue as extra delay for the
/// rest of the session. It is worked off by leaving out packets from inside pauses.
const SURPLUS_HIGH: i64 = (RATE as i64) * 80 / 1000; // start trimming above 80 ms of backlog
const SURPLUS_TARGET: i64 = (RATE as i64) * 30 / 1000; // ...until 30 ms: enough to ride out jitter

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

const WAITING_FOR_DEVICE: &str = "音声デバイスを待っています…";

/// Everything the C# exposed as public fields on `LoopbackCapture`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub packets: u64,
    pub input_frames: u64,
    pub out_samples: u64,
    pub pad_samples: u64,
    pub trimmed_samples: u64,
    pub discontinuity_packets: u64,
    pub silent_packets: u64,
    pub reopens: u32,
}

pub struct Capture {
    sh: Arc<Shared>,
    pacer: Option<Slot>,
    thread: Option<Slot>,
}

impl Capture {
    /// Starts the capture thread. `target_app` "" = everything the PC plays, else a process name.
    pub fn start(
        target_app: &str,
        on_frame: Box<dyn Fn(&[u8]) + Send>,
        on_status: Box<dyn Fn(&str) + Send>,
        diag: Box<dyn Fn(&str) + Send>,
    ) -> Capture {
        let sh = Arc::new(Shared::new(on_frame, on_status, diag));
        *lk(&sh.target) = target_app.to_string();
        // The pacer is deliberately separate from the capture thread: opening a
        // loopback stream can block for many seconds on some machines, and the
        // server still needs frames at real-time pace during that time.
        let pacer = Slot::spawn("LiveSubs-pacer", sh.clone(), pace);
        let thread = Slot::spawn("LiveSubs-capture", sh.clone(), capture_loop);
        Capture {
            sh,
            pacer: Some(pacer),
            thread: Some(thread),
        }
    }

    /// Switch what is captured; the running stream is reopened within a moment.
    pub fn set_target(&self, app: &str) {
        *lk(&self.sh.target) = app.to_string();
        lk(&self.sh.deaf_app).clear(); // a new choice is tried on its own first
        self.sh.retarget.store(true, Ordering::SeqCst);
    }

    /// 0..1, latest emitted frame (sidebar level meter).
    pub fn last_rms(&self) -> f32 {
        f32::from_bits(self.sh.last_rms.load(Ordering::SeqCst))
    }

    pub fn counters(&self) -> Counters {
        self.sh.ctr.snapshot()
    }

    /// The device mix format as the C# logged it; "" until a stream has been opened.
    pub fn format_note(&self) -> String {
        lk(&self.sh.fmt_note).clone()
    }

    pub fn stop(self) {
        let mut me = self;
        me.sh.stop.store(true, Ordering::SeqCst);
        if let Some(p) = me.pacer.take() {
            p.join_for(1000);
        }
        if let Some(t) = me.thread.take() {
            // the capture thread may be stuck inside IAudioClient::Initialize
            t.join_for(1500);
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // C# ran both threads as background threads; in Rust a forgotten thread would
        // outlive the object, so dropping without stop() at least asks them to finish.
        self.sh.stop.store(true, Ordering::SeqCst);
    }
}

// ----------------------------------------------------------------- self-tests

/// Self-test: gain in dB of a sine at `hz` after conversion from `in_rate` to 16 kHz.
pub fn tone_gain_db(in_rate: u32, hz: f64) -> f64 {
    let mut r = Resamp::new(in_rate);
    for i in 0..in_rate {
        let t = 2.0 * std::f64::consts::PI * hz * (i as f64) / (in_rate as f64);
        r.push((0.5 * t.sin()) as f32);
    }
    let mut sq = 0.0f64;
    let mut n = 0i64;
    for i in 800..r.stage.len() {
        sq += (r.stage[i] as f64) * (r.stage[i] as f64);
        n += 1;
    }
    let rms = (sq / (n.max(1) as f64)).sqrt();
    20.0 * (rms.max(1e-9) / (0.5 / 2.0f64.sqrt())).log10()
}

/// Self-test: why a mix format would be refused, `None` when it is usable. A driver that
/// reports a `WAVEFORMATEX` whose frames are too small for its own channel count would
/// otherwise make the packet reader index past the packet.
pub fn mix_format_error(channels: u16, rate: u32, bits: u16, block_align: u16, is_float: bool) -> Option<String> {
    let mf = MixFormat {
        tag: if is_float { WAVE_FORMAT_IEEE_FLOAT } else { WAVE_FORMAT_PCM },
        channels,
        rate,
        block_align,
        bits,
        is_float,
    };
    mf.validate().err()
}

/// Self-test: a frame sink that panics is logged and costs nothing but its own frame.
/// Returns the report line and whether the pacer went on delivering afterwards.
pub fn frame_panic_scenario() -> (String, bool) {
    let seen = Arc::new(AtomicU64::new(0));
    let n = seen.clone();
    let logged: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let lines = logged.clone();
    let sh = Arc::new(Shared::new(
        Box::new(move |_: &[u8]| {
            if n.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("the frame sink gave up");
            }
        }),
        Box::new(|_| {}),
        Box::new(move |s: &str| lines.lock().unwrap_or_else(|e| e.into_inner()).push(s.to_string())),
    ));
    let mut r = reset_resampler(&sh, RATE);
    let pacer = Slot::spawn("panic-selftest", sh.clone(), pace);
    // a second of silence is enough: the pacer releases ten frames a second
    let sw = Instant::now();
    while sw.elapsed() < Duration::from_millis(1000) {
        r.push_silence(160, &sh);
        thread::sleep(Duration::from_millis(10));
    }
    sh.stop.store(true, Ordering::SeqCst);
    pacer.join_for(1000);

    let frames = seen.load(Ordering::SeqCst);
    let said = logged
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|l| l.contains("frame sink threw") && l.contains("gave up"));
    let ok = said && frames >= 5;
    (
        format!(
            "a panicking frame sink: {} frames delivered, the panic {} logged{}",
            frames,
            if said { "was" } else { "was NOT" },
            if ok { "  ok" } else { "  FAIL" }
        ),
        ok,
    )
}

/// Self-test of the latency control, driven like the real thing: 10 ms packets in real
/// time, a 400 ms stall whose audio then arrives in one burst, and the real pacer.
/// `pauses` alternates 300 ms of loud signal with 200 ms of soft background.
pub fn pacing_scenario(pauses: bool) -> (String, bool) {
    let loud_out = Arc::new(AtomicU64::new(0));
    let sink_total = loud_out.clone();
    let sh = Arc::new(Shared::new(
        Box::new(move |b: &[u8]| {
            let mut c = 0u64;
            let mut i = 0usize;
            while i + 1 < b.len() {
                let v = i16::from_le_bytes([b[i], b[i + 1]]);
                if v > 3000 || v < -3000 {
                    c += 1;
                }
                i += 2;
            }
            if c > 0 {
                sink_total.fetch_add(c, Ordering::SeqCst);
            }
        }),
        Box::new(|_| {}),
        Box::new(|_| {}),
    ));
    let mut r = reset_resampler(&sh, RATE);
    let pacer = Slot::spawn("pacing-selftest", sh.clone(), pace);

    let sw = Instant::now();
    let mut loud_in: u64 = 0;
    let mut sent: i64 = 0;
    let mut peak: i64 = 0;
    let mut at_end: i64 = 0;
    const TOTAL: i64 = 7000;
    // run on past the end of the input: the backlog (up to 1 s) still has to play out
    loop {
        let ms = sw.elapsed().as_millis() as i64;
        if ms >= TOTAL + 1500 {
            break;
        }
        let stalled = ms >= 2000 && ms < 2400;
        let due = ms.min(TOTAL) / 10;
        while !stalled && sent < due {
            let loud = !pauses || (sent % 50) < 30;
            for i in 0..160 {
                let v = (if loud { 0.3f32 } else { 0.0005f32 }) * if i & 1 == 0 { 1.0 } else { -1.0 };
                r.push(v);
            }
            if loud {
                loud_in += 160;
            }
            r.flush(&sh);
            sent += 1;
        }
        let surplus = lk(&sh.fifo).surplus;
        if ms > 2400 && surplus > peak {
            peak = surplus;
        }
        if ms > TOTAL - 1500 && ms <= TOTAL && surplus > at_end {
            at_end = surplus;
        }
        thread::sleep(Duration::from_millis(2));
    }
    sh.stop.store(true, Ordering::SeqCst);
    pacer.join_for(1000);

    let trimmed = sh.ctr.trimmed_samples.load(Ordering::SeqCst) as i64;
    let out = loud_out.load(Ordering::SeqCst);
    let kept = out == loud_in;
    let drained = if pauses { at_end <= SURPLUS_HIGH } else { trimmed == 0 };
    let ok = kept && drained && peak >= 4800;
    let line = format!(
        "{}: backlog after the stall {} ms, in the last 1.5 s {} ms, trimmed {} ms, loud samples in {} out {}{}",
        if pauses { "speech with pauses" } else { "continuous sound  " },
        peak / 16,
        at_end / 16,
        trimmed / 16,
        loud_in,
        out,
        if ok { "  ok" } else { "  FAIL" }
    );
    (line, ok)
}

// -------------------------------------------------------------- shared state

fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct Ctr {
    packets: AtomicU64,
    input_frames: AtomicU64,
    out_samples: AtomicU64,
    pad_samples: AtomicU64,
    trimmed_samples: AtomicU64,
    discontinuity_packets: AtomicU64,
    silent_packets: AtomicU64,
    reopens: AtomicU32,
}

impl Ctr {
    fn snapshot(&self) -> Counters {
        Counters {
            packets: self.packets.load(Ordering::SeqCst),
            input_frames: self.input_frames.load(Ordering::SeqCst),
            out_samples: self.out_samples.load(Ordering::SeqCst),
            pad_samples: self.pad_samples.load(Ordering::SeqCst),
            trimmed_samples: self.trimmed_samples.load(Ordering::SeqCst),
            discontinuity_packets: self.discontinuity_packets.load(Ordering::SeqCst),
            silent_packets: self.silent_packets.load(Ordering::SeqCst),
            reopens: self.reopens.load(Ordering::SeqCst),
        }
    }
}

/// `_fifo`, `_surplus` and `_trimming`: the C# guards all three with the same lock.
struct Fifo {
    q: VecDeque<f32>,
    surplus: i64,
    trimming: bool,
}

struct Cb {
    on_frame: Box<dyn Fn(&[u8]) + Send>,
    on_status: Box<dyn Fn(&str) + Send>,
    diag: Box<dyn Fn(&str) + Send>,
}

struct Shared {
    fifo: Mutex<Fifo>,
    cb: Mutex<Cb>,
    ctr: Ctr,
    stop: AtomicBool,
    retarget: AtomicBool,
    /// An app whose own sound never reaches a capture of that app alone, although it plays
    /// (Windows' Media Player was one): everything the PC plays is taken instead, until the
    /// user picks another source.
    deaf_app: Mutex<String>,
    /// "" = everything the PC plays; otherwise a process name such as "chrome".
    target: Mutex<String>,
    device_id: Mutex<String>,
    fmt_note: Mutex<String>,
    last_rms: AtomicU32,
    clock: Instant,
}

impl Shared {
    fn new(
        on_frame: Box<dyn Fn(&[u8]) + Send>,
        on_status: Box<dyn Fn(&str) + Send>,
        diag: Box<dyn Fn(&str) + Send>,
    ) -> Shared {
        Shared {
            fifo: Mutex::new(Fifo {
                q: VecDeque::new(),
                surplus: 0,
                trimming: false,
            }),
            cb: Mutex::new(Cb {
                on_frame,
                on_status,
                diag,
            }),
            ctr: Ctr::default(),
            stop: AtomicBool::new(false),
            retarget: AtomicBool::new(false),
            deaf_app: Mutex::new(String::new()),
            target: Mutex::new(String::new()),
            device_id: Mutex::new(String::new()),
            fmt_note: Mutex::new(String::new()),
            last_rms: AtomicU32::new(0),
            clock: Instant::now(),
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn retargeting(&self) -> bool {
        self.retarget.load(Ordering::SeqCst)
    }

    fn elapsed_ms(&self) -> i64 {
        self.clock.elapsed().as_millis() as i64
    }

    fn say(&self, s: &str) {
        let cb = lk(&self.cb);
        (cb.on_status)(s);
    }

    fn d(&self, s: &str) {
        let cb = lk(&self.cb);
        (cb.diag)(s);
    }

    fn frame(&self, b: &[u8]) {
        let r = {
            let cb = lk(&self.cb);
            catch_unwind(AssertUnwindSafe(|| (cb.on_frame)(b)))
        };
        if let Err(e) = r {
            self.d(&format!("frame sink threw: {}", panic_msg(&e)));
        }
    }
}

fn panic_msg(e: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

/// A thread plus the flag that says it has finished, so that it can be joined
/// with a timeout the way `Thread.Join(ms)` does in the C#.
struct Slot {
    done: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Slot {
    fn spawn(name: &str, sh: Arc<Shared>, body: fn(&Shared)) -> Slot {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let handle = thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let _guard = DoneGuard(flag);
                body(&sh);
            })
            .expect("spawn capture thread");
        Slot { done, handle }
    }

    fn join_for(self, ms: u64) {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while !self.done.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if self.done.load(Ordering::SeqCst) {
            let _ = self.handle.join();
        }
        // else: give up as the C# Join(timeout) does and leave it to finish on its own.
    }
}

struct DoneGuard(Arc<AtomicBool>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------- the resampler

struct Resamp {
    in_rate: u32,
    decim: u32, // >0 when the ratio is an integer: keep one sample in decim
    acc_n: u32,
    pos: f64,
    prev: f32,
    have_prev: bool,
    taps: Option<Vec<f32>>, // anti-alias low-pass applied before any rate reduction
    hist: Vec<f32>,
    hist_pos: usize,
    stage: Vec<f32>,
    avg_pow: f64,  // running loudness of what is being captured
    quiet_run: i32, // consecutive quiet packets
}

/// `ResetResampler`: a fresh resampler plus an emptied hand-over queue.
fn reset_resampler(sh: &Shared, in_rate: u32) -> Resamp {
    let r = Resamp::new(in_rate);
    let mut f = lk(&sh.fifo);
    f.q.clear();
    f.surplus = 0;
    f.trimming = false;
    r
}

impl Resamp {
    fn new(in_rate: u32) -> Resamp {
        let taps = build_low_pass(in_rate);
        let n = taps.as_ref().map(|t| t.len()).unwrap_or(0);
        Resamp {
            in_rate,
            decim: if in_rate % RATE == 0 { in_rate / RATE } else { 0 },
            acc_n: 0,
            pos: 0.0,
            prev: 0.0,
            have_prev: false,
            taps,
            hist: vec![0.0; n],
            hist_pos: 0,
            stage: Vec::new(),
            avg_pow: 0.0,
            quiet_run: 0,
        }
    }

    fn low_pass(&mut self, x: f32) -> f32 {
        let taps = self.taps.as_ref().unwrap();
        self.hist[self.hist_pos] = x;
        let mut acc = 0.0f64;
        let mut idx = self.hist_pos as isize;
        for k in 0..taps.len() {
            acc += (taps[k] * self.hist[idx as usize]) as f64;
            idx -= 1;
            if idx < 0 {
                idx = self.hist.len() as isize - 1;
            }
        }
        self.hist_pos += 1;
        if self.hist_pos == self.hist.len() {
            self.hist_pos = 0;
        }
        acc as f32
    }

    fn push(&mut self, mono: f32) {
        let mut mono = mono;
        if self.taps.is_some() {
            mono = self.low_pass(mono);
        }
        if self.decim == 1 {
            self.stage.push(mono);
            return;
        }
        if self.decim > 1 {
            // already band-limited by the low-pass, so every decim-th sample is enough
            self.acc_n += 1;
            if self.acc_n >= self.decim {
                self.stage.push(mono);
                self.acc_n = 0;
            }
            return;
        }
        // non-integer ratio: linear interpolation at pos (in input samples)
        let step = self.in_rate as f64 / RATE as f64;
        if !self.have_prev {
            self.prev = mono;
            self.have_prev = true;
            self.pos = 0.0;
            self.stage.push(mono);
            self.pos += step;
            return;
        }
        while self.pos < 1.0 {
            let frac = self.pos;
            self.stage
                .push((self.prev as f64 + ((mono - self.prev) as f64) * frac) as f32);
            self.pos += step;
        }
        self.pos -= 1.0;
        self.prev = mono;
    }

    /// Hand one packet worth of resampled audio to the pacer in a single lock.
    fn flush(&mut self, sh: &Shared) {
        let n = self.stage.len();
        if n == 0 {
            return;
        }
        let mut pow = 0.0f64;
        for i in 0..n {
            pow += (self.stage[i] as f64) * (self.stage[i] as f64);
        }
        pow /= n as f64;
        // "Quiet" is relative to the running level, so the gaps between words still count
        // over background music. Three packets in a row keeps it out of the short closures
        // inside words.
        let quiet = pow <= self.avg_pow * 0.15 || pow < 1e-7;
        self.quiet_run = if quiet { self.quiet_run + 1 } else { 0 };
        self.avg_pow = self.avg_pow * 0.997 + pow * 0.003;

        let drop;
        {
            let mut f = lk(&sh.fifo);
            if f.surplus > SURPLUS_HIGH {
                f.trimming = true;
            } else if f.surplus <= SURPLUS_TARGET {
                f.trimming = false;
            }
            drop = f.trimming && self.quiet_run >= 3;
            if drop {
                f.surplus -= n as i64;
            } else {
                f.q.extend(self.stage.iter().copied());
            }
        }
        if drop {
            sh.ctr
                .trimmed_samples
                .fetch_add(n as u64, Ordering::SeqCst);
        }
        self.stage.clear();
    }

    fn push_silence(&mut self, frames: usize, sh: &Shared) {
        for _ in 0..frames {
            self.push(0.0);
        }
        self.flush(sh);
    }

    fn push_pcm(&mut self, b: &[u8], frames: usize, mf: &MixFormat, sh: &Shared) {
        let ch = mf.channels as usize;
        let stride = mf.block_align as usize;
        let bytes_per_sample = stride / ch;
        for f in 0..frames {
            let off = f * stride;
            let mut sum = 0.0f64;
            for c in 0..ch {
                let o = off + c * bytes_per_sample;
                if mf.is_float {
                    if mf.bits == 64 {
                        sum += f64::from_le_bytes(b[o..o + 8].try_into().unwrap());
                    } else {
                        sum += f32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as f64;
                    }
                } else if mf.bits == 16 {
                    sum += i16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as f64 / 32768.0;
                } else if mf.bits == 32 {
                    sum += i32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as f64 / 2147483648.0;
                } else {
                    let mut v = b[o] as i32 | ((b[o + 1] as i32) << 8) | ((b[o + 2] as i32) << 16);
                    if v & 0x800000 != 0 {
                        v |= -16777216; // 0xFF000000
                    }
                    sum += v as f64 / 8388608.0;
                }
            }
            self.push((sum / ch as f64) as f32);
        }
        self.flush(sh);
    }
}

/// Whisper only hears up to 8 kHz. Reducing the rate without removing what lies
/// above that folds it back into the speech band as noise, so everything goes
/// through a windowed-sinc low-pass (cut-off 7.2 kHz) first.
fn build_low_pass(in_rate: u32) -> Option<Vec<f32>> {
    if in_rate <= RATE {
        return None;
    }
    let half = round_half_even(12.0 * in_rate as f64 / RATE as f64) as i64;
    let fc = 0.45 * RATE as f64 / in_rate as f64; // cycles per input sample
    let len = (2 * half + 1) as usize;
    let mut t = vec![0.0f32; len];
    let mut sum = 0.0f64;
    for i in 0..len {
        let m = i as i64 - half;
        let sinc = if m == 0 {
            2.0 * fc
        } else {
            (2.0 * std::f64::consts::PI * fc * m as f64).sin() / (std::f64::consts::PI * m as f64)
        };
        let hamming = 0.54 + 0.46 * (std::f64::consts::PI * m as f64 / half as f64).cos();
        t[i] = (sinc * hamming) as f32;
        sum += t[i] as f64;
    }
    for i in 0..len {
        t[i] = (t[i] as f64 / sum) as f32; // unity gain at DC
    }
    Some(t)
}

/// `Math.Round` in C# rounds a midpoint to the nearest even number.
fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        r - x.signum()
    } else {
        r
    }
}

// --------------------------------------------------------------------- pacing

#[derive(Default)]
struct PacerState {
    emitted: i64,
    prev_take: usize, // samples of real audio in the previously emitted frame
}

fn pace(sh: &Shared) {
    let mut st = PacerState::default();
    while !sh.stopped() {
        if let Err(e) = catch_unwind(AssertUnwindSafe(|| emit_due(sh, &mut st))) {
            sh.d(&format!("pacer: {}", panic_msg(&e)));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn emit_due(sh: &Shared, st: &mut PacerState) {
    let expected = sh.elapsed_ms() * (RATE as i64 / 1000);
    let mut emitted = false;
    if expected - st.emitted > 2 * RATE as i64 {
        // The clock jumped (the PC slept, the process was suspended): skip the
        // gap instead of replaying hours of silence frame by frame.
        st.emitted = expected - FRAME_SAMPLES as i64;
        lk(&sh.fifo).q.clear();
    }
    while expected - st.emitted >= FRAME_SAMPLES as i64 {
        let mut buf = [0u8; FRAME_SAMPLES * 2];
        let take;
        {
            let mut f = lk(&sh.fifo);
            take = f.q.len().min(FRAME_SAMPLES);
            // A short frame right after pure silence is sound starting: the padding
            // belongs in front of it, or a hole is cut into the first syllable.
            let at = if st.prev_take == 0 {
                FRAME_SAMPLES - take
            } else {
                0
            };
            for i in 0..take {
                let v = f.q.pop_front().unwrap() as f64 * 32767.0;
                let mut s = (if v >= 0.0 { v + 0.5 } else { v - 0.5 }) as i32;
                if s > 32767 {
                    s = 32767;
                } else if s < -32768 {
                    s = -32768;
                }
                buf[(at + i) * 2] = (s & 0xFF) as u8;
                buf[(at + i) * 2 + 1] = ((s >> 8) & 0xFF) as u8;
            }
        }
        st.prev_take = take;
        emitted = true;
        let mut sq = 0.0f64;
        for i in 0..FRAME_SAMPLES {
            let v = i16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]) as f64;
            sq += v * v;
        }
        let rms = (sq / FRAME_SAMPLES as f64).sqrt() / 32768.0;
        sh.last_rms.store((rms as f32).to_bits(), Ordering::SeqCst);
        sh.ctr
            .pad_samples
            .fetch_add((FRAME_SAMPLES - take) as u64, Ordering::SeqCst);
        sh.ctr
            .out_samples
            .fetch_add(FRAME_SAMPLES as u64, Ordering::SeqCst);
        st.emitted += FRAME_SAMPLES as i64;
        sh.frame(&buf);
    }
    let mut f = lk(&sh.fifo);
    while f.q.len() > RATE as usize {
        f.q.pop_front(); // last resort: never hold more than ~1 s
    }
    if emitted {
        f.surplus = f.q.len() as i64;
    }
}

// -------------------------------------------------------------- the mix format

/// WAVEFORMATEX(TENSIBLE) read straight out of the CoTaskMem block IAudioClient hands us.
#[derive(Clone, Copy)]
struct MixFormat {
    tag: u16,
    channels: u16,
    rate: u32,
    block_align: u16,
    bits: u16,
    is_float: bool,
}

impl MixFormat {
    /// How many bytes of a frame one channel's sample takes, as `push_pcm` reads it;
    /// `None` for a width it cannot read.
    fn sample_bytes(&self) -> Option<usize> {
        match (self.is_float, self.bits) {
            (true, 64) => Some(8),
            (true, 32) => Some(4),
            (false, 16) => Some(2),
            (false, 24) => Some(3),
            (false, 32) => Some(4),
            _ => None,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.channels < 1 || self.rate < 4000 || self.block_align < 1 {
            return Err(format!("implausible mix format: {}", self));
        }
        let w = match self.sample_bytes() {
            Some(w) => w,
            None => {
                return Err(if self.is_float {
                    format!("unsupported float width {}", self.bits)
                } else {
                    format!("unsupported PCM width {}", self.bits)
                })
            }
        };
        // Every channel of a frame has to lie inside that frame, or push_pcm would read
        // the next frame's bytes and, in the last frame, past the packet.
        if (self.block_align as usize) < self.channels as usize * w {
            return Err(format!(
                "a frame of {} bytes does not hold {} channels of {} bit: {}",
                self.block_align, self.channels, self.bits, self
            ));
        }
        Ok(())
    }

    unsafe fn parse(p: *const u8) -> MixFormat {
        let tag = (p.add(0) as *const u16).read_unaligned();
        let cb_size = (p.add(16) as *const u16).read_unaligned();
        let mut f = MixFormat {
            tag,
            channels: (p.add(2) as *const u16).read_unaligned(),
            rate: (p.add(4) as *const u32).read_unaligned(),
            block_align: (p.add(12) as *const u16).read_unaligned(),
            bits: (p.add(14) as *const u16).read_unaligned(),
            is_float: tag == WAVE_FORMAT_IEEE_FLOAT,
        };
        if tag == WAVE_FORMAT_EXTENSIBLE && cb_size >= 22 {
            let sub = (p.add(24) as *const GUID).read_unaligned();
            f.is_float = sub == SUBTYPE_IEEE_FLOAT;
        }
        f
    }
}

impl fmt::Display for MixFormat {
    fn fmt(&self, w: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            w,
            "{} Hz, {} ch, {} bit, {}, tag 0x{:04X}, blockAlign {}",
            self.rate,
            self.channels,
            self.bits,
            if self.is_float { "float" } else { "int" },
            self.tag,
            self.block_align
        )
    }
}

// ------------------------------------------------------------- capture threads

fn check<T>(r: windows::core::Result<T>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{} failed 0x{:08X}", what, e.code().0 as u32))
}

struct CoMem(*mut core::ffi::c_void);

impl Drop for CoMem {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0)) };
        }
    }
}

struct StopGuard<'a>(&'a IAudioClient);

impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.Stop();
        }
    }
}

struct EventH(HANDLE);

impl Drop for EventH {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn capture_loop(sh: &Shared) {
    unsafe {
        // the C# capture thread ran with SetApartmentState(ApartmentState.MTA)
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut first = true;
    while !sh.stopped() {
        if !first {
            sh.ctr.reopens.fetch_add(1, Ordering::SeqCst);
        }
        first = false;
        sh.retarget.store(false, Ordering::SeqCst);
        let app = lk(&sh.target).clone();
        // A panic inside a session ends that session only, as an exception did in the
        // C#: it is logged and the stream is opened again after the usual delay.
        let deaf = !app.is_empty() && *lk(&sh.deaf_app) == app;
        let r = catch_unwind(AssertUnwindSafe(|| {
            if app.is_empty() || deaf {
                sh.say(if deaf { NOT_ON_ITS_OWN } else { WAITING_FOR_DEVICE }); // cleared once the stream is running
                unsafe { session(sh) }
            } else {
                unsafe { app_session(sh, &app) }
            }
        }));
        let r = r.unwrap_or_else(|e| Err(panic_msg(&e)));
        if let Err(e) = r {
            sh.d(&format!("capture session ended: {}", e));
            sh.say(WAITING_FOR_DEVICE);
        }
        if sh.stopped() {
            break;
        }
        if !sh.retargeting() {
            thread::sleep(Duration::from_millis(800));
        }
    }
    unsafe { CoUninitialize() };
}

unsafe fn read_id(dev: &IMMDevice) -> Option<String> {
    let p = dev.GetId().ok()?;
    if p.is_null() {
        return None;
    }
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
    s
}

unsafe fn session(sh: &Shared) -> Result<(), String> {
    let en: IMMDeviceEnumerator = check(
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL),
        "CoCreateInstance(MMDeviceEnumerator)",
    )?;
    let dev = check(
        en.GetDefaultAudioEndpoint(eRender, eMultimedia),
        "GetDefaultAudioEndpoint",
    )?;
    let id = read_id(&dev);
    *lk(&sh.device_id) = id.clone().unwrap_or_default();

    let client: IAudioClient = check(dev.Activate(CLSCTX_ALL, None), "Activate(IAudioClient)")?;
    let raw = check(client.GetMixFormat(), "GetMixFormat")?;
    let fmt = CoMem(raw as *mut core::ffi::c_void);
    let mf = MixFormat::parse(raw as *const u8);
    *lk(&sh.fmt_note) = mf.to_string();
    sh.d(&format!(
        "endpoint {} / {}",
        id.as_deref().unwrap_or(""),
        mf
    ));
    mf.validate()?;

    let open = Instant::now();
    check(
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            10000000,
            0,
            raw,
            None,
        ),
        "IAudioClient.Initialize",
    )?;
    let t_init = open.elapsed().as_millis() as i64;

    let capture: IAudioCaptureClient =
        check(client.GetService(), "GetService(IAudioCaptureClient)")?;

    let mut rs = reset_resampler(sh, mf.rate);
    check(client.Start(), "IAudioClient.Start")?;
    let started = StopGuard(&client);
    sh.d(&format!(
        "stream open: Initialize {} ms, GetService+Start {} ms",
        t_init,
        open.elapsed().as_millis() as i64 - t_init
    ));
    sh.say("");

    let mut next_device_check = sh.elapsed_ms() + 2000;
    let mut scratch: Vec<u8> = Vec::new();

    while !sh.stopped() && !sh.retargeting() {
        drain_packets(&capture, &mf, &mut scratch, &mut rs, sh)?;

        if sh.elapsed_ms() >= next_device_check {
            next_device_check = sh.elapsed_ms() + 2000;
            if let Ok(cur) = en.GetDefaultAudioEndpoint(eRender, eMultimedia) {
                if let Some(nid) = read_id(&cur) {
                    if Some(&nid) != id.as_ref() {
                        sh.d(&format!("default render endpoint changed to {}", nid));
                        return Ok(());
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(10));
    }
    drop(started);
    drop(fmt);
    Ok(())
}

/// Move every packet that is ready into the resampler.
unsafe fn drain_packets(
    capture: &IAudioCaptureClient,
    mf: &MixFormat,
    scratch: &mut Vec<u8>,
    rs: &mut Resamp,
    sh: &Shared,
) -> Result<(), String> {
    let mut avail = check(capture.GetNextPacketSize(), "GetNextPacketSize")?;
    while avail > 0 {
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut frames: u32 = 0;
        let mut flags: u32 = 0;
        let mut dev_pos: u64 = 0;
        let mut qpc: u64 = 0;
        check(
            capture.GetBuffer(
                &mut data,
                &mut frames,
                &mut flags,
                Some(&mut dev_pos),
                Some(&mut qpc),
            ),
            "GetBuffer",
        )?;
        if frames == 0 {
            let _ = capture.ReleaseBuffer(0);
            break;
        }
        sh.ctr.packets.fetch_add(1, Ordering::SeqCst);
        sh.ctr
            .input_frames
            .fetch_add(frames as u64, Ordering::SeqCst);
        if flags & (AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0 {
            sh.ctr
                .discontinuity_packets
                .fetch_add(1, Ordering::SeqCst);
        }
        let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
        if silent {
            sh.ctr.silent_packets.fetch_add(1, Ordering::SeqCst);
        }
        let need = frames as usize * mf.block_align as usize;
        if !silent {
            if data.is_null() {
                // Nothing to read, and the packet is not marked silent: feeding the
                // previous packet's bytes again would put a repeat of that sound into
                // the stream. End the session instead, as the C# Marshal.Copy threw.
                let _ = capture.ReleaseBuffer(0);
                return Err(format!("GetBuffer gave no data for {} frames", frames));
            }
            if scratch.len() < need {
                scratch.resize(need, 0);
            }
            std::ptr::copy_nonoverlapping(data, scratch.as_mut_ptr(), need);
        }
        let _ = capture.ReleaseBuffer(frames);
        if silent {
            rs.push_silence(frames as usize, sh);
        } else {
            rs.push_pcm(&scratch[..need], frames as usize, mf, sh);
        }

        avail = check(capture.GetNextPacketSize(), "GetNextPacketSize")?;
    }
    Ok(())
}

/// Capture only what one application (and the processes it started) is playing.
unsafe fn app_session(sh: &Shared, app: &str) -> Result<(), String> {
    let pid = find_app_pid(app);
    if pid == 0 {
        sh.say(&format!("「{}」の起動を待っています…", app));
        for _ in 0..20 {
            if sh.stopped() || sh.retargeting() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        return Ok(());
    }

    sh.say(&format!("「{}」の音声に接続しています…", app));
    let client = activate_process_loopback(pid)?;

    // This virtual device has no mix format of its own; ask for one the resampler
    // divides exactly (48 kHz) and let Windows convert whatever the app plays.
    let mf = MixFormat {
        tag: WAVE_FORMAT_PCM,
        channels: 2,
        rate: 48000,
        bits: 16,
        block_align: 4,
        is_float: false,
    };
    let wf = WAVEFORMATEX {
        wFormatTag: mf.tag,
        nChannels: mf.channels,
        nSamplesPerSec: mf.rate,
        nAvgBytesPerSec: mf.rate * mf.block_align as u32,
        nBlockAlign: mf.block_align,
        wBitsPerSample: mf.bits,
        cbSize: 0,
    };
    *lk(&sh.fmt_note) = mf.to_string();
    let device_id = format!("app:{} (pid {})", app, pid);
    *lk(&sh.device_id) = device_id.clone();

    check(
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            2000000,
            0,
            &wf,
            None,
        ),
        "IAudioClient.Initialize(process loopback)",
    )?;
    let ready = EventH(
        CreateEventW(None, FALSE, FALSE, PCWSTR::null())
            .map_err(|e| format!("CreateEventW failed 0x{:08X}", e.code().0 as u32))?,
    );

    // A panic in here may not unwind past the two drops below, which would close the event
    // first and leave the client that was given it running for an instant with a closed
    // handle. `capture_loop` catches it too, but only after that damage: caught here, a panic
    // ends the session the way an error does and the order below still holds.
    let r = catch_unwind(AssertUnwindSafe(|| -> Result<(), String> {
        check(client.SetEventHandle(ready.0), "SetEventHandle")?;

        let capture: IAudioCaptureClient =
            check(client.GetService(), "GetService(IAudioCaptureClient)")?;

        let mut rs = reset_resampler(sh, mf.rate);
        check(client.Start(), "IAudioClient.Start")?;
        let started = StopGuard(&client);
        sh.d(&format!("capturing only {}", device_id));
        sh.say("");

        let mut next_check = sh.elapsed_ms() + 2000;
        let mut deaf = Deaf::new(sh.ctr.packets.load(Ordering::SeqCst));
        let mut scratch: Vec<u8> = Vec::new();
        while !sh.stopped() && !sh.retargeting() {
            WaitForSingleObject(ready.0, 100);
            drain_packets(&capture, &mf, &mut scratch, &mut rs, sh)?;
            if sh.elapsed_ms() >= next_check {
                next_check = sh.elapsed_ms() + 2000;
                if !process_alive(pid) {
                    sh.d(&format!("{} (pid {}) has exited", app, pid));
                    break;
                }
                let playing = sound_sessions().contains(&(pid, true));
                if deaf.step(sh.elapsed_ms(), sh.ctr.packets.load(Ordering::SeqCst), playing) {
                    sh.d(&format!("{} (pid {}) is playing but none of its sound comes through on its own; taking everything the PC plays instead", app, pid));
                    *lk(&sh.deaf_app) = app.to_string();
                    break;
                }
            }
        }
        // the stream is stopped and the capture client let go here, before the lines below
        drop(started);
        Ok(())
    }))
    .unwrap_or_else(|e| Err(panic_msg(&e)));

    // The order of the C# `finally`: stop, let the capture client go, then the audio
    // client, and the event it was given last of all. Rust's own drop order would run
    // backwards and close the event while the client could still signal it.
    drop(client);
    drop(ready);
    r
}

/// Whether an app's own capture hears nothing of it: playing all along, by Windows' own
/// account, and not one packet in all that time. (Just after playback starts the session is
/// active a moment before the first packet arrives, hence "all along".)
pub struct Deaf {
    since: Option<i64>,
    seen: u64,
}

impl Deaf {
    pub fn new(packets: u64) -> Deaf {
        Deaf { since: None, seen: packets }
    }

    /// One look, every couple of seconds: true once it has been deaf for DEAF_AFTER_MS.
    pub fn step(&mut self, now_ms: i64, packets: u64, playing: bool) -> bool {
        let quiet = packets == self.seen && playing;
        self.seen = packets;
        if !quiet {
            self.since = None;
            return false;
        }
        now_ms - *self.since.get_or_insert(now_ms) >= DEAF_AFTER_MS
    }
}

// ------------------------------------------------------- per-process loopback

/// Windows' per-application loopback (Windows 10 2004 and later): an audio client that hears
/// only one process and the processes it started, instead of everything the PC plays.
#[windows::core::implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Handler {
    /// The handler owns the event, as the C# `Handler` owned its `ManualResetEvent`:
    /// mmdevapi can call back long after the wait below has given up, and only the COM
    /// object's own lifetime says when the handle may be closed.
    done: EventH,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Handler_Impl {
    fn ActivateCompleted(
        &self,
        _op: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(self.done.0);
        }
        Ok(())
    }
}

impl IAgileObject_Impl for Handler_Impl {}

#[repr(C)]
struct ActivationParams {
    kind: i32, // AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK
    pid: u32,  // TargetProcessId
    mode: i32, // PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
}

/// A PROPVARIANT holding the activation params as a VT_BLOB, byte for byte as the C# built it.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    r1: u16,
    r2: u16,
    r3: u16,
    cb: u32,
    pad: u32,
    data: *mut core::ffi::c_void,
}

unsafe fn activate_process_loopback(pid: u32) -> Result<IAudioClient, String> {
    let mut blob = ActivationParams {
        kind: 1,
        pid,
        mode: 0,
    };
    let variant = BlobVariant {
        vt: 65, // VT_BLOB
        r1: 0,
        r2: 0,
        r3: 0,
        cb: 12,
        pad: 0,
        data: &mut blob as *mut ActivationParams as *mut core::ffi::c_void,
    };
    let done = CreateEventW(None, TRUE, FALSE, PCWSTR::null())
        .map_err(|e| format!("CreateEventW failed 0x{:08X}", e.code().0 as u32))?;
    // `handler` lives until this function returns, so the event it owns stays open for
    // the wait below; after that only the COM object decides when it is closed.
    let handler: IActivateAudioInterfaceCompletionHandler = Handler { done: EventH(done) }.into();
    let op = check(
        ActivateAudioInterfaceAsync(
            w!("VAD\\Process_Loopback"),
            &IAudioClient::IID,
            Some(&variant as *const BlobVariant as *const PROPVARIANT),
            &handler,
        ),
        "ActivateAudioInterfaceAsync",
    )?;
    if WaitForSingleObject(done, 8000) != WAIT_OBJECT_0 {
        return Err("process loopback activation timed out".to_string());
    }
    let mut result = windows::core::HRESULT(0);
    let mut iface: Option<windows::core::IUnknown> = None;
    check(
        op.GetActivateResult(&mut result, &mut iface),
        "GetActivateResult",
    )?;
    check(result.ok(), "process loopback activation")?;
    iface
        .ok_or_else(|| "process loopback activation returned nothing".to_string())?
        .cast::<IAudioClient>()
        .map_err(|e| format!("process loopback client failed 0x{:08X}", e.code().0 as u32))
}


// ------------------------------------------------------------------ processes

/// (pid, process name without ".exe") for every process on the machine.
fn processes() -> Vec<(u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return out,
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(e.szExeFile.len());
                let mut name = String::from_utf16_lossy(&e.szExeFile[..n]);
                if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
                    name.truncate(name.len() - 4);
                }
                out.push((e.th32ProcessID, name));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// pid -> title of its main window, the way .NET's Process.MainWindowHandle picks one
/// (the first visible top-level window without an owner).
fn main_windows() -> Vec<(u32, String)> {
    let mut v: Vec<(u32, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut v as *mut Vec<(u32, String)> as isize),
        );
    }
    v
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let v = &mut *(lparam.0 as *mut Vec<(u32, String)>);
    if IsWindowVisible(hwnd).as_bool() && GetWindow(hwnd, GW_OWNER).is_err() {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 && !v.iter().any(|(p, _)| *p == pid) {
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
            v.push((pid, String::from_utf16_lossy(&buf[..n])));
        }
    }
    TRUE
}

fn start_time(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid).ok()?;
        let mut c = FILETIME::default();
        let mut x = FILETIME::default();
        let mut k = FILETIME::default();
        let mut u = FILETIME::default();
        let ok = GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        Some(((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64)
    }
}

fn process_alive(pid: u32) -> bool {
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, pid) {
            Ok(h) => {
                let alive = WaitForSingleObject(h, 0) != WAIT_OBJECT_0;
                let _ = CloseHandle(h);
                alive
            }
            // no handle (a protected process, or it is gone): fall back to the process list
            Err(_) => processes().iter().any(|(p, _)| *p == pid),
        }
    }
}

/// The process that owns the app's window is the root of its tree (browsers play
/// sound from a child process); failing that, the oldest process of that name.
fn find_app_pid(name: &str) -> u32 {
    let wins = main_windows();
    let mut best = 0u32;
    let mut oldest = u64::MAX;
    for (pid, pname) in processes() {
        if !pname.eq_ignore_ascii_case(name) {
            continue;
        }
        if wins.iter().any(|(p, _)| *p == pid) {
            return pid;
        }
        if let Some(t) = start_time(pid) {
            if t < oldest {
                oldest = t;
                best = pid;
            }
        }
    }
    best
}

/// Every process with a sound session on the default output device, and whether it is
/// playing now. Through this a source can be picked that shows no window of its own: a player
/// in the notification area, or a Store app (its windows belong to ApplicationFrameHost).
/// The calling thread must have COM initialised.
pub fn sound_sessions() -> Vec<(u32, bool)> {
    let mut out: Vec<(u32, bool)> = Vec::new();
    unsafe {
        let Ok(e) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return out };
        let Ok(dev) = e.GetDefaultAudioEndpoint(eRender, eMultimedia) else { return out };
        let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { return out };
        let Ok(list) = mgr.GetSessionEnumerator() else { return out };
        for i in 0..list.GetCount().unwrap_or(0) {
            let Ok(s) = list.GetSession(i) else { continue };
            let Ok(pid) = s.cast::<IAudioSessionControl2>().and_then(|c| c.GetProcessId()) else { continue };
            if pid == 0 {
                continue; // the system sounds
            }
            let playing = s.GetState().map(|st| st == AudioSessionStateActive).unwrap_or(false);
            match out.iter_mut().find(|(p, _)| *p == pid) {
                Some(e) => e.1 |= playing,
                None => out.push((pid, playing)),
            }
        }
    }
    out
}

/// Process name of every app that currently has a window: the list the source menu
/// offers (`MainWindow.RunningApps`). The C# also built a display label from the
/// file description; only the names are needed to pick a capture target.
pub fn audio_apps() -> Vec<String> {
    let self_pid = unsafe { GetCurrentProcessId() };
    let wins = main_windows();
    let mut out: Vec<String> = Vec::new();
    for (pid, name) in processes() {
        if pid == self_pid || out.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let title = match wins.iter().find(|(p, _)| *p == pid) {
            Some((_, t)) => t,
            None => continue,
        };
        if title.is_empty() {
            continue;
        }
        if name == "explorer"
            || name == "ApplicationFrameHost"
            || name == "TextInputHost"
            || name == "SystemSettings"
        {
            continue;
        }
        out.push(name);
    }
    out.sort_by_key(|n| n.to_uppercase());
    out
}
