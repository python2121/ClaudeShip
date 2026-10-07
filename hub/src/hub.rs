//! The session hub: a long-lived process that owns every pty, so a session
//! outlives whichever terminal or browser happens to be looking at it.
//!
//! One task owns all of this (`run`) and takes `Command`s over a channel —
//! the shape of the Swift hub's serial queue. Nothing here blocks: pty
//! reads and writes are non-blocking, and every client is written to by
//! its own writer task through a `Sink`.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};

use crate::approvals::{Approvals, PendingApproval, Rule};
use crate::config::HubConfig;
use crate::frame::PROTOCOL;
use crate::session::{Client, ClientId, Event, Session, Sink};
use crate::term::input::is_user_activity;
use crate::term::replay::ReplayBuffer;
use crate::term::stream::{Piece, TerminalStream};
use crate::{paths, pty, supervisor};

/// Reads per wakeup (so one chatty session can't starve the others), and
/// when collecting what a program wrote on its way out.
const DRAIN_ROUNDS: usize = 8;
const DRAIN_ROUNDS_ON_EXIT: usize = 256;
/// How long a session that ended stays attachable.
const ENDED_KEEP: Duration = Duration::from_secs(30);
/// Hang-up, then SIGTERM after this, then SIGKILL a second later.
const TERMINATE_GRACE: Duration = Duration::from_secs(5);
const KILL_GRACE: Duration = Duration::from_secs(1);
/// Keystrokes queued for a program that isn't reading.
const MAX_PENDING_INPUT: usize = 8 << 20;
const INPUT_RETRY_FIRST_MS: u64 = 3;
const INPUT_RETRY_MAX_MS: u64 = 100;

pub fn log(message: &str) {
    eprintln!("{} {message}", iso8601(SystemTime::now()));
}

/// `2026-10-07T12:34:56Z`.
pub fn iso8601(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn seconds_since_epoch(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// What a drain found: the pty has nothing more for now, it has more (the
/// round limit stopped us), or it is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    Empty,
    More,
    Closed,
}

pub enum Command {
    /// A client connected; its first frame will follow.
    Connected {
        client: ClientId,
        sink: Sink,
    },
    Hello {
        client: ClientId,
        request: Map<String, Value>,
    },
    Input {
        client: ClientId,
        data: Vec<u8>,
    },
    Resize {
        client: ClientId,
        rows: u16,
        cols: u16,
    },
    /// The client's connection is gone.
    Closed {
        client: ClientId,
    },
    /// A session's pty is readable.
    Drain {
        key: u64,
        ack: oneshot::Sender<Drained>,
    },
    FlushInput {
        key: u64,
    },
    ForgetEnded {
        key: u64,
    },
    /// `terminate`'s later steps: 1 = SIGTERM, 2 = SIGKILL.
    Escalate {
        key: u64,
        step: u8,
    },
    Shutdown,
    /// A browser terminal (already `Connected`) asks for session `id`.
    /// `claim: false` (`claim=0`) joins as a spectator.
    WebAttach {
        client: ClientId,
        id: String,
        rows: u16,
        cols: u16,
        claim: bool,
    },
    /// A browser's window changed size while it may only be watching.
    Fit {
        client: ClientId,
        rows: u16,
        cols: u16,
    },
    /// What the directory build needs, copied off the hub task.
    WebSnapshot {
        reply: oneshot::Sender<Snapshot>,
    },
    /// `POST /api/launch`: answered with the HTTP status and JSON body.
    WebLaunch {
        path: String,
        mode: Option<String>,
        resume: Option<String>,
        reply: oneshot::Sender<(u16, Value)>,
    },
    /// `POST /api/kill`: whether there was such a session.
    WebKill {
        id: String,
        reply: oneshot::Sender<bool>,
    },
    /// `POST /api/settings`, already validated.
    WebSettings {
        mode: String,
        reply: oneshot::Sender<()>,
    },
    /// A permission-hook helper sent its request; `verdict` is what its
    /// connection waits on (dropping it hangs up without one).
    ApprovalArrived {
        approval: PendingApproval,
        verdict: oneshot::Sender<bool>,
    },
    /// A helper went away (Claude Code killed it: the prompt is settled).
    ApprovalClosed {
        id: String,
    },
    /// `POST /api/approve`: whether there was such a request.
    Approve {
        id: String,
        allow: bool,
        reply: oneshot::Sender<bool>,
    },
    /// `POST /api/auto-approve`: set (`Some`) or clear a session's rule.
    AutoApprove {
        session_id: String,
        rule: Option<Rule>,
        reply: oneshot::Sender<()>,
    },
    /// What a directory build found: requests to hang up on (answered in
    /// the terminal, or their session gone), and the Claude sessions alive.
    Reconcile {
        stale: Vec<String>,
        live: HashSet<String>,
    },
    /// The approvals ticker asks whether to carry on (anything pending).
    ApprovalTick {
        reply: oneshot::Sender<bool>,
    },
}

/// What the directory build needs from the hub task.
pub struct Snapshot {
    pub sessions: Vec<SessionSnapshot>,
    pub config: HubConfig,
    pub approvals: Vec<PendingApproval>,
    pub rules: HashMap<String, Rule>,
}

/// What the directory build knows about a hub-owned session.
#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub id: String,
    pub pid: i32,
    pub cwd: String,
    pub started_at: SystemTime,
    pub viewers: usize,
    pub permission_mode: Option<String>,
}

/// The web server's part of the hub's state: whether it is listening, and
/// the pairing secret (shared with the server's tasks).
pub struct WebState {
    pub shared: Arc<crate::web::Shared>,
}

impl WebState {
    pub fn listening(&self) -> bool {
        self.shared.is_listening()
    }

    pub fn token(&self) -> Option<String> {
        self.shared.token()
    }

    /// A new pairing secret: every browser paired so far is out, and every
    /// open web connection is dropped.
    pub fn rotate_token(&mut self) -> bool {
        self.shared.rotate_token()
    }
}

pub struct Hub {
    tx: mpsc::UnboundedSender<Command>,
    sessions: Vec<Session>,
    /// Sessions that ended in the last half minute, kept so a screen that
    /// arrives just too late still gets what the program said and how it
    /// exited — a launch that fails at once would otherwise just vanish.
    ended: Vec<Session>,
    clients: HashMap<ClientId, Client>,
    pub config: HubConfig,
    started_at: SystemTime,
    next_key: u64,
    /// This binary, which every session runs as its supervisor.
    supervisor: PathBuf,
    pub web: WebState,
    scratch: Vec<u8>,
    /// Permission requests waiting on a screen, and the standing rules.
    approvals: Approvals,
    /// Whether the task that keeps reconciling pendings is running.
    approvals_ticking: bool,
}

/// The shell's convention: the exit status, or 128 + the fatal signal.
pub use supervisor::exit_code;

pub fn clamp_size(rows: i64, cols: i64) -> Option<(u16, u16)> {
    ((2..=1000).contains(&rows) && (10..=2000).contains(&cols))
        .then_some((rows as u16, cols as u16))
}

/// Waits for a session's pty to be readable and asks the hub to read it.
/// Readiness is cleared only when the hub read it dry, so a drain stopped
/// by the round limit is followed by another at once.
async fn pump(
    master: Arc<AsyncFd<std::os::fd::OwnedFd>>,
    key: u64,
    tx: mpsc::UnboundedSender<Command>,
) {
    loop {
        let Ok(mut guard) = master.readable().await else {
            return;
        };
        let (ack, answer) = oneshot::channel();
        if tx.send(Command::Drain { key, ack }).is_err() {
            return;
        }
        match answer.await {
            Ok(Drained::Empty) => guard.clear_ready(),
            Ok(Drained::More) => {}
            _ => return,
        }
    }
}

impl Hub {
    pub fn new(config: HubConfig, tx: mpsc::UnboundedSender<Command>, supervisor: PathBuf) -> Hub {
        let web = WebState {
            shared: crate::web::Shared::new(&config, tx.clone()),
        };
        Hub {
            tx,
            sessions: Vec::new(),
            ended: Vec::new(),
            clients: HashMap::new(),
            config,
            started_at: SystemTime::now(),
            next_key: 1,
            supervisor,
            web,
            scratch: vec![0; 65_536],
            approvals: Approvals::default(),
            approvals_ticking: false,
        }
    }

    fn after(&self, delay: Duration, command: Command) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(command);
        });
    }

    pub fn handle(&mut self, command: Command) {
        match command {
            Command::Connected { client, sink } => {
                self.clients.insert(client, Client::new(sink));
            }
            Command::Hello { client, request } => {
                let Some(c) = self.clients.get_mut(&client) else {
                    return;
                };
                if c.greeted {
                    return;
                }
                c.greeted = true;
                self.handle_hello(client, request);
            }
            Command::Input { client, data } => {
                if let Some(key) = self.clients.get(&client).and_then(|c| c.session) {
                    self.input(client, key, &data);
                }
            }
            Command::Resize { client, rows, cols } => {
                if let Some(key) = self.clients.get(&client).and_then(|c| c.session) {
                    self.resize(client, key, rows, cols);
                }
            }
            Command::Closed { client } => self.close_client(client),
            Command::Drain { key, ack } => {
                let result = self.drain(key, DRAIN_ROUNDS);
                let _ = ack.send(result);
            }
            Command::FlushInput { key } => {
                if let Some(s) = self.live_mut(key) {
                    s.input_retry_scheduled = false;
                }
                self.flush_input(key);
            }
            Command::ForgetEnded { key } => self.ended.retain(|s| s.key != key),
            Command::Escalate { key, step } => self.escalate(key, step),
            Command::Shutdown => self.shutdown(),
            Command::WebAttach {
                client,
                id,
                rows,
                cols,
                claim,
            } => self.web_attach(client, &id, rows, cols, claim),
            Command::Fit { client, rows, cols } => {
                if let Some(key) = self.clients.get(&client).and_then(|c| c.session) {
                    self.note_fit(client, key, rows, cols);
                }
            }
            Command::WebSnapshot { reply } => {
                let _ = reply.send(Snapshot {
                    sessions: self.snapshot(),
                    config: self.config.clone(),
                    approvals: self.approvals.pendings(),
                    rules: self.approvals.rules(),
                });
            }
            Command::WebLaunch {
                path,
                mode,
                resume,
                reply,
            } => {
                let _ = reply.send(self.web_launch(&path, mode, resume.as_deref()));
            }
            Command::WebKill { id, reply } => {
                let key = self.session_key(&id);
                if let Some(key) = key {
                    self.terminate(key);
                }
                let _ = reply.send(key.is_some());
            }
            Command::WebSettings { mode, reply } => {
                self.config.default_permission_mode = mode;
                if let Err(e) = self.config.save(&paths::config()) {
                    log(&format!("cannot save {}: {e}", paths::config().display()));
                }
                let _ = reply.send(());
            }
            Command::ApprovalArrived { approval, verdict } => {
                // The tool, not the summary: a command line can carry a
                // secret, and the log is no place for one.
                let what = format!("{} ({})", approval.tool, approval.id);
                if self.approvals.arrive(approval, verdict, SystemTime::now()) {
                    log(&format!("approval pending: {what}"));
                    self.approvals_changed();
                    self.tick_approvals();
                } else {
                    log(&format!("approval answered by a standing rule: {what}"));
                }
            }
            Command::ApprovalClosed { id } => {
                if self.approvals.cancel(&id) {
                    self.approvals_changed();
                }
            }
            Command::Approve { id, allow, reply } => {
                let found = self.approvals.respond(&id, allow);
                if found {
                    log(&format!("approval {id}: {}", if allow { "allowed" } else { "denied" }));
                    self.approvals_changed();
                }
                let _ = reply.send(found);
            }
            Command::AutoApprove {
                session_id,
                rule,
                reply,
            } => {
                let answered = self.approvals.set_rule(&session_id, rule);
                if answered > 0 {
                    log(&format!("{answered} approval(s) answered by a new rule for {session_id}"));
                }
                self.approvals_changed();
                self.tick_approvals();
                let _ = reply.send(());
            }
            Command::Reconcile { stale, live } => {
                let mut changed = false;
                for id in stale {
                    if self.approvals.cancel(&id) {
                        log(&format!("approval {id} dropped: answered in the terminal, or its session is gone"));
                        changed = true;
                    }
                }
                changed |= self.approvals.prune_rules(&live, SystemTime::now());
                if changed {
                    self.approvals_changed();
                }
            }
            Command::ApprovalTick { reply } => {
                let more = self.approvals.needs_ticks();
                self.approvals_ticking = more;
                let _ = reply.send(more);
            }
        }
    }

    // MARK: Approvals

    /// Clients see a new, answered, or dropped request on their next poll.
    fn approvals_changed(&self) {
        self.web.shared.state.invalidate();
    }

    /// While anything is pending, or any rule stands, build the directory
    /// every 2 s even with no screen polling it: the build is what notices
    /// a prompt answered in the terminal (and hangs up on its helper) or a
    /// session gone — whose "approve all for this session" must go with it,
    /// or a later `claude --resume` of the same conversation (same session
    /// id) would inherit it.
    fn tick_approvals(&mut self) {
        if self.approvals_ticking {
            return;
        }
        self.approvals_ticking = true;
        let shared = self.web.shared.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let (reply, more) = oneshot::channel();
                if shared.hub.send(Command::ApprovalTick { reply }).is_err()
                    || !more.await.unwrap_or(false)
                {
                    return;
                }
                shared.state.get(&shared.hub).await;
            }
        });
    }

    // MARK: Web

    /// The live sessions as the directory shows them.
    fn snapshot(&self) -> Vec<SessionSnapshot> {
        self.sessions
            .iter()
            .map(|s| SessionSnapshot {
                id: s.id.clone(),
                pid: s.pid,
                cwd: s.cwd.clone(),
                started_at: s.started_at,
                viewers: s.attachments.len(),
                permission_mode: s.permission_mode.clone(),
            })
            .collect()
    }

    /// A browser terminal joins session `id` at its window's size; one
    /// that ended moments ago still gets its output and exit code, and an
    /// unknown id is told the session is gone.
    fn web_attach(&mut self, client: ClientId, id: &str, rows: u16, cols: u16, claim: bool) {
        let key = self.session_or_recently_ended(id);
        let Some(c) = self.clients.get_mut(&client) else {
            return;
        };
        let Some(key) = key else {
            c.sink.send(Event::Gone);
            return;
        };
        c.rows = rows;
        c.cols = cols;
        c.session = Some(key);
        self.attach(client, key, claim);
    }

    /// `POST /api/launch`, checked here because the target and the default
    /// mode come from the config: a project folder directly under the
    /// root, or the home directory; a mode `claude` knows; a conversation
    /// id that is a UUID.
    fn web_launch(&mut self, path: &str, mode: Option<String>, resume: Option<&str>) -> (u16, Value) {
        let home = crate::config::home_dir().to_string_lossy().into_owned();
        let Some(target) = crate::web::state::launch_target(path, &self.config.root, &home) else {
            return (400, json!({"error": "not a project directory"}));
        };
        let mode = mode.unwrap_or_else(|| self.config.default_permission_mode.clone());
        if !HubConfig::permission_modes().contains(&mode.as_str()) {
            return (400, json!({"error": "unknown permission mode"}));
        }
        if resume.is_some_and(|r| !crate::web::state::is_session_id(r)) {
            return (400, json!({"error": "bad conversation id"}));
        }
        match self.launch_from_web(&target, &mode, resume) {
            Ok(key) => {
                let id = self
                    .sessions
                    .iter()
                    .chain(&self.ended)
                    .find(|s| s.key == key)
                    .map(|s| s.id.clone())
                    .unwrap_or_default();
                (200, json!({"id": id}))
            }
            Err(e) => (500, json!({"error": e})),
        }
    }

    // MARK: Sessions

    fn live_mut(&mut self, key: u64) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.key == key)
    }

    pub fn session_key(&self, id: &str) -> Option<u64> {
        self.sessions.iter().find(|s| s.id == id).map(|s| s.key)
    }

    /// A live session, or one that ended moments ago (attach handles both).
    pub fn session_or_recently_ended(&self, id: &str) -> Option<u64> {
        self.session_key(id)
            .or_else(|| self.ended.iter().find(|s| s.id == id).map(|s| s.key))
    }

    fn new_id(&self) -> String {
        loop {
            let id = format!("{:06x}", random_u32() & 0xff_ffff);
            if !self.sessions.iter().chain(&self.ended).any(|s| s.id == id) {
                return id;
            }
        }
    }

    /// Start `argv` on a new pty. Returns the session's key.
    #[allow(clippy::too_many_arguments)]
    pub fn launch(
        &mut self,
        argv: Vec<OsString>,
        cwd: &str,
        env: Vec<(OsString, OsString)>,
        rows: u16,
        cols: u16,
        origin: &str,
        permission_mode: Option<String>,
    ) -> Result<u64, String> {
        if !Path::new(cwd).is_dir() {
            return Err(format!("not a directory: {cwd}"));
        }
        let spawned = pty::spawn(&self.supervisor, &argv, Path::new(cwd), &env, rows, cols)?;
        let master = match AsyncFd::with_interest(spawned.master, Interest::READABLE) {
            Ok(fd) => Arc::new(fd),
            Err(e) => {
                // SAFETY: our own child, not yet known to anyone else.
                unsafe {
                    libc::kill(-spawned.pid, libc::SIGKILL);
                    libc::waitpid(spawned.pid, std::ptr::null_mut(), 0);
                }
                return Err(format!("cannot watch the pty: {e}"));
            }
        };
        let key = self.next_key;
        self.next_key += 1;
        let id = self.new_id();
        let pump = tokio::spawn(pump(master.clone(), key, self.tx.clone()));
        self.sessions.push(Session {
            key,
            id: id.clone(),
            pid: spawned.pid,
            master: Some(master),
            pump: Some(pump),
            cwd: cwd.to_string(),
            started_at: SystemTime::now(),
            origin: origin.to_string(),
            permission_mode,
            rows,
            cols,
            stream: TerminalStream::new(),
            replay: ReplayBuffer::default(),
            attachments: Vec::new(),
            size_owner: None,
            pending_input: Vec::new(),
            pending_from: 0,
            input_retry_scheduled: false,
            input_retry_ms: INPUT_RETRY_FIRST_MS,
            finished: false,
            exit_code: 0,
        });
        // Covers a child that died before SIGCHLD was being listened for.
        self.reap(key);
        log(&format!(
            "session {id}: pid {} in {cwd} ({origin})",
            spawned.pid
        ));
        Ok(key)
    }

    /// Read what the pty has, at most `rounds` reads.
    fn drain(&mut self, key: u64, rounds: usize) -> Drained {
        let Some(fd) = self
            .live_mut(key)
            .and_then(|s| s.master.as_ref().map(|m| m.get_ref().as_raw_fd()))
        else {
            return Drained::Closed;
        };
        let mut buffer = std::mem::take(&mut self.scratch);
        let mut result = Drained::More;
        for _ in 0..rounds {
            // SAFETY: reading into a buffer we own from a descriptor the
            // session holds open (it is closed only by dropping `master`,
            // which nothing here does until `close_master`).
            let n = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if n > 0 {
                self.output(key, &buffer[..n as usize]);
                continue;
            }
            let e = errno();
            if n < 0 && e == libc::EINTR {
                continue;
            }
            if n < 0 && e == libc::EAGAIN {
                result = Drained::Empty;
                break;
            }
            // EOF or EIO: every slave descriptor is closed.
            self.scratch = buffer;
            self.close_master(key);
            self.reap(key);
            return Drained::Closed;
        }
        self.scratch = buffer;
        result
    }

    fn output(&mut self, key: u64, chunk: &[u8]) {
        let Some(s) = self.sessions.iter_mut().find(|s| s.key == key) else {
            return;
        };
        for piece in s.stream.feed(chunk) {
            match piece {
                Piece::Bytes(data) => s.replay.append(&data, s.stream.modes()),
                Piece::Clear(modes) => s.replay.reset(modes),
            }
        }
        for id in &s.attachments {
            if let Some(client) = self.clients.get(id) {
                client.sink.send(Event::Output(chunk.to_vec()));
            }
        }
    }

    fn close_master(&mut self, key: u64) {
        let Some(s) = self.live_mut(key) else { return };
        if s.master.take().is_none() {
            return;
        }
        s.clear_pending();
        // The pump holds the other reference; ending it closes the fd.
        if let Some(pump) = s.pump.take() {
            pump.abort();
        }
    }

    /// Every session whose process has exited: collect it and end it.
    pub fn reap_all(&mut self) {
        let keys: Vec<u64> = self.sessions.iter().map(|s| s.key).collect();
        for key in keys {
            self.reap(key);
        }
    }

    /// If the session's process has exited, collect it and end the session.
    fn reap(&mut self, key: u64) {
        let Some(s) = self.live_mut(key) else { return };
        if s.finished {
            return;
        }
        let mut status = 0;
        // SAFETY: waitpid on our own child, never -1: other children are
        // not ours to collect.
        if unsafe { libc::waitpid(s.pid, &mut status, libc::WNOHANG) } != s.pid {
            return;
        }
        s.finished = true;
        // Whatever the program wrote on its way out (its terminal cleanup,
        // usually) still has to reach the clients before the exit notice.
        self.drain(key, DRAIN_ROUNDS_ON_EXIT);
        self.close_master(key);

        let Some(index) = self.sessions.iter().position(|s| s.key == key) else {
            return;
        };
        let mut session = self.sessions.remove(index);
        let code = exit_code(status);
        session.exit_code = code;
        log(&format!(
            "session {}: pid {} exited with {code}",
            session.id, session.pid
        ));
        for id in std::mem::take(&mut session.attachments) {
            if let Some(client) = self.clients.get_mut(&id) {
                client.session = None;
                client.sink.send(Event::Exit(code));
            }
        }
        session.size_owner = None;
        self.ended.push(session);
        self.after(ENDED_KEEP, Command::ForgetEnded { key });
    }

    /// End a session the way closing its terminal window would: hang up
    /// the supervisor, which passes it to the program (once — some
    /// programs take a second SIGHUP as "skip the cleanup") and stays until
    /// the program has exited, so `finished` means it is really gone. If
    /// it still isn't after five seconds: SIGTERM, which the supervisor
    /// answers by SIGKILLing its job, then SIGKILL for whatever is left.
    pub fn terminate(&mut self, key: u64) {
        let Some(s) = self.live_mut(key) else { return };
        if s.finished {
            return;
        }
        // SAFETY: the supervisor is unreaped (the session is live), so its
        // pid — and group — can't have been reused.
        unsafe { libc::kill(-s.pid, libc::SIGHUP) };
        self.after(TERMINATE_GRACE, Command::Escalate { key, step: 1 });
    }

    fn escalate(&mut self, key: u64, step: u8) {
        let Some(s) = self.live_mut(key) else { return };
        if s.finished {
            return;
        }
        if step == 1 {
            // SAFETY: as in `terminate`.
            unsafe { libc::kill(-s.pid, libc::SIGTERM) };
            self.after(KILL_GRACE, Command::Escalate { key, step: 2 });
            return;
        }
        // Both groups are in the pty's own session, and the supervisor is
        // unreaped until `finished`, so neither id can have been reused.
        if let Some(job) = s
            .master
            .as_ref()
            .and_then(|m| pty::foreground_group(m.get_ref().as_raw_fd()))
            && job != s.pid
        {
            // SAFETY: see above.
            unsafe { libc::kill(-job, libc::SIGKILL) };
        }
        // SAFETY: see above.
        unsafe { libc::kill(-s.pid, libc::SIGKILL) };
    }

    // MARK: Attachments

    /// `claim: false` joins as a spectator: the session keeps the size it
    /// has (unless nobody owns it), and this client is told what that is.
    pub fn attach(&mut self, client: ClientId, key: u64, claim: bool) {
        if let Some(s) = self.ended.iter().find(|s| s.key == key) {
            // Died before anyone was looking: show what it said, then go.
            if let Some(c) = self.clients.get_mut(&client) {
                c.session = None;
                c.sink.send(Event::Output(s.replay.snapshot()));
                c.sink.send(Event::Exit(s.exit_code));
            }
            return;
        }
        let Some(s) = self.live_mut(key) else { return };
        s.attachments.push(client);
        let unowned = s.size_owner.is_none();
        // Size first, so the program is already redrawing for this window
        // by the time the replay lands.
        if !((claim || unowned) && self.claim_size(client, key)) {
            let Some(s) = self.sessions.iter().find(|s| s.key == key) else {
                return;
            };
            if let Some(c) = self.clients.get(&client) {
                c.sink.send(Event::Size {
                    rows: s.rows,
                    cols: s.cols,
                    owner: s.size_owner == Some(client),
                });
            }
        }
        let Some(s) = self.sessions.iter().find(|s| s.key == key) else {
            return;
        };
        // Plus the front of any sequence a read boundary has split, so the
        // live stream this client now joins continues it.
        let mut replay = s.replay.snapshot();
        replay.extend_from_slice(&s.stream.pending_bytes());
        if let Some(c) = self.clients.get(&client) {
            c.sink.send(Event::Output(replay));
        }
    }

    pub fn detach(&mut self, client: ClientId, key: u64) {
        let Some(s) = self.live_mut(key) else { return };
        let Some(index) = s.attachments.iter().position(|&c| c == client) else {
            return;
        };
        s.attachments.remove(index);
        if s.size_owner == Some(client) {
            s.size_owner = None;
            if let Some(&next) = s.attachments.last() {
                self.claim_size(next, key);
            }
        }
    }

    pub fn input(&mut self, client: ClientId, key: u64, data: &[u8]) {
        let Some(s) = self.live_mut(key) else { return };
        if s.master.is_none() {
            return;
        }
        // Typing here makes this the screen the session is sized for — but
        // only typing, not the terminal's own chatter.
        if s.size_owner != Some(client) && is_user_activity(data) {
            self.claim_size(client, key);
        }
        let Some(s) = self.live_mut(key) else { return };
        if s.pending_len() + data.len() > MAX_PENDING_INPUT {
            return;
        }
        s.pending_input.extend_from_slice(data);
        self.flush_input(key);
    }

    pub fn resize(&mut self, client: ClientId, key: u64, rows: u16, cols: u16) {
        if let Some(c) = self.clients.get_mut(&client) {
            c.rows = rows;
            c.cols = cols;
        }
        // Always answered, so the asker knows where it stands even when
        // the session was already that size.
        if !self.claim_size(client, key)
            && let (Some(s), Some(c)) = (
                self.sessions.iter().find(|s| s.key == key),
                self.clients.get(&client),
            )
        {
            c.sink.send(Event::Size {
                rows: s.rows,
                cols: s.cols,
                owner: true,
            });
        }
    }

    /// A client's window changed while it is only watching: remember the
    /// size for when it next takes over, and resize now only if it already
    /// is the owner.
    pub fn note_fit(&mut self, client: ClientId, key: u64, rows: u16, cols: u16) {
        if let Some(c) = self.clients.get_mut(&client) {
            c.rows = rows;
            c.cols = cols;
        }
        if self
            .live_mut(key)
            .is_some_and(|s| s.size_owner == Some(client))
        {
            self.claim_size(client, key);
        }
    }

    /// Make `client` the one the pty is sized for. Returns whether anything
    /// changed — the size, or who owns it — in which case every client
    /// (this one included) has been told.
    fn claim_size(&mut self, client: ClientId, key: u64) -> bool {
        let Some(&Client { rows, cols, .. }) = self.clients.get(&client) else {
            return false;
        };
        let Some(s) = self.sessions.iter_mut().find(|s| s.key == key) else {
            return false;
        };
        let new_owner = s.size_owner != Some(client);
        s.size_owner = Some(client);
        let mut resized = false;
        if let Some(master) = &s.master
            && (rows, cols) != (s.rows, s.cols)
        {
            s.rows = rows;
            s.cols = cols;
            pty::resize(master.get_ref().as_raw_fd(), rows, cols);
            resized = true;
        }
        if !(resized || new_owner) {
            return false;
        }
        for other in &s.attachments {
            if let Some(c) = self.clients.get(other) {
                c.sink.send(Event::Size {
                    rows: s.rows,
                    cols: s.cols,
                    owner: *other == client,
                });
            }
        }
        true
    }

    /// Write queued keystrokes to the pty. Its input queue is small (about
    /// 1 KB on macOS), so a paste takes many rounds: write what fits, and
    /// come back in a few milliseconds for the rest.
    ///
    /// A timer, not write-readiness: kqueue's EVFILT_WRITE on a pty master
    /// does not reliably fire when the slave drains its input (measured in
    /// the Swift hub: an 8 KB paste stalled after the first kilobyte most
    /// of the time). Polling a queue that is only ever non-empty mid-paste
    /// costs nothing the rest of the time.
    fn flush_input(&mut self, key: u64) {
        let mut schedule = None;
        {
            let Some(s) = self.live_mut(key) else { return };
            let Some(fd) = s.master.as_ref().map(|m| m.get_ref().as_raw_fd()) else {
                return;
            };
            while s.pending_len() > 0 {
                let pending = &s.pending_input[s.pending_from..];
                // SAFETY: writing from a buffer we own to an open descriptor.
                let n = unsafe { libc::write(fd, pending.as_ptr().cast(), pending.len()) };
                if n > 0 {
                    s.pending_from += n as usize;
                    s.input_retry_ms = INPUT_RETRY_FIRST_MS;
                    continue;
                }
                let e = errno();
                if n < 0 && e == libc::EINTR {
                    continue;
                }
                if n < 0 && e == libc::EAGAIN {
                    if !s.input_retry_scheduled {
                        s.input_retry_scheduled = true;
                        let delay = s.input_retry_ms;
                        s.input_retry_ms = (delay * 2).min(INPUT_RETRY_MAX_MS);
                        schedule = Some(delay);
                    }
                    break;
                }
                s.clear_pending();
            }
            if s.pending_len() == 0 {
                s.clear_pending();
            } else if s.pending_from > (1 << 16) && s.pending_from * 2 > s.pending_input.len() {
                s.pending_input.drain(..s.pending_from);
                s.pending_from = 0;
            }
        }
        if let Some(ms) = schedule {
            self.after(Duration::from_millis(ms), Command::FlushInput { key });
        }
    }

    // MARK: Clients

    fn close_client(&mut self, client: ClientId) {
        let Some(c) = self.clients.remove(&client) else {
            return;
        };
        c.sink.abort();
        if let Some(key) = c.session {
            // Detach needs the client's size gone already: it isn't a
            // candidate for the next owner.
            self.detach(client, key);
        }
    }

    fn send(&self, client: ClientId, event: Event) {
        if let Some(c) = self.clients.get(&client) {
            c.sink.send(event);
        }
    }

    fn fail(&self, client: ClientId, message: String) {
        self.send(client, Event::Error(message));
    }

    fn reply(&self, client: ClientId, value: Value) {
        self.send(client, Event::Reply(value, None));
    }

    /// Attach a terminal client to a session and tell it so.
    fn begin(&mut self, client: ClientId, key: u64, rows: u16, cols: u16) {
        let Some(c) = self.clients.get_mut(&client) else {
            return;
        };
        c.rows = rows;
        c.cols = cols;
        c.session = Some(key);
        let Some(s) = self
            .sessions
            .iter()
            .chain(&self.ended)
            .find(|s| s.key == key)
        else {
            return;
        };
        c.sink.send(Event::Attached {
            id: s.id.clone(),
            pid: s.pid,
        });
        self.attach(client, key, true);
    }

    // MARK: Launch recipes

    /// What `claudeship [args]` runs: the program, straight from the
    /// client's own environment and directory, as if typed there.
    pub fn launch_from_terminal(
        &mut self,
        cwd: &str,
        args: Vec<String>,
        env: Vec<(OsString, OsString)>,
        rows: u16,
        cols: u16,
    ) -> Result<u64, String> {
        let mut argv = vec![paths::program()];
        argv.extend(args.into_iter().map(OsString::from));
        self.launch(argv, cwd, env, rows, cols, "terminal", None)
    }

    /// What the web app runs. There is no terminal to inherit from, so the
    /// program starts under the user's login shell (`-l -i`, which is where
    /// PATH and version managers get set up) with a small clean
    /// environment rather than whatever the hub happened to be started in.
    pub fn launch_from_web(
        &mut self,
        cwd: &str,
        permission_mode: &str,
        resume: Option<&str>,
    ) -> Result<u64, String> {
        let mut args: Vec<OsString> = vec!["--permission-mode".into(), permission_mode.into()];
        if let Some(resume) = resume {
            args.push("--resume".into());
            args.push(resume.into());
        }
        let shell = login_shell();
        let env = web_environment(&shell, std::env::vars_os())
            .into_iter()
            .map(|(k, v)| (OsString::from(k), v))
            .collect();
        let argv = login_shell_argv(&shell, paths::program(), args);
        self.launch(
            argv,
            cwd,
            env,
            36,
            120,
            "web",
            Some(permission_mode.to_string()),
        )
    }

    // MARK: Local clients

    pub fn describe(&self) -> Value {
        let sessions: Vec<Value> = self
            .sessions
            .iter()
            .map(|s| {
                json!({
                    "id": s.id, "pid": s.pid, "cwd": s.cwd,
                    "startedAt": seconds_since_epoch(s.started_at),
                    "clients": s.attachments.len(), "origin": s.origin,
                    "rows": s.rows, "cols": s.cols,
                })
            })
            .collect();
        json!({
            "pid": std::process::id(),
            "protocol": PROTOCOL,
            "startedAt": seconds_since_epoch(self.started_at),
            "port": self.config.port,
            "webListening": self.web.listening(),
            "root": self.config.root,
            "swarmId": self.web.shared.swarm.book.id(),
            "peerRequests": self.web.shared.swarm.client.sent(),
            "jobs": self.web.shared.jobs.enabled,
            "sessions": sessions,
        })
    }

    /// The first frame from a terminal client says what it wants.
    fn handle_hello(&mut self, client: ClientId, request: Map<String, Value>) {
        let op = request.get("op").and_then(Value::as_str).unwrap_or("");
        let int = |key: &str| request.get(key).and_then(Value::as_i64).unwrap_or(0);
        let size = clamp_size(int("rows"), int("cols"));
        let string = |key: &str| {
            request
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        // Managing the hub works across builds; sharing a terminal doesn't.
        if op == "launch" || op == "attach" {
            let theirs = request.get("protocol").and_then(Value::as_i64);
            if theirs != Some(i64::from(PROTOCOL)) {
                let message = if theirs.unwrap_or(0) < i64::from(PROTOCOL) {
                    "this claudeship is an older build than the running hub. Run the installed one \
                     (a new terminal, or reinstall)."
                } else {
                    "the running hub is an older build than this command. Restart it when its sessions \
                     can end: claudeship hub stop, then try again."
                };
                self.fail(client, message.into());
                return;
            }
        }
        match op {
            "launch" => {
                let (Some(cwd), Some((rows, cols))) =
                    (request.get("cwd").and_then(Value::as_str), size)
                else {
                    self.fail(client, "launch needs a directory and a terminal size".into());
                    return;
                };
                let args: Vec<String> = request
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let env: Vec<(OsString, OsString)> = request
                    .get("env")
                    .and_then(Value::as_object)
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| Some((k.into(), v.as_str()?.into())))
                            .collect()
                    })
                    .unwrap_or_default();
                match self.launch_from_terminal(cwd, args, env, rows, cols) {
                    Ok(key) => self.begin(client, key, rows, cols),
                    Err(e) => self.fail(client, e),
                }
            }
            "attach" => {
                let Some((rows, cols)) = size else {
                    self.fail(client, "attach needs a terminal size".into());
                    return;
                };
                let id = string("id");
                match self.session_or_recently_ended(&id) {
                    Some(key) => self.begin(client, key, rows, cols),
                    None => self.fail(client, format!("no such session: {id}")),
                }
            }
            "status" => self.reply(client, self.describe()),
            // Only this user can reach the socket; the secret goes no further.
            "link" => self.reply(
                client,
                json!({"port": self.config.port, "token": self.web.token().unwrap_or_default()}),
            ),
            // Every browser, phone, and peer pairs again: a new pairing
            // secret, and this hub leaves its swarm (a new swarm secret,
            // no peers) without telling the old peers the new one.
            "unlink" => {
                let ok = self.web.rotate_token();
                let left = self.web.shared.swarm.book.leave();
                self.reply(client, json!({"ok": ok && left}));
            }
            "peers" => self.reply(client, self.web.shared.swarm.book.view()),
            "unpair" => match self.web.shared.swarm.book.unpair(&string("target")) {
                Ok(record) => self.reply(client, json!({"ok": true, "record": record.to_value()})),
                Err(e) => self.fail(client, e),
            },
            "kill" => {
                let id = string("id");
                match self.session_key(&id) {
                    Some(key) => {
                        self.terminate(key);
                        self.reply(client, json!({"ok": true}));
                    }
                    None => self.fail(client, format!("no such session: {id}")),
                }
            }
            "stop" => {
                let force = request.get("force").and_then(Value::as_bool) == Some(true);
                if !self.sessions.is_empty() && !force {
                    self.fail(
                        client,
                        format!(
                            "{} session(s) still running — they end with the hub. Use --force to stop anyway.",
                            self.sessions.len()
                        ),
                    );
                    return;
                }
                let (done, replied) = oneshot::channel();
                self.send(client, Event::Reply(json!({"ok": true}), Some(done)));
                let tx = self.tx.clone();
                // After the reply is out (or the client gone, either way).
                tokio::spawn(async move {
                    let _ = replied.await;
                    let _ = tx.send(Command::Shutdown);
                });
            }
            _ => self.fail(client, format!("unknown request: {op}")),
        }
    }

    /// Hang up every session and exit.
    pub fn shutdown(&mut self) -> ! {
        log(&format!(
            "hub shutting down with {} session(s)",
            self.sessions.len()
        ));
        for s in &self.sessions {
            // SAFETY: unreaped supervisors; see `terminate`.
            unsafe { libc::kill(-s.pid, libc::SIGHUP) };
        }
        self.web.shared.jobs.kill_all();
        let _ = std::fs::remove_file(paths::socket());
        let _ = std::fs::remove_file(paths::approvals_socket());
        std::process::exit(0);
    }
}

/// Run the hub until it is stopped.
pub async fn run(mut hub: Hub, mut rx: mpsc::UnboundedReceiver<Command>) -> ! {
    let mut children = signal(SignalKind::child()).expect("SIGCHLD handler");
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT handler");
    loop {
        tokio::select! {
            command = rx.recv() => match command {
                Some(command) => hub.handle(command),
                None => hub.shutdown(),
            },
            _ = children.recv() => hub.reap_all(),
            _ = terminate.recv() => hub.shutdown(),
            _ = interrupt.recv() => hub.shutdown(),
        }
    }
}

/// `program args…` run by a login, interactive shell. The words travel as
/// arguments, never inside the script text: after `-c <script>`, the next
/// word is $0 in POSIX shells and the first of $argv in fish — the program
/// name either way.
pub fn login_shell_argv(shell: &str, program: OsString, args: Vec<OsString>) -> Vec<OsString> {
    let exec = if shell.ends_with("/fish") {
        "exec $argv"
    } else {
        "exec \"$0\" \"$@\""
    };
    let mut argv: Vec<OsString> = vec![
        shell.into(),
        "-l".into(),
        "-i".into(),
        "-c".into(),
        exec.into(),
        program,
    ];
    argv.extend(args);
    argv
}

#[cfg(target_os = "macos")]
const FALLBACK_SHELL: &str = "/bin/zsh";
#[cfg(not(target_os = "macos"))]
const FALLBACK_SHELL: &str = "/bin/bash";

/// The user's login shell from the password database, if it is one we
/// know how to drive; else the platform's default.
pub fn login_shell() -> String {
    const KNOWN: [&str; 6] = ["zsh", "bash", "fish", "sh", "dash", "ksh"];
    // SAFETY: getpwuid's result is copied out at once.
    let shell = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_shell.is_null() {
            None
        } else {
            Some(
                std::ffi::CStr::from_ptr((*pw).pw_shell)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    };
    if let Some(path) = shell {
        let name = Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let executable = std::ffi::CString::new(path.clone())
            // SAFETY: access with a valid C string.
            .map(|c| unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0)
            .unwrap_or(false);
        if KNOWN.contains(&name) && executable {
            return path;
        }
    }
    FALLBACK_SHELL.into()
}

/// The small, clean environment a web launch gets.
pub fn web_environment(
    shell: &str,
    base: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(String, OsString)> {
    const KEEP: [&str; 10] = [
        "HOME",
        "USER",
        "LOGNAME",
        "PATH",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TMPDIR",
        "SSH_AUTH_SOCK",
        "__CF_USER_TEXT_ENCODING",
    ];
    let mut env: std::collections::BTreeMap<String, OsString> = base
        .into_iter()
        .filter_map(|(k, v)| {
            let k = k.into_string().ok()?;
            KEEP.contains(&k.as_str()).then_some((k, v))
        })
        .collect();
    env.insert("SHELL".into(), shell.into());
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    env.entry("LANG".into())
        .or_insert_with(|| "en_US.UTF-8".into());
    env.entry("PATH".into())
        .or_insert_with(|| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into());
    env.into_iter().collect()
}

/// Not secret (session ids are names, not credentials): /dev/urandom, or
/// the clock if that fails.
pub fn random_u32() -> u32 {
    use std::io::Read;
    let mut bytes = [0u8; 4];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok()
    {
        return u32::from_ne_bytes(bytes);
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos ^ std::process::id().rotate_left(16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_clamp() {
        assert_eq!(clamp_size(24, 80), Some((24, 80)));
        assert_eq!(clamp_size(2, 10), Some((2, 10)));
        assert_eq!(clamp_size(1000, 2000), Some((1000, 2000)));
        assert_eq!(clamp_size(1, 80), None);
        assert_eq!(clamp_size(24, 9), None);
        assert_eq!(clamp_size(1001, 80), None);
        assert_eq!(clamp_size(24, 2001), None);
        assert_eq!(clamp_size(0, 0), None);
    }

    #[test]
    fn login_shell_recipe() {
        let argv = login_shell_argv(
            "/bin/zsh",
            "claude".into(),
            vec!["--permission-mode".into(), "auto".into()],
        );
        assert_eq!(
            argv,
            [
                "/bin/zsh",
                "-l",
                "-i",
                "-c",
                "exec \"$0\" \"$@\"",
                "claude",
                "--permission-mode",
                "auto"
            ]
            .map(OsString::from)
        );
        let fish = login_shell_argv("/opt/homebrew/bin/fish", "claude".into(), vec![]);
        assert_eq!(fish[4], "exec $argv");
        assert_eq!(fish[5], "claude");
        assert!(login_shell().starts_with('/'));
    }

    #[test]
    fn web_environment_is_whitelisted() {
        let base = [
            ("HOME", "/h"),
            ("SECRET", "x"),
            ("PATH", "/p"),
            ("TERM", "dumb"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env: HashMap<String, OsString> =
            web_environment("/bin/zsh", base).into_iter().collect();
        assert_eq!(env["HOME"], "/h");
        assert_eq!(env["PATH"], "/p");
        assert!(!env.contains_key("SECRET"));
        assert_eq!(env["TERM"], "xterm-256color");
        assert_eq!(env["COLORTERM"], "truecolor");
        assert_eq!(env["SHELL"], "/bin/zsh");
        assert_eq!(env["LANG"], "en_US.UTF-8");
        let bare: HashMap<String, OsString> =
            web_environment("/bin/sh", Vec::<(OsString, OsString)>::new())
                .into_iter()
                .collect();
        assert_eq!(bare["PATH"], "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin");
    }

    #[test]
    fn timestamps() {
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            iso8601(UNIX_EPOCH + Duration::from_secs(1_791_376_496)),
            "2026-10-07T12:34:56Z"
        );
        assert_eq!(
            iso8601(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
    }
}
