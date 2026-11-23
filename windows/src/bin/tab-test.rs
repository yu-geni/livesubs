//! tab-test.exe <report> [port]: the receiving end for the Chrome extension, without
//! Chrome. A port of SelfTest.Tab in windows/SelfTest.cs; it plays the extension's part
//! itself with a tungstenite client in this same process, and writes every check to the
//! report file. Exit code 0 when all of them passed.

use std::env;
use std::f64::consts::PI;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use livesubs::tabaudio::{TabAudio, EXTENSION_ORIGIN, REPLACED};
use tungstenite::client::ClientRequestBuilder;
use tungstenite::http::Uri;
use tungstenite::protocol::WebSocket;
use tungstenite::{Error as WsError, Message};

const FRAME: usize = 3200;
const HOLD: u32 = 600;
/// MAX_CONNS in tabaudio.rs: more than this at once must be turned away.
const CAP: usize = 8;

type Client = WebSocket<TcpStream>;

#[derive(Default)]
struct Gate {
    frames: Vec<Vec<u8>>,
    events: String,
    diag: Vec<String>, // written at the end: diag fires on other threads
}

fn lock(g: &Mutex<Gate>) -> MutexGuard<'_, Gate> {
    g.lock().unwrap_or_else(|e| e.into_inner())
}

fn retryable(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

struct Rep {
    f: File,
    ok: bool,
}

impl Rep {
    fn step(&mut self, name: &str, pass: bool) {
        let _ = writeln!(self.f, "{}{}", if pass { "ok   " } else { "FAIL " }, name);
        let _ = self.f.flush();
        self.ok &= pass;
    }

    fn line(&mut self, s: &str) {
        let _ = writeln!(self.f, "{}", s);
        let _ = self.f.flush();
    }
}

fn wait_for(mut cond: impl FnMut() -> bool, ms: u64) -> bool {
    let sw = Instant::now();
    while (sw.elapsed().as_millis() as u64) < ms {
        if cond() {
            return true;
        }
        sleep(Duration::from_millis(10));
    }
    cond()
}

/// Connects the way the Chrome extension does; fails when the app turns it away.
fn tab_client(port: u16, origin: Option<&str>, path: &str) -> Result<(Client, TcpStream), String> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    let raw = tcp.try_clone().map_err(|e| e.to_string())?;
    let _ = tcp.set_read_timeout(Some(Duration::from_millis(3000)));
    let uri: Uri = format!("ws://127.0.0.1:{}{}", port, path)
        .parse()
        .map_err(|e: tungstenite::http::uri::InvalidUri| e.to_string())?;
    let mut b = ClientRequestBuilder::new(uri);
    if let Some(o) = origin {
        b = b.with_header("Origin", o);
    }
    let (mut ws, _resp) = tungstenite::client(b, tcp).map_err(|e| e.to_string())?;
    // Windows keeps the timeout per handle, not per socket: it has to be set on the one
    // the WebSocket itself reads through, never on the `raw` clone beside it.
    let _ = ws.get_mut().set_read_timeout(None);
    Ok((ws, raw))
}

fn tab_send(c: &mut Client, data: &[u8]) -> Result<(), String> {
    c.send(Message::Binary(data.to_vec().into())).map_err(|e| e.to_string())
}

fn tab_close(c: &mut Client) {
    let _ = c.get_mut().set_read_timeout(Some(Duration::from_millis(200)));
    let _ = c.close(None);
    // as CloseAsync does, wait for the app's answering close
    for _ in 0..10 {
        match c.read() {
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(WsError::Io(ref e)) if retryable(e) => {}
            Err(_) => break,
        }
    }
}

fn tab_refused(port: u16, origin: Option<&str>, path: &str) -> bool {
    tab_client(port, origin, path).is_err()
}

/// Writes a masked binary frame straight onto the socket. .NET's ClientWebSocket still
/// sends after it has received a close frame, which is what the C# test does to find out
/// whether the app let the replaced tab's socket go; tungstenite refuses to.
fn raw_binary(sock: &TcpStream, payload: &[u8]) -> io::Result<()> {
    let mut h: Vec<u8> = Vec::with_capacity(14);
    h.push(0x82); // FIN, binary
    let len = payload.len();
    if len < 126 {
        h.push(0x80 | len as u8);
    } else if len <= 0xffff {
        h.push(0x80 | 126);
        h.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        h.push(0x80 | 127);
        h.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mask = [0x12u8, 0x34, 0x56, 0x78];
    h.extend_from_slice(&mask);
    let body: Vec<u8> = payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect();
    let mut s = sock;
    s.write_all(&h)?;
    s.write_all(&body)?;
    s.flush()
}

/// Opens a socket and says nothing: a handshake that never comes.
fn half_open(port: u16) -> Option<TcpStream> {
    let s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    let _ = s.set_nodelay(true);
    Some(s)
}

/// Reads everything that has arrived and then waits for the end of the connection: true once
/// the app has let the socket go, false while it is still there after `ms`.
fn read_to_end_of(s: &TcpStream, ms: u64) -> bool {
    let _ = s.set_read_timeout(Some(Duration::from_millis(ms)));
    let mut b = [0u8; 65536];
    let mut r = s;
    loop {
        match r.read(&mut b) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e) => return !retryable(&e),
        }
    }
}

/// Whether the app has let go of this socket, decided within `ms`.
fn gone(s: &TcpStream, ms: u64) -> bool {
    let _ = s.set_read_timeout(Some(Duration::from_millis(ms)));
    let mut b = [0u8; 64];
    let mut r = s;
    match r.read(&mut b) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !retryable(&e),
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(|s| s == "soak").unwrap_or(false) && args.len() >= 5 {
        std::process::exit(soak(&args[2], args[3].parse().unwrap_or(18870), args[4].parse().unwrap_or(600)));
    }
    let report = match args.get(1) {
        Some(p) => p.clone(),
        None => {
            eprintln!("usage: tab-test <report> [port]");
            std::process::exit(2);
        }
    };
    let port: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(18866);
    std::process::exit(run(&report, port));
}

/// tab-test soak <report> <port> <seconds>: one tab sending a frame every 100 ms, as the
/// extension does, for a long time; it connects again 2 s after losing the connection, as the
/// extension does. In real use the connection was lost every few minutes (reads that timed out
/// left the socket broken), so this counts every time the app ended the connection.
fn soak(report: &str, port: u16, secs: u64) -> i32 {
    let gate: Arc<Mutex<Gate>> = Arc::new(Mutex::new(Gate::default()));
    let t0 = Instant::now();
    let (on_frame, on_active, diag) = hooks(&gate, t0);
    let Some(tab) = TabAudio::start(port, 2000, on_frame, on_active, diag) else {
        eprintln!("port {} is taken", port);
        return 1;
    };
    let frame = vec![0x11u8; 3200]; // a level, not silence; nothing is ever played
    let end = Instant::now() + Duration::from_secs(secs);
    let (mut sent, mut failed) = (0u64, 0u32);
    'outer: while Instant::now() < end {
        let Ok((mut c, _raw)) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab") else {
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        let mut next = Instant::now();
        loop {
            if Instant::now() >= end {
                tab_close(&mut c);
                break 'outer;
            }
            if tab_send(&mut c, &frame).is_err() {
                failed += 1;
                thread::sleep(Duration::from_secs(2));
                break;
            }
            sent += 1;
            next += Duration::from_millis(100);
            if let Some(w) = next.checked_duration_since(Instant::now()) {
                thread::sleep(w);
            }
        }
    }
    thread::sleep(Duration::from_millis(300));
    tab.stop();
    let g = lock(&gate);
    let ended: Vec<&String> = g.diag.iter().filter(|l| l.contains("connection ended")).collect();
    let text = format!(
        "soak {} s on port {}: {} frames sent, {} received, the app ended the connection {} time(s), {} send(s) failed\n{}\n",
        secs, port, sent, g.frames.len(), ended.len(), failed,
        ended.iter().map(|l| l.as_str()).collect::<Vec<_>>().join("\n"));
    let _ = std::fs::write(report, text);
    if ended.is_empty() && failed == 0 { 0 } else { 1 }
}

fn run(report: &str, port: u16) -> i32 {
    let f = match File::create(report) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot write {}: {}", report, e);
            return 1;
        }
    };
    let mut rep = Rep { f, ok: true };
    let gate: Arc<Mutex<Gate>> = Arc::new(Mutex::new(Gate::default()));
    let t0 = Instant::now();

    let outcome = body(&mut rep, &gate, port, t0);
    if let Err(e) = &outcome {
        rep.line(&format!("exception: {}", e));
    }
    let lines = lock(&gate).diag.clone();
    for l in lines {
        rep.line(&l);
    }
    let ok = rep.ok && outcome.is_ok();
    rep.line(&format!("RESULT: {}", if ok { "PASS" } else { "FAIL" }));
    if ok {
        0
    } else {
        1
    }
}

fn hooks(
    gate: &Arc<Mutex<Gate>>,
    t0: Instant,
) -> (
    Box<dyn Fn(&[u8]) + Send + Sync>,
    Box<dyn Fn(bool) + Send + Sync>,
    Box<dyn Fn(&str) + Send + Sync>,
) {
    let a = gate.clone();
    let b = gate.clone();
    let c = gate.clone();
    (
        Box::new(move |f: &[u8]| lock(&a).frames.push(f.to_vec())),
        Box::new(move |on: bool| lock(&b).events.push(if on { '+' } else { '-' })),
        Box::new(move |s: &str| {
            let line = format!("+{:7.3}  [tab] {}", t0.elapsed().as_secs_f64(), s);
            lock(&c).diag.push(line)
        }),
    )
}

fn body(rep: &mut Rep, gate: &Arc<Mutex<Gate>>, port: u16, t0: Instant) -> Result<(), String> {
    let count = || lock(gate).frames.len();
    let seen = || lock(gate).events.clone();

    let (on_frame, on_active, diag) = hooks(gate, t0);
    let tab = TabAudio::start(port, HOLD, on_frame, on_active, diag);
    rep.step(
        &format!("the listener opens port {} without admin rights", port),
        tab.is_some(),
    );
    let tab = match tab {
        Some(t) => t,
        None => return Err("the port could not be opened".into()),
    };

    // five frames' worth of bytes, cut into messages that do not line up with frames
    let mut data = vec![0u8; FRAME * 5];
    for i in 0..data.len() {
        data[i] = (i * 7 + i / 251) as u8;
    }
    let (mut c1, c1raw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
    sleep(Duration::from_millis(300));
    rep.step(
        &format!("a connection alone does not switch the source (events '{}')", seen()),
        seen().is_empty(),
    );
    let cuts = [FRAME, FRAME, FRAME, 1000, FRAME * 2 - 1000];
    let mut at = 0;
    for n in cuts {
        tab_send(&mut c1, &data[at..at + n])?;
        at += n;
    }
    rep.step(
        "5 frames come out of 5 messages of uneven size",
        wait_for(|| count() == 5, 2000),
    );
    let mut same = count() == 5;
    {
        let g = lock(gate);
        let mut f = 0;
        while same && f < 5 && f < g.frames.len() {
            for i in 0..FRAME {
                if g.frames[f][i] != data[f * FRAME + i] {
                    same = false;
                    break;
                }
            }
            f += 1;
        }
    }
    rep.step("the bytes arrive unchanged and in order", same);
    rep.step(
        &format!(
            "the source is reported active once, with the first frame (events '{}')",
            seen()
        ),
        seen() == "+",
    );

    rep.step(
        "a web page origin is refused",
        tab_refused(port, Some("https://example.com"), "/tab"),
    );
    rep.step("a client with no origin is refused", tab_refused(port, None, "/tab"));
    rep.step(
        "another extension is refused",
        tab_refused(port, Some("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "/tab"),
    );
    rep.step(
        "another path is refused",
        tab_refused(port, Some(EXTENSION_ORIGIN), "/audio"),
    );
    tab_send(&mut c1, &data[..FRAME])?;
    rep.step(
        "the first tab is still being received after the refusals",
        wait_for(|| count() == 6, 2000),
    );

    // a second tab takes over; the first one is dropped
    let (mut c2, _c2raw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
    let mut level = vec![0u8; FRAME];
    for i in (0..FRAME).step_by(2) {
        level[i] = 0x00;
        level[i + 1] = if i % 4 == 0 { 0x40 } else { 0xC0 }; // +16384, -16384, ...
    }
    tab_send(&mut c2, &level)?;
    rep.step("a newer tab is received", wait_for(|| count() == 7, 2000));
    rep.step(
        &format!("level of a half-scale square wave is 0.5 (got {:.4})", tab.last_rms()),
        (tab.last_rms() - 0.5).abs() < 0.001,
    );
    let _ = c1.get_mut().set_read_timeout(Some(Duration::from_millis(250)));
    let mut bye: Option<u16> = None;
    let until = Instant::now() + Duration::from_millis(3000);
    while Instant::now() < until {
        match c1.read() {
            Ok(Message::Close(cf)) => {
                bye = cf.map(|f| u16::from(f.code));
                break;
            }
            Ok(_) => {}
            Err(WsError::Io(ref e)) if retryable(e) => {}
            Err(_) => break,
        }
    }
    rep.step(
        &format!(
            "the older tab is told that it was replaced (close code {})",
            bye.map(|c| c.to_string()).unwrap_or_else(|| "none".into())
        ),
        bye == Some(REPLACED),
    );
    // The answering close. A browser (and .NET's ClientWebSocket, which the C# test used)
    // sends it by itself; tungstenite only queues it, and the app waits for it.
    let _ = c1.flush();
    let _ = c1raw.set_read_timeout(Some(Duration::from_millis(100)));
    let dropped = wait_for(
        || {
            let sent = raw_binary(&c1raw, &data[..FRAME]).is_err();
            let mut b = [0u8; 64];
            let gone = match (&c1raw).read(&mut b) {
                Ok(0) => true,
                Ok(_) => false,
                Err(e) => !retryable(&e),
            };
            sent || gone
        },
        3000,
    );
    rep.step("and its socket is closed by the app", dropped);
    sleep(Duration::from_millis(200));
    rep.step(
        &format!("nothing more arrived from the older tab (frames {})", count()),
        count() == 7,
    );
    rep.step(
        &format!(
            "the newer tab was announced, and the source never reported gone (events '{}')",
            seen()
        ),
        seen() == "++",
    );
    drop(c1);
    drop(c1raw);

    // closing, and another tab within the hold time: the loopback must not cut in
    tab_close(&mut c2);
    drop(c2);
    sleep(Duration::from_millis(250));
    let (mut c3, _c3raw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
    for _ in 0..((HOLD + 400) / 100) {
        tab_send(&mut c3, &level)?;
        sleep(Duration::from_millis(100));
    }
    rep.step(
        &format!(
            "a tab that follows within the hold time keeps the source active (events '{}')",
            seen()
        ),
        seen() == "+++",
    );

    // a tab that stays connected but stops sending is as good as gone
    sleep(Duration::from_millis((HOLD / 2) as u64));
    rep.step(
        "still active halfway through the hold time",
        seen() == "+++",
    );
    rep.step(
        "reported gone when nothing has arrived for the hold time",
        wait_for(|| seen() == "+++-", (HOLD + 1000) as u64),
    );
    rep.step(
        &format!("no level is shown while nothing arrives ({:.2})", tab.last_rms()),
        tab.last_rms() == 0.0,
    );
    tab_send(&mut c3, &level)?;
    rep.step(
        "active again when the same tab resumes",
        wait_for(|| seen() == "+++-+", 2000),
    );
    tab_close(&mut c3);
    drop(c3);
    rep.step(
        "reported gone after the tab is let go",
        wait_for(|| seen() == "+++-+-", (HOLD + 1000) as u64),
    );

    // a short stretch of exact silence cut into a tone is counted, a clean tone and plain
    // silence are not
    let (mut c4, _c4raw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
    let mut tone = vec![0u8; FRAME];
    for i in 0..FRAME / 2 {
        let v = (8000.0 * (2.0 * PI * 440.0 * i as f64 / 16000.0 + 0.3).sin()) as i16;
        tone[2 * i] = v as u8;
        tone[2 * i + 1] = (v >> 8) as u8;
    }
    let mut cut = tone.clone();
    for b in cut[2 * 500..2 * 500 + 2 * 160].iter_mut() {
        *b = 0; // 10 ms
    }
    let before = count();
    let silence = vec![0u8; FRAME];
    tab_send(&mut c4, &tone)?;
    tab_send(&mut c4, &silence)?;
    tab_send(&mut c4, &silence)?;
    tab_send(&mut c4, &tone)?;
    tab_send(&mut c4, &cut)?;
    tab_send(&mut c4, &tone)?;
    wait_for(|| count() == before + 6, 2000);
    rep.step(
        &format!(
            "one 10 ms gap inside a tone is counted, 200 ms of silence between tones is not (gaps {})",
            tab.gaps()
        ),
        tab.gaps() == 1,
    );
    rep.step(
        &format!("active again with the next tab (events '{}')", seen()),
        seen() == "+++-+-+",
    );

    // a connection that is reset the moment it is made: the C# HttpListener went on
    // accepting, and so must this one
    for _ in 0..3 {
        if let Some(s) = half_open(port) {
            let _ = socket2::SockRef::from(&s).set_linger(Some(Duration::ZERO));
            drop(s);
        }
    }
    let had = count();
    tab_send(&mut c4, &tone)?;
    rep.step(
        "the listener survives a connection that is reset right after connecting",
        wait_for(|| count() == had + 1, 2000),
    );

    // a handshake dribbled a byte at a time: cut off at its deadline, not whenever the
    // peer loses interest
    let slow = half_open(port).ok_or("cannot connect")?;
    let mut head: Vec<u8> = b"GET /tab HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n".to_vec();
    head.extend_from_slice(b"Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n");
    let started = Instant::now();
    let mut dribbled = 0usize;
    let mut cut = None;
    for b in head.iter() {
        let mut w = &slow;
        if w.write_all(&[*b]).is_err() {
            break;
        }
        dribbled += 1;
        if gone(&slow, 150) {
            cut = Some(started.elapsed().as_millis());
            break;
        }
    }
    rep.step(
        &format!(
            "a dribbled handshake is cut off at the deadline ({} bytes, after {} ms)",
            dribbled,
            cut.map(|m| m.to_string()).unwrap_or_else(|| "never".into())
        ),
        cut.map_or(false, |m| m <= 2500),
    );
    drop(slow);

    // the tab that another one replaces is told so even while its own audio is still
    // arriving: the extension takes any other close for the app restarting
    let before = count();
    let (mut c6, c6raw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
    tab_send(&mut c6, &level)?;
    wait_for(|| count() > before, 2000);
    let (mut c7, _c7raw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
    // four frames right behind the replacement, as a tab in mid-sentence sends them
    for _ in 0..4 {
        let _ = raw_binary(&c6raw, &level);
    }
    let _ = c6.get_mut().set_read_timeout(Some(Duration::from_millis(250)));
    let mut code: Option<u16> = None;
    let mut abnormal: Option<String> = None;
    let until = Instant::now() + Duration::from_millis(3000);
    while Instant::now() < until {
        match c6.read() {
            Ok(Message::Close(cf)) => {
                code = cf.map(|f| u16::from(f.code));
                break;
            }
            Ok(_) => {}
            Err(WsError::Io(ref e)) if retryable(e) => {}
            Err(e) => {
                abnormal = Some(e.to_string());
                break;
            }
        }
    }
    let _ = c6.flush(); // the answering close, as the browser sends it
    rep.step(
        &format!(
            "a replaced tab that is still sending audio is told why (close code {}{})",
            code.map(|c| c.to_string()).unwrap_or_else(|| "none".into()),
            abnormal.as_deref().unwrap_or("")
        ),
        code == Some(REPLACED) && abnormal.is_none(),
    );
    drop(c6);
    drop(c6raw);
    tab_send(&mut c7, &level)?;
    rep.step(
        "the tab that took over is received",
        wait_for(|| count() >= before + 2, 2000),
    );

    // Stop() while one tab dribbles the bytes of a frame and another never reads: both
    // threads sit in a read that nothing else would end.
    let (c8, c8raw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
    let hdr = [0x82u8, 126, 0x0f, 0xa0, 0x12, 0x34, 0x56, 0x78]; // 4000 bytes, masked
    {
        let mut w = &c8raw;
        let _ = w.write_all(&hdr);
    }
    let feeder = {
        let s = c8raw.try_clone().map_err(|e| e.to_string())?;
        thread::spawn(move || {
            for _ in 0..200 {
                let mut w = &s;
                if w.write_all(&[0x00]).is_err() {
                    break;
                }
                sleep(Duration::from_millis(25));
            }
        })
    };
    let (c9, _c9raw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?; // never read from
    sleep(Duration::from_millis(200));
    let stopping = Instant::now();
    tab.stop();
    let stop_ms = stopping.elapsed().as_millis();
    rep.step(
        &format!("Stop returns while a tab dribbles a frame and another never reads ({} ms)", stop_ms),
        stop_ms < 1000,
    );
    let _ = feeder.join();
    drop(c8);
    drop(c8raw);
    drop(c9);
    rep.step(
        "after Stop nothing is listening",
        tab_refused(port, Some(EXTENSION_ORIGIN), "/tab"),
    );
    drop(c4);
    let (on_frame, on_active, diag) = hooks(gate, t0);
    let again = TabAudio::start(port, HOLD, on_frame, on_active, diag);
    rep.step("the port can be opened again at once", again.is_some());
    if let Some(tab) = again {
        let (mut c5, _c5raw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
        let had = count();
        tab_send(&mut c5, &data[..FRAME])?;
        rep.step("and frames flow again", wait_for(|| count() == had + 1, 2000));

        // A page that opens sockets and says nothing cannot have a thread each: the ones
        // over the cap are turned away at once, and the tab being received is untouched.
        let held: Vec<TcpStream> = (0..CAP + 4).filter_map(|_| half_open(port)).collect();
        let away = held.iter().filter(|s| gone(s, 20)).count();
        let had = count();
        tab_send(&mut c5, &data[..FRAME])?;
        let still = wait_for(|| count() == had + 1, 2000);
        rep.step(
            &format!(
                "{} of {} connections at once are turned away, the tab keeps being received",
                away,
                held.len()
            ),
            away >= held.len() - CAP && still,
        );
        drop(held);
        // the half-open ones have let their threads go by now (the handshake deadline)
        sleep(Duration::from_millis(700));
        let fresh = tab_client(port, Some(EXTENSION_ORIGIN), "/tab");
        let mut ok = fresh.is_ok();
        if let Ok((mut c, _raw)) = fresh {
            let had = count();
            tab_send(&mut c, &data[..FRAME])?;
            ok = wait_for(|| count() == had + 1, 2000);
            drop(c);
        }
        rep.step("and a proper client gets in afterwards", ok);
        drop(c5);
        tab.stop();
    }

    // with panic = "unwind": a frame sink that panics costs its own frame and nothing else
    let seen_frames = Arc::new(AtomicU64::new(0));
    let n = seen_frames.clone();
    let d = gate.clone();
    let third = TabAudio::start(
        port,
        HOLD,
        Box::new(move |_: &[u8]| {
            if n.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("the frame sink gave up");
            }
        }),
        Box::new(|_| {}),
        Box::new(move |s: &str| {
            let line = format!("+{:7.3}  [tab3] {}", t0.elapsed().as_secs_f64(), s);
            lock(&d).diag.push(line)
        }),
    );
    rep.step("the port can be opened a third time", third.is_some());
    if let Some(tab) = third {
        let (mut c, _raw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
        for _ in 0..3 {
            tab_send(&mut c, &data[..FRAME])?;
            sleep(Duration::from_millis(50));
        }
        let delivered = wait_for(|| seen_frames.load(Ordering::SeqCst) == 3, 2000);
        let logged = lock(gate).diag.iter().any(|l| l.contains("frame sink threw"));
        rep.step(
            &format!(
                "a panicking frame sink is logged ({}) and the tab goes on ({} frames)",
                if logged { "yes" } else { "no" },
                seen_frames.load(Ordering::SeqCst)
            ),
            delivered && logged && tab.frames() == 3,
        );
        drop(c);
        tab.stop();
    }

    // ---- what a peer that means no good, or one that gets stuck, can do to the listener:
    // the room kept for a proper client, a frame from a tab that was replaced, a write that
    // runs into its timeout, and a serving thread that does not come out of a frame.
    let hold = Arc::new(AtomicBool::new(false));
    let in_sink = Arc::new(AtomicBool::new(false));
    let fourth = {
        let (g, d) = (gate.clone(), gate.clone());
        let (h, b) = (hold.clone(), in_sink.clone());
        TabAudio::start(
            port,
            HOLD,
            Box::new(move |f: &[u8]| {
                lock(&g).frames.push(f.to_vec());
                // held up on purpose, to stand in for a thread that does not come back
                if h.load(Ordering::SeqCst) {
                    b.store(true, Ordering::SeqCst);
                    let until = Instant::now() + Duration::from_secs(5);
                    while h.load(Ordering::SeqCst) && Instant::now() < until {
                        sleep(Duration::from_millis(5));
                    }
                    b.store(false, Ordering::SeqCst);
                }
            }),
            Box::new(|_| {}),
            Box::new(move |s: &str| {
                let line = format!("+{:7.3}  [tab4] {}", t0.elapsed().as_secs_f64(), s);
                lock(&d).diag.push(line)
            }),
        )
    };
    rep.step("the port can be opened a fourth time", fourth.is_some());
    if let Some(tab) = fourth {
        // A page opening sockets in a loop: each of them is only refused once its request has
        // been read, so they must not be able to hold the room the extension needs.
        let flood: Vec<TcpStream> = (0..CAP + 8).filter_map(|_| half_open(port)).collect();
        sleep(Duration::from_millis(100));
        let proper = tab_client(port, Some(EXTENSION_ORIGIN), "/tab");
        rep.step(
            &format!("a proper client gets in while {} sockets sit in the handshake", flood.len()),
            proper.is_ok(),
        );
        drop(flood);
        let (mut ca, caraw) = proper?;

        // Two frames in one message: the sink is held up inside the first, another tab takes
        // over meanwhile, and the second is read when it no longer belongs to any tab. Its
        // silence must not be counted as a gap in the new tab's sound.
        let mut gap_frame = tone.clone();
        for b in gap_frame[2 * 500..2 * 500 + 2 * 160].iter_mut() {
            *b = 0; // 10 ms of exact silence inside the tone
        }
        let mut two = Vec::with_capacity(FRAME * 2);
        two.extend_from_slice(&gap_frame);
        two.extend_from_slice(&gap_frame);
        let before = count();
        hold.store(true, Ordering::SeqCst);
        tab_send(&mut ca, &two)?;
        let inside = wait_for(|| in_sink.load(Ordering::SeqCst), 2000);
        let (mut cb, _cbraw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
        sleep(Duration::from_millis(100)); // the newer tab has been adopted by now
        hold.store(false, Ordering::SeqCst);
        sleep(Duration::from_millis(300));
        rep.step(
            &format!(
                "the frame a replaced tab had already sent is dropped whole (frames {}, gaps {})",
                count() - before,
                tab.gaps()
            ),
            inside && count() - before == 1 && tab.gaps() == 1 && tab.frames() == 1,
        );
        tab_send(&mut cb, &tone)?;
        rep.step("and the tab that took over is received", wait_for(|| tab.frames() == 2, 2000));
        drop(ca);
        drop(caraw);

        // A write of the app's own that runs into its timeout: tungstenite answers every ping
        // with a pong inside its read, and a client that never reads them jams those writes.
        // The socket is then in the state Windows calls undefined, so the connection has to
        // end instead of being read from again.
        let (mut cw, cwraw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
        let _ = socket2::SockRef::from(&cwraw).set_recv_buffer_size(2048);
        let _ = cw.get_mut().set_write_timeout(Some(Duration::from_millis(300)));
        let mut pinged = 0u32;
        for _ in 0..4000 {
            if cw.send(Message::Ping(vec![0xA5u8; 125].into())).is_err() {
                break;
            }
            pinged += 1;
        }
        sleep(Duration::from_millis(2600)); // longer than the app's write timeout
        let ended = read_to_end_of(&cwraw, 3000);
        let said = lock(gate).diag.iter().any(|l| l.contains("a write to the tab timed out"));
        rep.step(
            &format!("a write that timed out ends the connection ({} pings, logged {})", pinged, said),
            ended && said,
        );
        drop(cw);
        drop(cwraw);

        // Stop() with a thread that is not coming back: it may wait for it, but not for long.
        let (mut cs, _csraw) = tab_client(port, Some(EXTENSION_ORIGIN), "/tab")?;
        hold.store(true, Ordering::SeqCst);
        tab_send(&mut cs, &tone)?;
        let stuck = wait_for(|| in_sink.load(Ordering::SeqCst), 2000);
        let stopping = Instant::now();
        tab.stop();
        let stop_ms = stopping.elapsed().as_millis();
        let left = lock(gate).diag.iter().any(|l| l.contains("did not finish in time"));
        rep.step(
            &format!("Stop returns while a serving thread hangs in a frame ({} ms, logged {})", stop_ms, left),
            stuck && left && stop_ms < 2500,
        );
        hold.store(false, Ordering::SeqCst);
        sleep(Duration::from_millis(100)); // let that thread finish before the report is written
        drop(cs);
        drop(cb);
    }
    Ok(())
}
