//! Jobs (plan phase 11): a command run on pipes, not a pty — a headless
//! `claude -p` another machine's Claude starts, waits for, reads, and
//! resumes later. See docs/jobs.md.
//!
//! A job is spawned under the web launch's recipe (the user's login shell,
//! `-l -i -c 'exec "$0" "$@"'`, the whitelisted environment), in a process
//! group of its own, with stdin `/dev/null` and stdout/stderr on pipes into
//! bounded buffers (4 MB / 256 KB, oldest bytes dropped, `truncated`). A
//! Claude job's stdout is stream-json: its `system` and `result` events
//! carry the conversation's `session_id`, which the next job `--resume`s.
//!
//! Every job has a wall clock (`maxSeconds`): on expiry its group gets
//! SIGTERM, then SIGKILL 10 s later, and it ends with exit code 124 and
//! `timedOut`, the output so far kept. Finished jobs are kept an hour (or
//! until read with `consume`), at most 64 jobs per hub (the oldest finished
//! one evicted for a new one). `hub stop` kills every running job's group.
//!
//! State lives behind one mutex here, not on the hub task: nothing about a
//! job touches a session, and the web handlers long-poll on it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::watch;

use crate::hub::{log, login_shell, login_shell_argv, web_environment};

pub const STDOUT_CAP: usize = 4 << 20;
pub const STDERR_CAP: usize = 256 << 10;
pub const MAX_JOBS: usize = 64;
pub const DEFAULT_MAX_SECONDS: u64 = 1800;
/// What a request may ask for without `config.jobsMaxSeconds`.
pub const REQUEST_CEILING: u64 = 14_400;
/// SIGTERM, then SIGKILL this much later (the cap and a kill alike).
pub const KILL_GRACE: Duration = Duration::from_secs(10);
/// The longest a `wait` holds a request.
pub const MAX_WAIT: u64 = 60;
/// Exit code of a job the wall clock ended (as `timeout(1)`).
pub const TIMED_OUT: i32 = 124;
/// How long a finished job is kept.
const KEEP: Duration = Duration::from_secs(3600);
/// Test knob: a shorter `KEEP`, in seconds.
pub const KEEP_ENV: &str = "CLAUDESHIP_JOBS_KEEP_SECONDS";
/// Output read after the program exits, for this long at most (a
/// background process it left behind may hold the pipe open forever).
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);

/// An error for the HTTP layer: status and JSON body.
pub type Refusal = (u16, Value);

fn refuse(status: u16, error: impl Into<String>) -> Refusal {
    (status, json!({"error": error.into()}))
}

pub fn ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// MARK: Bounded output

/// A byte stream with only its newest `cap` bytes kept. Offsets are
/// absolute (bytes since the job started), so a reader's `since` stays
/// meaningful after the front is dropped.
#[derive(Debug)]
pub struct Bounded {
    data: VecDeque<u8>,
    dropped: u64,
    cap: usize,
}

impl Bounded {
    pub fn new(cap: usize) -> Bounded {
        Bounded { data: VecDeque::new(), dropped: 0, cap }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= self.cap {
            self.dropped += (self.data.len() + chunk.len() - self.cap) as u64;
            self.data.clear();
            self.data.extend(&chunk[chunk.len() - self.cap..]);
            return;
        }
        let over = (self.data.len() + chunk.len()).saturating_sub(self.cap);
        self.data.drain(..over);
        self.dropped += over as u64;
        self.data.extend(chunk);
    }

    /// Every byte ever written.
    pub fn total(&self) -> u64 {
        self.dropped + self.data.len() as u64
    }

    pub fn truncated(&self) -> bool {
        self.dropped > 0
    }

    /// The text from offset `since` (or the oldest byte kept, if that is
    /// later): `(text, start, next)`, where `next` is the `since` for the
    /// following read. Never splits a character: a partial one at the
    /// start (the front was dropped mid-character) is skipped, and one at
    /// the end is left for the next read unless `complete` (nothing more
    /// will come). Bytes that aren't UTF-8 read as U+FFFD.
    pub fn read_from(&self, since: u64, complete: bool) -> (String, u64, u64) {
        let start = since.clamp(self.dropped, self.total());
        let bytes: Vec<u8> = self.data.range((start - self.dropped) as usize..).copied().collect();
        let skip = bytes.iter().take(3).take_while(|&&b| b & 0xC0 == 0x80).count();
        let skip = if start > 0 { skip } else { 0 };
        let mut end = bytes.len();
        if !complete {
            end -= incomplete_tail(&bytes[skip..]);
        }
        let end = end.max(skip);
        let text = String::from_utf8_lossy(&bytes[skip..end]).into_owned();
        (text, start + skip as u64, start + end as u64)
    }

    /// Everything kept, as text.
    pub fn all_text(&self) -> String {
        let bytes: Vec<u8> = self.data.iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// How many bytes at the end of `bytes` are the start of a character not
/// yet complete.
fn incomplete_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let b = bytes[bytes.len() - back];
        if b & 0xC0 == 0x80 {
            continue;
        }
        let need = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return 0,
        };
        return if need > back { back } else { 0 };
    }
    0
}

// MARK: stream-json

/// What one stream-json line says that a job keeps: the conversation id
/// (`system` and `result` events) and the final text (`result`).
pub fn parse_event(line: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(Value::Object(event)) = serde_json::from_slice::<Value>(line) else {
        return (None, None);
    };
    let kind = event.get("type").and_then(Value::as_str);
    if !matches!(kind, Some("system" | "result")) {
        return (None, None);
    }
    let session = event
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| crate::web::state::is_session_id(s))
        .map(str::to_string);
    let result = (kind == Some("result"))
        .then(|| event.get("result").and_then(Value::as_str).map(str::to_string))
        .flatten();
    (session, result)
}

/// Whether `argv[0]` is Claude (its stdout is then read as stream-json).
pub fn is_claude(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with("claude"))
}

// MARK: Requests

/// A validated `POST /api/jobs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub cwd: String,
    /// What runs (the program and its arguments, before the login shell).
    pub argv: Vec<String>,
    pub permission_mode: Option<String>,
    pub max_seconds: u64,
    pub env: Vec<(String, String)>,
    /// Read stdout as stream-json.
    pub claude: bool,
}

/// Where a job may run: the home directory, the root, or any directory
/// under the root (subfolders, worktrees). Symlinks resolved on both sides,
/// so a link under the root can't point out of it. The canonical path.
pub fn job_cwd(requested: &str, root: &str, home: &str) -> Option<String> {
    let expanded = if requested == "~" {
        home.to_string()
    } else if let Some(rest) = requested.strip_prefix("~/") {
        format!("{}/{rest}", home.trim_end_matches('/'))
    } else {
        requested.to_string()
    };
    if !expanded.starts_with('/') {
        return None;
    }
    let path = std::fs::canonicalize(&expanded).ok()?;
    if !path.is_dir() {
        return None;
    }
    let root = std::fs::canonicalize(root).ok();
    let home = std::fs::canonicalize(home).ok();
    let allowed = home.as_ref().is_some_and(|h| *h == path)
        || root.as_ref().is_some_and(|r| path.starts_with(r));
    let path = path.to_str()?.to_string();
    allowed.then_some(path)
}

/// Variables a request may set: well-formed names, but none that changes
/// how the login shell or the loader starts.
pub fn env_name_allowed(name: &str) -> bool {
    const REFUSED: [&str; 7] = ["HOME", "SHELL", "USER", "LOGNAME", "ENV", "BASH_ENV", "ZDOTDIR"];
    let mut chars = name.chars();
    let well_formed = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    well_formed
        && !REFUSED.contains(&name)
        && !name.starts_with("DYLD_")
        && !name.starts_with("LD_")
}

/// Checks a request body. `program` is what the convenience form runs
/// (`claude`, or `CLAUDESHIP_CMD`).
pub fn parse_spec(
    body: &Map<String, Value>,
    root: &str,
    home: &str,
    ceiling: u64,
    program: &str,
) -> Result<Spec, Refusal> {
    let requested = body.get("cwd").and_then(Value::as_str).filter(|c| !c.is_empty()).unwrap_or(home);
    let Some(cwd) = job_cwd(requested, root, home) else {
        return Err(refuse(400, format!("cwd must be a directory under {root} or the home directory")));
    };
    let mode = match body.get("permissionMode") {
        None | Some(Value::Null) => None,
        Some(Value::String(m)) if crate::config::PERMISSION_MODES.contains(&m.as_str()) => Some(m.clone()),
        Some(_) => return Err(refuse(400, "unknown permission mode")),
    };
    let max_seconds = match body.get("maxSeconds") {
        None | Some(Value::Null) => DEFAULT_MAX_SECONDS.min(ceiling),
        Some(v) => match v.as_u64() {
            Some(n) if (1..=ceiling).contains(&n) => n,
            _ => return Err(refuse(400, format!("maxSeconds must be 1 to {ceiling}"))),
        },
    };
    let mut env = Vec::new();
    match body.get("env") {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            for (k, v) in map {
                let (true, Some(v)) = (env_name_allowed(k), v.as_str()) else {
                    return Err(refuse(400, format!("env: {k} can't be set")));
                };
                if v.contains('\0') {
                    return Err(refuse(400, format!("env: {k} can't be set")));
                }
                env.push((k.clone(), v.to_string()));
            }
        }
        Some(_) => return Err(refuse(400, "env must be an object of strings")),
    }
    let prompt = body.get("prompt").and_then(Value::as_str);
    let resume = match body.get("resume") {
        None | Some(Value::Null) => None,
        Some(Value::String(r)) if crate::web::state::is_session_id(r) => Some(r.clone()),
        Some(_) => return Err(refuse(400, "bad conversation id")),
    };
    let (argv, claude, permission_mode) = match (body.get("argv"), prompt) {
        (Some(Value::Array(_)), Some(_)) => return Err(refuse(400, "a job is argv or prompt, not both")),
        (Some(Value::Array(list)), None) => {
            if resume.is_some() {
                return Err(refuse(400, "resume goes with prompt, not argv"));
            }
            let argv: Option<Vec<String>> = list
                .iter()
                .map(|v| v.as_str().filter(|s| !s.contains('\0')).map(str::to_string))
                .collect();
            let Some(argv) = argv.filter(|a| a.first().is_some_and(|p| !p.is_empty())) else {
                return Err(refuse(400, "argv must be a non-empty list of strings"));
            };
            let claude = is_claude(&argv[0]);
            (argv, claude, mode)
        }
        (None | Some(Value::Null), Some(prompt)) => {
            let mode = mode.unwrap_or_else(|| "auto".into());
            // stream-json under -p needs --verbose (Claude Code refuses it
            // otherwise).
            let mut argv: Vec<String> = [program, "-p", prompt, "--verbose", "--output-format", "stream-json"]
                .map(str::to_string)
                .to_vec();
            argv.extend(["--permission-mode".to_string(), mode.clone()]);
            if let Some(resume) = resume {
                argv.extend(["--resume".to_string(), resume]);
            }
            (argv, true, Some(mode))
        }
        _ => return Err(refuse(400, "a job needs argv (a list of strings) or a prompt")),
    };
    Ok(Spec { cwd, argv, permission_mode, max_seconds, env, claude })
}

// MARK: Jobs

#[derive(Debug)]
struct Job {
    id: String,
    cwd: String,
    argv: Vec<String>,
    permission_mode: Option<String>,
    started_at: SystemTime,
    finished_at: Option<SystemTime>,
    exit_code: Option<i32>,
    pid: i32,
    max_seconds: u64,
    stdout: Bounded,
    stderr: Bounded,
    claude: bool,
    /// The stdout line being assembled (stream-json), and whether the
    /// current one is too long to keep (skipped to its newline).
    line: Vec<u8>,
    skipping: bool,
    claude_session_id: Option<String>,
    result: Option<String>,
    timed_out: bool,
    /// The program has been reaped (its group may be gone).
    exited: bool,
    /// A kill or the wall clock is ending it: when the program goes, what
    /// is left of its group (a child that ignored SIGTERM) goes with it.
    ending: bool,
}

impl Job {
    fn running(&self) -> bool {
        self.finished_at.is_none()
    }

    fn take_stdout(&mut self, chunk: &[u8]) {
        self.stdout.push(chunk);
        if !self.claude {
            return;
        }
        for piece in chunk.split_inclusive(|&b| b == b'\n') {
            let ends = piece.last() == Some(&b'\n');
            let body = if ends { &piece[..piece.len() - 1] } else { piece };
            if !self.skipping {
                if self.line.len() + body.len() > STDOUT_CAP {
                    self.line.clear();
                    self.skipping = true;
                } else {
                    self.line.extend_from_slice(body);
                }
            }
            if ends {
                if !self.skipping {
                    self.event();
                }
                self.line.clear();
                self.skipping = false;
            }
        }
    }

    fn event(&mut self) {
        let (session, result) = parse_event(&self.line);
        if session.is_some() {
            self.claude_session_id = session;
        }
        if result.is_some() {
            self.result = result;
        }
    }

    fn summary(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("id".into(), self.id.clone().into());
        out.insert("cwd".into(), self.cwd.clone().into());
        out.insert("argv".into(), self.argv.clone().into());
        out.insert("permissionMode".into(), self.permission_mode.clone().into());
        out.insert("running".into(), self.running().into());
        out.insert("exitCode".into(), self.exit_code.into());
        out.insert("pid".into(), self.pid.into());
        out.insert("startedAt".into(), ms(self.started_at).into());
        out.insert("finishedAt".into(), self.finished_at.map(ms).into());
        out.insert("maxSeconds".into(), self.max_seconds.into());
        out.insert("timedOut".into(), self.timed_out.into());
        out.insert("claudeSessionId".into(), self.claude_session_id.clone().into());
        out.insert("stdoutBytes".into(), self.stdout.total().into());
        out.insert("truncated".into(), self.stdout.truncated().into());
        out
    }

    fn detail(&self, since: u64) -> Map<String, Value> {
        let mut out = self.summary();
        let (stdout, start, next) = self.stdout.read_from(since, !self.running());
        out.insert("stdout".into(), stdout.into());
        out.insert("since".into(), start.into());
        out.insert("next".into(), next.into());
        out.insert("stderr".into(), self.stderr.all_text().into());
        out.insert("stderrTruncated".into(), self.stderr.truncated().into());
        out.insert("result".into(), self.result.clone().into());
        out
    }
}

pub struct Jobs {
    /// `config.jobs`: off, every jobs route is refused.
    pub enabled: bool,
    /// The largest `maxSeconds` a request may ask for.
    pub ceiling: u64,
    root: String,
    keep: Duration,
    inner: Mutex<Vec<Job>>,
    /// Bumped on every output chunk and every finish: what `wait` watches.
    changed: watch::Sender<u64>,
}

impl Jobs {
    pub fn new(config: &crate::config::HubConfig) -> Arc<Jobs> {
        let keep = std::env::var(KEEP_ENV)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(KEEP);
        Arc::new(Jobs {
            enabled: config.jobs,
            ceiling: config.jobs_max_seconds.unwrap_or(REQUEST_CEILING).max(1),
            root: config.root.clone(),
            keep,
            inner: Mutex::new(Vec::new()),
            changed: watch::channel(0).0,
        })
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Job>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Finished jobs past their hour go.
    fn prune(&self, jobs: &mut Vec<Job>, now: SystemTime) {
        jobs.retain(|j| {
            j.finished_at
                .is_none_or(|f| now.duration_since(f).unwrap_or_default() < self.keep)
        });
    }

    /// Starts a job: its id.
    pub fn start(self: &Arc<Self>, spec: Spec) -> Result<String, Refusal> {
        let mut jobs = self.lock();
        self.prune(&mut jobs, SystemTime::now());
        make_room(&mut jobs)?;
        let id = loop {
            let id = format!("{:06x}", crate::hub::random_u32() & 0xff_ffff);
            if !jobs.iter().any(|j| j.id == id) {
                break id;
            }
        };
        let mut child = spawn(&spec).map_err(|e| refuse(500, e))?;
        let pid = child.id().map_or(0, |p| p as i32);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        jobs.push(Job {
            id: id.clone(),
            cwd: spec.cwd.clone(),
            argv: spec.argv.clone(),
            permission_mode: spec.permission_mode.clone(),
            started_at: SystemTime::now(),
            finished_at: None,
            exit_code: None,
            pid,
            max_seconds: spec.max_seconds,
            stdout: Bounded::new(STDOUT_CAP),
            stderr: Bounded::new(STDERR_CAP),
            claude: spec.claude,
            line: Vec::new(),
            skipping: false,
            claude_session_id: None,
            result: None,
            timed_out: false,
            exited: false,
            ending: false,
        });
        drop(jobs);
        log(&format!("job {id}: pid {pid} in {} ({})", spec.cwd, spec.argv.first().map_or("", |s| s.as_str())));
        let out = stdout.map(|s| tokio::spawn(self.clone().pump(id.clone(), s, false)));
        let err = stderr.map(|s| tokio::spawn(self.clone().pump(id.clone(), s, true)));
        tokio::spawn(self.clone().supervise(id.clone(), child, pid, spec.max_seconds, [out, err]));
        Ok(id)
    }

    async fn pump(self: Arc<Self>, id: String, mut pipe: impl tokio::io::AsyncRead + Unpin, stderr: bool) {
        let mut buffer = vec![0u8; 65_536];
        loop {
            let n = match pipe.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            {
                let mut jobs = self.lock();
                let Some(job) = jobs.iter_mut().find(|j| j.id == id) else {
                    // Evicted or consumed while running can't happen (only
                    // finished jobs go), but don't hold the pipe for nothing.
                    return;
                };
                if stderr {
                    job.stderr.push(&buffer[..n]);
                } else {
                    job.take_stdout(&buffer[..n]);
                }
            }
            self.bump();
        }
    }

    /// Waits for the job, enforcing its wall clock; then records the end.
    async fn supervise(
        self: Arc<Self>,
        id: String,
        mut child: tokio::process::Child,
        pid: i32,
        max_seconds: u64,
        readers: [Option<tokio::task::JoinHandle<()>>; 2],
    ) {
        let status = tokio::select! {
            status = child.wait() => status,
            _ = tokio::time::sleep(Duration::from_secs(max_seconds)) => {
                log(&format!("job {id}: past its {max_seconds} s; ending it"));
                if let Some(job) = self.lock().iter_mut().find(|j| j.id == id) {
                    job.timed_out = true;
                }
                tokio::spawn(self.clone().end(id.clone(), pid));
                child.wait().await
            }
        };
        // From here the group may be gone and its id free: `end` stops.
        // A job being ended loses the rest of its group now, while its id
        // is still held (a group's id isn't reused while it has members;
        // this is the instant after the reap, not seconds later).
        if let Some(job) = self.lock().iter_mut().find(|j| j.id == id) {
            if job.ending {
                signal_group(pid, libc::SIGKILL);
            }
            job.exited = true;
        }
        for reader in readers.into_iter().flatten() {
            let abort = reader.abort_handle();
            if tokio::time::timeout(DRAIN_AFTER_EXIT, reader).await.is_err() {
                abort.abort();
            }
        }
        let code = status.map(|s| exit_code(&s)).unwrap_or(-1);
        {
            let mut jobs = self.lock();
            if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
                // A partial last line is still an event if it parses.
                if job.claude && !job.line.is_empty() && !job.skipping {
                    job.event();
                }
                job.finished_at = Some(SystemTime::now());
                job.exit_code = Some(if job.timed_out { TIMED_OUT } else { code });
                log(&format!("job {id}: exited {}", job.exit_code.unwrap_or(-1)));
            }
        }
        self.bump();
    }

    /// `POST /api/jobs/<id>/kill`: SIGTERM to the group, SIGKILL 10 s later
    /// if it is still running. Whether there was such a running job (a
    /// finished one is left as it is and reads as found).
    pub fn kill(self: &Arc<Self>, id: &str) -> bool {
        let jobs = self.lock();
        let Some(job) = jobs.iter().find(|j| j.id == id) else {
            return false;
        };
        if !job.running() {
            return true;
        }
        let pid = job.pid;
        drop(jobs);
        tokio::spawn(self.clone().end(id.to_string(), pid));
        true
    }

    /// SIGTERM to the job's group, again every second (one that lands
    /// while the login shell is still starting is ignored, as interactive
    /// shells ignore SIGTERM), then SIGKILL once `KILL_GRACE` is up.
    async fn end(self: Arc<Self>, id: String, pid: i32) {
        let exited = |this: &Self| !this.lock().iter().any(|j| j.id == id && !j.exited);
        if let Some(job) = self.lock().iter_mut().find(|j| j.id == id) {
            job.ending = true;
        }
        let deadline = tokio::time::Instant::now() + KILL_GRACE;
        while tokio::time::Instant::now() < deadline {
            if exited(&self) {
                return;
            }
            signal_group(pid, libc::SIGTERM);
            tokio::time::sleep(Duration::from_secs(1).min(deadline - tokio::time::Instant::now())).await;
        }
        if !exited(&self) {
            signal_group(pid, libc::SIGKILL);
        }
    }

    /// `hub stop`: every running job's group, at once (no one is left to
    /// escalate).
    pub fn kill_all(&self) {
        for job in self.lock().iter().filter(|j| j.running()) {
            signal_group(job.pid, libc::SIGKILL);
        }
    }

    pub fn list(&self) -> Vec<Value> {
        let mut jobs = self.lock();
        self.prune(&mut jobs, SystemTime::now());
        jobs.iter().map(|j| Value::Object(j.summary())).collect()
    }

    /// One job's state with its stdout from `since`, after waiting up to
    /// `wait` for it to finish or write past `since`. `consume` removes a
    /// finished job once read. `None`: no such job.
    pub async fn get(&self, id: &str, since: u64, wait: Duration, consume: bool) -> Option<Map<String, Value>> {
        let mut changed = self.changed.subscribe();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            {
                let mut jobs = self.lock();
                self.prune(&mut jobs, SystemTime::now());
                let index = jobs.iter().position(|j| j.id == id)?;
                let job = &jobs[index];
                let ready = !job.running() || job.stdout.total() > since;
                if ready || tokio::time::Instant::now() >= deadline {
                    let detail = job.detail(since);
                    if consume && !job.running() {
                        jobs.remove(index);
                    }
                    return Some(detail);
                }
            }
            tokio::select! {
                r = changed.changed() => if r.is_err() { return None },
                _ = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    /// The Claude conversation a running job has, for `waitingFor`.
    pub fn conversation(&self, id: &str) -> Option<String> {
        self.lock()
            .iter()
            .find(|j| j.id == id && j.running())
            .and_then(|j| j.claude_session_id.clone())
    }

}

/// Room for one more: at `MAX_JOBS`, the job that finished first goes;
/// with every one still running, the new one is refused.
fn make_room(jobs: &mut Vec<Job>) -> Result<(), Refusal> {
    if jobs.len() < MAX_JOBS {
        return Ok(());
    }
    let oldest = jobs
        .iter()
        .enumerate()
        .filter_map(|(i, j)| j.finished_at.map(|f| (f, i)))
        .min()
        .map(|(_, i)| i);
    match oldest {
        Some(i) => {
            jobs.remove(i);
            Ok(())
        }
        None => Err(refuse(429, format!("{MAX_JOBS} jobs are running on this host"))),
    }
}

/// The shell's convention: the exit status, or 128 + the fatal signal.
fn exit_code(status: &ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|s| 128 + s)).unwrap_or(-1)
}

fn signal_group(pid: i32, signal: i32) {
    if pid > 1 {
        // SAFETY: a process group this hub created (setpgid in the child).
        unsafe { libc::kill(-pid, signal) };
    }
}

/// The job's program under the login shell, on pipes, in a group of its
/// own, with nothing of the hub's but stdio.
fn spawn(spec: &Spec) -> Result<tokio::process::Child, String> {
    let shell = login_shell();
    let mut env: Vec<(String, OsString)> = web_environment(&shell, std::env::vars_os());
    for (k, v) in &spec.env {
        env.retain(|(name, _)| name != k);
        env.push((k.clone(), v.into()));
    }
    let program = OsString::from(&spec.argv[0]);
    let args = spec.argv[1..].iter().map(OsString::from).collect();
    let argv = login_shell_argv(&shell, program, args);
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(env)
        .env("PWD", &spec.cwd)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let limit = crate::pty::descriptor_limit();
    // SAFETY: between fork and exec, async-signal-safe calls only (as in
    // pty.rs): a group of its own, every descriptor above stdio
    // close-on-exec, default signals.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            crate::pty::cloexec_above_stdio(limit);
            crate::pty::reset_signals();
            Ok(())
        });
    }
    command.spawn().map_err(|e| format!("cannot start {}: {e}", spec.argv[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_keeps_the_newest_bytes() {
        let mut b = Bounded::new(10);
        b.push(b"hello ");
        assert_eq!(b.read_from(0, true), ("hello ".into(), 0, 6));
        b.push(b"world!!");
        assert!(b.truncated());
        assert_eq!(b.total(), 13);
        assert_eq!(b.read_from(0, true), ("lo world!!".into(), 3, 13));
        assert_eq!(b.read_from(8, true), ("rld!!".into(), 8, 13));
        assert_eq!(b.read_from(99, true), ("".into(), 13, 13));
        b.push(&[b'x'; 25]);
        assert_eq!(b.total(), 38);
        assert_eq!(b.read_from(0, true).1, 28);
        assert_eq!(b.read_from(0, true).0, "x".repeat(10));
    }

    #[test]
    fn reads_never_split_a_character() {
        let mut b = Bounded::new(64);
        b.push("aé".as_bytes());
        b.push(&"€".as_bytes()[..2]);
        // The euro sign is half there: left for the next read.
        assert_eq!(b.read_from(0, false), ("aé".into(), 0, 3));
        // Starting inside é skips its tail.
        assert_eq!(b.read_from(2, false), ("".into(), 3, 3));
        b.push(&"€".as_bytes()[2..]);
        assert_eq!(b.read_from(3, false), ("€".into(), 3, 6));
        let mut cut = Bounded::new(3);
        cut.push("ab€".as_bytes());
        assert_eq!(cut.read_from(0, true), ("€".into(), 2, 5));
        // The front was dropped mid-character.
        let mut cut = Bounded::new(2);
        cut.push("ab€".as_bytes());
        assert_eq!(cut.read_from(0, true), ("".into(), 5, 5));
        assert_eq!(incomplete_tail("x".as_bytes()), 0);
        assert_eq!(incomplete_tail(&"€".as_bytes()[..1]), 1);
        assert_eq!(incomplete_tail("€".as_bytes()), 0);
    }

    #[test]
    fn stream_json_events() {
        let sid = "93fb531a-9e91-4926-ab89-93ded70cba7e";
        let init = format!(r#"{{"type":"system","subtype":"init","session_id":"{sid}"}}"#);
        assert_eq!(parse_event(init.as_bytes()), (Some(sid.into()), None));
        let done = format!(r#"{{"type":"result","result":"hi","session_id":"{sid}"}}"#);
        assert_eq!(parse_event(done.as_bytes()), (Some(sid.into()), Some("hi".into())));
        let other = format!(r#"{{"type":"assistant","session_id":"{sid}","result":"x"}}"#);
        assert_eq!(parse_event(other.as_bytes()), (None, None));
        assert_eq!(parse_event(br#"{"type":"system","session_id":"nope"}"#), (None, None));
        assert_eq!(parse_event(b"not json"), (None, None));
        assert!(is_claude("claude") && is_claude("/opt/bin/claude"));
        assert!(!is_claude("claudeship") && !is_claude("/bin/sh"));
    }

    fn job(id: &str, finished: Option<SystemTime>) -> Job {
        Job {
            id: id.into(),
            cwd: "/".into(),
            argv: vec![],
            permission_mode: None,
            started_at: SystemTime::now(),
            finished_at: finished,
            exit_code: finished.map(|_| 0),
            pid: 0,
            max_seconds: 1,
            stdout: Bounded::new(STDOUT_CAP),
            stderr: Bounded::new(STDERR_CAP),
            claude: true,
            line: Vec::new(),
            skipping: false,
            claude_session_id: None,
            result: None,
            timed_out: false,
            exited: finished.is_some(),
            ending: false,
        }
    }

    #[test]
    fn stdout_lines_across_chunks() {
        let sid = "93fb531a-9e91-4926-ab89-93ded70cba7e";
        let mut job = job("a", None);
        let text = format!("noise\n{{\"type\":\"result\",\"result\":\"ok\",\"session_id\":\"{sid}\"}}\n");
        let (a, b) = text.as_bytes().split_at(20);
        job.take_stdout(a);
        assert_eq!(job.result, None);
        job.take_stdout(b);
        assert_eq!(job.result.as_deref(), Some("ok"));
        assert_eq!(job.claude_session_id.as_deref(), Some(sid));
    }

    #[test]
    fn where_jobs_may_run() {
        let base = std::env::temp_dir().join(format!("cs-jobcwd-{}", std::process::id()));
        let root = base.join("root");
        let home = base.join("home");
        for d in [root.join("proj/.claude/worktrees/w1"), home.join("x"), base.join("out")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::os::unix::fs::symlink(base.join("out"), root.join("escape")).unwrap();
        let (r, h) = (root.to_str().unwrap(), home.to_str().unwrap());
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap().to_str().unwrap().to_string();
        assert_eq!(job_cwd(h, r, h), Some(canon(&home)));
        assert_eq!(job_cwd("~", r, h), Some(canon(&home)));
        assert_eq!(job_cwd(r, r, h), Some(canon(&root)));
        let wt = root.join("proj/.claude/worktrees/w1");
        assert_eq!(job_cwd(wt.to_str().unwrap(), r, h), Some(canon(&wt)));
        assert_eq!(job_cwd(&format!("{r}/proj/../proj"), r, h), Some(canon(&root.join("proj"))));
        assert_eq!(job_cwd(home.join("x").to_str().unwrap(), r, h), None, "under home is not home");
        assert_eq!(job_cwd(root.join("escape").to_str().unwrap(), r, h), None, "a link out of the root");
        assert_eq!(job_cwd(&format!("{r}/../out"), r, h), None);
        assert_eq!(job_cwd("/", r, h), None);
        assert_eq!(job_cwd("relative", r, h), None);
        assert_eq!(job_cwd(&format!("{r}/missing"), r, h), None);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn request_bodies() {
        let base = std::env::temp_dir().join(format!("cs-jobspec-{}", std::process::id()));
        std::fs::create_dir_all(base.join("root/p")).unwrap();
        let (root, home) = (base.join("root"), base.clone());
        let (r, h) = (root.to_str().unwrap(), home.to_str().unwrap());
        let parse = |v: Value| parse_spec(v.as_object().unwrap(), r, h, 14_400, "claude");
        let sid = "93fb531a-9e91-4926-ab89-93ded70cba7e";
        let spec = parse(json!({"cwd": format!("{r}/p"), "prompt": "hi", "resume": sid})).unwrap();
        assert_eq!(
            spec.argv,
            ["claude", "-p", "hi", "--verbose", "--output-format", "stream-json", "--permission-mode", "auto", "--resume", sid]
        );
        assert!(spec.claude);
        assert_eq!(spec.max_seconds, DEFAULT_MAX_SECONDS);
        let spec = parse(json!({"argv": ["make", "test"], "permissionMode": "plan", "maxSeconds": 5, "env": {"CI": "1"}})).unwrap();
        assert_eq!(spec.cwd, std::fs::canonicalize(&home).unwrap().to_str().unwrap(), "cwd defaults to home");
        assert!(!spec.claude);
        assert_eq!(spec.permission_mode.as_deref(), Some("plan"));
        assert_eq!(spec.env, vec![("CI".to_string(), "1".to_string())]);
        assert!(parse(json!({"argv": ["/usr/local/bin/claude", "-p", "x"]})).unwrap().claude);
        for bad in [
            json!({}),
            json!({"argv": []}),
            json!({"argv": [""]}),
            json!({"argv": ["a", 1]}),
            json!({"argv": "ls"}),
            json!({"argv": ["ls"], "prompt": "x"}),
            json!({"argv": ["ls"], "resume": sid}),
            json!({"prompt": "x", "resume": "nope"}),
            json!({"prompt": "x", "permissionMode": "yolo"}),
            json!({"prompt": "x", "maxSeconds": 0}),
            json!({"prompt": "x", "maxSeconds": 14_401}),
            json!({"prompt": "x", "maxSeconds": "5"}),
            json!({"prompt": "x", "env": {"DYLD_INSERT_LIBRARIES": "x"}}),
            json!({"prompt": "x", "env": {"HOME": "/"}}),
            json!({"prompt": "x", "env": {"A": 1}}),
            json!({"prompt": "x", "cwd": "/"}),
        ] {
            assert_eq!(parse(bad.clone()).map_err(|e| e.0), Err(400), "{bad}");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn retention_and_eviction() {
        let config = crate::config::HubConfig { jobs: true, ..crate::config::HubConfig::fallback() };
        let jobs = Jobs::new(&config);
        let now = SystemTime::now();
        let hour = Duration::from_secs(3600);
        {
            let mut list = jobs.lock();
            list.push(job("old", Some(now - hour - Duration::from_secs(1))));
            list.push(job("new", Some(now - hour + Duration::from_secs(60))));
            list.push(job("run", None));
        }
        let ids = |jobs: &Jobs| -> Vec<String> {
            jobs.list().iter().map(|j| j["id"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(ids(&jobs), ["new", "run"], "finished an hour ago: gone");
        let mut list: Vec<Job> = (0..MAX_JOBS).map(|i| job(&format!("f{i}"), Some(now - Duration::from_secs(i as u64)))).collect();
        make_room(&mut list).unwrap();
        assert_eq!(list.len(), MAX_JOBS - 1);
        assert!(!list.iter().any(|j| j.id == format!("f{}", MAX_JOBS - 1)), "the first to finish goes");
        let mut running: Vec<Job> = (0..MAX_JOBS).map(|i| job(&format!("r{i}"), None)).collect();
        assert_eq!(make_room(&mut running).map_err(|e| e.0), Err(429));
        assert_eq!(running.len(), MAX_JOBS);
    }
}
