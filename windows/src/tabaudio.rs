//! Receives the audio of a single Chrome tab from the extension in ../extension, which
//! connects to ws://127.0.0.1:<port>/tab and sends what the app would otherwise capture
//! itself: mono 16 kHz s16le. While a tab's audio is arriving it is used instead of the
//! loopback, which can only tell programs apart, not tabs.
//!
//! A port of windows/TabAudio.cs. No async runtime: one thread accepts, one thread serves
//! each connection (at most `MAX_CONNS` of them), one waits out the hold time. All sockets
//! block, with a short read timeout so that a thread notices a replacement or `stop()`
//! without being woken by the operating system in between, and every read goes through
//! `Served`, which looks at those two flags and at the handshake's deadline.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::{WebSocket, WebSocketConfig};
use tungstenite::{Error as WsError, Message, Utf8Bytes};

/// Fixed by the "key" in extension/manifest.json. A web page cannot fake its Origin,
/// so this keeps every site out; other local programs could play into the loopback
/// anyway and are not what the check is for.
pub const EXTENSION_ORIGIN: &str = "chrome-extension://jdedekchhjaibbiphpcolempoknocimf";

/// Close code for a tab that another one has replaced. Its extension lets go of the
/// tab when it sees this; after a plain disconnect it would take the app for restarting,
/// connect again, and two captures would take the source from each other for ever.
pub const REPLACED: u16 = 4001;

/// 100 ms at 16 kHz, as in LoopbackCapture.FrameSamples.
const FRAME_SAMPLES: usize = 1600;
const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// How long a receiving thread may sit in one read before it looks at the stop and
/// replacement flags. Frames arrive every 100 ms, so this costs nothing while a tab plays.
const POLL_MS: u64 = 100;
/// How long the whole handshake may take, however many reads that is: a client that opens
/// the socket and then says nothing, or dribbles its request a byte at a time, must not
/// hold a thread (stop() waits for the threads). The extension's request arrives in one
/// go over loopback.
const HANDSHAKE_MS: u64 = 500;
/// At most this many connections past the handshake are served at once. The extension uses
/// one, two while a tab is being switched; the rest of the room is for the overlap.
const MAX_CONNS: usize = 8;
/// And at most this many may be inside the handshake at the same time. A page opening sockets
/// in a loop is refused only once its request has been read, so without a budget of their own
/// those connections take every slot and the extension's next tab is turned away. When the
/// budget is full the oldest one that has not sent a byte makes way for a newcomer, which may
/// be that tab; a peer that says nothing is dropped at `HANDSHAKE_MS` in any case.
const MAX_HANDSHAKES: usize = 6;
/// How long `stop()` waits for the serving threads altogether, as `EngineLink::stop` waits for
/// the link thread: a read that blocks although the poll said there was something to read must
/// not hold the app's exit.
const STOP_JOIN_MS: u64 = 1500;
/// Writing to a tab that has stopped reading must not hold a thread for ever either.
const WRITE_MS: u64 = 2000;
/// Lines a peer can provoke at will: the first few go into the log, then at most one a
/// minute, so a page hammering the port cannot grow log.txt without bound.
const LOG_FIRST: u32 = 5;
const LOG_SUMMARY_MS: u64 = 60_000;
/// As the C# timer: the hold time is checked four times a second while a tab is active.
const WATCH_MS: u64 = 250;
/// The replaced tab gets this long to answer the close (20 x 50 ms in the C#).
const DISMISS_TRIES: u32 = 20;
const DISMISS_STEP_MS: u64 = 50;
/// Sending the close to a tab that does not read any more must not block for ever.
const CLOSE_WRITE_MS: u64 = 1000;
/// The extension sends one 3200-byte frame per message; nothing here needs more than a
/// fraction of this, and the cap keeps a confused sender from eating memory.
const MAX_MESSAGE: usize = 64 * 1024;

/// One accepted connection. The socket is a second handle on the same socket as the
/// `WebSocket`'s, for cutting the connection from another thread. Timeouts are not set
/// through it: Windows keeps them per handle, not per socket, so they would be lost.
struct Conn {
    id: u64,
    sock: TcpStream,
    dismiss: AtomicBool,
    /// true until the handshake is through: these are counted against `MAX_HANDSHAKES`, not
    /// against the room kept for the tabs that passed the Origin check
    shaking: AtomicBool,
}

#[derive(Default)]
struct State {
    current: Option<Arc<Conn>>, // the tab being received
    fresh: bool,                // `current` has not delivered a frame yet
    active: bool,
    last_frame: u64, // milliseconds on Shared::clock
    watching: bool,  // the watch thread is counting out the hold time
    // gap counting, as in the C#: shared by the connections, reset when one is adopted
    zero_run: i32,
    before_run: i32,
    gap_samples: u64,
    gaps_logged: u64,
}

/// How often a line that a peer can provoke has gone out, so that it can be held back.
#[derive(Default)]
struct Limit {
    shown: u32,
    hidden: u64,
    next: u64, // clock ms before which nothing more is logged
}

struct Shared {
    hold_ms: u64,
    on_frame: Box<dyn Fn(&[u8]) + Send + Sync>,
    on_active: Box<dyn Fn(bool) + Send + Sync>,
    diag: Box<dyn Fn(&str) + Send + Sync>,
    st: Mutex<State>,
    wake: Condvar,
    clock: Instant,
    frames: AtomicU64,
    gaps: AtomicU64,
    rms: AtomicU32, // f32 bits
    stop: AtomicBool,
    next_id: AtomicU64,
    serving: Mutex<Vec<(Arc<AtomicBool>, JoinHandle<()>)>>,
    /// Every connection being served, handshake or tab: where `stop()` finds the sockets
    /// of the threads it has to get out of a read or a write, and what the cap counts.
    conns: Mutex<Vec<Arc<Conn>>>,
    refused: Mutex<Limit>,
    turned_away: Mutex<Limit>,
    accept_failed: Mutex<Limit>,
    sink_threw: Mutex<Limit>,
}

impl Shared {
    /// A panicking callback must not leave the state locked for ever.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> u64 {
        self.clock.elapsed().as_millis() as u64
    }

    fn d(&self, s: &str) {
        let _ = catch_unwind(AssertUnwindSafe(|| (self.diag)(s)));
    }

    /// A line a peer decides the rate of: the first `LOG_FIRST` of its kind go into the
    /// log, then one a minute with the number that were held back.
    fn d_limited(&self, lim: &Mutex<Limit>, s: &str) {
        let now = self.now();
        let out = {
            let mut l = lim.lock().unwrap_or_else(|e| e.into_inner());
            if l.shown < LOG_FIRST {
                l.shown += 1;
                l.next = now + LOG_SUMMARY_MS;
                Some(s.to_string())
            } else if now >= l.next {
                l.next = now + LOG_SUMMARY_MS;
                let n = std::mem::take(&mut l.hidden);
                Some(format!("{} ({} more of these were left out)", s, n))
            } else {
                l.hidden += 1;
                None
            }
        };
        if let Some(line) = out {
            self.d(&line);
        }
    }

    fn raise(&self, on: bool) {
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| (self.on_active)(on))) {
            self.d(&format!("on_active: {}", panic_text(&p)));
        }
    }

    fn set_rms(&self, v: f32) {
        self.rms.store(v.to_bits(), Ordering::Relaxed);
    }
}

pub struct TabAudio {
    sh: Arc<Shared>,
    addr: SocketAddr,
    accept: Option<JoinHandle<()>>,
    watch: Option<JoinHandle<()>>,
}

impl TabAudio {
    /// Starts listening; `None` when the port could not be opened (another program has it).
    /// `on_frame` gets every 3200-byte frame, `on_active` true when a tab started feeding
    /// (also when it replaces another one) and false when nothing has arrived for `hold_ms`,
    /// `diag` log lines. All of them are called on this module's own threads; `on_active` is
    /// called with the state lock held so that the calls keep the order of the changes, so
    /// that handler must return at once.
    pub fn start(
        port: u16,
        hold_ms: u32,
        on_frame: Box<dyn Fn(&[u8]) + Send + Sync>,
        on_active: Box<dyn Fn(bool) + Send + Sync>,
        diag: Box<dyn Fn(&str) + Send + Sync>,
    ) -> Option<TabAudio> {
        let sh = Arc::new(Shared {
            hold_ms: hold_ms as u64,
            on_frame,
            on_active,
            diag,
            st: Mutex::new(State::default()),
            wake: Condvar::new(),
            clock: Instant::now(),
            frames: AtomicU64::new(0),
            gaps: AtomicU64::new(0),
            rms: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            serving: Mutex::new(Vec::new()),
            conns: Mutex::new(Vec::new()),
            refused: Mutex::new(Limit::default()),
            turned_away: Mutex::new(Limit::default()),
            accept_failed: Mutex::new(Limit::default()),
            sink_threw: Mutex::new(Limit::default()),
        });
        // a loopback-only listener needs neither admin rights nor a firewall rule, and
        // without SO_REUSEADDR a port another program already has stays refused
        let listener = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => l,
            Err(e) => {
                sh.d(&format!("cannot listen on port {}: {}", port, e));
                return None;
            }
        };
        let addr = match listener.local_addr() {
            Ok(a) => a,
            Err(_) => SocketAddr::from(([127, 0, 0, 1], port)),
        };
        let accept = {
            let sh = sh.clone();
            thread::Builder::new()
                .name("tab-accept".into())
                .stack_size(96 * 1024)
                .spawn(move || accept_loop(sh, listener))
        };
        let accept = match accept {
            Ok(h) => h,
            Err(e) => {
                sh.d(&format!("cannot start the listening thread: {}", e));
                return None;
            }
        };
        let watch = {
            let sh = sh.clone();
            thread::Builder::new()
                .name("tab-watch".into())
                .stack_size(96 * 1024)
                .spawn(move || watch_loop(sh))
        };
        let watch = match watch {
            Ok(h) => h,
            Err(e) => {
                sh.d(&format!("cannot start the hold thread: {}", e));
                sh.stop.store(true, Ordering::Release);
                let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(300));
                let _ = accept.join();
                return None;
            }
        };
        sh.d(&format!("listening on 127.0.0.1:{}", port));
        Some(TabAudio { sh, addr, accept: Some(accept), watch: Some(watch) })
    }

    /// 0..1, latest frame (sidebar level meter).
    pub fn last_rms(&self) -> f32 {
        f32::from_bits(self.sh.rms.load(Ordering::Relaxed))
    }

    pub fn frames(&self) -> u64 {
        self.sh.frames.load(Ordering::Relaxed)
    }

    /// See `emit`: short stretches of exact silence cut into the tab's sound.
    pub fn gaps(&self) -> u64 {
        self.sh.gaps.load(Ordering::Relaxed)
    }

    /// Closes the listener and every connection and joins the threads.
    pub fn stop(mut self) {
        self.sh.stop.store(true, Ordering::Release);
        {
            let mut g = self.sh.lock();
            g.active = false;
            g.watching = false;
            g.current = None;
        }
        self.sh.wake.notify_all();
        // Every connection being served, not only the tab's: a thread may be inside a read
        // that a peer keeps feeding, or inside a write to a peer that has stopped reading,
        // and neither ends by itself. Cutting the socket ends both at once; the linger
        // makes it a reset, as WebSocket.Abort() in the C# is.
        for c in lock_conns(&self.sh).iter() {
            hard_close(&c.sock);
            let _ = c.sock.shutdown(Shutdown::Both);
        }
        // A connection of our own so that the accepting thread leaves its blocking accept.
        // It is held open until that thread has been joined: a connection closed before it
        // is accepted can be dropped from the queue, and the accept would go on waiting.
        let wake = TcpStream::connect_timeout(&self.addr, Duration::from_millis(300)).ok();
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
        if let Some(s) = wake {
            hard_close(&s);
        }
        if let Some(h) = self.watch.take() {
            let _ = h.join();
        }
        // Every serving thread says when it is done. They end on the cut sockets above, but
        // one that does not - a read that blocks although the poll said there was something
        // to read - may not hold the app's exit: wait the bound out, join what has finished
        // and leave the rest to themselves, as `EngineLink::stop` does.
        let left: Vec<_> = lock_serving(&self.sh).drain(..).collect();
        let deadline = Instant::now() + Duration::from_millis(STOP_JOIN_MS);
        for (done, h) in left {
            while !done.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if done.load(Ordering::Acquire) {
                let _ = h.join();
            } else {
                self.sh.d("a tab connection did not finish in time; left to itself");
            }
        }
    }
}

impl Drop for TabAudio {
    fn drop(&mut self) {
        // Only when stop() was not called: it leaves nothing to do here. This tells the
        // threads to end but does not wait for them; stop() is what closes everything.
        if self.accept.is_none() && self.watch.is_none() {
            return;
        }
        self.sh.stop.store(true, Ordering::Release);
        self.sh.wake.notify_all();
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(300));
    }
}

fn lock_serving(sh: &Shared) -> MutexGuard<'_, Vec<(Arc<AtomicBool>, JoinHandle<()>)>> {
    sh.serving.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_conns(sh: &Shared) -> MutexGuard<'_, Vec<Arc<Conn>>> {
    sh.conns.lock().unwrap_or_else(|e| e.into_inner())
}

fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "panicked".to_string()
    }
}

/// A read that came to nothing rather than to a broken connection; retrying is safe, because
/// the frame reader keeps what it has already read. `TimedOut` is not one of these: the reads
/// of a session never report it (see `Served::read`), so one that turns up came from a write
/// that ran into SO_SNDTIMEO inside tungstenite - a pong or a close it was flushing - and a
/// socket a write timed out on is in the undefined state `crate::net` is about.
fn retryable(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
}

/// Cuts the connection the way WebSocket.Abort() does: a reset, which leaves no socket
/// behind in TIME_WAIT to keep the port from being listened on again.
fn hard_close(sock: &TcpStream) {
    let _ = socket2::SockRef::from(sock).set_linger(Some(Duration::ZERO));
}

/// The stream a connection is served through. tungstenite reads on until a message is
/// complete, so a peer that dribbles the bytes of one frame would hold a thread inside a
/// single `read()` for as long as it likes: every read here is a chance to see that the
/// listener is stopping or that another tab has taken over. Before the handshake is
/// through, `until` gives the whole of it one deadline instead of one per read.
struct Served {
    s: TcpStream,
    until: Option<Instant>,
    sh: Arc<Shared>,
    /// `None` while the handshake runs, and again while the close is being sent: the
    /// thread is then doing what the flag would have told it to do.
    conn: Option<Arc<Conn>>,
    /// how long one read waits for something to arrive before it reports "nothing yet"
    /// (WouldBlock, or TimedOut while the handshake runs), so that the flags above are
    /// looked at again
    wait: Duration,
}

impl Read for Served {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if self.sh.stop.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "stopping"));
        }
        if let Some(c) = &self.conn {
            if c.dismiss.load(Ordering::Acquire) {
                // leave the frame half-read: tungstenite keeps it and goes on from there
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "replaced"));
            }
        }
        let mut wait = self.wait;
        if let Some(t) = self.until {
            let now = Instant::now();
            if now >= t {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "took too long"));
            }
            wait = wait.min(t - now);
        }
        if !crate::net::readable(&self.s, wait)? {
            // WouldBlock once the handshake is through, so that `retryable` can tell this
            // apart from a write of tungstenite's own that ran into its timeout. The
            // handshake wants TimedOut: there it is the deadline, and it has to end.
            let kind = match self.until {
                Some(_) => io::ErrorKind::TimedOut,
                None => io::ErrorKind::WouldBlock,
            };
            return Err(io::Error::new(kind, "nothing yet"));
        }
        self.s.read(b)
    }
}

impl Write for Served {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.s.write(b)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.s.flush()
    }
}

/// How a connection ended, and therefore how its socket is let go.
enum End {
    /// the peer closed or answered the close: the socket can go the civil way
    Peer,
    /// replaced or stopped without an answer: cut, as WebSocket.Abort() does in the C#
    Cut,
}

fn accept_loop(sh: Arc<Shared>, l: TcpListener) {
    while !sh.stop.load(Ordering::Acquire) {
        // A panic while a connection is being taken on must not take the listener away:
        // the C# HttpListener went on accepting whatever one request did.
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| accept_one(&sh, &l))) {
            sh.d(&format!("taking a tab on failed: {}", panic_text(&p)));
        }
    }
    drop(l);
}

/// One turn of the accept loop.
fn accept_one(sh: &Arc<Shared>, l: &TcpListener) {
    let (stream, _peer) = match l.accept() {
        Ok(x) => x,
        Err(e) => {
            if sh.stop.load(Ordering::Acquire) {
                return;
            }
            // A connection the peer reset before we got to it, or no handles left for a
            // moment: the listener stays, or the extension could never connect again.
            sh.d_limited(&sh.accept_failed, &format!("a connection could not be taken on: {}", e));
            thread::sleep(Duration::from_millis(50));
            return;
        }
    };
    if sh.stop.load(Ordering::Acquire) {
        // the connection stop() makes to get us out of accept(); reset rather than
        // closed, which would leave this port in TIME_WAIT and unable to listen again
        hard_close(&stream);
        return;
    }
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(Duration::from_millis(WRITE_MS)));
    let sock = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            sh.d_limited(&sh.accept_failed, &format!("a connection could not be taken on: {}", e));
            hard_close(&stream);
            return;
        }
    };
    let id = sh.next_id.fetch_add(1, Ordering::Relaxed);
    let conn = Arc::new(Conn { id, sock, dismiss: AtomicBool::new(false), shaking: AtomicBool::new(true) });

    // the next tab may connect while this one is still open: a thread of its own, but
    // only up to the cap, and the finished ones are joined here
    let mut v = lock_serving(sh);
    let mut i = 0;
    while i < v.len() {
        if v[i].0.load(Ordering::Acquire) {
            let (_, old) = v.remove(i);
            let _ = old.join();
        } else {
            i += 1;
        }
    }
    {
        let mut live = lock_conns(sh);
        let shaking: Vec<usize> = live
            .iter()
            .enumerate()
            .filter(|(_, c)| c.shaking.load(Ordering::Acquire))
            .map(|(i, _)| i)
            .collect();
        if live.len() - shaking.len() >= MAX_CONNS {
            sh.d_limited(
                &sh.turned_away,
                &format!("more than {} tabs at once; turned one away", MAX_CONNS),
            );
            hard_close(&stream);
            return;
        }
        if shaking.len() >= MAX_HANDSHAKES {
            // The oldest handshake that has not sent a byte makes way for this connection,
            // which may be the extension's; one that is sending its request is left alone.
            let waiting = shaking
                .iter()
                .copied()
                .find(|&i| !crate::net::readable(&live[i].sock, Duration::ZERO).unwrap_or(true));
            match waiting {
                Some(i) => {
                    let old = live.remove(i);
                    sh.d_limited(
                        &sh.turned_away,
                        &format!("{} handshakes at once; dropped the oldest that had sent nothing", MAX_HANDSHAKES),
                    );
                    hard_close(&old.sock);
                    let _ = old.sock.shutdown(Shutdown::Both);
                }
                None => {
                    sh.d_limited(
                        &sh.turned_away,
                        &format!("more than {} handshakes at once; turned one away", MAX_HANDSHAKES),
                    );
                    hard_close(&stream);
                    return;
                }
            }
        }
        live.push(conn.clone());
    }
    let done = Arc::new(AtomicBool::new(false));
    let spawned = {
        let sh = sh.clone();
        let done = done.clone();
        thread::Builder::new()
            .name("tab-conn".into())
            .stack_size(128 * 1024)
            .spawn(move || serve(sh, stream, conn, done))
    };
    match spawned {
        Ok(h) => v.push((done, h)),
        Err(e) => {
            // the socket went with the closure the thread would have run
            forget_conn(sh, id);
            sh.d(&format!("cannot serve the tab: {}", e));
        }
    }
}

fn serve(sh: Arc<Shared>, stream: TcpStream, conn: Arc<Conn>, done: Arc<AtomicBool>) {
    if let Err(p) = catch_unwind(AssertUnwindSafe(|| serve_inner(&sh, stream, &conn))) {
        sh.d(&format!("tab connection ended: {}", panic_text(&p)));
    }
    forget_conn(&sh, conn.id);
    done.store(true, Ordering::Release);
}

/// Takes the connection out of the list `stop()` cuts and the cap counts.
fn forget_conn(sh: &Shared, id: u64) {
    lock_conns(sh).retain(|c| c.id != id);
}

fn serve_inner(sh: &Arc<Shared>, stream: TcpStream, conn: &Arc<Conn>) {
    let cfg = WebSocketConfig::default()
        .read_buffer_size(4096)
        .write_buffer_size(0)
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let shc = sh.clone();
    let check = move |rq: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        let path = rq.uri().path();
        let origin = rq.headers().get("Origin").and_then(|v| v.to_str().ok());
        if path != "/tab" || origin != Some(EXTENSION_ORIGIN) {
            shc.d_limited(
                &shc.refused,
                &format!(
                    "refused {} {} from {}",
                    rq.method(),
                    path,
                    origin.unwrap_or("(no origin)")
                ),
            );
            let mut r = ErrorResponse::new(None);
            *r.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
            return Err(r);
        }
        Ok(resp)
    };
    let served = Served {
        s: stream,
        until: Some(Instant::now() + Duration::from_millis(HANDSHAKE_MS)),
        sh: sh.clone(),
        conn: None,
        wait: Duration::from_millis(HANDSHAKE_MS), // `until` cuts it short
    };
    let mut ws = match tungstenite::accept_hdr_with_config(served, check, Some(cfg)) {
        Ok(ws) => ws,
        Err(e) => {
            // a refusal is already in the log, from the check above; the 403 went out
            // before this, and the socket is closed the civil way, as the C# closes
            // the response
            if !refused(&e) && !sh.stop.load(Ordering::Acquire) {
                sh.d_limited(&sh.refused, &format!("the handshake came to nothing: {}", e));
            }
            return;
        }
    };
    // the handshake is through: reads may now take as long as the tab is quiet, and the
    // replacement flag is what they watch for
    {
        let st = ws.get_mut();
        st.until = None;
        st.conn = Some(conn.clone());
    }
    conn.shaking.store(false, Ordering::Release); // it has a tab's room now, not a handshake's
    adopt(sh, conn);
    let end = receive(sh, &mut ws, conn);
    release(sh, conn);
    // A tab that was replaced or stopped before it could answer has its connection cut,
    // as WebSocket.Abort() does in the C#. One that went away by itself, or that has
    // answered the close and let go, gets the civil close that dropping the socket is.
    if let End::Cut = end {
        hard_close(&ws.get_ref().s);
    }
    drop(ws);
}

fn refused<T: tungstenite::handshake::HandshakeRole>(e: &tungstenite::HandshakeError<T>) -> bool {
    match e {
        tungstenite::HandshakeError::Failure(WsError::Http(r)) => r.status().is_client_error(),
        _ => false,
    }
}

/// The newest connection is the one the user just clicked: it replaces any other.
fn adopt(sh: &Shared, conn: &Arc<Conn>) {
    let old = {
        let mut g = sh.lock();
        let old = g.current.replace(conn.clone());
        g.fresh = true;
        // the silence a tab starts with is not a gap in its sound
        g.zero_run = 0;
        g.before_run = 0;
        old
    };
    if let Some(old) = old {
        // Its own thread sends the close and waits for the answer: see dismiss(). Doing it
        // from here would mean writing to a socket another thread is reading.
        old.dismiss.store(true, Ordering::Release);
    }
    sh.d("tab connected");
}

/// Tells the replaced tab why, and waits for its answer: cutting the connection right
/// away can throw the close message away before it is read, and the extension would take
/// the abnormal close for the app restarting and come back in two seconds.
///
/// The sound the tab had already sent arrives before that answer, so those frames are
/// read and dropped until the close comes back or the whole wait (20 x 50 ms, as in the
/// C#) is over.
fn dismiss(sh: &Shared, ws: &mut WebSocket<Served>) -> End {
    {
        let st = ws.get_mut();
        st.conn = None; // this is the answer to the flag; reads must go through again
        let _ = st.s.set_write_timeout(Some(Duration::from_millis(CLOSE_WRITE_MS)));
        st.wait = Duration::from_millis(DISMISS_STEP_MS);
    }
    let frame = CloseFrame {
        code: CloseCode::Library(REPLACED),
        reason: Utf8Bytes::from_static("replaced"),
    };
    if let Err(e) = ws.close(Some(frame)) {
        sh.d(&format!("could not tell the replaced tab: {}", e));
        return End::Cut;
    }
    let until = Instant::now() + Duration::from_millis(DISMISS_TRIES as u64 * DISMISS_STEP_MS);
    while Instant::now() < until {
        match ws.read() {
            // The answer: the tab has read our close, so letting the socket go the civil
            // way can no longer turn it into an abnormal one. (A connection closed here
            // leaves this port in TIME_WAIT, which the reset was for, but a listener can
            // be opened on it again all the same - "after Stop" in tab-test shows it.)
            Ok(Message::Close(_)) => return End::Peer,
            Ok(_) => {} // sound that was already on its way
            Err(WsError::Io(e)) if retryable(&e) => {}
            // the socket went before the answer could come: nothing left to cut
            Err(_) => return End::Peer,
        }
        if sh.stop.load(Ordering::Acquire) {
            break;
        }
    }
    End::Cut
}

fn release(sh: &Shared, conn: &Conn) {
    {
        let mut g = sh.lock();
        match g.current.as_ref() {
            Some(c) if c.id == conn.id => g.current = None,
            _ => return, // already replaced by a newer tab
        }
    }
    sh.set_rms(0.0);
    sh.d("tab disconnected");
}

fn receive(sh: &Shared, ws: &mut WebSocket<Served>, conn: &Arc<Conn>) -> End {
    ws.get_mut().wait = Duration::from_millis(POLL_MS);
    let mut frame = [0u8; FRAME_BYTES];
    let mut filled = 0usize;
    loop {
        if sh.stop.load(Ordering::Acquire) {
            return End::Cut;
        }
        if conn.dismiss.load(Ordering::Acquire) {
            return dismiss(sh, ws);
        }
        match ws.read() {
            Ok(Message::Binary(b)) => {
                // The extension sends one frame per message, but nothing here depends on it.
                let mut off = 0;
                while off < b.len() {
                    let n = (FRAME_BYTES - filled).min(b.len() - off);
                    frame[filled..filled + n].copy_from_slice(&b[off..off + n]);
                    filled += n;
                    off += n;
                    if filled < FRAME_BYTES {
                        continue;
                    }
                    emit(sh, conn, &frame);
                    filled = 0;
                }
            }
            Ok(Message::Close(_)) => {
                // the answering close is queued; this sends it
                let _ = ws.flush();
                return End::Peer;
            }
            Ok(_) => {} // text, ping and pong: tungstenite answers pings itself
            Err(WsError::Io(e)) if retryable(&e) => {}
            // A write tungstenite made inside this read - the pong it answers a ping with,
            // or the answer to a close - ran into SO_SNDTIMEO: the tab is not reading, and
            // part of a frame may be on the socket. Nothing may be read or written on it
            // after that, so the connection ends, and is cut rather than closed politely.
            Err(WsError::Io(ref e)) if e.kind() == io::ErrorKind::TimedOut => {
                if !sh.stop.load(Ordering::Acquire) {
                    sh.d("tab connection ended: a write to the tab timed out");
                }
                return End::Cut;
            }
            Err(WsError::ConnectionClosed) => return End::Peer,
            Err(e) => {
                // stop() cuts the socket to get this thread out of its read; that is not
                // something to log, and nothing is left to close either
                if sh.stop.load(Ordering::Acquire) {
                    return End::Cut;
                }
                sh.d(&format!("tab connection ended: {}", e));
                return End::Peer;
            }
        }
    }
}

fn emit(sh: &Shared, conn: &Conn, frame: &[u8; FRAME_BYTES]) {
    // A hiccup between the tab and the extension shows up as a short stretch of exact
    // silence cut into the sound. Counted and logged, so that real use tells whether
    // it happens: the test browser had a few a minute, with no sound card behind it.
    let mut sq = 0f64;
    let mut log: Option<String> = None;
    {
        let mut g = sh.lock();
        // Before anything is counted: a frame of a tab that another one has replaced may be
        // read after the newer tab was adopted (several frames can come out of one message),
        // and the gap counting below is shared, so it would be laid at the new tab's door.
        match g.current.as_ref() {
            Some(c) if c.id == conn.id => {}
            _ => return, // a newer tab has taken over
        }
        for i in (0..FRAME_BYTES).step_by(2) {
            let v = i16::from_le_bytes([frame[i], frame[i + 1]]) as i32;
            sq += (v as f64) * (v as f64);
            if v == 0 {
                if g.zero_run <= 1600 {
                    g.zero_run += 1; // longer than that is a pause, however long
                }
                continue;
            }
            if g.zero_run >= 32
                && g.zero_run <= 1600
                && g.before_run.abs() >= 32
                && v.abs() >= 32
            {
                sh.gaps.fetch_add(1, Ordering::Relaxed);
                g.gap_samples += g.zero_run as u64;
            }
            g.zero_run = 0;
            g.before_run = v;
        }

        g.last_frame = sh.now();
        if !g.active || g.fresh {
            g.active = true;
            g.fresh = false;
            if !g.watching {
                g.watching = true;
                sh.wake.notify_all();
            }
            sh.raise(true);
        }
        sh.set_rms(((sq / FRAME_SAMPLES as f64).sqrt() / 32768.0) as f32);
        let n = sh.frames.fetch_add(1, Ordering::Relaxed) + 1;
        let gaps = sh.gaps.load(Ordering::Relaxed);
        if n % 600 == 0 && gaps > g.gaps_logged {
            log = Some(format!(
                "{} short gaps in the tab's audio in the last minute ({} ms of silence in total)",
                gaps - g.gaps_logged,
                g.gap_samples / 16
            ));
            g.gaps_logged = gaps;
            g.gap_samples = 0;
        }
    }
    if let Some(s) = log {
        sh.d(&s);
    }
    // A sink that keeps panicking would otherwise write ten lines a second into the log.
    if let Err(p) = catch_unwind(AssertUnwindSafe(|| (sh.on_frame)(frame.as_slice()))) {
        sh.d_limited(&sh.sink_threw, &format!("frame sink threw: {}", panic_text(&p)));
    }
}

/// The C# timer that runs while a tab is active, as a thread that sleeps while none is.
/// Its own work cannot panic; the handler it calls is guarded in `raise`.
fn watch_loop(sh: Arc<Shared>) {
    let mut g = sh.lock();
    loop {
        while !sh.stop.load(Ordering::Acquire) && !g.watching {
            g = sh.wake.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        if sh.stop.load(Ordering::Acquire) {
            return;
        }
        let (next, _) = sh
            .wake
            .wait_timeout(g, Duration::from_millis(WATCH_MS))
            .unwrap_or_else(|e| e.into_inner());
        g = next;
        if sh.stop.load(Ordering::Acquire) {
            return;
        }
        if !g.watching || !g.active || sh.now().saturating_sub(g.last_frame) < sh.hold_ms {
            continue;
        }
        g.active = false;
        g.watching = false;
        sh.set_rms(0.0);
        sh.raise(false);
    }
}
