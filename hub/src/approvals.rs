//! Permission requests answered from any screen (plan phase 7, the Swift
//! app's `ApprovalCenter` and the approval half of its `SessionStore`).
//!
//! Claude Code runs `claudeship permission-hook` (`hook.rs`) for every
//! permission prompt. The helper connects to `<home>/approvals.sock`, sends
//! one request line, and blocks for one verdict line. One connection is one
//! pending approval: the hub answers it (`respond`), hangs up on it without
//! a verdict (`cancel` — the prompt was answered in the terminal, or its
//! session is gone), or sees the helper go away (Claude Code killed it) and
//! forgets it. The terminal prompt is live the whole time; whichever side
//! answers first wins, and the helper prints a decision only on a verdict.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::hub::{Command, log};

/// A request or verdict line longer than this is refused.
pub const MAX_LINE: usize = 1 << 20;
/// A pending whose session the registry doesn't list is dropped after this
/// (covers registry lag at a session's start).
pub const UNMATCHED_GRACE: Duration = Duration::from_secs(10);
/// "Approve all for 5 minutes".
pub const FIVE_MINUTES: Duration = Duration::from_secs(300);

// MARK: - The wire

/// What the hub learns about one request from its line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestInfo {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub tool: String,
    /// One line for the row.
    pub summary: String,
    /// Newlines kept, a generous cap: the tooltip.
    pub detail: String,
}

/// Helper side: Claude Code's hook input (snake_case) → our request line
/// (camelCase), newline-terminated. `None` for input that isn't a JSON
/// object.
pub fn request_line(hook_input: &[u8]) -> Option<Vec<u8>> {
    let Ok(Value::Object(input)) = serde_json::from_slice::<Value>(hook_input) else {
        return None;
    };
    let mut out = Map::new();
    let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("unknown");
    out.insert("toolName".into(), tool.into());
    if let Some(id) = input.get("session_id").and_then(Value::as_str) {
        out.insert("sessionId".into(), id.into());
    }
    if let Some(cwd) = input.get("cwd").and_then(Value::as_str) {
        out.insert("cwd".into(), cwd.into());
    }
    if let Some(tool_input) = input.get("tool_input") {
        out.insert("toolInput".into(), tool_input.clone());
    }
    let mut line = serde_json::to_vec(&Value::Object(out)).ok()?;
    line.push(b'\n');
    Some(line)
}

/// Hub side: a request line (without its newline) → what to show.
pub fn parse_request(line: &[u8]) -> Option<RequestInfo> {
    let Ok(Value::Object(request)) = serde_json::from_slice::<Value>(line) else {
        return None;
    };
    let text = |k: &str| request.get(k).and_then(Value::as_str).map(str::to_string);
    let tool = text("toolName").unwrap_or_else(|| "unknown".into());
    let input = request.get("toolInput");
    Some(RequestInfo {
        session_id: text("sessionId"),
        cwd: text("cwd"),
        summary: summary(&tool, input, 200, true),
        detail: summary(&tool, input, 4000, false),
        tool,
    })
}

/// "Bash: swift build" / "Edit: /path/to/file" / "WebFetch: {…}": the most
/// meaningful single field of the tool's input, else the input as compact
/// JSON; clipped to `max_chars` characters (counting the "…").
pub fn summary(tool: &str, input: Option<&Value>, max_chars: usize, collapse_newlines: bool) -> String {
    let mut detail = String::new();
    if let Some(Value::Object(fields)) = input {
        // The fields people recognize, in priority order.
        for key in ["command", "file_path", "url", "pattern", "prompt", "description"] {
            if let Some(v) = fields.get(key).and_then(Value::as_str)
                && !v.is_empty()
            {
                detail = v.to_string();
                break;
            }
        }
        if detail.is_empty() && !fields.is_empty() {
            detail = serde_json::to_string(fields).unwrap_or_default();
        }
    }
    if collapse_newlines {
        // Swift's `split(whereSeparator: \.isNewline)`: empty pieces go.
        detail = detail
            .split(['\n', '\r', '\u{0B}', '\u{0C}', '\u{85}', '\u{2028}', '\u{2029}'])
            .filter(|piece| !piece.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
    }
    if detail.chars().count() > max_chars {
        detail = detail.chars().take(max_chars.saturating_sub(1)).collect::<String>() + "…";
    }
    if detail.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}: {detail}")
    }
}

/// Hub → helper.
pub fn response_line(allow: bool) -> &'static [u8] {
    if allow {
        b"{\"behavior\":\"allow\"}\n"
    } else {
        b"{\"behavior\":\"deny\"}\n"
    }
}

/// Helper side: a verdict line → allow? `None` (no decision) for anything
/// but an explicit allow or deny.
pub fn parse_response(line: &[u8]) -> Option<bool> {
    let value: Value = serde_json::from_slice(line).ok()?;
    match value.get("behavior")?.as_str()? {
        "allow" => Some(true),
        "deny" => Some(false),
        _ => None,
    }
}

/// What the hook prints for Claude Code (the schema checked against the
/// CLI's own validator: hookSpecificOutput → PermissionRequest → decision).
pub fn decision_json(allow: bool) -> &'static str {
    if allow {
        r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
    } else {
        r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied via ClaudeShip"}}}"#
    }
}

// MARK: - Rules and reconciliation (pure)

/// A standing "approve all" for one Claude session. In memory only: a
/// standing approval shouldn't outlive the hub that granted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    Until(SystemTime),
    Session,
}

impl Rule {
    /// `"5m"`, `"session"`; `"off"` is `Ok(None)`; anything else `Err`.
    pub fn parse(text: &str, now: SystemTime) -> Result<Option<Rule>, ()> {
        match text {
            "5m" => Ok(Some(Rule::Until(now + FIVE_MINUTES))),
            "session" => Ok(Some(Rule::Session)),
            "off" => Ok(None),
            _ => Err(()),
        }
    }
}

pub fn rule_allows(rule: Option<Rule>, now: SystemTime) -> bool {
    match rule {
        Some(Rule::Until(expiry)) => now < expiry,
        Some(Rule::Session) => true,
        None => false,
    }
}

/// True when a status change *newer than the request's arrival* moved the
/// session out of `waiting`: the prompt was answered in the terminal. At
/// arrival the session still reads `busy` from before the prompt (an older
/// `statusUpdatedAt`: kept); the prompt itself reads `waiting` (kept).
pub fn resolved_in_terminal(waiting: bool, status_updated_at: Option<SystemTime>, received_at: SystemTime) -> bool {
    !waiting && status_updated_at.is_some_and(|t| t > received_at)
}

pub fn should_prune_unmatched(received_at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(received_at).is_ok_and(|d| d > UNMATCHED_GRACE)
}

/// What the registry says about one live Claude session.
#[derive(Clone, Copy, Debug)]
pub struct Registered {
    pub waiting: bool,
    pub status_updated_at: Option<SystemTime>,
}

/// The pendings to hang up on: answered in the terminal, or without a live
/// session for longer than the grace period.
pub fn stale(pendings: &[PendingApproval], live: &HashMap<String, Registered>, now: SystemTime) -> Vec<String> {
    pendings
        .iter()
        .filter(|p| match p.session_id.as_ref().and_then(|id| live.get(id)) {
            Some(r) => resolved_in_terminal(r.waiting, r.status_updated_at, p.received_at),
            None => should_prune_unmatched(p.received_at, now),
        })
        .map(|p| p.id.clone())
        .collect()
}

// MARK: - The hub's part

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingApproval {
    /// A UUID: the handle `POST /api/approve` answers by.
    pub id: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub tool: String,
    pub summary: String,
    pub detail: String,
    pub received_at: SystemTime,
}

impl PendingApproval {
    pub fn new(info: RequestInfo, received_at: SystemTime) -> PendingApproval {
        PendingApproval {
            id: uuid(),
            session_id: info.session_id,
            cwd: info.cwd,
            tool: info.tool,
            summary: info.summary,
            detail: info.detail,
            received_at,
        }
    }
}

/// A random (v4) UUID, lower case.
pub fn uuid() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // Unique within this hub is all that matters.
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b = nanos.to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The pendings and the rules, owned by the hub task.
#[derive(Default)]
pub struct Approvals {
    /// In arrival order, each with the channel its connection waits on:
    /// a value is the verdict; dropping the sender hangs up without one.
    pending: Vec<(PendingApproval, oneshot::Sender<bool>)>,
    rules: HashMap<String, Rule>,
}

impl Approvals {
    /// A request arrived. Answered at once (allow) when its session has a
    /// live rule; otherwise held. True when held.
    pub fn arrive(&mut self, approval: PendingApproval, verdict: oneshot::Sender<bool>, now: SystemTime) -> bool {
        let rule = approval
            .session_id
            .as_ref()
            .and_then(|id| self.rules.get(id).copied());
        if rule_allows(rule, now) {
            let _ = verdict.send(true);
            return false;
        }
        self.pending.push((approval, verdict));
        true
    }

    fn take(&mut self, id: &str) -> Option<oneshot::Sender<bool>> {
        let index = self.pending.iter().position(|(p, _)| p.id == id)?;
        Some(self.pending.remove(index).1)
    }

    /// Answer one. False if there is no such pending (answered, gone).
    pub fn respond(&mut self, id: &str, allow: bool) -> bool {
        match self.take(id) {
            Some(verdict) => {
                let _ = verdict.send(allow);
                true
            }
            None => false,
        }
    }

    /// Hang up without a verdict (or forget one whose helper went away).
    pub fn cancel(&mut self, id: &str) -> bool {
        self.take(id).is_some()
    }

    /// Set (`Some`) or clear a session's rule. Setting one also answers
    /// (allow) every request that session has pending. Returns how many.
    pub fn set_rule(&mut self, session_id: &str, rule: Option<Rule>) -> usize {
        let Some(rule) = rule else {
            self.rules.remove(session_id);
            return 0;
        };
        self.rules.insert(session_id.to_string(), rule);
        let ids: Vec<String> = self
            .pending
            .iter()
            .filter(|(p, _)| p.session_id.as_deref() == Some(session_id))
            .map(|(p, _)| p.id.clone())
            .collect();
        for id in &ids {
            self.respond(id, true);
        }
        ids.len()
    }

    /// Expired rules, and rules for sessions no longer registered, go.
    pub fn prune_rules(&mut self, live: &HashSet<String>, now: SystemTime) -> bool {
        let before = self.rules.len();
        self.rules
            .retain(|id, rule| live.contains(id) && rule_allows(Some(*rule), now));
        self.rules.len() != before
    }

    /// Whether the hub must keep reconciling: something pending, or a rule
    /// that has to be dropped when its session leaves the registry.
    pub fn needs_ticks(&self) -> bool {
        !self.pending.is_empty() || !self.rules.is_empty()
    }

    pub fn pendings(&self) -> Vec<PendingApproval> {
        self.pending.iter().map(|(p, _)| p.clone()).collect()
    }

    pub fn rules(&self) -> HashMap<String, Rule> {
        self.rules.clone()
    }
}

/// A rule as `/api/state` shows it: `{until: ms}`, `{session: true}`, or
/// null (none, or expired).
pub fn rule_json(rule: Option<Rule>, now: SystemTime, ms: impl Fn(SystemTime) -> i64) -> Value {
    match rule {
        Some(Rule::Until(t)) if now < t => json!({"until": ms(t)}),
        Some(Rule::Session) => json!({"session": true}),
        _ => Value::Null,
    }
}

// MARK: - The socket

/// Accept helpers for as long as the hub runs.
pub async fn serve(listener: UnixListener, hub: mpsc::UnboundedSender<Command>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(connection(stream, hub.clone()));
            }
            Err(e) => {
                log(&format!("approvals: accept: {e}"));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// One helper: its request line, then wait for whichever comes first —
/// the hub's verdict (write it, hang up), the hub dropping it (hang up),
/// or the helper going away (tell the hub).
async fn connection(mut stream: UnixStream, hub: mpsc::UnboundedSender<Command>) {
    let mut line = Vec::new();
    let mut buffer = [0u8; 4096];
    let request = loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(n) => line.extend_from_slice(&buffer[..n]),
        }
        if let Some(end) = line.iter().position(|&b| b == b'\n') {
            break parse_request(&line[..end]);
        }
        if line.len() >= MAX_LINE {
            return;
        }
    };
    let Some(info) = request else { return };
    let approval = PendingApproval::new(info, SystemTime::now());
    let id = approval.id.clone();
    let (verdict, mut answer) = oneshot::channel();
    if hub
        .send(Command::ApprovalArrived { approval, verdict })
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            answer = &mut answer => {
                if let Ok(allow) = answer {
                    let _ = stream.write_all(response_line(allow)).await;
                }
                let _ = stream.shutdown().await;
                return;
            }
            read = stream.read(&mut buffer) => match read {
                Ok(0) | Err(_) => {
                    let _ = hub.send(Command::ApprovalClosed { id });
                    return;
                }
                Ok(_) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    // MARK: approval wire format (the Swift self-test's vectors)

    #[test]
    fn wire_format() {
        let hook_input = br#"{"session_id":"s-1","cwd":"/tmp/proj","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm -rf build\necho done","description":"clean"}}"#;
        let line = request_line(hook_input).expect("a request line");
        assert_eq!(line.last(), Some(&b'\n'), "wire: request line newline-terminated");
        let req = parse_request(&line[..line.len() - 1]).unwrap();
        assert_eq!(req.session_id.as_deref(), Some("s-1"), "wire: sessionId round-trips");
        assert_eq!(req.cwd.as_deref(), Some("/tmp/proj"), "wire: cwd round-trips");
        assert_eq!(req.tool, "Bash", "wire: toolName round-trips");
        assert_eq!(req.summary, "Bash: rm -rf build echo done", "wire: row summary collapses newlines");
        assert_eq!(req.detail, "Bash: rm -rf build\necho done", "wire: hover detail keeps newlines");
        assert_eq!(request_line(b"nope"), None, "wire: garbage stdin → nil");
        assert_eq!(request_line(b"[1]"), None, "wire: not an object → nil");

        let unknown = request_line(b"{}").unwrap();
        assert_eq!(parse_request(&unknown[..unknown.len() - 1]).unwrap().tool, "unknown");

        let input = |v: Value| Some(v);
        assert_eq!(
            summary("Edit", input(json!({"file_path": "/a/b.swift"})).as_ref(), 200, true),
            "Edit: /a/b.swift",
            "wire: Edit summary uses file_path"
        );
        assert_eq!(summary("Mystery", None, 200, true), "Mystery", "wire: no input → bare tool name");
        assert_eq!(summary("Mystery", input(json!({})).as_ref(), 200, true), "Mystery");
        assert_eq!(
            summary("Bash", input(json!({"command": "", "url": "u"})).as_ref(), 200, true),
            "Bash: u",
            "an empty field is skipped"
        );
        assert_eq!(
            summary("Grep", input(json!({"pattern": "x", "url": "u", "prompt": "p"})).as_ref(), 200, true),
            "Grep: u",
            "priority: url before pattern before prompt"
        );
        assert_eq!(
            summary("WebX", input(json!({"b": 1, "a": [true]})).as_ref(), 200, true),
            r#"WebX: {"a":[true],"b":1}"#,
            "else compact JSON"
        );
        let long = summary("Bash", input(json!({"command": "x".repeat(300)})).as_ref(), 200, true);
        assert!(long.chars().count() <= 206, "wire: summary clipped");
        assert_eq!(long, format!("Bash: {}…", "x".repeat(199)));
        let detail = summary("Bash", input(json!({"command": "y".repeat(5000)})).as_ref(), 4000, false);
        assert!(detail.chars().count() <= 4006, "wire: detail clipped at its own cap");
        assert_eq!(
            summary("Bash", input(json!({"command": "a\r\n\nb\u{2028}c"})).as_ref(), 200, true),
            "Bash: a b c",
            "every newline kind, runs collapsed to one space"
        );
        assert_eq!(
            summary("Bash", input(json!({"command": "é".repeat(201)})).as_ref(), 200, true),
            format!("Bash: {}…", "é".repeat(199)),
            "clipped by characters, not bytes"
        );

        let strip = |l: &[u8]| l[..l.len() - 1].to_vec();
        assert_eq!(parse_response(&strip(response_line(true))), Some(true), "wire: allow round-trips");
        assert_eq!(parse_response(&strip(response_line(false))), Some(false), "wire: deny round-trips");
        assert_eq!(parse_response(b"{}"), None, "wire: missing behavior → nil");
        assert_eq!(parse_response(br#"{"behavior":"ask"}"#), None);
        assert_eq!(parse_response(b"garbage"), None);

        assert_eq!(
            decision_json(true),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#,
            "wire: allow decision JSON matches CLI schema"
        );
        assert!(decision_json(false).contains(r#""behavior":"deny""#), "wire: deny decision JSON has deny behavior");
    }

    // MARK: auto-approve rules

    #[test]
    fn rules() {
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000);
        assert!(!rule_allows(None, now), "rules: no rule → no auto-approve");
        assert!(rule_allows(Some(Rule::Session), now), "rules: forSession always allows");
        assert!(
            rule_allows(Some(Rule::Until(now + Duration::from_secs(60))), now),
            "rules: unexpired timer allows"
        );
        assert!(
            !rule_allows(Some(Rule::Until(now - Duration::from_secs(1))), now),
            "rules: expired timer denies"
        );
        assert_eq!(Rule::parse("5m", now), Ok(Some(Rule::Until(now + FIVE_MINUTES))));
        assert_eq!(Rule::parse("session", now), Ok(Some(Rule::Session)));
        assert_eq!(Rule::parse("off", now), Ok(None));
        assert_eq!(Rule::parse("forever", now), Err(()));
        let ms = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
        assert_eq!(rule_json(Some(Rule::Session), now, ms), json!({"session": true}));
        assert_eq!(
            rule_json(Some(Rule::Until(now + Duration::from_secs(1))), now, ms),
            json!({"until": 2_000_001_000i64})
        );
        assert_eq!(rule_json(Some(Rule::Until(now)), now, ms), Value::Null, "expired");
        assert_eq!(rule_json(None, now, ms), Value::Null);
    }

    fn pending(id: &str, session: Option<&str>, at: SystemTime) -> PendingApproval {
        PendingApproval {
            id: id.into(),
            session_id: session.map(str::to_string),
            cwd: None,
            tool: "Bash".into(),
            summary: "Bash".into(),
            detail: "Bash".into(),
            received_at: at,
        }
    }

    #[test]
    fn rules_answer_on_arrival_and_when_set() {
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000);
        let mut approvals = Approvals::default();
        let (tx, mut rx) = oneshot::channel();
        assert!(approvals.arrive(pending("a", Some("s"), now), tx, now));
        let (tx2, mut rx2) = oneshot::channel();
        assert!(approvals.arrive(pending("b", Some("t"), now), tx2, now));
        assert!(rx.try_recv().is_err(), "held, unanswered");
        assert_eq!(approvals.set_rule("s", Some(Rule::Session)), 1, "the pending one is answered");
        assert_eq!(rx.try_recv(), Ok(true));
        assert!(rx2.try_recv().is_err(), "another session's is not");
        let (tx3, mut rx3) = oneshot::channel();
        assert!(!approvals.arrive(pending("c", Some("s"), now), tx3, now), "answered on arrival");
        assert_eq!(rx3.try_recv(), Ok(true));
        assert_eq!(approvals.pendings().len(), 1);
        assert_eq!(approvals.set_rule("s", None), 0, "off");
        let (tx4, _rx4) = oneshot::channel();
        assert!(approvals.arrive(pending("d", Some("s"), now), tx4, now), "held again");

        assert!(approvals.respond("b", false));
        assert_eq!(rx2.try_recv(), Ok(false));
        assert!(!approvals.respond("b", true), "answered once");
        assert!(approvals.cancel("d"));
        assert!(approvals.pendings().is_empty());

        approvals.set_rule("s", Some(Rule::Until(now + Duration::from_secs(5))));
        approvals.set_rule("gone", Some(Rule::Session));
        let live: HashSet<String> = ["s".to_string()].into();
        assert!(approvals.prune_rules(&live, now), "a session that left loses its rule");
        assert_eq!(approvals.rules().len(), 1);
        assert!(approvals.needs_ticks(), "a standing rule keeps the ticker going");
        assert!(approvals.prune_rules(&live, now + Duration::from_secs(6)), "expired");
        assert!(approvals.rules().is_empty());
        assert!(!approvals.needs_ticks(), "nothing pending, no rule: the ticker stops");
    }

    // MARK: terminal-answer reconciliation

    #[test]
    fn reconciliation() {
        let recv = UNIX_EPOCH + Duration::from_secs(3_000_000);
        let s = Duration::from_secs;
        assert!(!resolved_in_terminal(true, Some(recv + s(1)), recv), "reconcile: still waiting → keep buttons");
        assert!(!resolved_in_terminal(false, Some(recv - s(60)), recv), "reconcile: pre-prompt busy (stale stateSince) → keep");
        assert!(resolved_in_terminal(false, Some(recv + s(3)), recv), "reconcile: fresh busy → answered in terminal, drop");
        assert!(!resolved_in_terminal(false, None, recv), "reconcile: no stateSince → keep (never guess)");
        assert!(!should_prune_unmatched(recv, recv + s(5)), "reconcile: unmatched within grace → keep");
        assert!(should_prune_unmatched(recv, recv + s(11)), "reconcile: unmatched past grace → prune");
        assert!(!should_prune_unmatched(recv, recv - s(11)), "a clock step back is not age");

        let live: HashMap<String, Registered> = [
            ("waiting".to_string(), Registered { waiting: true, status_updated_at: Some(recv + s(1)) }),
            ("answered".to_string(), Registered { waiting: false, status_updated_at: Some(recv + s(2)) }),
            ("before".to_string(), Registered { waiting: false, status_updated_at: Some(recv - s(2)) }),
        ]
        .into();
        let pendings = [
            pending("1", Some("waiting"), recv),
            pending("2", Some("answered"), recv),
            pending("3", Some("before"), recv),
            pending("4", Some("missing"), recv),
            pending("5", None, recv - s(20)),
        ];
        assert_eq!(stale(&pendings, &live, recv + s(3)), vec!["2", "5"]);
        assert_eq!(stale(&pendings, &live, recv + s(30)), vec!["2", "4", "5"]);
    }

    #[test]
    fn uuids() {
        let id = uuid();
        assert!(crate::web::state::is_session_id(&id), "{id}");
        assert_eq!(&id[14..15], "4");
        assert_ne!(uuid(), id);
    }

    // MARK: approval server ↔ helper socket round-trip

    #[tokio::test]
    async fn socket_round_trip() {
        let path = std::env::temp_dir().join(format!("cs-ap-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(serve(listener, tx));

        // Allow.
        let client = |line: &'static [u8]| {
            let path = path.clone();
            tokio::task::spawn_blocking(move || {
                use std::io::{Read, Write};
                let mut s = std::os::unix::net::UnixStream::connect(&path).unwrap();
                s.write_all(line).unwrap();
                let mut out = Vec::new();
                s.read_to_end(&mut out).unwrap();
                out
            })
        };
        let done = client(b"{\"toolName\":\"Bash\",\"toolInput\":{\"command\":\"ls\"},\"sessionId\":\"s-9\"}\n");
        let Some(Command::ApprovalArrived { approval, verdict }) = rx.recv().await else {
            panic!("server: request arrives");
        };
        assert_eq!(approval.summary, "Bash: ls", "server: summary parsed");
        assert_eq!(approval.session_id.as_deref(), Some("s-9"));
        verdict.send(true).unwrap();
        let out = done.await.unwrap();
        assert_eq!(parse_response(&out[..out.len() - 1]), Some(true), "server: client received allow");

        // Cancel: EOF, no verdict.
        let done = client(b"{\"toolName\":\"Read\"}\n");
        let Some(Command::ApprovalArrived { verdict, .. }) = rx.recv().await else {
            panic!("arrives");
        };
        drop(verdict);
        assert!(done.await.unwrap().is_empty(), "cancel: nothing written");

        // The helper goes away: the hub is told.
        let mut s = UnixStream::connect(&path).await.unwrap();
        s.write_all(b"{\"toolName\":\"Read\"}\n").await.unwrap();
        let Some(Command::ApprovalArrived { approval, verdict: _verdict }) = rx.recv().await else {
            panic!("arrives");
        };
        drop(s);
        match rx.recv().await {
            Some(Command::ApprovalClosed { id }) => assert_eq!(id, approval.id),
            _ => panic!("closed"),
        }

        // Garbage, and a line past the cap, are dropped without a word.
        let done = client(b"not json\n");
        assert!(done.await.unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
