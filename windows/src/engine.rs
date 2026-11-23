//! The link to the subtitle engine: a WebSocket to the Python server that runs in
//! WSL, which this module launches through `wsl.exe` when nothing is listening, and
//! reconnects to for ever. It never kills a server it did not start; the server exits
//! on its own after `--exit-when-idle` seconds.
//!
//! A port of `windows/Engine.cs`, behaviour for behaviour. No async runtime: the link
//! owns one thread for the connect loop (which also reads the socket) and, while a
//! connection is up, one for sending; every launch adds two short-lived threads that
//! drain the engine's stdout and stderr.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::{Message, Role, WebSocket};

const SEQ: Ordering = Ordering::SeqCst;

/// ~1 s of 100 ms frames.
const QUEUE_CAP: usize = 10;

/// A send that cannot get through must fail instead of holding the sending thread: the
/// link then shuts the socket down and connects again, as it does for any send failure.
/// Long enough that only an engine that has stopped reading altogether runs into it (the
/// socket's buffers hold seconds of audio), and short enough to be over before `stop()`
/// gives up waiting: a blocked send only ends when this runs out.
const SEND_MS: u64 = 2000;
/// How long the reading thread may sit in one read before it looks at the cancel flag and
/// at whether the socket has been shut down under it. Windows does not wake a blocked
/// `recv` when another handle on the same socket is shut down, so a read that never ends
/// is the only thing left that could keep a dead session alive and the state at Live.
const READ_POLL_MS: u64 = 1000;
/// How long `stop()` waits for the link thread. The C# `Stop` waited this long too; with
/// an interruptible connect and interruptible sends it never runs out, but the UI thread
/// is the one calling and must get on with it either way.
const STOP_JOIN_MS: u64 = 2500;
/// The goodbye on a clean quit: a Close frame so that the engine hears the app go instead of
/// only seeing the transport drop. How long its write may take, and how long the socket is
/// left alone for it - quitting may not wait on an engine that is not reading any more.
const BYE_WRITE_MS: u64 = 300;
const BYE_WAIT_MS: u64 = 400;

const TXT_STARTING: &str = "翻訳エンジンを起動中…";
const TXT_CONNECTING: &str = "接続中…";
const TXT_PREPARING: &str = "翻訳エンジンを準備中…（初回はダウンロードに数分かかります）";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conn {
    Idle,
    StartingEngine,
    /// launched and alive, but not listening after a long while: a first start, downloading
    PreparingEngine,
    Connecting,
    Live,
    Failed,
}

/// One message from the server. The fields of the C# `SubMsg`; `kind` is its `Type`.
#[derive(Clone, Debug)]
pub struct SubMsg {
    pub kind: String,
    pub id: i32,
    pub en: String,
    pub ja: String,
    pub lag_ms: i32,
    pub seg: i32,
    pub title: String,
    pub summary: String,
    pub error: String,
    pub asr: String,
    pub mt: String,
    pub device: String,
}

impl Default for SubMsg {
    /// The field initializers of the C# class: "" and -1, not 0.
    fn default() -> Self {
        SubMsg {
            kind: String::new(),
            id: -1,
            en: String::new(),
            ja: String::new(),
            lag_ms: -1,
            seg: -1,
            title: String::new(),
            summary: String::new(),
            error: String::new(),
            asr: String::new(),
            mt: String::new(),
            device: String::new(),
        }
    }
}

pub enum Event {
    /// state + Japanese status text ("" when live)
    State(Conn, String),
    Message(SubMsg),
    /// every text frame, verbatim (used by the self-tests); only when `want_raw`
    Raw(String),
}

pub struct Config {
    pub distro: String,
    pub dir: String,
    pub port: u16,
    pub want_raw: bool,
}

/// An `AutoResetEvent`: something to send, or one of the flags below was set.
struct Wake {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Wake {
    fn set(&self) {
        let mut g = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        *g = true;
        self.cv.notify_all();
    }

    fn wait(&self, d: Duration) {
        let mut g = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        if *g {
            *g = false;
            return;
        }
        let r = self.cv.wait_timeout(g, d);
        if let Ok((mut g, _)) = r {
            *g = false;
        }
    }
}

struct Shared {
    cfg: Config,
    on_event: Box<dyn Fn(Event) + Send + Sync>,
    diag: Box<dyn Fn(&str) + Send + Sync>,

    q: Mutex<VecDeque<Vec<u8>>>,
    wake: Wake,
    live: AtomicBool,
    reset_pending: AtomicBool,
    reset_new_stream: AtomicBool,
    topics: AtomicBool,
    config_pending: AtomicBool,
    /// a ping arrived on the reading thread; the sending thread answers it
    pong: Mutex<Option<Vec<u8>>>,

    cancel: AtomicBool,
    /// held while `cancel` is set, so a nap cannot miss the wake-up
    nap_m: Mutex<()>,
    nap_cv: Condvar,

    /// the socket of the current connection, so `stop` can unblock the reading thread
    sock: Mutex<Option<TcpStream>>,
    /// the last thing the current launch printed: why it ended, if it did
    engine_last: Mutex<Option<String>>,
    /// lines logged for the current launch
    printed: AtomicU32,
    /// The current launch's pipe, kept open for as long as that engine runs, as the C#
    /// `Process` kept its pipes. Only the current one: a handle per launch would pile up.
    stdin: Mutex<Option<ChildStdin>>,
    /// the link thread has finished, so that `stop()` can wait with a bound
    ended: AtomicBool,
    /// the sending thread has said goodbye to the engine, or given up on it: until then the
    /// socket of a cancelled session is left alone (see `send_pump`)
    said_bye: AtomicBool,

    launch_count: AtomicU32,
    pid: AtomicU32,
}

impl Shared {
    fn d(&self, s: &str) {
        (self.diag)(s);
    }

    fn drain(&self) {
        self.q.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// A cancellable `Task.Delay`.
    fn nap(&self, ms: u64) {
        let g = self.nap_m.lock().unwrap_or_else(|e| e.into_inner());
        if self.cancel.load(SEQ) {
            return;
        }
        let _ = self.nap_cv.wait_timeout(g, Duration::from_millis(ms));
    }

    fn cancel_now(&self) {
        let g = self.nap_m.lock().unwrap_or_else(|e| e.into_inner());
        self.cancel.store(true, SEQ);
        drop(g);
        self.nap_cv.notify_all();
        self.wake.set();
    }
}

pub struct EngineLink {
    sh: Arc<Shared>,
    runner: Option<JoinHandle<()>>,
}

impl EngineLink {
    /// Starts the threads. `on_event` and `diag` are called on the link's own threads.
    pub fn start(
        cfg: Config,
        on_event: Box<dyn Fn(Event) + Send + Sync>,
        diag: Box<dyn Fn(&str) + Send + Sync>,
    ) -> EngineLink {
        let sh = Arc::new(Shared {
            cfg,
            on_event,
            diag,
            q: Mutex::new(VecDeque::with_capacity(QUEUE_CAP + 1)),
            wake: Wake { flag: Mutex::new(false), cv: Condvar::new() },
            live: AtomicBool::new(false),
            reset_pending: AtomicBool::new(false),
            reset_new_stream: AtomicBool::new(false),
            topics: AtomicBool::new(true),
            config_pending: AtomicBool::new(false),
            pong: Mutex::new(None),
            cancel: AtomicBool::new(false),
            nap_m: Mutex::new(()),
            nap_cv: Condvar::new(),
            sock: Mutex::new(None),
            engine_last: Mutex::new(None),
            printed: AtomicU32::new(0),
            stdin: Mutex::new(None),
            ended: AtomicBool::new(false),
            said_bye: AtomicBool::new(false),
            launch_count: AtomicU32::new(0),
            pid: AtomicU32::new(0),
        });
        let s2 = sh.clone();
        let runner = thread::Builder::new()
            .name("engine-link".into())
            .spawn(move || {
                let _end = EndGuard(s2.clone());
                // What the connect loop keeps lives here and not inside `run`: a caught panic
                // must not make it forget the engine it launched and when, or it would launch
                // a second one beside the first and let go of the first one's handles.
                let mut st = Link::new(s2.clone());
                while !s2.cancel.load(SEQ) {
                    // A panic in one of the handlers, or anywhere else in a session, is
                    // logged and the connect loop starts over, as an exception did not
                    // take the link away in the C#.
                    if let Err(p) = catch_unwind(AssertUnwindSafe(|| run(&mut st))) {
                        s2.live.store(false, SEQ);
                        // whatever the panic left: the session's socket, and with it the
                        // sending thread, has to go before the loop starts over
                        if let Some(s) = s2.sock.lock().unwrap_or_else(|e| e.into_inner()).take() {
                            let _ = s.shutdown(Shutdown::Both);
                        }
                        s2.d(&format!("the link thread panicked: {}", panic_msg(&p)));
                    }
                }
            })
            .ok();
        if runner.is_none() {
            sh.ended.store(true, SEQ);
            sh.d("cannot start the link thread");
        }
        EngineLink { sh, runner }
    }

    /// One 3200-byte frame; never blocks the caller.
    pub fn send_frame(&self, pcm: &[u8]) {
        if !self.sh.live.load(SEQ) {
            return;
        }
        {
            let mut q = self.sh.q.lock().unwrap_or_else(|e| e.into_inner());
            while q.len() >= QUEUE_CAP {
                q.pop_front();
            }
            q.push_back(pcm.to_vec());
        }
        self.sh.wake.set();
    }

    /// Ask the server to start a fresh segment (used when resuming from pause).
    /// `new_stream`: the user switched to another stream, so the engine also drops what
    /// it had collected for the outline.
    pub fn send_reset(&self, new_stream: bool) {
        if !self.sh.live.load(SEQ) {
            return;
        }
        if new_stream {
            self.sh.reset_new_stream.store(true, SEQ);
        }
        self.sh.reset_pending.store(true, SEQ);
        self.sh.wake.set();
    }

    /// Turn the 目次 outline on or off on the server (re-sent on every reconnect).
    pub fn set_topics(&self, on: bool) {
        self.sh.topics.store(on, SEQ);
        self.sh.config_pending.store(true, SEQ);
        self.sh.wake.set();
    }

    pub fn is_live(&self) -> bool {
        self.sh.live.load(SEQ)
    }

    pub fn launched_engine(&self) -> bool {
        self.sh.launch_count.load(SEQ) > 0
    }

    /// Of the `wsl.exe` it started, 0 if none.
    pub fn engine_pid(&self) -> u32 {
        self.sh.pid.load(SEQ)
    }

    /// Closes the socket and joins the threads; leaves the engine running (it exits by
    /// itself when idle).
    pub fn stop(mut self) {
        let in_session = self.sh.live.load(SEQ);
        self.sh.cancel_now();
        // The sending thread has a goodbye to send first (see `send_pump`); the socket is cut
        // once it has, and after this long whatever it is doing.
        if in_session {
            let deadline = Instant::now() + Duration::from_millis(BYE_WAIT_MS);
            while !self.sh.said_bye.load(SEQ) && !self.sh.ended.load(SEQ) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
        }
        if let Some(s) = self.sh.sock.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(h) = self.runner.take() {
            // The UI thread calls this: wait as the C# `Stop` did, and no longer, however
            // the link thread is occupied.
            let deadline = Instant::now() + Duration::from_millis(STOP_JOIN_MS);
            while !self.sh.ended.load(SEQ) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if self.sh.ended.load(SEQ) {
                let _ = h.join();
            } else {
                self.sh.d("the link thread did not finish in time; left to itself");
            }
        }
    }
}

impl Drop for EngineLink {
    fn drop(&mut self) {
        // dropped without stop(): at least let the threads go
        self.sh.cancel_now();
        if let Some(s) = self.sh.sock.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

/// Says that the link thread has finished, a panic included.
struct EndGuard(Arc<Shared>);

impl Drop for EndGuard {
    fn drop(&mut self) {
        self.0.ended.store(true, SEQ);
    }
}

fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "panicked".to_string()
    }
}

fn config_json(topics: bool) -> String {
    let mut s = String::from("{\"type\":\"config\",\"topics\":");
    s.push_str(if topics { "true" } else { "false" });
    s.push('}');
    s
}

// ---------------------------------------------------------------- the connect loop

/// What only the loop thread touches.
struct Link {
    sh: Arc<Shared>,
    child: Option<Child>,
    launch_count: u32,
    last_launch: Option<Instant>,
    /// why the engine could not even be started, for the status line
    launch_error: Option<String>,
    /// the engine of the current launch has been connected to
    reached: bool,
    reported: Conn,
    reported_text: Option<String>,
    /// how long the next try waits after a failure, and until when an engine that was just
    /// launched still counts as starting: kept with the rest across a caught panic
    backoff: u64,
    starting_until: Option<Instant>,
}

impl Link {
    fn new(sh: Arc<Shared>) -> Link {
        Link {
            sh,
            child: None,
            launch_count: 0,
            last_launch: None,
            launch_error: None,
            reached: false,
            reported: Conn::Idle,
            reported_text: None,
            backoff: 1000,
            starting_until: None,
        }
    }

    /// Called on every turn of the connect loop, several times a second while the engine
    /// starts: only a change is passed on.
    fn report(&mut self, st: Conn, text: &str) {
        if st == self.reported && self.reported_text.as_deref() == Some(text) {
            return;
        }
        self.reported = st;
        self.reported_text = Some(text.to_string());
        if text.is_empty() {
            self.sh.d(&format!("state: {:?}", st));
        } else {
            self.sh.d(&format!("state: {:?} - {}", st, text));
        }
        (self.sh.on_event)(Event::State(st, text.to_string()));
    }

    fn engine_alive(&mut self) -> bool {
        match self.child.as_mut() {
            None => false,
            Some(c) => matches!(c.try_wait(), Ok(None)),
        }
    }

    /// A start takes seconds once everything is on the disk. The very first one downloads
    /// the Python packages and the models, which takes minutes: the engine is alive and
    /// busy then, and that must not be shown as a failure.
    fn preparing(&mut self) -> bool {
        self.launch_count > 0
            && !self.reached
            && self.last_launch.map_or(false, |t| t.elapsed() > Duration::from_secs(30))
            && self.engine_alive()
    }

    fn launch_engine(&mut self) -> bool {
        if self.sh.cfg.dir.is_empty() {
            self.sh.d("cannot launch the engine: LiveSubs.ini has no dir=");
            self.launch_error =
                Some("翻訳エンジンを起動できません（LiveSubs.ini に dir= がありません）".into());
            self.last_launch = Some(Instant::now());
            return false;
        }
        let exe = wsl_exe();
        // wsl.exe parses its own command line and does NOT strip quotes from -d/--cd,
        // so these must be passed bare (and therefore cannot contain spaces).
        if self.sh.cfg.distro.contains(' ') || self.sh.cfg.dir.contains(' ') {
            self.sh.d("warning: wsl.exe cannot take a distro/dir containing spaces");
        }
        let port = self.sh.cfg.port.to_string();
        let args: [&str; 10] = [
            "-d",
            &self.sh.cfg.distro,
            "--cd",
            &self.sh.cfg.dir,
            "--exec",
            "./run.sh",
            "--port",
            &port,
            "--exit-when-idle",
            "20",
        ];
        let mut cmd = Command::new(&exe);
        cmd.args(args);
        // wsl.exe needs usable standard handles; a winexe has none to inherit, so it
        // dies instantly unless we hand it pipes. They are drained, never left to fill.
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.sh.d(&format!("engine launch failed: {}", e));
                self.launch_error = Some(format!("翻訳エンジンを起動できません: {}", e));
                self.last_launch = Some(Instant::now());
                return false;
            }
        };
        let pid = child.id();
        let out = child.stdout.take();
        let err = child.stderr.take();
        // the previous engine has exited (that is why we are here); let go of its handles
        *self.sh.stdin.lock().unwrap_or_else(|e| e.into_inner()) = child.stdin.take();
        self.child = Some(child);
        self.launch_count += 1;
        self.sh.launch_count.store(self.launch_count, SEQ);
        self.sh.pid.store(pid, SEQ);
        self.last_launch = Some(Instant::now());
        self.launch_error = None;
        self.sh.printed.store(0, SEQ);
        *self.sh.engine_last.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.reached = false;
        let tag = self.launch_count;
        if let Some(o) = out {
            spawn_reader(self.sh.clone(), tag, "out", o);
        }
        if let Some(o) = err {
            spawn_reader(self.sh.clone(), tag, "err", o);
        }
        self.sh.d(&format!("launched engine: {} {} (pid {})", exe, args.join(" "), pid));
        true
    }
}

fn wsl_exe() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let wsl = format!("{}\\system32\\wsl.exe", root);
    if std::path::Path::new(&wsl).exists() {
        wsl
    } else {
        "wsl.exe".into()
    }
}

/// Logs what a launch prints, and remembers its last line.
fn spawn_reader<R: Read + Send + 'static>(sh: Arc<Shared>, tag: u32, stream: &'static str, r: R) {
    let _ = thread::Builder::new().name(format!("engine-{}", stream)).spawn(move || {
        let mut r = r;
        let mut buf = [0u8; 4096];
        let mut line: Vec<u8> = Vec::new();
        loop {
            let n = match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for &b in &buf[..n] {
                if b == b'\n' {
                    engine_said(&sh, tag, stream, &line);
                    line.clear();
                } else if line.len() < 1 << 16 {
                    line.push(b);
                }
            }
        }
        if !line.is_empty() {
            engine_said(&sh, tag, stream, &line);
        }
    });
}

fn engine_said(sh: &Shared, tag: u32, stream: &str, raw: &[u8]) {
    // wsl.exe prints its own errors as UTF-16LE; dropping the NULs makes them legible
    let s = String::from_utf8_lossy(raw);
    let line: String = s.chars().filter(|c| *c != '\0').collect();
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    *sh.engine_last.lock().unwrap_or_else(|e| e.into_inner()) = Some(line.to_string());
    if sh.printed.load(SEQ) >= 60 {
        return;
    }
    sh.printed.fetch_add(1, SEQ);
    sh.d(&format!("engine#{} {}: {}", tag, stream, line));
}

/// The first 90 UTF-16 units of `s`, as the C# `Substring(0, 90)` takes them.
fn trunc90(s: &str) -> String {
    let mut n = 0usize;
    for (i, c) in s.char_indices() {
        let w = c.len_utf16();
        if n + w > 90 {
            return format!("{}…", &s[..i]);
        }
        n += w;
    }
    s.to_string()
}

/// The connect loop. It is called again after a caught panic, with the same `st`: everything
/// it keeps between turns is in there, launch bookkeeping and all.
fn run(st: &mut Link) {
    let sh = st.sh.clone();

    while !sh.cancel.load(SEQ) {
        let starting = st.starting_until.map_or(false, |t| Instant::now() < t);
        if st.preparing() {
            st.report(Conn::PreparingEngine, TXT_PREPARING);
        } else if starting || st.reported != Conn::Failed {
            // a failure stays on show while it is retried
            st.report(
                if starting { Conn::StartingEngine } else { Conn::Connecting },
                if starting { TXT_STARTING } else { TXT_CONNECTING },
            );
        }

        // Connecting to a port nobody listens on takes Windows two seconds to give up.
        // A short probe says the same at once: a missing engine is started without that
        // wait, and one that is starting is found as soon as it listens.
        let listening = port_open(sh.cfg.port, 300);
        if sh.cancel.load(SEQ) {
            break;
        }

        let mut conn = None;
        if listening {
            match ws_connect(&sh, sh.cfg.port, Duration::from_millis(8000)) {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    if sh.cancel.load(SEQ) {
                        break;
                    }
                    sh.d(&format!("connect to port {} failed: {}", sh.cfg.port, e));
                }
            }
        }

        let (mut reader, mut writer, sock) = match conn {
            Some(c) => c,
            None => {
                let alive = st.engine_alive();
                // An engine we launched that has already exited is not "starting" any
                // more: say so now instead of showing 起動中 until the window runs out.
                if st.launch_count > 0 && !alive {
                    st.starting_until = None;
                }
                // back the relaunch interval off so a broken environment cannot be spammed
                let gate =
                    std::cmp::min(30u64 << std::cmp::min(st.launch_count.saturating_sub(1), 8), 300);
                let need_launch = !alive
                    && st.last_launch.map_or(true, |t| t.elapsed() > Duration::from_secs(gate));
                if need_launch && st.launch_engine() {
                    st.starting_until = Some(Instant::now() + Duration::from_secs(180));
                    st.backoff = 1000;
                }
                if st.preparing() {
                    st.report(Conn::PreparingEngine, TXT_PREPARING);
                    sh.nap(1000);
                } else if st.starting_until.map_or(false, |t| Instant::now() < t) {
                    st.report(Conn::StartingEngine, TXT_STARTING);
                    sh.nap(100); // the probe above is the wait
                } else {
                    let last = sh.engine_last.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if let Some(err) = st.launch_error.clone() {
                        st.report(Conn::Failed, &err);
                    } else if st.launch_count > 0 && last.is_some() {
                        let t = format!(
                            "翻訳エンジンが終了しました: {}（再試行中。詳細は log.txt）",
                            trunc90(last.as_deref().unwrap_or(""))
                        );
                        st.report(Conn::Failed, &t);
                    } else if st.launch_count > 0 {
                        st.report(
                            Conn::Failed,
                            "翻訳エンジンが終了しました（再試行中）。詳細は log.txt にあります",
                        );
                    } else {
                        st.report(Conn::Connecting, TXT_CONNECTING);
                    }
                    sh.nap(st.backoff);
                    st.backoff = std::cmp::min(st.backoff * 2, 10000);
                }
                continue;
            }
        };

        st.starting_until = None;
        st.backoff = 1000;
        st.reached = true;
        sh.d(&format!("connected to ws://127.0.0.1:{}/ws", sh.cfg.port));
        sh.drain();
        // ws_connect has put the socket where stop() looks for it
        sh.live.store(true, SEQ);
        st.report(Conn::Live, "");

        let link = Arc::new(AtomicBool::new(false));
        // Both directions get a thread of their own that blocks while there is nothing
        // to do: the loop thread reads, a second thread sends.
        let mut pump: Option<JoinHandle<WebSocket<WrOnly>>> = None;
        let mut init = writer.send(Message::text("{\"type\":\"reset\"}"));
        if init.is_ok() {
            sh.config_pending.store(false, SEQ);
            init = writer.send(Message::text(config_json(sh.topics.load(SEQ))));
        }
        match init {
            Err(e) => sh.d(&format!("socket error: {}", e)),
            Ok(()) => {
                let s2 = sh.clone();
                let l2 = link.clone();
                pump = thread::Builder::new()
                    .name("engine-send".into())
                    .spawn(move || {
                        let mut writer = writer;
                        // A panic here must not cost the writer, which the loop thread
                        // waits for, nor leave the reader blocked on a live socket.
                        if let Err(p) =
                            catch_unwind(AssertUnwindSafe(|| send_pump(&s2, &mut writer, &l2)))
                        {
                            s2.d(&format!("the sending thread panicked: {}", panic_msg(&p)));
                            shutdown_socket(&s2);
                            l2.store(true, SEQ);
                        }
                        writer
                    })
                    .ok();
                if let Some(ended) = receive_loop(&sh, &mut reader, &link) {
                    sh.d(&ended);
                }
            }
        }

        sh.live.store(false, SEQ);
        link.store(true, SEQ);
        sh.wake.set();
        // On a clean quit the sending thread still has the goodbye to send (see `send_pump`):
        // the socket may not be cut from under it before that, nor waited for any longer.
        if sh.cancel.load(SEQ) {
            let deadline = Instant::now() + Duration::from_millis(BYE_WAIT_MS);
            while !sh.said_bye.load(SEQ) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
        }
        // The sending thread may be inside a write to an engine that has stopped reading.
        // Cutting the socket ends that write now instead of when it times out, and the
        // join below would otherwise hold the loop (and `stop()`) up for that long.
        let _ = sock.shutdown(Shutdown::Both);
        drop(pump.and_then(|h| h.join().ok()));
        sh.drain();
        sh.d(&format!("disconnected from port {}", sh.cfg.port));
        *sh.sock.lock().unwrap_or_else(|e| e.into_inner()) = None;
        drop(sock);
        if sh.cancel.load(SEQ) {
            break;
        }
        st.report(Conn::Connecting, TXT_CONNECTING);
        sh.nap(1000);
    }
    sh.live.store(false, SEQ);
    st.report(Conn::Idle, "");
}

/// Whether something accepts connections on the port, decided within `ms`.
fn port_open(port: u16, ms: u64) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&addr, Duration::from_millis(ms)) {
        Ok(s) => {
            let _ = s.shutdown(Shutdown::Both);
            true
        }
        Err(_) => false,
    }
}

/// A read or write that ended in nothing rather than in a broken connection.
fn retryable(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// Cuts the current connection, whichever thread finds that it has to go.
fn shutdown_socket(sh: &Shared) {
    if let Some(s) = sh.sock.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = s.shutdown(Shutdown::Both);
    }
}

/// Runs on its own thread until the link is cancelled or the socket fails.
fn send_pump(sh: &Shared, w: &mut WebSocket<WrOnly>, link: &AtomicBool) {
    let mut last_ping = Instant::now();
    let r: Result<(), tungstenite::Error> = (|| {
        while !link.load(SEQ) && !sh.cancel.load(SEQ) {
            sh.wake.wait(Duration::from_millis(500));
            if link.load(SEQ) || sh.cancel.load(SEQ) {
                break;
            }
            if sh.reset_pending.swap(false, SEQ) {
                let fresh = sh.reset_new_stream.swap(false, SEQ);
                let json = if fresh {
                    "{\"type\":\"reset\",\"new_stream\":true}"
                } else {
                    "{\"type\":\"reset\"}"
                };
                w.send(Message::text(json))?;
            }
            if sh.config_pending.swap(false, SEQ) {
                w.send(Message::text(config_json(sh.topics.load(SEQ))))?;
            }
            let pong = sh.pong.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(p) = pong {
                w.send(Message::Pong(p.into()))?;
            }
            loop {
                let f = sh.q.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                match f {
                    Some(f) => w.send(Message::binary(f))?,
                    None => break,
                }
            }
            // the KeepAliveInterval of the C# client
            if last_ping.elapsed() >= Duration::from_secs(20) {
                last_ping = Instant::now();
                w.send(Message::Ping(Vec::new().into()))?;
            }
        }
        Ok(())
    })();
    // The link is being stopped and the socket still carries: tell the engine that the app is
    // going, as the C# client's close did. Its own short write timeout bounds it, and the loop
    // thread and `stop()` wait no longer than BYE_WAIT_MS for it.
    if sh.cancel.load(SEQ) && r.is_ok() {
        let _ = w.get_ref().0.set_write_timeout(Some(Duration::from_millis(BYE_WRITE_MS)));
        let _ = w.close(Some(CloseFrame { code: CloseCode::Normal, reason: "bye".into() }));
        let _ = w.flush();
    }
    sh.said_bye.store(true, SEQ);
    if let Err(e) = r {
        if !link.load(SEQ) && !sh.cancel.load(SEQ) {
            // a write that ran into its timeout counts as a failure like any other: the
            // engine is not taking what it is sent any more
            sh.d(&format!("send failed: {}", e));
        }
        shutdown_socket(sh);
        // and the reader ends this session with it: a socket shut down from this side does
        // not wake the reader's wait for something to read
        link.store(true, SEQ);
    }
}

/// Runs on the loop thread; returns what to log when the server ends the connection.
fn receive_loop(sh: &Shared, r: &mut WebSocket<RdOnly>, link: &AtomicBool) -> Option<String> {
    loop {
        if link.load(SEQ) || sh.cancel.load(SEQ) {
            return None;
        }
        match r.read() {
            Ok(Message::Text(t)) => {
                if sh.cfg.want_raw {
                    (sh.on_event)(Event::Raw(t.as_str().to_string()));
                }
                if let Some(m) = parse(t.as_str()) {
                    (sh.on_event)(Event::Message(m));
                }
            }
            Ok(Message::Close(c)) => {
                let (code, reason) = match c {
                    Some(f) => (f.code.to_string(), f.reason.as_str().to_string()),
                    None => (String::new(), String::new()),
                };
                return Some(format!("server closed the socket: {} {}", code, reason));
            }
            Ok(Message::Ping(p)) => {
                // tungstenite queues its own pong, but only the sending thread may write
                *sh.pong.lock().unwrap_or_else(|e| e.into_inner()) = Some(p.to_vec());
                sh.wake.set();
            }
            Ok(_) => {}
            // nothing from the engine for a moment: what has been read of a message is
            // kept, and the flags above are looked at again
            Err(tungstenite::Error::Io(ref e)) if retryable(e) => {}
            Err(e) => {
                if !link.load(SEQ) && !sh.cancel.load(SEQ) {
                    sh.d(&format!("socket error: {}", e));
                }
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------- the socket halves

/// The reading half. Everything tungstenite would write back by itself (a pong, the
/// answer to a close) is dropped here and sent by the sending thread instead: two
/// threads writing to one socket would interleave their frames.
pub struct RdOnly(TcpStream);

impl Read for RdOnly {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        // up for air now and then, to see whether the link is being stopped (see crate::net)
        if !crate::net::readable(&self.0, Duration::from_millis(READ_POLL_MS))? {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "nothing yet"));
        }
        self.0.read(b)
    }
}

impl Write for RdOnly {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The writing half; its reads are never used.
pub struct WrOnly(TcpStream);

impl Read for WrOnly {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::WouldBlock, "write half"))
    }
}

impl Write for WrOnly {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Connects and performs the upgrade by hand, so that the two directions end up on
/// separate handles (tungstenite's own `client()` keeps both in one object, which two
/// threads cannot use at once).
///
/// `limit` is for the whole of it, connect and handshake together: a server that dribbles
/// its response one byte at a time would keep every single read inside its own timeout.
/// The socket goes where `stop()` looks for it as soon as there is one, so that it can be
/// cut while this is still running.
fn ws_connect(
    sh: &Shared,
    port: u16,
    limit: Duration,
) -> Result<(WebSocket<RdOnly>, WebSocket<WrOnly>, TcpStream), String> {
    let deadline = Instant::now() + limit;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, limit).map_err(|e| e.to_string())?;
    let _ = s.set_nodelay(true);
    // Without a second handle nothing could interrupt this session later: better to give
    // up on the connection now and let the loop try again.
    *sh.sock.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.try_clone().map_err(|e| e.to_string())?);
    let r = handshake(sh, &mut s, port, deadline);
    match r {
        Ok((reader, writer)) => Ok((reader, writer, s)),
        Err(e) => {
            *sh.sock.lock().unwrap_or_else(|e| e.into_inner()) = None;
            Err(e)
        }
    }
}

fn handshake(
    sh: &Shared,
    s: &mut TcpStream,
    port: u16,
    deadline: Instant,
) -> Result<(WebSocket<RdOnly>, WebSocket<WrOnly>), String> {
    let left = |what: &str| -> Result<Duration, String> {
        let now = Instant::now();
        if now >= deadline {
            return Err(format!("the handshake was still {} when the time ran out", what));
        }
        Ok(deadline - now)
    };
    s.set_read_timeout(Some(left("starting")?)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(left("starting")?)).map_err(|e| e.to_string())?;

    let key = b64(&nonce16());
    let req = format!(
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {}\r\n\r\n",
        port, key
    );
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())?;

    let mut got: Vec<u8> = Vec::with_capacity(512);
    let mut buf = [0u8; 1024];
    let head = loop {
        if let Some(p) = find(&got, b"\r\n\r\n") {
            break p + 4;
        }
        if got.len() > 32768 {
            return Err("handshake response too long".into());
        }
        if sh.cancel.load(SEQ) {
            return Err("the link was stopped during the handshake".into());
        }
        // whatever is left of the whole limit, so no read can outlast it
        s.set_read_timeout(Some(left("being read")?)).map_err(|e| e.to_string())?;
        let n = s.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the server closed the connection during the handshake".into());
        }
        got.extend_from_slice(&buf[..n]);
    };
    let headers = String::from_utf8_lossy(&got[..head]).to_string();
    let mut lines = headers.split("\r\n");
    let status = lines.next().unwrap_or("");
    if !status.contains(" 101") {
        return Err(format!("the server refused the upgrade: {}", status.trim()));
    }
    let want = accept_key(&key);
    let mut ok = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("sec-websocket-accept") && v.trim() == want {
                ok = true;
            }
        }
    }
    if !ok {
        return Err("the server sent no usable Sec-WebSocket-Accept".into());
    }

    s.set_read_timeout(None).map_err(|e| e.to_string())?;
    s.set_write_timeout(None).map_err(|e| e.to_string())?;
    let rd = s.try_clone().map_err(|e| e.to_string())?;
    let wr = s.try_clone().map_err(|e| e.to_string())?;
    // Windows keeps a timeout per handle, so it goes on the handle whose thread uses it:
    // the sender gives up on a write that cannot get through. The reader has none: it
    // waits for something to read instead (RdOnly), see crate::net.
    wr.set_write_timeout(Some(Duration::from_millis(SEND_MS))).map_err(|e| e.to_string())?;
    let leftover = got[head..].to_vec();
    let reader = WebSocket::from_partially_read(RdOnly(rd), leftover, Role::Client, None);
    let writer = WebSocket::from_raw_socket(WrOnly(wr), Role::Client, None);
    Ok((reader, writer))
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    if h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| &h[i..i + n.len()] == n)
}

/// 16 bytes that are different every time; a local client needs no more than that.
fn nonce16() -> [u8; 16] {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let mut x = (t.as_nanos() as u64) ^ ((std::process::id() as u64) << 32);
    let here = &x as *const u64 as u64;
    x ^= here.rotate_left(17);
    let mut out = [0u8; 16];
    for half in 0..2 {
        // splitmix64
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out[half * 8..half * 8 + 8].copy_from_slice(&z.to_be_bytes());
    }
    out
}

fn b64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

/// RFC 6455: base64(sha1(key + the magic GUID)).
fn accept_key(key: &str) -> String {
    let mut m = Vec::with_capacity(key.len() + 36);
    m.extend_from_slice(key.as_bytes());
    m.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    b64(&sha1(&m))
}

fn sha1(msg: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut data = msg.to_vec();
    let bits = (msg.len() as u64) * 8;
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bits.to_be_bytes());
    for block in data.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i / 20 {
                0 => ((b & c) | ((!b) & d), 0x5A82_7999),
                1 => (b ^ c ^ d, 0x6ED9_EBA1),
                2 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for i in 0..5 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------- the messages

/// The server only ever sends flat objects of strings and numbers, so these few lines
/// stand in for a JSON library (the C# comment: which would pull System.Web and friends
/// into the process).
enum Val {
    S(String),
    N(f64),
    B(bool),
    Null,
}

pub fn parse(json: &str) -> Option<SubMsg> {
    let d = parse_object(json).ok()??;
    let mut m = SubMsg::default();
    m.kind = str_of(&d, "type");
    m.en = str_of(&d, "en");
    m.ja = str_of(&d, "ja");
    m.error = str_of(&d, "message");
    m.asr = str_of(&d, "asr");
    m.mt = str_of(&d, "mt");
    m.device = str_of(&d, "device");
    m.title = str_of(&d, "title");
    m.summary = str_of(&d, "summary");
    m.id = num_of(&d, "id", -1);
    m.seg = num_of(&d, "seg", -1);
    m.lag_ms = num_of(&d, "lag_ms", -1);
    Some(m)
}

fn get<'a>(d: &'a [(String, Val)], k: &str) -> Option<&'a Val> {
    // as a Dictionary does: the last of repeated keys wins
    d.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v)
}

fn str_of(d: &[(String, Val)], k: &str) -> String {
    match get(d, k) {
        Some(Val::S(s)) => s.clone(),
        // as Convert.ToString(object, InvariantCulture) renders them
        Some(Val::N(n)) => fmt_num(*n),
        Some(Val::B(b)) => (if *b { "True" } else { "False" }).to_string(),
        _ => String::new(),
    }
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

fn num_of(d: &[(String, Val)], k: &str, fallback: i32) -> i32 {
    match get(d, k) {
        Some(Val::N(n)) => {
            let r = n.round_ties_even();
            if r.is_finite() && r >= i32::MIN as f64 && r <= i32::MAX as f64 {
                r as i32
            } else {
                fallback
            }
        }
        Some(Val::S(s)) => s.trim().parse::<i32>().unwrap_or(fallback),
        Some(Val::B(b)) => {
            if *b {
                1
            } else {
                0
            }
        }
        _ => fallback,
    }
}

/// `Ok(None)` when the text is not a JSON object, `Err` when it is malformed.
fn parse_object(s: &str) -> Result<Option<Vec<(String, Val)>>, ()> {
    let b: Vec<u16> = s.encode_utf16().collect();
    let mut i = 0usize;
    skip_ws(&b, &mut i);
    if i >= b.len() || b[i] != '{' as u16 {
        return Ok(None);
    }
    i += 1;
    let mut d: Vec<(String, Val)> = Vec::new();
    skip_ws(&b, &mut i);
    if i < b.len() && b[i] == '}' as u16 {
        return Ok(Some(d));
    }
    loop {
        skip_ws(&b, &mut i);
        let key = read_string(&b, &mut i)?;
        skip_ws(&b, &mut i);
        if i >= b.len() || b[i] != ':' as u16 {
            return Err(());
        }
        i += 1;
        let v = read_value(&b, &mut i)?;
        d.push((key, v));
        skip_ws(&b, &mut i);
        if i >= b.len() {
            return Err(());
        }
        if b[i] == ',' as u16 {
            i += 1;
            continue;
        }
        if b[i] == '}' as u16 {
            return Ok(Some(d));
        }
        return Err(());
    }
}

fn read_value(b: &[u16], i: &mut usize) -> Result<Val, ()> {
    skip_ws(b, i);
    if *i >= b.len() {
        return Err(());
    }
    let c = b[*i];
    if c == '"' as u16 {
        return Ok(Val::S(read_string(b, i)?));
    }
    if c == '{' as u16 || c == '[' as u16 {
        skip_nested(b, i)?; // nothing in the protocol nests; step over it if it ever does
        return Ok(Val::Null);
    }
    let start = *i;
    while *i < b.len() && !",}] \t\r\n".encode_utf16().any(|u| u == b[*i]) {
        *i += 1;
    }
    let tok = String::from_utf16_lossy(&b[start..*i]);
    match tok.as_str() {
        "true" => Ok(Val::B(true)),
        "false" => Ok(Val::B(false)),
        "null" => Ok(Val::Null),
        _ => tok.parse::<f64>().map(Val::N).map_err(|_| ()),
    }
}

fn skip_nested(b: &[u16], i: &mut usize) -> Result<(), ()> {
    let mut depth = 0i32;
    while *i < b.len() {
        let c = b[*i];
        if c == '"' as u16 {
            read_string(b, i)?;
            continue;
        }
        if c == '{' as u16 || c == '[' as u16 {
            depth += 1;
        } else if c == '}' as u16 || c == ']' as u16 {
            depth -= 1;
            if depth == 0 {
                *i += 1;
                return Ok(());
            }
        }
        *i += 1;
    }
    Err(())
}

fn read_string(b: &[u16], i: &mut usize) -> Result<String, ()> {
    if *i >= b.len() || b[*i] != '"' as u16 {
        return Err(());
    }
    *i += 1;
    let mut out: Vec<u16> = Vec::new();
    loop {
        if *i >= b.len() {
            return Err(());
        }
        let c = b[*i];
        *i += 1;
        if c == '"' as u16 {
            return Ok(String::from_utf16_lossy(&out));
        }
        if c != '\\' as u16 {
            out.push(c);
            continue;
        }
        if *i >= b.len() {
            return Err(());
        }
        let e = b[*i];
        *i += 1;
        match e {
            0x6E => out.push(0x0A), // n
            0x74 => out.push(0x09), // t
            0x72 => out.push(0x0D), // r
            0x62 => out.push(0x08), // b
            0x66 => out.push(0x0C), // f
            0x75 => {
                // u
                if *i + 4 > b.len() {
                    return Err(());
                }
                let h = String::from_utf16_lossy(&b[*i..*i + 4]);
                let v = u16::from_str_radix(&h, 16).map_err(|_| ())?;
                out.push(v);
                *i += 4;
            }
            _ => out.push(e), // \" \\ \/
        }
    }
}

fn skip_ws(b: &[u16], i: &mut usize) {
    // char.IsWhiteSpace over the ASCII range the protocol can contain
    while *i < b.len() && matches!(b[*i] as u32, 0x20 | 0x09 | 0x0A | 0x0D | 0x0B | 0x0C) {
        *i += 1;
    }
}
