//! Drives the engine link and writes everything it reports to a file.
//!
//!   engine-test.exe <report> <port> <distro> <dir> <pcm-file|-> <seconds>
//!   engine-test.exe parse <report>
//!   engine-test.exe fake <report>
//!
//! The PCM file is 16 kHz mono 16-bit; it is streamed 3200 bytes every 100 ms as soon
//! as the link is live, and silence is sent once it runs out.
//!
//! `fake` puts the link in front of two servers of its own making, on ports 18854 and
//! 18853: one that dribbles its handshake response, one that answers the handshake and
//! then never reads again. Neither must be able to hold the link or `stop()`.

use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use livesubs::engine::{parse, Config, Conn, EngineLink, Event};

const SEQ: Ordering = Ordering::SeqCst;
const FRAME: usize = 3200; // 100 ms of 16 kHz mono 16-bit

struct Log {
    f: Mutex<File>,
    t0: Instant,
}

impl Log {
    fn new(path: &str) -> Log {
        Log { f: Mutex::new(File::create(path).expect("cannot write the report")), t0: Instant::now() }
    }
    fn line(&self, s: &str) {
        let txt = format!("[{:8.3}] {}\n", self.t0.elapsed().as_secs_f64(), s);
        let mut f = self.f.lock().unwrap();
        let _ = f.write_all(txt.as_bytes());
        let _ = f.flush();
        print!("{}", txt);
    }
    fn plain(&self, s: &str) {
        let mut f = self.f.lock().unwrap();
        let _ = f.write_all(s.as_bytes());
        let _ = f.write_all(b"\n");
        let _ = f.flush();
        println!("{}", s);
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() == 3 && a[1] == "parse" {
        std::process::exit(parse_checks(&a[2]));
    }
    if a.len() == 3 && a[1] == "fake" {
        std::process::exit(fake_checks(&a[2]));
    }
    if a.len() != 7 {
        eprintln!("usage: engine-test <report> <port> <distro> <dir> <pcm|-> <seconds>");
        eprintln!("       engine-test parse|fake <report>");
        std::process::exit(2);
    }
    let report = a[1].clone();
    let port: u16 = a[2].parse().expect("port");
    let distro = a[3].clone();
    let dir = a[4].clone();
    let pcm_path = a[5].clone();
    let seconds: f64 = a[6].parse().expect("seconds");

    let pcm: Vec<u8> = if pcm_path == "-" {
        Vec::new()
    } else {
        std::fs::read(&pcm_path).expect("cannot read the pcm file")
    };

    let log = Arc::new(Log::new(&report));
    log.line(&format!(
        "run port={} distro={} dir={} pcm={} ({} bytes) seconds={}",
        port, distro, dir, pcm_path, pcm.len(), seconds
    ));

    let counts: Arc<[AtomicU64; 8]> = Arc::new(std::array::from_fn(|_| AtomicU64::new(0)));
    const C_STATE: usize = 0;
    const C_READY: usize = 1;
    const C_INTERIM: usize = 2;
    const C_FINAL: usize = 3;
    const C_TOPIC: usize = 4;
    const C_ERROR: usize = 5;
    const C_OTHER: usize = 6;
    const C_RAW: usize = 7;
    let states: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let lags: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));

    let l2 = log.clone();
    let c2 = counts.clone();
    let s2 = states.clone();
    let g2 = lags.clone();
    let on_event = Box::new(move |e: Event| match e {
        Event::State(c, t) => {
            c2[C_STATE].fetch_add(1, SEQ);
            s2.lock().unwrap().push(format!("{:?}", c));
            l2.line(&format!("STATE {:?} | {}", c, t));
        }
        Event::Message(m) => {
            match m.kind.as_str() {
                "ready" => {
                    c2[C_READY].fetch_add(1, SEQ);
                    l2.line(&format!("MSG ready asr={} mt={} device={}", m.asr, m.mt, m.device));
                }
                "interim" => {
                    c2[C_INTERIM].fetch_add(1, SEQ);
                    l2.line(&format!("MSG interim id={} en={} ja={}", m.id, m.en, m.ja));
                }
                "final" => {
                    c2[C_FINAL].fetch_add(1, SEQ);
                    g2.lock().unwrap().push(m.lag_ms);
                    l2.line(&format!(
                        "MSG final id={} lag_ms={} en={} ja={}",
                        m.id, m.lag_ms, m.en, m.ja
                    ));
                }
                "topic" => {
                    c2[C_TOPIC].fetch_add(1, SEQ);
                    l2.line(&format!(
                        "MSG topic id={} seg={} title={} summary={}",
                        m.id, m.seg, m.title, m.summary
                    ));
                }
                "error" => {
                    c2[C_ERROR].fetch_add(1, SEQ);
                    l2.line(&format!("MSG error message={}", m.error));
                }
                _ => {
                    c2[C_OTHER].fetch_add(1, SEQ);
                    l2.line(&format!("MSG other kind={} id={}", m.kind, m.id));
                }
            }
        }
        Event::Raw(s) => {
            // one verbatim frame is enough to show the Raw path works
            if c2[C_RAW].fetch_add(1, SEQ) == 0 {
                l2.line(&format!("RAW {}", s));
            }
        }
    });

    let l3 = log.clone();
    let diag = Box::new(move |s: &str| l3.line(&format!("DIAG {}", s)));

    let link = EngineLink::start(Config { distro, dir, port, want_raw: true }, on_event, diag);

    let start = Instant::now();
    let mut pos = 0usize;
    let mut sent = 0u64;
    let mut silent = 0u64;
    let mut max_gap_ms = 0f64;
    let mut live_at: Option<f64> = None;
    let mut last = Instant::now();
    let zeros = vec![0u8; FRAME];
    let mut tick = 0u64;
    while start.elapsed().as_secs_f64() < seconds {
        let due = start + Duration::from_millis(100 * tick);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        tick += 1;
        if !link.is_live() {
            continue;
        }
        if live_at.is_none() {
            live_at = Some(log.t0.elapsed().as_secs_f64());
            log.line(&format!("live after {:.3} s, engine pid {}", live_at.unwrap(), link.engine_pid()));
            last = Instant::now();
        }
        let gap = last.elapsed().as_secs_f64() * 1000.0;
        if gap > max_gap_ms {
            max_gap_ms = gap;
        }
        last = Instant::now();
        if pos + FRAME <= pcm.len() {
            link.send_frame(&pcm[pos..pos + FRAME]);
            pos += FRAME;
            sent += 1;
        } else {
            link.send_frame(&zeros);
            silent += 1;
        }
    }

    // the two control messages, now that the stream is over
    log.line("send set_topics(true) and send_reset(new_stream=true)");
    link.set_topics(true);
    link.send_reset(true);
    std::thread::sleep(Duration::from_millis(500));

    log.line(&format!(
        "stopping: launched_engine={} engine_pid={}",
        link.launched_engine(),
        link.engine_pid()
    ));
    let t = Instant::now();
    link.stop();
    log.line(&format!("stopped in {} ms", t.elapsed().as_millis()));

    let lv = lags.lock().unwrap();
    let lag_min = lv.iter().min().copied().unwrap_or(-1);
    let lag_max = lv.iter().max().copied().unwrap_or(-1);
    let lag_avg = if lv.is_empty() { 0 } else { lv.iter().map(|x| *x as i64).sum::<i64>() / lv.len() as i64 };
    log.plain(&format!(
        "SUMMARY states={} ready={} interim={} final={} topic={} error={} other={} raw={}",
        counts[C_STATE].load(SEQ),
        counts[C_READY].load(SEQ),
        counts[C_INTERIM].load(SEQ),
        counts[C_FINAL].load(SEQ),
        counts[C_TOPIC].load(SEQ),
        counts[C_ERROR].load(SEQ),
        counts[C_OTHER].load(SEQ),
        counts[C_RAW].load(SEQ),
    ));
    log.plain(&format!(
        "SUMMARY frames_speech={} frames_silence={} max_send_gap_ms={:.1} lag_ms min/avg/max={}/{}/{}",
        sent, silent, max_gap_ms, lag_min, lag_avg, lag_max
    ));
    log.plain(&format!("SUMMARY state_path={}", states.lock().unwrap().join(" -> ")));
}

// ----------------------------------------------------------------- the fake servers

/// Accepts for ever and answers every connection one byte of an HTTP header every 2 s:
/// a server that is there, says something, and never finishes.
fn dribble_server(l: TcpListener, accepted: Arc<AtomicU64>) {
    for s in l.incoming() {
        let Ok(mut s) = s else { continue };
        accepted.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf); // the request, as far as it comes in half a second
            for b in b"HTTP/1.1 101 Switching Protocols\r\n".iter() {
                if s.write_all(&[*b]).is_err() {
                    return;
                }
                thread::sleep(Duration::from_secs(2));
            }
        });
    }
}

/// Answers the handshake, then says whether a close frame arrived before the socket went: on
/// a clean quit the engine is told that the app is going, instead of only seeing the transport
/// drop. It serves one connection and lets its listener go at once, so that the port is free
/// again for the servers below.
fn polite_server(l: TcpListener, closed: Arc<AtomicU64>, until: Instant) {
    let _ = l.set_nonblocking(true);
    while Instant::now() < until {
        let s = match l.accept() {
            Ok((s, _)) => s,
            Err(_) => {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        let _ = s.set_nonblocking(false);
        let _ = s.set_read_timeout(Some(Duration::from_millis(1000)));
        // The link probes the port with a connection it drops at once before it connects for
        // real; that one never gets through a handshake, and the next is taken instead.
        let Ok(mut ws) = tungstenite::accept(s) else { continue };
        let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(200)));
        let mut said = false;
        while Instant::now() < until && !said {
            match ws.read() {
                Ok(tungstenite::Message::Close(_)) => {
                    closed.fetch_add(1, Ordering::SeqCst);
                    said = true;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(ref e)) if retryable(e) => {}
                Err(_) => break,
            }
        }
        // a reset, so that nothing of this is left to keep the port from being listened on again
        let _ = socket2::SockRef::from(ws.get_ref()).set_linger(Some(Duration::ZERO));
        if said {
            return;
        }
    }
}

fn retryable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
    )
}

/// Answers the handshake properly and then never reads a byte again.
fn deaf_server(l: TcpListener, done: Arc<AtomicU64>) {
    for s in l.incoming() {
        let Ok(s) = s else { continue };
        let done = done.clone();
        thread::spawn(move || {
            let Ok(ws) = tungstenite::accept(s) else { return };
            done.fetch_add(1, Ordering::SeqCst);
            // hold the socket, read nothing: the kernel's buffers fill and stay full
            loop {
                thread::sleep(Duration::from_secs(1));
                if done.load(Ordering::SeqCst) == u64::MAX {
                    drop(ws);
                    return;
                }
            }
        });
    }
}

fn fake_checks(report: &str) -> i32 {
    let log = Arc::new(Log::new(report));
    let mut ok = 0;
    let mut bad = 0;
    {
        let mut chk = |name: &str, cond: bool, detail: String| {
            if cond {
                ok += 1;
                log.plain(&format!("PASS {} | {}", name, detail));
            } else {
                bad += 1;
                log.plain(&format!("FAIL {} | {}", name, detail));
            }
        };

        // ---- the engine is told goodbye when the link is stopped
        // On 18853, which the deaf server takes later: this one lets its listener go as soon
        // as it has taken the link's connection.
        {
            let polite = match TcpListener::bind(("127.0.0.1", 18853)) {
                Ok(l) => l,
                Err(e) => {
                    log.plain(&format!("FAIL cannot listen on 18853 | {}", e));
                    return 1;
                }
            };
            let closed = Arc::new(AtomicU64::new(0));
            let server = {
                let c = closed.clone();
                let until = Instant::now() + Duration::from_secs(10);
                thread::spawn(move || polite_server(polite, c, until))
            };
            let lines0: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let live0 = Arc::new(AtomicBool::new(false));
            let link0 = start_link(&log, 18853, &lines0, &live0);
            let became_live = wait_until(|| link0.is_live(), 10000);
            let t = Instant::now();
            link0.stop();
            let ms = t.elapsed().as_millis();
            let said_bye = wait_until(|| closed.load(Ordering::SeqCst) > 0, 1000);
            chk(
                "goodbye-close-frame-on-stop",
                became_live && said_bye && ms < 1500,
                format!("live {}, close frame seen {}, stop() took {} ms", became_live, said_bye, ms),
            );
            let _ = server.join();
        }

        // ---- a caught panic does not make the link forget what it has already tried
        // Nothing listens on 18853 now and there is no dir= to launch from: the reason is
        // reported once, and not tried again for the next half minute. A loop that started
        // over with a clean slate after the panic would try again at once - which with a real
        // dir= means a second engine beside the one that is already running.
        {
            let lines1: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let fell = Arc::new(AtomicU64::new(0));
            let link1 = start_panicking_link(&log, 18853, &lines1, &fell);
            let threw = wait_until(|| lines_contain(&lines1, "the link thread panicked"), 10000);
            thread::sleep(Duration::from_millis(2500)); // several turns of the connect loop
            let tried = lines_count(&lines1, "cannot launch the engine");
            chk(
                "a-panic-keeps-the-launch-bookkeeping",
                threw && tried == 1,
                format!("panicked {}, the launch was tried {} time(s)", threw, tried),
            );
            link1.stop();
        }

        let dribble = match TcpListener::bind(("127.0.0.1", 18854)) {
            Ok(l) => l,
            Err(e) => {
                log.plain(&format!("FAIL cannot listen on 18854 | {}", e));
                return 1;
            }
        };
        let accepted = Arc::new(AtomicU64::new(0));
        {
            let a = accepted.clone();
            thread::spawn(move || dribble_server(dribble, a));
        }

        // ---- the link gives up on a dribbled handshake at its deadline
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let live = Arc::new(AtomicBool::new(false));
        let link = start_link(&log, 18854, &lines, &live);
        let gave_up = wait_until(
            || lines_contain(&lines, "connect to port 18854 failed"),
            12000,
        );
        let waited = elapsed_of(&lines, "connect to port 18854 failed");
        chk(
            "dribbled-handshake-deadline",
            gave_up && !live.load(Ordering::SeqCst),
            format!(
                "gave up after about {} s, never live, {} connections taken",
                waited,
                accepted.load(Ordering::SeqCst)
            ),
        );
        let t = Instant::now();
        link.stop();
        chk(
            "stop-after-dribbled-handshake",
            t.elapsed() < Duration::from_millis(1000),
            format!("stop() took {} ms", t.elapsed().as_millis()),
        );

        // ---- stop() while the link is in the middle of such a handshake
        let lines2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let live2 = Arc::new(AtomicBool::new(false));
        let before = accepted.load(Ordering::SeqCst);
        let link2 = start_link(&log, 18854, &lines2, &live2);
        let in_handshake = wait_until(|| accepted.load(Ordering::SeqCst) > before, 5000);
        std::thread::sleep(Duration::from_millis(1500)); // it is reading the dribble now
        let t = Instant::now();
        link2.stop();
        let ms = t.elapsed().as_millis();
        chk(
            "stop-inside-the-handshake",
            in_handshake && ms < 1000,
            format!("stop() took {} ms", ms),
        );

        // ---- a server that answers the handshake and then stops reading
        let deaf = match TcpListener::bind(("127.0.0.1", 18853)) {
            Ok(l) => l,
            Err(e) => {
                log.plain(&format!("FAIL cannot listen on 18853 | {}", e));
                return 1;
            }
        };
        let up = Arc::new(AtomicU64::new(0));
        {
            let u = up.clone();
            thread::spawn(move || deaf_server(deaf, u));
        }
        let lines3: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let live3 = Arc::new(AtomicBool::new(false));
        let link3 = start_link(&log, 18853, &lines3, &live3);
        let became_live = wait_until(|| link3.is_live(), 10000);
        // as fast as the queue takes them: the socket's buffers fill within moments
        let frame = vec![0u8; FRAME];
        let until = Instant::now() + Duration::from_secs(25);
        while !lines_contain(&lines3, "send failed") && Instant::now() < until {
            if link3.is_live() {
                link3.send_frame(&frame);
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // the send failure has to end the session by itself, before anything stops it
        let dropped = wait_until(|| lines_contain(&lines3, "disconnected from port 18853"), 3000);
        chk(
            "deaf-server-is-noticed",
            became_live && lines_contain(&lines3, "send failed") && dropped,
            format!(
                "live {}, then {}, session dropped {}",
                became_live,
                lines_find(&lines3, "send failed"),
                dropped
            ),
        );
        let t = Instant::now();
        link3.stop();
        chk(
            "stop-after-a-deaf-server",
            t.elapsed() < Duration::from_millis(2500),
            format!("stop() took {} ms", t.elapsed().as_millis()),
        );

        // ---- stop() while a send into the deaf server is blocked
        let lines4: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let live4 = Arc::new(AtomicBool::new(false));
        let link4 = start_link(&log, 18853, &lines4, &live4);
        let live_again = wait_until(|| link4.is_live(), 10000);
        let until = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < until {
            link4.send_frame(&frame); // the buffers are full long before this is over
        }
        let t = Instant::now();
        link4.stop();
        let ms = t.elapsed().as_millis();
        chk(
            "stop-while-a-send-is-blocked",
            live_again && ms < 2500,
            format!("stop() took {} ms", ms),
        );
        up.store(u64::MAX, Ordering::SeqCst);
    }
    log.plain(&format!("SUMMARY fake-server checks: {} passed, {} failed", ok, bad));
    if bad == 0 {
        0
    } else {
        1
    }
}

/// A link with no engine to launch (no dir=), so that nothing but the fake server answers.
fn start_link(
    log: &Arc<Log>,
    port: u16,
    lines: &Arc<Mutex<Vec<String>>>,
    live: &Arc<AtomicBool>,
) -> EngineLink {
    let l2 = log.clone();
    let seen = live.clone();
    let on_event = Box::new(move |e: Event| {
        if let Event::State(c, t) = e {
            if c == Conn::Live {
                seen.store(true, Ordering::SeqCst);
            }
            l2.line(&format!("STATE {:?} | {}", c, t));
        }
    });
    let l3 = log.clone();
    let keep = lines.clone();
    let diag = Box::new(move |s: &str| {
        keep.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{:.3} {}", l3.t0.elapsed().as_secs_f64(), s));
        l3.line(&format!("DIAG {}", s));
    });
    EngineLink::start(
        Config { distro: String::new(), dir: String::new(), port, want_raw: false },
        on_event,
        diag,
    )
}

/// A link whose state handler panics the first time it hears that the engine failed, where the
/// connect loop itself calls it: what a handler of the app's could do at any time.
fn start_panicking_link(
    log: &Arc<Log>,
    port: u16,
    lines: &Arc<Mutex<Vec<String>>>,
    fell: &Arc<AtomicU64>,
) -> EngineLink {
    let l2 = log.clone();
    let once = fell.clone();
    let on_event = Box::new(move |e: Event| {
        if let Event::State(c, t) = e {
            l2.line(&format!("STATE {:?} | {}", c, t));
            if c == Conn::Failed && once.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("the state handler gave up");
            }
        }
    });
    let l3 = log.clone();
    let keep = lines.clone();
    let diag = Box::new(move |s: &str| {
        keep.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{:.3} {}", l3.t0.elapsed().as_secs_f64(), s));
        l3.line(&format!("DIAG {}", s));
    });
    EngineLink::start(
        Config { distro: String::new(), dir: String::new(), port, want_raw: false },
        on_event,
        diag,
    )
}

fn lines_contain(lines: &Arc<Mutex<Vec<String>>>, what: &str) -> bool {
    lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|l| l.contains(what))
}

fn lines_count(lines: &Arc<Mutex<Vec<String>>>, what: &str) -> usize {
    lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|l| l.contains(what))
        .count()
}

fn lines_find(lines: &Arc<Mutex<Vec<String>>>, what: &str) -> String {
    lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|l| l.contains(what))
        .cloned()
        .unwrap_or_else(|| format!("(no line with \"{}\")", what))
}

/// The seconds on the diag clock at which a line turned up.
fn elapsed_of(lines: &Arc<Mutex<Vec<String>>>, what: &str) -> String {
    let line = lines_find(lines, what);
    line.split_whitespace().next().unwrap_or("?").to_string()
}

fn wait_until(mut cond: impl FnMut() -> bool, ms: u64) -> bool {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

// --------------------------------------------------------------------- parse checks

fn parse_checks(report: &str) -> i32 {
    let log = Log::new(report);
    let mut ok = 0;
    let mut bad = 0;
    {
        let mut chk = |name: &str, cond: bool, detail: String| {
            if cond {
                ok += 1;
                log.plain(&format!("PASS {} | {}", name, detail));
            } else {
                bad += 1;
                log.plain(&format!("FAIL {} | {}", name, detail));
            }
        };

        // every message type the server sends (server.py / pipeline.py)
        let j = r#"{"type":"ready","asr":"distil-large-v3.5","mt":"gemma-2-2b-jpn-it","device":"cuda"}"#;
        let m = parse(j).unwrap();
        chk(
            "ready",
            m.kind == "ready"
                && m.asr == "distil-large-v3.5"
                && m.mt == "gemma-2-2b-jpn-it"
                && m.device == "cuda"
                && m.id == -1
                && m.seg == -1
                && m.lag_ms == -1
                && m.en.is_empty(),
            format!("kind={} asr={} mt={} device={} id={} seg={} lag={}", m.kind, m.asr, m.mt, m.device, m.id, m.seg, m.lag_ms),
        );

        let j = r#"{"type":"interim","id":7,"en":"the next thing","ja":"次のこと"}"#;
        let m = parse(j).unwrap();
        chk(
            "interim",
            m.kind == "interim" && m.id == 7 && m.en == "the next thing" && m.ja == "次のこと" && m.lag_ms == -1,
            format!("kind={} id={} en={} ja={} lag={}", m.kind, m.id, m.en, m.ja, m.lag_ms),
        );

        let j = r#"{"type":"final","id":7,"en":"The next thing.","ja":"次のことです。","lag_ms":812}"#;
        let m = parse(j).unwrap();
        chk(
            "final",
            m.kind == "final" && m.id == 7 && m.en == "The next thing." && m.ja == "次のことです。" && m.lag_ms == 812 && m.seg == -1,
            format!("kind={} id={} lag={} en={} ja={}", m.kind, m.id, m.lag_ms, m.en, m.ja),
        );

        let j = r#"{"type":"topic","id":2,"seg":31,"title":"量子計算の基礎","summary":"話者は量子ビットの説明をした。"}"#;
        let m = parse(j).unwrap();
        chk(
            "topic",
            m.kind == "topic" && m.id == 2 && m.seg == 31 && m.title == "量子計算の基礎" && m.summary == "話者は量子ビットの説明をした。",
            format!("kind={} id={} seg={} title={} summary={}", m.kind, m.id, m.seg, m.title, m.summary),
        );

        let j = r#"{"type":"error","message":"CUDA out of memory"}"#;
        let m = parse(j).unwrap();
        chk(
            "error",
            m.kind == "error" && m.error == "CUDA out of memory" && m.id == -1,
            format!("kind={} error={} id={}", m.kind, m.error, m.id),
        );

        // shapes that must not produce a message
        chk("not-an-object", parse("[1,2]").is_none(), "[1,2]".into());
        chk("empty-text", parse("").is_none(), "(empty)".into());
        chk("malformed", parse(r#"{"type":"final""#).is_none(), "unterminated object".into());
        chk("bad-number", parse(r#"{"id":1.2.3}"#).is_none(), "1.2.3".into());

        // an object with nothing in it: every field at its default
        let m = parse("{}").unwrap();
        chk(
            "empty-object",
            m.kind.is_empty() && m.id == -1 && m.seg == -1 && m.lag_ms == -1 && m.ja.is_empty() && m.title.is_empty(),
            format!("id={} seg={} lag={}", m.id, m.seg, m.lag_ms),
        );

        // escapes, as a non-ensure_ascii client would send them
        let j = r#"{"type":"final","id":1,"en":"a\"b\\c\nd\/e","ja":"こんにちは","lag_ms":1200}"#;
        let m = parse(j).unwrap();
        chk(
            "escapes",
            m.en == "a\"b\\c\nd/e" && m.ja == "こんにちは" && m.lag_ms == 1200,
            format!("en={:?} ja={} lag={}", m.en, m.ja, m.lag_ms),
        );

        // nested values are stepped over, not fatal; raw UTF-8 Japanese passes through
        let j = r#"{"type":"topic","id":3,"seg":9,"title":"日本語","summary":"ok","extra":{"a":[1,2]},"arr":[1,{"b":2}]}"#;
        let m = parse(j).unwrap();
        chk(
            "nested-skipped",
            m.kind == "topic" && m.id == 3 && m.seg == 9 && m.title == "日本語" && m.summary == "ok",
            format!("kind={} id={} seg={} title={}", m.kind, m.id, m.seg, m.title),
        );

        // the coercions of the C# Convert.To*: a string number, a float lag, null, a bool
        let j = r#"{"type":"final","id":"12","lag_ms":812.0,"seg":3,"ja":null,"en":42,"device":true}"#;
        let m = parse(j).unwrap();
        chk(
            "coercions",
            m.id == 12 && m.lag_ms == 812 && m.seg == 3 && m.ja.is_empty() && m.en == "42" && m.device == "True",
            format!("id={} lag={} seg={} ja={:?} en={} device={}", m.id, m.lag_ms, m.seg, m.ja, m.en, m.device),
        );

        // whitespace and a trailing newline, as a framed message may carry
        let m = parse("  { \"type\" : \"final\" , \"id\" : 5 }\n").unwrap();
        chk("whitespace", m.kind == "final" && m.id == 5, format!("kind={} id={}", m.kind, m.id));
    }
    log.plain(&format!("SUMMARY parse checks: {} passed, {} failed", ok, bad));
    if bad == 0 {
        0
    } else {
        1
    }
}
