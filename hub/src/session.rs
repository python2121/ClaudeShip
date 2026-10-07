//! The hub's records: a program on a hub-owned pty (`Session`), a screen
//! looking at one (`Client`), and the handle the hub talks to a screen
//! through (`Sink`). All of it is owned by the hub task; only a `Sink`'s
//! far end (a writer task) lives elsewhere.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use serde_json::Value;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::term::replay::ReplayBuffer;
use crate::term::stream::TerminalStream;

/// Identifies one connection — a terminal client on the Unix socket, or a
/// browser on a WebSocket.
pub type ClientId = u64;

/// Not reading for this long means it's gone or stuck; a client that comes
/// back can re-attach and get the replay.
pub const MAX_QUEUED_BYTES: usize = 32 << 20;

/// What the hub tells a client. Each kind of client's writer task turns
/// these into its own wire format.
#[derive(Debug)]
pub enum Event {
    /// Terminal output, untouched.
    Output(Vec<u8>),
    /// The session's size, and whether this client is the one it is sized
    /// for. Sent when either changes, and in answer to a resize.
    Size { rows: u16, cols: u16, owner: bool },
    /// The session ended; the writer closes after sending it.
    Exit(i32),
    /// This client is now attached to session `id` (leader `pid`).
    Attached { id: String, pid: i32 },
    /// The answer to a one-shot request; the writer closes after sending
    /// it, then fires `then` (the hub's shutdown, for `stop`).
    Reply(Value, Option<oneshot::Sender<()>>),
    /// A refusal; the writer closes after sending it.
    Error(String),
    /// A browser asked for a session that doesn't exist (or no longer
    /// does); the writer closes after sending it.
    Gone,
}

impl Event {
    fn weight(&self) -> usize {
        match self {
            Event::Output(bytes) => bytes.len(),
            _ => 64,
        }
    }
}

/// The hub's end of a client's output channel. Unbounded in messages but
/// bounded in bytes: past `MAX_QUEUED_BYTES` unwritten, the client is cut
/// off (`abort`), which ends its reader and writer, and the reader's
/// `Closed` takes it out of the hub.
#[derive(Clone)]
pub struct Sink {
    tx: mpsc::UnboundedSender<Event>,
    queued: Arc<AtomicUsize>,
    abort: Arc<watch::Sender<bool>>,
}

/// The writer task's end.
pub struct SinkReceiver {
    pub rx: mpsc::UnboundedReceiver<Event>,
    queued: Arc<AtomicUsize>,
}

impl SinkReceiver {
    /// Call once an event has been written (or dropped).
    pub fn written(&self, event: &Event) {
        self.queued.fetch_sub(event.weight(), Ordering::Relaxed);
    }
}

impl Sink {
    /// A sink, its receiver, and the abort signal both of the client's
    /// tasks watch.
    pub fn new() -> (Sink, SinkReceiver, watch::Receiver<bool>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let (abort, abort_rx) = watch::channel(false);
        (
            Sink {
                tx,
                queued: queued.clone(),
                abort: Arc::new(abort),
            },
            SinkReceiver { rx, queued },
            abort_rx,
        )
    }

    pub fn send(&self, event: Event) {
        if *self.abort.borrow() {
            return;
        }
        let n = event.weight();
        if self.queued.fetch_add(n, Ordering::Relaxed) + n > MAX_QUEUED_BYTES {
            self.abort();
            return;
        }
        let _ = self.tx.send(event);
    }

    /// The abort signal, for a task that should stop with the client.
    pub fn aborted(&self) -> watch::Receiver<bool> {
        self.abort.subscribe()
    }

    /// Cut the client off now, whatever is still queued.
    pub fn abort(&self) {
        self.abort.send_replace(true);
    }
}

/// One screen. Its size is the size its own window fits.
pub struct Client {
    pub sink: Sink,
    pub rows: u16,
    pub cols: u16,
    /// The session (key) this client is attached to.
    pub session: Option<u64>,
    /// Whether its first frame (the hello) has been handled.
    pub greeted: bool,
}

impl Client {
    pub fn new(sink: Sink) -> Client {
        Client {
            sink,
            rows: 24,
            cols: 80,
            session: None,
            greeted: false,
        }
    }
}

/// A program running on a hub-owned pty.
pub struct Session {
    /// The hub's own handle, never reused (ids can be, once a session is
    /// forgotten); timers name sessions by this.
    pub key: u64,
    pub id: String,
    /// The session leader — the supervisor; the program is its child.
    pub pid: i32,
    /// The pty master; `None` once closed (no more reads, writes, ioctls).
    pub master: Option<Arc<AsyncFd<OwnedFd>>>,
    /// The task that waits for the master to be readable.
    pub pump: Option<JoinHandle<()>>,
    pub cwd: String,
    pub started_at: SystemTime,
    /// "terminal" (started by the `claudeship` command) or "web".
    pub origin: String,
    pub permission_mode: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub stream: TerminalStream,
    pub replay: ReplayBuffer,
    pub attachments: Vec<ClientId>,
    /// The client whose window the pty is currently sized for: the one
    /// that attached, typed, or resized most recently.
    pub size_owner: Option<ClientId>,
    /// Keystrokes not yet written; `pending_from` is how far they have been.
    pub pending_input: Vec<u8>,
    pub pending_from: usize,
    pub input_retry_scheduled: bool,
    /// Grows while the program isn't reading, so a wedged one doesn't keep
    /// the hub waking up 300 times a second for nothing.
    pub input_retry_ms: u64,
    pub finished: bool,
    pub exit_code: i32,
}

impl Session {
    pub fn pending_len(&self) -> usize {
        self.pending_input.len() - self.pending_from
    }

    pub fn clear_pending(&mut self) {
        self.pending_input.clear();
        self.pending_from = 0;
    }
}
