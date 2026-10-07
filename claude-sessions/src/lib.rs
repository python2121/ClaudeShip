//! Claude Code's live-session registry (`~/.claude/sessions/<pid>.json`) and
//! transcript tails (`~/.claude/projects/<dashed-cwd>/<uuid>.jsonl`).
//! Phase 1 of docs/rust-core-plan.md; a port of `SessionScanner.swift`.
//!
//! The registry is the source of truth for state. Transcripts are read only
//! for garnish (git branch, title, mtime): a first cut inferred state from
//! transcript tails and could not tell "idle at the prompt" from "waiting on a
//! permission prompt". Don't regress to that.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The transcript tail window: the last 64 KB.
pub const TAIL_BYTES: usize = 65_536;
/// The tail cache is cleared when it grows past this many entries.
pub const TAIL_CACHE_LIMIT: usize = 256;

/// One registry file, as Claude Code writes it.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryEntry {
    pub pid: u32,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    /// Claude's derived session name, e.g. "claude-status-b3".
    pub name: Option<String>,
    /// Raw status: "busy", "shell", "idle" or "waiting".
    pub status: Option<String>,
    /// What the session is blocked on, when the CLI says.
    pub waiting_for: Option<String>,
    /// Session launch time (registry epoch milliseconds).
    pub started_at: Option<SystemTime>,
    /// When the current status began (registry epoch milliseconds).
    pub status_updated_at: Option<SystemTime>,
    /// "interactive" for a terminal session, "bg" for a daemon-run one.
    pub kind: Option<String>,
    /// The daemon's short id for a background session.
    pub job_id: Option<String>,
}

/// Session state mapped from the registry's status string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Claude is working.
    Busy,
    /// The user is running a `!` shell command inside the session.
    Shell,
    /// At the prompt; nothing requested of the user.
    Idle,
    /// Blocked on the user (permission prompt etc.).
    Waiting,
}

/// A live session: registry entry plus transcript garnish.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveSession {
    pub entry: RegistryEntry,
    pub state: State,
    /// Registry kind is "bg": run by the Claude Code daemon, in no terminal.
    pub is_background: bool,
    pub git_branch: Option<String>,
    /// Newest `ai-title` in the transcript.
    pub title: Option<String>,
    /// Transcript mtime: the last moment the session wrote anything.
    pub last_activity: Option<SystemTime>,
}

/// What a transcript's tail window holds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptTail {
    pub session_id: Option<String>,
    pub git_branch: Option<String>,
    /// Newest `{"type":"ai-title","aiTitle":…}` entry. Claude Code rewrites it
    /// every turn, so it is reliably inside the tail window.
    pub ai_title: Option<String>,
}

/// The Claude config root: `$CLAUDE_CONFIG_DIR` if set and non-empty, else
/// `~/.claude`.
pub fn claude_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".claude")
}

/// `<root>/sessions`, where the registry lives.
pub fn sessions_root() -> PathBuf {
    claude_root().join("sessions")
}

/// `<root>/projects`, where transcripts live.
pub fn projects_root() -> PathBuf {
    claude_root().join("projects")
}

/// Claude Code names each project dir by flattening the cwd: every char that
/// isn't alphanumeric (Unicode-aware, like Swift's `isLetter`/`isNumber`) or
/// `-` becomes `-` (slashes included, so the name leads with one).
pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

fn epoch_ms(ms: f64) -> Option<SystemTime> {
    if !ms.is_finite() {
        return None;
    }
    let d = Duration::try_from_secs_f64(ms.abs() / 1000.0).ok()?;
    if ms >= 0.0 { UNIX_EPOCH.checked_add(d) } else { UNIX_EPOCH.checked_sub(d) }
}

/// Parse one registry file. `None` unless it is a JSON object with a `pid`.
pub fn parse_registry_entry(json: &[u8]) -> Option<RegistryEntry> {
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    let obj = value.as_object()?;
    let pid = u32::try_from(obj.get("pid")?.as_i64()?).ok()?;
    let string = |key: &str| obj.get(key).and_then(|v| v.as_str()).map(String::from);
    let time = |key: &str| obj.get(key).and_then(|v| v.as_f64()).and_then(epoch_ms);
    Some(RegistryEntry {
        pid,
        cwd: string("cwd"),
        session_id: string("sessionId"),
        name: string("name"),
        status: string("status"),
        waiting_for: string("waitingFor"),
        started_at: time("startedAt"),
        status_updated_at: time("statusUpdatedAt"),
        kind: string("kind"),
        job_id: string("jobId"),
    })
}

/// Only an explicit "bg" counts; a missing kind (older CLI) is a terminal
/// session, so the row keeps its click-to-focus behavior.
pub fn is_background(kind: Option<&str>) -> bool {
    kind == Some("bg")
}

/// Map the registry's status string to a [`State`]. Unknown or missing values
/// read as idle — never a false alarm color.
pub fn state_from_status(status: Option<&str>) -> State {
    match status {
        Some("busy") => State::Busy,
        Some("shell") => State::Shell,
        Some("waiting") => State::Waiting,
        _ => State::Idle,
    }
}

/// `kill(pid, 0)` sends no signal, just checks deliverability. EPERM still
/// means "alive" (someone else's process — don't false-negative on it).
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: signal 0 performs only the existence/permission check.
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

/// Read the last `max_bytes` of a transcript and parse them; `None` if the
/// file can't be read or holds nothing of interest.
pub fn read_tail(path: &Path, max_bytes: usize) -> Option<TranscriptTail> {
    let mut file = fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let offset = size.saturating_sub(max_bytes as u64);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    // Lossy: the window may start mid-character; that only garbles the
    // (discarded) truncated first line.
    parse_tail(&String::from_utf8_lossy(&buf))
}

/// Parse transcript text (one JSON object per line). sessionId, gitBranch and
/// ai-title are hunted independently, each from the newest line that carries
/// it — plenty of entries have a sessionId but no gitBranch (progress records,
/// tool results), and taking the branch only off the newest sessionId entry
/// made the label flicker between polls. The first line may be truncated by
/// the byte window and is skipped if it doesn't parse.
pub fn parse_tail(text: &str) -> Option<TranscriptTail> {
    // Only the four fields we read are materialized; everything else on the
    // line (tool results, message bodies) is skipped without allocating.
    // A field of an unexpected type is kept as a Value and ignored below, so
    // it doesn't cost the line its other fields.
    #[derive(serde::Deserialize)]
    struct Line {
        #[serde(rename = "sessionId")]
        session_id: Option<serde_json::Value>,
        #[serde(rename = "gitBranch")]
        git_branch: Option<serde_json::Value>,
        #[serde(rename = "type")]
        kind: Option<serde_json::Value>,
        #[serde(rename = "aiTitle")]
        ai_title: Option<serde_json::Value>,
    }
    fn string(v: Option<serde_json::Value>) -> Option<String> {
        match v {
            Some(serde_json::Value::String(s)) => Some(s),
            _ => None,
        }
    }
    let mut tail = TranscriptTail::default();
    for line in text.split('\n').rev() {
        // Objects only: a derived struct would also accept a JSON array.
        if !line.trim_start().starts_with('{') {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Line>(line) else { continue };
        if tail.session_id.is_none() {
            tail.session_id = string(obj.session_id);
        }
        if tail.git_branch.is_none() {
            tail.git_branch = string(obj.git_branch).filter(|b| !b.is_empty());
        }
        if tail.ai_title.is_none() && obj.kind.as_ref().and_then(|k| k.as_str()) == Some("ai-title") {
            tail.ai_title = string(obj.ai_title).filter(|t| !t.is_empty());
        }
        if tail.session_id.is_some() && tail.git_branch.is_some() && tail.ai_title.is_some() {
            break;
        }
    }
    if tail == TranscriptTail::default() { None } else { Some(tail) }
}

/// A project's transcripts (`<projects>/<dashed cwd>/*.jsonl`), newest first
/// by mtime, as `(path, session uuid, mtime)`.
pub fn transcripts_for_project(cwd: &str) -> Vec<(PathBuf, String, SystemTime)> {
    let dir = projects_root().join(project_dir_name(cwd));
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<_> = rd
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            // Like the Swift original's `.skipsHiddenFiles`.
            if e.file_name().as_encoded_bytes().starts_with(b".")
                || path.extension().and_then(|x| x.to_str()) != Some("jsonl")
            {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            let id = path.file_stem()?.to_str()?.to_string();
            Some((path, id, mtime))
        })
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.2));
    out
}

/// Every registry entry whose pid is alive (files named `<pid>.json`; others
/// aren't ours, and a dead pid is a crash leftover). Unsorted.
pub fn read_live_entries() -> Vec<RegistryEntry> {
    let Ok(rd) = fs::read_dir(sessions_root()) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let named_by_pid = path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.parse::<i32>().is_ok());
        if !named_by_pid {
            continue;
        }
        let Some(entry) = fs::read(&path).ok().and_then(|d| parse_registry_entry(&d)) else {
            continue;
        };
        if pid_alive(entry.pid) {
            out.push(entry);
        }
    }
    out
}

/// Scans the registry and caches transcript tails by mtime: a busy session
/// rewrites its transcript constantly but an idle one doesn't, and re-reading
/// 64 KB per session every poll is the expensive part.
#[derive(Default)]
pub struct Scanner {
    tail_cache: HashMap<PathBuf, (SystemTime, Option<TranscriptTail>)>,
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// The tail of `path`, re-read only when `mtime` changed. Bounded at
    /// [`TAIL_CACHE_LIMIT`] entries (sessions come and go).
    pub fn cached_tail(&mut self, path: &Path, mtime: SystemTime) -> Option<TranscriptTail> {
        if let Some((m, tail)) = self.tail_cache.get(path)
            && *m == mtime
        {
            return tail.clone();
        }
        let tail = read_tail(path, TAIL_BYTES);
        self.tail_cache.insert(path.to_path_buf(), (mtime, tail.clone()));
        if self.tail_cache.len() > TAIL_CACHE_LIMIT {
            self.tail_cache.clear();
        }
        tail
    }

    /// Live sessions, sorted by (cwd, pid) so rows don't jump between polls.
    /// A missing cwd sorts as "?" like the Swift scanner.
    pub fn scan(&mut self) -> Vec<LiveSession> {
        let projects = projects_root();
        let mut sessions: Vec<LiveSession> = read_live_entries()
            .into_iter()
            .map(|entry| self.session_for(entry, &projects))
            .collect();
        sessions.sort_by(|a, b| {
            fn key(s: &LiveSession) -> (&str, u32) {
                (s.entry.cwd.as_deref().unwrap_or("?"), s.entry.pid)
            }
            key(a).cmp(&key(b))
        });
        sessions
    }

    fn session_for(&mut self, entry: RegistryEntry, projects: &Path) -> LiveSession {
        let mut git_branch = None;
        let mut title = None;
        let mut last_activity = None;
        if let (Some(cwd), Some(sid)) = (&entry.cwd, &entry.session_id) {
            let transcript = projects.join(project_dir_name(cwd)).join(format!("{sid}.jsonl"));
            last_activity = fs::metadata(&transcript).ok().and_then(|m| m.modified().ok());
            if let Some(mtime) = last_activity
                && let Some(tail) = self.cached_tail(&transcript, mtime)
            {
                git_branch = tail.git_branch;
                title = tail.ai_title;
            }
        }
        LiveSession {
            state: state_from_status(entry.status.as_deref()),
            is_background: is_background(entry.kind.as_deref()),
            entry,
            git_branch,
            title,
            last_activity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_ms(ms: f64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs_f64(ms / 1000.0)
    }

    #[test]
    fn registry_parsing() {
        let json = r#"{"pid":43606,"sessionId":"93fb531a-9e91-4926-ab89-93ded70cba7e","cwd":"/Users/x/code/claude-status","startedAt":1783455271121,"version":"2.1.202","kind":"interactive","name":"claude-status-b3","status":"busy","updatedAt":1783457374373,"statusUpdatedAt":1783457374373}"#;
        let e = parse_registry_entry(json.as_bytes()).unwrap();
        assert_eq!(e.pid, 43606);
        assert_eq!(e.cwd.as_deref(), Some("/Users/x/code/claude-status"));
        assert_eq!(e.session_id.as_deref(), Some("93fb531a-9e91-4926-ab89-93ded70cba7e"));
        assert_eq!(e.name.as_deref(), Some("claude-status-b3"));
        assert_eq!(e.status.as_deref(), Some("busy"));
        assert_eq!(e.started_at, Some(at_ms(1783455271121.0)));
        assert_eq!(e.status_updated_at, Some(at_ms(1783457374373.0)));
        assert_eq!(e.waiting_for, None);
        assert_eq!(e.kind.as_deref(), Some("interactive"));
        assert_eq!(e.job_id, None);

        let bg = parse_registry_entry(
            br#"{"pid":2,"kind":"bg","jobId":"a34398c4","sessionId":"a34398c4-12a2-4b54-aaac-edc6a5e935a6","status":"shell"}"#,
        )
        .unwrap();
        assert_eq!(bg.kind.as_deref(), Some("bg"));
        assert_eq!(bg.job_id.as_deref(), Some("a34398c4"));
        assert!(is_background(Some("bg")));
        assert!(!is_background(Some("interactive")));
        assert!(!is_background(None));

        let w = parse_registry_entry(br#"{"pid":1,"status":"waiting","waitingFor":"permission"}"#).unwrap();
        assert_eq!(w.waiting_for.as_deref(), Some("permission"));

        assert!(parse_registry_entry(br#"{"nopid":true}"#).is_none());
        assert!(parse_registry_entry(b"garbage").is_none());
    }

    #[test]
    fn status_mapping() {
        assert_eq!(state_from_status(Some("busy")), State::Busy);
        assert_eq!(state_from_status(Some("shell")), State::Shell);
        assert_eq!(state_from_status(Some("idle")), State::Idle);
        assert_eq!(state_from_status(Some("waiting")), State::Waiting);
        assert_eq!(state_from_status(Some("someday-new")), State::Idle);
        assert_eq!(state_from_status(None), State::Idle);
    }

    #[test]
    fn dir_name() {
        assert_eq!(project_dir_name("/Users/x/code/claude-status"), "-Users-x-code-claude-status");
        assert_eq!(project_dir_name("/Users/a_b/foo.bar"), "-Users-a-b-foo-bar");
        // Unicode letters and digits survive, like Swift's isLetter/isNumber.
        assert_eq!(project_dir_name("/Users/x/Café ٣"), "-Users-x-Café-٣");
    }

    #[test]
    fn tail_parsing() {
        let two = "{\"type\":\"user\",\"sessionId\":\"abc-123\",\"gitBranch\":\"main\",\"message\":{\"role\":\"user\"}}\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"stop_reason\":\"end_turn\"}}";
        let p = parse_tail(two).unwrap();
        assert_eq!(p.session_id.as_deref(), Some("abc-123"));
        assert_eq!(p.git_branch.as_deref(), Some("main"));

        let truncated = "...half a json object\"}\n{\"type\":\"assistant\",\"sessionId\":\"s1\",\"gitBranch\":\"feature/x\",\"message\":{}}";
        assert_eq!(parse_tail(truncated).unwrap().git_branch.as_deref(), Some("feature/x"));

        let branchless = "{\"type\":\"user\",\"sessionId\":\"abc-123\",\"gitBranch\":\"feature/y\",\"message\":{}}\n{\"type\":\"progress\",\"sessionId\":\"abc-123\"}\n{\"type\":\"assistant\",\"sessionId\":\"abc-123\",\"message\":{\"stop_reason\":\"tool_use\"}}";
        let t = parse_tail(branchless).unwrap();
        assert_eq!(t.session_id.as_deref(), Some("abc-123"));
        assert_eq!(t.git_branch.as_deref(), Some("feature/y"));

        assert_eq!(parse_tail(r#"{"sessionId":"s","gitBranch":""}"#).unwrap().git_branch, None);
        assert!(parse_tail("").is_none());
        assert!(parse_tail("not json at all").is_none());

        // SelfTest.swift's titled-tail vectors.
        let titled = "{\"type\":\"user\",\"sessionId\":\"s1\",\"gitBranch\":\"main\",\"cwd\":\"/x\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Old title\",\"sessionId\":\"s1\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Swift builds failing\",\"sessionId\":\"s1\"}\n{\"type\":\"progress\",\"sessionId\":\"s1\"}";
        let t = parse_tail(titled).unwrap();
        assert_eq!(t.ai_title.as_deref(), Some("Swift builds failing"));
        assert_eq!(t.git_branch.as_deref(), Some("main"));
        assert_eq!(parse_tail(r#"{"type":"ai-title","aiTitle":"","sessionId":"s1"}"#).unwrap().ai_title, None);
        assert_eq!(parse_tail(r#"{"type":"user","aiTitle":"not a title entry","sessionId":"s1"}"#).unwrap().ai_title, None);

        // A wrongly typed field doesn't cost the line its other fields;
        // arrays aren't objects.
        assert_eq!(parse_tail(r#"{"sessionId":7,"gitBranch":"dev"}"#).unwrap().git_branch.as_deref(), Some("dev"));
        assert!(parse_tail(r#"["s","b","ai-title","t"]"#).is_none());
    }

    #[test]
    fn tail_window() {
        // A window that starts mid-line (and mid-character) skips the
        // truncated first line and still finds the rest.
        let path = std::env::temp_dir().join(format!("cs-tail-{}.jsonl", std::process::id()));
        let filler = format!("{{\"type\":\"user\",\"gitBranch\":\"old\",\"x\":\"{}\"}}\n", "é".repeat(40_000));
        let last = "{\"type\":\"assistant\",\"sessionId\":\"s9\",\"gitBranch\":\"new\"}\n";
        fs::write(&path, format!("{filler}{last}")).unwrap();
        let t = read_tail(&path, TAIL_BYTES).unwrap();
        assert_eq!(t.session_id.as_deref(), Some("s9"));
        assert_eq!(t.git_branch.as_deref(), Some("new"));
        let short = read_tail(&path, last.len() + 4).unwrap();
        assert_eq!(short.git_branch.as_deref(), Some("new"));
        assert!(read_tail(&path.with_extension("missing"), TAIL_BYTES).is_none());

        let mut scanner = Scanner::new();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(scanner.cached_tail(&path, mtime).unwrap().session_id.as_deref(), Some("s9"));
        // Same mtime: served from the cache even though the file changed.
        fs::write(&path, "{\"sessionId\":\"other\"}\n").unwrap();
        assert_eq!(scanner.cached_tail(&path, mtime).unwrap().session_id.as_deref(), Some("s9"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn pid_liveness() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn scan_scratch_root() {
        // The only test touching CLAUDE_CONFIG_DIR, so no env races.
        let root = std::env::temp_dir().join(format!("cs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let cwd = "/Users/x/my_proj";
        let sid = "11111111-2222-3333-4444-555555555555";
        let pid = std::process::id();
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::create_dir_all(root.join("projects").join(project_dir_name(cwd))).unwrap();
        fs::write(
            root.join("sessions").join(format!("{pid}.json")),
            format!(r#"{{"pid":{pid},"sessionId":"{sid}","cwd":"{cwd}","status":"waiting","kind":"bg"}}"#),
        )
        .unwrap();
        // A dead pid and a non-pid filename are skipped.
        fs::write(root.join("sessions/999999999.json"), r#"{"pid":999999999}"#).unwrap();
        fs::write(root.join("sessions/notes.json"), format!(r#"{{"pid":{pid}}}"#)).unwrap();
        fs::write(
            root.join("projects").join(project_dir_name(cwd)).join(format!("{sid}.jsonl")),
            "{\"type\":\"user\",\"sessionId\":\"s\",\"gitBranch\":\"main\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Fix it\"}\n",
        )
        .unwrap();

        // SAFETY: single test mutating the environment.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &root) };
        assert_eq!(claude_root(), root);
        let mut scanner = Scanner::new();
        let sessions = scanner.scan();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.entry.pid, pid);
        assert_eq!(s.state, State::Waiting);
        assert!(s.is_background);
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.title.as_deref(), Some("Fix it"));
        assert!(s.last_activity.is_some());
        let ts = transcripts_for_project(cwd);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].1, sid);
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = fs::remove_dir_all(&root);
    }
}
