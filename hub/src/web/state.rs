//! The web app's directory (`GET /api/state`): the pure rules behind it and
//! its launches, the builder that reads the disk and the registry, and the
//! one-second cache in front of the builder.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::config::{HubConfig, home_dir};
use crate::frame::PROTOCOL;
use crate::approvals::{self, PendingApproval, Registered};
use crate::hub::{Command, SessionSnapshot, Snapshot};
use crate::procs;

/// What `order` needs to know about one project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectKey {
    pub name: String,
    /// Live sessions in it.
    pub running: usize,
    pub last_activity: Option<SystemTime>,
}

/// Display order for the directory: projects with running sessions
/// first, most instances first; then everything else, most recently
/// active first, then name. Running projects with the same count go by
/// name alone: their activity time moves every time a session writes,
/// and cards that swap places between polls are worse than any order.
pub fn order(projects: &[ProjectKey]) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..projects.len()).collect();
    indices.sort_by(|&a, &b| {
        let (x, y) = (&projects[a], &projects[b]);
        let by_running = (y.running > 0)
            .cmp(&(x.running > 0))
            .then(y.running.cmp(&x.running));
        if by_running != Ordering::Equal {
            return by_running;
        }
        if x.running == 0 && x.last_activity != y.last_activity {
            // Newest first; never used goes after everything that was.
            return match (x.last_activity, y.last_activity) {
                (Some(xa), Some(ya)) => ya.cmp(&xa),
                (None, _) => Ordering::Greater,
                (_, None) => Ordering::Less,
            };
        }
        compare_names(&x.name, &y.name)
    });
    indices
}

/// Stand-in for Foundation's `localizedCaseInsensitiveCompare`: Unicode
/// lowercase, then code points. Agrees with it on the ASCII names that
/// project folders have; locale collation (accents, punctuation weights)
/// is not reproduced.
fn compare_names(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// The project a working directory belongs to: the one it equals or
/// sits inside (a worktree, a subfolder). Longest match wins.
pub fn project_index<S: AsRef<str>>(cwd: &str, paths: &[S]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (index, path) in paths.iter().enumerate() {
        let path = path.as_ref();
        let inside =
            cwd == path || (cwd.starts_with(path) && cwd.as_bytes().get(path.len()) == Some(&b'/'));
        if inside && best.is_none_or(|b| path.len() > paths[b].as_ref().len()) {
            best = Some(index);
        }
    }
    best
}

/// Branch name from the text of `.git/HEAD`; a short hash when detached.
pub fn branch_from_head(text: &str) -> Option<String> {
    let head = text.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        return Some(branch.to_string());
    }
    if head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(head[..7].to_string());
    }
    None
}

/// `URL(fileURLWithPath:).standardizedFileURL.path`, lexically: a leading
/// `~/` is the home directory (a bare `~` is a relative name, as it is
/// there), a relative path is taken from the current
/// directory, `.` and empty components go, `..` takes one off (never past
/// `/`), and no trailing slash. Symlinks are not resolved.
pub fn standardize(path: &str, home: &str) -> String {
    let absolute = if path.starts_with("~/") {
        format!("{home}/{}", &path[1..])
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into());
        format!("{cwd}/{path}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in absolute.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// Where a web or phone launch may start Claude: a project folder
/// directly under the root — the same set the directory page lists — or
/// the home directory itself (the quick "+" in the clients' top bar, for a
/// session that isn't about any one project). Nothing else: the request
/// body is untrusted. Returns the canonical path.
pub fn launch_target(requested: &str, root: &str, home: &str) -> Option<String> {
    let path = standardize(requested, home);
    let root_path = standardize(root, home);
    let home_path = standardize(home, home);
    let allowed = path == home_path
        || match path.rsplit_once('/') {
            // `/` itself has no parent (Foundation calls `/` its own parent,
            // which would let a root of `/` launch in the root).
            Some((parent, name)) if !name.is_empty() => {
                let parent = if parent.is_empty() { "/" } else { parent };
                parent == root_path && !name.starts_with('.')
            }
            _ => false,
        };
    if !allowed || !Path::new(&path).is_dir() {
        return None;
    }
    Some(path)
}

/// A Claude session id: a UUID in its 8-4-4-4-12 hex text form (either case).
pub fn is_session_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

// MARK: - Build

/// A moment and where it came from, because the Swift hub this must match
/// turned the two kinds into milliseconds by different floating-point
/// paths: registry times were `Date(timeIntervalSince1970: ms / 1000)`,
/// file times Foundation's own conversion of the `timespec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Stamp {
    time: SystemTime,
    file: bool,
}

impl Stamp {
    fn registry(time: SystemTime) -> Stamp {
        Stamp { time, file: false }
    }

    fn file(time: SystemTime) -> Stamp {
        Stamp { time, file: true }
    }

    /// Epoch milliseconds as `Int(date.timeIntervalSince1970 * 1000)`.
    fn ms(self) -> i64 {
        const REFERENCE: f64 = 978_307_200.0; // 2001-01-01, Foundation's epoch
        let (secs, nanos) = match self.time.duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as f64, d.subsec_nanos() as f64),
            Err(e) => (-(e.duration().as_secs_f64()), 0.0),
        };
        let since_reference = if self.file {
            (secs - REFERENCE) + 1.0e-9 * nanos
        } else {
            (secs + nanos / 1.0e9) - REFERENCE
        };
        ((since_reference + REFERENCE) * 1000.0) as i64
    }
}

fn ms(time: SystemTime) -> i64 {
    Stamp::registry(time).ms()
}

/// Transcript path → the title read from it at a given mtime, so an
/// unchanged transcript is never tail-read twice. Only the (coalesced)
/// build touches it, and the scanner with its tail cache.
type Titles = HashMap<PathBuf, (SystemTime, Option<String>)>;
static TITLES: Mutex<Option<Titles>> = Mutex::new(None);
static SCANNER: Mutex<Option<claude_sessions::Scanner>> = Mutex::new(None);

/// `/Users/me/x` → `~/x` (`abbreviatingWithTildeInPath`).
fn abbreviate(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".into();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// The web app's view of the machine: every project directory under the
/// root, the Claude sessions running in each, and each project's recent
/// conversations. Blocking (directory listings, transcript tails): run it
/// on a blocking thread, one at a time (`Cache` sees to both).
/// What a build found for the hub to act on: permission requests to hang
/// up on (`approvals::stale`), and the Claude sessions alive (whose rules
/// survive).
pub struct Reconciliation {
    pub stale: Vec<String>,
    pub live: HashSet<String>,
}

pub fn build(snapshot: &Snapshot) -> (Value, Reconciliation) {
    let hub_sessions: &[SessionSnapshot] = &snapshot.sessions;
    let config = &snapshot.config;
    let now = SystemTime::now();
    let home = home_dir().to_string_lossy().into_owned();
    let root = standardize(&config.root, &home);
    let mut directories: Vec<String> = std::fs::read_dir(&root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|name| !name.starts_with('.'))
                .map(|name| standardize(&format!("{root}/{name}"), &home))
                .collect()
        })
        .unwrap_or_default();
    directories.sort();

    // Live sessions: the registry is the truth about status; the hub adds
    // which of them it owns (and can therefore attach to). A hub session's
    // pid is its supervisor, so the Claude it runs is the registry session
    // with that pid nearest in its ancestry — nearest, because a Claude
    // started *inside* that session (by a tool call) has the same
    // supervisor further up.
    let scanned = {
        let mut scanner = SCANNER.lock().unwrap_or_else(|e| e.into_inner());
        scanner
            .get_or_insert_with(claude_sessions::Scanner::new)
            .scan()
    };
    let mut candidates: Vec<(usize, i32, i32)> = Vec::new();
    for session in &scanned {
        let pid = session.entry.pid as i32;
        let chain = procs::ancestor_pids(pid, procs::ANCESTRY_LIMIT);
        if let Some(depth) = chain
            .iter()
            .position(|p| hub_sessions.iter().any(|h| h.pid == *p))
        {
            candidates.push((depth, pid, chain[depth]));
        }
    }
    candidates.sort();
    let mut unmatched: Vec<SessionSnapshot> = hub_sessions.to_vec();
    let mut owner: HashMap<i32, SessionSnapshot> = HashMap::new();
    for (_, pid, hub_pid) in candidates {
        if let Some(index) = unmatched.iter().position(|h| h.pid == hub_pid) {
            owner.insert(pid, unmatched.remove(index));
        }
    }

    // Permission requests: the ones answered in the terminal (or without
    // a session for too long) are left out here and hung up on by the hub
    // after the build; the rest go on their session's entry.
    let registered: HashMap<String, Registered> = scanned
        .iter()
        .filter_map(|s| {
            Some((
                s.entry.session_id.clone()?,
                Registered {
                    waiting: s.state == claude_sessions::State::Waiting,
                    status_updated_at: s.entry.status_updated_at,
                },
            ))
        })
        .collect();
    let stale = approvals::stale(&snapshot.approvals, &registered, now);
    let mut pending_for: HashMap<&str, Vec<&PendingApproval>> = HashMap::new();
    for approval in &snapshot.approvals {
        if let Some(id) = approval.session_id.as_deref()
            && !stale.contains(&approval.id)
        {
            pending_for.entry(id).or_default().push(approval);
        }
    }

    let mut per_project: Vec<Vec<Map<String, Value>>> = vec![Vec::new(); directories.len()];
    let mut activity: Vec<Option<Stamp>> = vec![None; directories.len()];
    let mut elsewhere: Vec<Value> = Vec::new();
    let mut live_ids: HashSet<String> = HashSet::new();

    let mut place = |mut entry: Map<String, Value>, cwd: &str, active: Option<Stamp>| {
        let Some(index) = project_index(cwd, &directories) else {
            elsewhere.push(Value::Object(entry));
            return;
        };
        if cwd != directories[index] {
            entry.insert(
                "sub".into(),
                cwd[directories[index].len() + 1..].to_string().into(),
            );
        }
        per_project[index].push(entry);
        if let Some(active) = active
            && activity[index].is_none_or(|a| active.time > a.time)
        {
            activity[index] = Some(active);
        }
    };

    for session in &scanned {
        let pid = session.entry.pid as i32;
        if let Some(id) = &session.entry.session_id {
            live_ids.insert(id.clone());
        }
        let hub = owner.get(&pid);
        let status = match session.state {
            claude_sessions::State::Busy => "busy",
            claude_sessions::State::Shell => "shell",
            claude_sessions::State::Idle => "idle",
            claude_sessions::State::Waiting => "waiting",
        };
        let session_id = session.entry.session_id.as_deref();
        let pending = session_id
            .and_then(|id| pending_for.get(id))
            .map(Vec::as_slice)
            .unwrap_or_default();
        // A session with a request open is waiting on the user, whatever
        // the registry has caught up with.
        let status = if pending.is_empty() { status } else { "waiting" };
        let cwd = session.entry.cwd.clone().unwrap_or_else(|| "?".into());
        let mut entry = Map::new();
        let key = match hub {
            Some(h) => format!("h:{}", h.id),
            None => format!("p:{pid}"),
        };
        entry.insert("key".into(), key.into());
        entry.insert("pid".into(), pid.into());
        entry.insert("cwd".into(), cwd.clone().into());
        entry.insert("status".into(), status.into());
        entry.insert("attachable".into(), hub.is_some().into());
        entry.insert("background".into(), session.is_background.into());
        entry.insert("viewers".into(), hub.map_or(0, |h| h.viewers).into());
        entry.insert("approvals".into(), approvals_json(pending));
        entry.insert(
            "autoApprove".into(),
            approvals::rule_json(session_id.and_then(|id| snapshot.rules.get(id).copied()), now, ms),
        );
        let mut optional = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                entry.insert(key.into(), value);
            }
        };
        optional("sessionId", session_id.map(Value::from));
        optional("hubId", hub.map(|h| h.id.clone().into()));
        optional(
            "mode",
            hub.and_then(|h| h.permission_mode.clone()).map(Value::from),
        );
        optional("name", session.entry.name.clone().map(Value::from));
        optional("title", session.title.clone().map(Value::from));
        optional("branch", session.git_branch.clone().map(Value::from));
        optional("waitingFor", session.entry.waiting_for.clone().map(Value::from));
        optional("jobId", session.entry.job_id.clone().map(Value::from));
        optional(
            "since",
            session.entry.status_updated_at.map(|t| ms(t).into()),
        );
        optional(
            "startedAt",
            session
                .entry
                .started_at
                .map(|t| ms(t).into())
                .or_else(|| hub.map(|h| ms(h.started_at).into())),
        );
        let active = [
            session.last_activity.map(Stamp::file),
            session.entry.status_updated_at.map(Stamp::registry),
            session.entry.started_at.map(Stamp::registry),
        ]
        .into_iter()
        .flatten()
        .max_by_key(|s| s.time);
        place(entry, &cwd, active);
    }
    // Hub sessions Claude hasn't registered yet: still starting up, or
    // parked on a first-run prompt. Attachable all the same.
    for hub in &unmatched {
        let mut entry = Map::new();
        entry.insert("key".into(), format!("h:{}", hub.id).into());
        entry.insert("hubId".into(), hub.id.clone().into());
        entry.insert("pid".into(), hub.pid.into());
        entry.insert("cwd".into(), hub.cwd.clone().into());
        entry.insert("status".into(), "starting".into());
        entry.insert("attachable".into(), true.into());
        entry.insert("background".into(), false.into());
        entry.insert("viewers".into(), hub.viewers.into());
        entry.insert("approvals".into(), Value::Array(Vec::new()));
        entry.insert("autoApprove".into(), Value::Null);
        entry.insert("startedAt".into(), ms(hub.started_at).into());
        entry.insert("since".into(), ms(hub.started_at).into());
        if let Some(mode) = &hub.permission_mode {
            entry.insert("mode".into(), mode.clone().into());
        }
        place(entry, &hub.cwd, Some(Stamp::registry(hub.started_at)));
    }

    let mut titles = TITLES.lock().unwrap_or_else(|e| e.into_inner());
    let titles = titles.get_or_insert_with(HashMap::new);
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut projects: Vec<Value> = Vec::new();
    let mut keys: Vec<ProjectKey> = Vec::new();
    for (index, path) in directories.iter().enumerate() {
        let transcripts = claude_sessions::transcripts_for_project(path);
        seen.extend(transcripts.iter().map(|t| t.0.clone()));
        let newest = transcripts.first().map(|t| Stamp::file(t.2));
        let last = [activity[index], newest]
            .into_iter()
            .flatten()
            .max_by_key(|s| s.time);

        let mut recent: Vec<Value> = Vec::new();
        for (file, session_id, mtime) in transcripts.iter().take(8) {
            if recent.len() >= 3 {
                break;
            }
            if live_ids.contains(session_id) {
                continue;
            }
            let title = match titles.get(file) {
                Some((at, title)) if at == mtime => title.clone(),
                _ => {
                    let title = claude_sessions::read_tail(file, claude_sessions::TAIL_BYTES)
                        .and_then(|t| t.ai_title);
                    titles.insert(file.clone(), (*mtime, title.clone()));
                    title
                }
            };
            let Some(title) = title else { continue };
            recent.push(json!({
                "sessionId": session_id,
                "title": title,
                "at": Stamp::file(*mtime).ms(),
            }));
        }

        let mut sessions = std::mem::take(&mut per_project[index]);
        let int = |e: &Map<String, Value>, k: &str| e.get(k).and_then(Value::as_i64).unwrap_or(0);
        sessions.sort_by_key(|e| (int(e, "startedAt"), int(e, "pid")));
        let count = sessions.len();
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        let mut project = Map::new();
        project.insert("name".into(), name.clone().into());
        project.insert("path".into(), path.clone().into());
        project.insert(
            "sessions".into(),
            Value::Array(sessions.into_iter().map(Value::Object).collect()),
        );
        project.insert("recent".into(), Value::Array(recent));
        if let Some(branch) = std::fs::read_to_string(format!("{path}/.git/HEAD"))
            .ok()
            .and_then(|text| branch_from_head(&text))
        {
            project.insert("branch".into(), branch.into());
        }
        if let Some(last) = last {
            project.insert("lastActivity".into(), last.ms().into());
        }
        projects.push(Value::Object(project));
        keys.push(ProjectKey {
            name,
            running: count,
            last_activity: last.map(|s| s.time),
        });
    }
    titles.retain(|path, _| seen.contains(path));

    let ordered: Vec<Value> = order(&keys)
        .into_iter()
        .map(|i| projects[i].clone())
        .collect();
    let state = json!({
        "host": config.display_name(),
        "protocol": PROTOCOL,
        "approvalsSupported": true,
        "root": root,
        "rootDisplay": abbreviate(&root, &home),
        "home": home,
        "defaultPermissionMode": config.default_permission_mode,
        "permissionModes": HubConfig::permission_modes(),
        // Live (the hub updates its config when the switch is flipped).
        "jobs": config.jobs,
        "now": ms(SystemTime::now()),
        "projects": ordered,
        "elsewhere": elsewhere,
    });
    (state, Reconciliation { stale, live: live_ids })
}

/// A session's open requests as the clients show them, oldest first.
fn approvals_json(pending: &[&PendingApproval]) -> Value {
    Value::Array(
        pending
            .iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "tool": a.tool,
                    "summary": a.summary,
                    "detail": a.detail,
                    "receivedAt": ms(a.received_at),
                })
            })
            .collect(),
    )
}

/// The directory as JSON, built at most once a second however many
/// browsers are polling: a request that finds a fresh copy gets it, one
/// that arrives while a build runs waits for that build.
#[derive(Default)]
pub struct Cache {
    inner: Mutex<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    cached: Option<(Instant, Bytes)>,
    /// Each with the generation it asked in: a build answers only the
    /// requests that arrived before it began, never one made after a
    /// launch with what was there before it.
    waiters: Vec<(u64, oneshot::Sender<Bytes>)>,
    /// The generation of the build running now, if any.
    building: Option<u64>,
    /// Bumped by `invalidate`, so a build that started before a launch
    /// doesn't put pre-launch state back in the cache or hand it to a
    /// request that came after.
    generation: u64,
}

const FRESH: Duration = Duration::from_secs(1);

impl Cache {
    pub async fn get(self: &Arc<Self>, hub: &mpsc::UnboundedSender<Command>) -> Bytes {
        self.get_with(move |generation, cache| {
            tokio::spawn(cache.refresh(hub.clone(), generation));
        })
        .await
    }

    /// `get`, with how a build is started left to the caller (the tests).
    async fn get_with(self: &Arc<Self>, start: impl FnOnce(u64, Arc<Self>)) -> Bytes {
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, data)) = &inner.cached
                && at.elapsed() < FRESH
            {
                return data.clone();
            }
            let generation = inner.generation;
            inner.waiters.push((generation, tx));
            if inner.building != Some(generation) {
                inner.building = Some(generation);
                // Its own task: the request that started it may go away.
                start(generation, self.clone());
            }
        }
        rx.await.unwrap_or_else(|_| Bytes::from_static(b"{}"))
    }

    /// Build once and answer everyone waiting: a snapshot of the hub's
    /// sessions first, then the disk work on a blocking thread.
    async fn refresh(self: Arc<Self>, hub: mpsc::UnboundedSender<Command>, generation: u64) {
        let (tx, rx) = oneshot::channel();
        let snapshot = match hub.send(Command::WebSnapshot { reply: tx }) {
            Ok(()) => rx.await.ok(),
            Err(_) => None,
        };
        let built = match snapshot {
            Some(snapshot) => tokio::task::spawn_blocking(move || {
                let (state, reconciliation) = build(&snapshot);
                (
                    serde_json::to_vec(&state).unwrap_or_else(|_| b"{}".to_vec()),
                    Some(reconciliation),
                )
            })
            .await
            .unwrap_or_else(|_| (b"{}".to_vec(), None)),
            None => (b"{}".to_vec(), None),
        };
        self.finish(generation, Bytes::from(built.0));
        // After the answer is cached: if the hub drops anything, it
        // invalidates, and the next request sees the change.
        if let Some(Reconciliation { stale, live }) = built.1 {
            let _ = hub.send(Command::Reconcile { stale, live });
        }
    }

    /// A build of `generation` is done: cache it if nothing has changed
    /// since it began, and answer the requests it was begun for.
    fn finish(&self, generation: u64, data: Bytes) {
        let waiters = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.generation == generation {
                inner.cached = Some((Instant::now(), data.clone()));
            }
            if inner.building == Some(generation) {
                inner.building = None;
            }
            let (answered, waiting) = std::mem::take(&mut inner.waiters)
                .into_iter()
                .partition(|(asked, _)| *asked <= generation);
            inner.waiters = waiting;
            answered
        };
        for (_, waiter) in waiters {
            let _ = waiter.send(data.clone());
        }
    }

    /// After a launch, kill, or settings change: the next request rebuilds.
    pub fn invalidate(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.cached = None;
        inner.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn key(name: &str, running: usize, at: Option<SystemTime>) -> ProjectKey {
        ProjectKey {
            name: name.into(),
            running,
            last_activity: at,
        }
    }

    #[test]
    fn running_by_count_then_name_idle_by_recency_unused_last_by_name() {
        let day = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ago = |s: u64| Some(day - Duration::from_secs(s));
        let ordered = order(&[
            key("idle-old", 0, ago(9000)),
            key("one-b", 1, Some(day)),
            key("never", 0, None),
            key("two", 2, ago(500)),
            key("idle-new", 0, ago(10)),
            key("one-a", 1, ago(900)),
            key("also-never", 0, None),
        ]);
        assert_eq!(ordered, vec![3, 5, 1, 4, 0, 6, 2]);
    }

    #[test]
    fn order_names_case_insensitively_and_ties_on_time() {
        let t = Some(UNIX_EPOCH + Duration::from_secs(5));
        assert_eq!(order(&[key("beta", 0, t), key("Alpha", 0, t)]), vec![1, 0]);
        assert_eq!(order(&[]), Vec::<usize>::new());
    }

    #[test]
    fn project_for_a_working_directory() {
        let paths = ["/code/app", "/code/app-two", "/code/lib"];
        assert_eq!(
            project_index("/code/app", &paths),
            Some(0),
            "project: exact directory"
        );
        assert_eq!(
            project_index("/code/app/.claude/worktrees/x", &paths),
            Some(0),
            "project: a worktree inside it"
        );
        assert_eq!(
            project_index("/code/app-two/src", &paths),
            Some(1),
            "project: a sibling sharing a name prefix"
        );
        assert_eq!(
            project_index("/elsewhere", &paths),
            None,
            "project: outside the root"
        );
        assert_eq!(
            project_index("/code/app/x", &["/code", "/code/app"]),
            Some(1),
            "longest match wins"
        );
    }

    #[test]
    fn branch_names() {
        assert_eq!(
            branch_from_head("ref: refs/heads/feature/x\n").as_deref(),
            Some("feature/x"),
            "branch: symbolic ref"
        );
        assert_eq!(
            branch_from_head("9dfe4f7a1b2c3d4e5f60718293a4b5c6d7e8f901\n").as_deref(),
            Some("9dfe4f7"),
            "branch: detached head"
        );
        assert_eq!(
            branch_from_head("gitdir: ../x"),
            None,
            "branch: not a HEAD file"
        );
        assert_eq!(branch_from_head("abc12"), None, "too short to be a hash");
    }

    #[test]
    fn standardized_paths() {
        assert_eq!(standardize("/a/b/../c/", "/h"), "/a/c");
        assert_eq!(standardize("/a/./b", "/h"), "/a/b");
        assert_eq!(standardize("/../x", "/h"), "/x");
        assert_eq!(standardize("//a//b", "/h"), "/a/b");
        assert_eq!(standardize("/a/..", "/h"), "/");
        assert_eq!(standardize("~/x", "/h"), "/h/x");
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            standardize("~", "/h"),
            standardize(&format!("{cwd}/~"), "/h"),
            "a bare ~ is a name in the current directory, as in Foundation"
        );
        assert_eq!(
            standardize("/tmp/x", "/h"),
            "/tmp/x",
            "symlinks are not resolved"
        );
    }

    #[test]
    fn launch_targets() {
        let tmp_root =
            std::env::temp_dir().join(format!("claudeship-selftest-{}", std::process::id()));
        std::fs::create_dir_all(tmp_root.join("proj/inner")).unwrap();
        std::fs::create_dir_all(tmp_root.join(".hidden")).unwrap();
        std::fs::write(tmp_root.join("file"), b"").unwrap();
        let root = tmp_root.to_string_lossy().into_owned();
        let canonical = standardize(&root, "/nowhere");
        let elsewhere = "/nonexistent-home";
        assert_eq!(
            launch_target(&format!("{root}/proj"), &root, elsewhere),
            Some(format!("{canonical}/proj")),
            "launch: a project directory"
        );
        assert_eq!(
            launch_target(&format!("{root}/proj/../proj/"), &root, elsewhere),
            Some(format!("{canonical}/proj")),
            "launch: path is normalized"
        );
        assert_eq!(
            launch_target(&format!("{root}/proj/inner"), &root, elsewhere),
            None,
            "launch: not a nested directory"
        );
        assert_eq!(
            launch_target(&root, &format!("{root}/proj"), &root),
            Some(canonical.clone()),
            "launch: the home directory itself"
        );
        assert_eq!(
            launch_target(
                &format!("{root}/proj"),
                &format!("{root}/other"),
                &format!("{root}/x")
            ),
            None,
            "launch: home does not open its children"
        );
        assert_eq!(
            launch_target(&format!("{root}/proj/../.."), &root, elsewhere),
            None,
            "launch: not above the root"
        );
        assert_eq!(
            launch_target(&root, &root, elsewhere),
            None,
            "launch: not the root itself"
        );
        assert_eq!(
            launch_target(&format!("{root}/.hidden"), &root, elsewhere),
            None,
            "launch: not a hidden directory"
        );
        assert_eq!(
            launch_target(&format!("{root}/missing"), &root, elsewhere),
            None,
            "launch: must exist"
        );
        assert_eq!(
            launch_target(&format!("{root}/file"), &root, elsewhere),
            None,
            "launch: must be a directory"
        );
        assert_eq!(
            launch_target("/", "/", elsewhere),
            None,
            "launch: a root of / does not open / itself"
        );
        assert_eq!(
            launch_target("/tmp", "/", elsewhere).is_some(),
            Path::new("/tmp").is_dir(),
            "launch: a root of / opens its children"
        );
        std::fs::remove_dir_all(&tmp_root).unwrap();
    }

    #[test]
    fn session_ids() {
        assert!(
            is_session_id("93fb531a-9e91-4926-ab89-93ded70cba7e"),
            "resume: a uuid"
        );
        assert!(
            is_session_id("93FB531A-9E91-4926-AB89-93DED70CBA7E"),
            "upper case too"
        );
        assert!(
            !is_session_id("x; rm -rf ~"),
            "resume: anything else refused"
        );
        assert!(
            !is_session_id("93fb531a9e914926ab8993ded70cba7e"),
            "no hyphens"
        );
        assert!(
            !is_session_id("93fb531a-9e91-4926-ab89-93ded70cba7"),
            "short"
        );
        assert!(
            !is_session_id("93fb531g-9e91-4926-ab89-93ded70cba7e"),
            "not hex"
        );
    }

    /// A request made after a launch is never answered with a build that
    /// began before it, and that build is not cached.
    #[tokio::test]
    async fn a_build_from_before_an_invalidation_answers_only_earlier_requests() {
        let cache = Arc::new(Cache::default());
        let started = Arc::new(Mutex::new(Vec::new()));
        let start = |started: &Arc<Mutex<Vec<u64>>>| {
            let started = started.clone();
            move |generation, _| started.lock().unwrap().push(generation)
        };
        let early = tokio::spawn({
            let (cache, start) = (cache.clone(), start(&started));
            async move { cache.get_with(start).await }
        });
        tokio::task::yield_now().await;
        while started.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        cache.invalidate();
        let late = tokio::spawn({
            let (cache, start) = (cache.clone(), start(&started));
            async move { cache.get_with(start).await }
        });
        while started.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(*started.lock().unwrap(), vec![0, 1], "a new build for the late request");
        cache.finish(0, Bytes::from_static(b"before"));
        assert_eq!(early.await.unwrap(), "before");
        assert!(!late.is_finished(), "the late request waits for its own build");
        assert!(cache.inner.lock().unwrap().cached.is_none(), "stale data not cached");
        cache.finish(1, Bytes::from_static(b"after"));
        assert_eq!(late.await.unwrap(), "after");
        let again = cache.get_with(|_, _| panic!("cached")).await;
        assert_eq!(again, "after");
    }
}
