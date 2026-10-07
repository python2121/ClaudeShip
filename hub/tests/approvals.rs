//! Remote approval end to end, against a private hub (`CLAUDESHIP_HOME`,
//! `HOME`, `CLAUDE_CONFIG_DIR`, and the project root all in a short scratch
//! directory; its own port): the real `claudeship permission-hook` with a
//! request on stdin, the hub's `approvals.sock`, `/api/state`, and the two
//! POSTs. Claude's registry is faked with files in the scratch
//! `CLAUDE_CONFIG_DIR/sessions`, each naming a `sleep` this test owns.
//!
//! Also the hook installer against a scratch settings file and
//! `install-service --no-load` against a scratch `HOME` (nothing is loaded
//! into launchd or systemd; the label carries a test-only bundle id so not
//! even `hub stop --force` could unload a real one).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const CLAUDESHIP: &str = env!("CARGO_BIN_EXE_claudeship");
const STAND_IN: &str = env!("CARGO_BIN_EXE_claudeship-stand-in");
const SECOND: Duration = Duration::from_secs(1);
const ALLOW: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
const DENY: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied via ClaudeShip"}}}"#;

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as f64
}

fn scratch(prefix: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    // Short: the socket paths must fit in sun_path (104 bytes on macOS).
    let home = PathBuf::from(format!(
        "/tmp/cs-{prefix}{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// The environment every command here runs with.
fn private(command: &mut Command, home: &Path) {
    command
        .env("CLAUDESHIP_HOME", home)
        .env("CLAUDESHIP_CMD", STAND_IN)
        .env("CLAUDESHIP_WEB", Path::new(env!("CARGO_MANIFEST_DIR")).join("../web"))
        .env("HOME", home.join("h"))
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .env("BUNDLE_ID", format!("test.claudeship.{}", std::process::id()))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME");
}

/// A live process for a fake registry entry to name.
struct Sleeper(Child);

impl Sleeper {
    fn new() -> Sleeper {
        Sleeper(Command::new("sleep").arg("600").spawn().unwrap())
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A port nothing listens on right now (the private hub's own).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Hub {
    home: PathBuf,
    port: u16,
    pid: i32,
}

impl Hub {
    fn start() -> Hub {
        let home = scratch("a");
        for dir in ["root/proj", "h", "claude/sessions"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        // A port that was free a moment ago can be taken before the hub
        // binds it (another test's hub, an outgoing connection), and the
        // hub then retries only every 10 s: start again on another.
        for attempt in 1.. {
            let port = free_port();
            std::fs::write(
                home.join("config.json"),
                json!({"port": port, "root": home.join("root")}).to_string(),
            )
            .unwrap();
            let mut hub = Hub { home: home.clone(), port, pid: 0 };
            let started = hub.cli(&["hub", "start"]);
            assert!(started.status.success(), "hub start: {started:?}");
            hub.pid = hub.status()["pid"].as_i64().unwrap() as i32;
            // This hub listening, not whoever else might hold the port.
            let deadline = Instant::now() + 5 * SECOND;
            while Instant::now() < deadline {
                if hub.status()["webListening"] == true {
                    return hub;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(attempt < 5, "the web server never listened (5 ports tried)");
            let _ = hub.cli(&["hub", "stop", "--force"]);
            // Not dropped: that would remove the home.
            std::mem::forget(hub);
        }
        unreachable!()
    }

    fn status(&self) -> Value {
        serde_json::from_slice(&self.cli(&["hub", "status", "--json"]).stdout).unwrap()
    }

    fn cli(&self, args: &[&str]) -> Output {
        let mut command = Command::new(CLAUDESHIP);
        command.args(args);
        private(&mut command, &self.home);
        command.output().unwrap()
    }

    /// Claude's registry entry for a session.
    fn register(&self, sleeper: &Sleeper, session_id: &str, status: &str, updated_ms: f64) {
        let path = self.home.join(format!("claude/sessions/{}.json", sleeper.pid()));
        let entry = json!({
            "pid": sleeper.pid(), "sessionId": session_id,
            "cwd": self.home.join("root/proj"), "status": status,
            "startedAt": now_ms() - 60_000.0, "statusUpdatedAt": updated_ms,
        });
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, entry.to_string()).unwrap();
        std::fs::rename(temp, path).unwrap();
    }

    /// `claudeship permission-hook`, as Claude Code runs it.
    fn helper(&self, session_id: &str, command: &str) -> Helper {
        let mut c = Command::new(CLAUDESHIP);
        c.arg("permission-hook")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        private(&mut c, &self.home);
        let mut child = c.spawn().unwrap();
        let input = json!({
            "session_id": session_id, "cwd": "/tmp", "hook_event_name": "PermissionRequest",
            "tool_name": "Bash", "tool_input": {"command": command, "description": "test"},
        });
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        Helper(Some(child))
    }

    fn http(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let token = std::fs::read_to_string(self.home.join("token")).unwrap();
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nCookie: claude_ship={token}\r\n\
             Origin: http://localhost:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
            port = self.port,
            token = token.trim(),
        );
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.set_read_timeout(Some(5 * SECOND)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&raw[..end]).into_owned();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_slice(&raw[end + 4..]).unwrap_or(Value::Null))
    }

    fn state(&self) -> Value {
        let (status, state) = self.http("GET", "/api/state", None);
        assert_eq!(status, 200);
        state
    }

    /// The session entry for a Claude session id in `/api/state`.
    fn entry(&self, session_id: &str) -> Option<Value> {
        let state = self.state();
        let mut all: Vec<Value> = state["elsewhere"].as_array().cloned().unwrap_or_default();
        for project in state["projects"].as_array().unwrap() {
            all.extend(project["sessions"].as_array().unwrap().iter().cloned());
        }
        all.into_iter().find(|s| s["sessionId"] == session_id)
    }

    fn approvals(&self, session_id: &str) -> Vec<Value> {
        self.entry(session_id)
            .and_then(|e| e["approvals"].as_array().cloned())
            .unwrap_or_default()
    }

    /// Poll the state until `session_id` shows `n` requests.
    fn wait_for_approvals(&self, session_id: &str, n: usize) -> Vec<Value> {
        let deadline = Instant::now() + 5 * SECOND;
        loop {
            let approvals = self.approvals(session_id);
            if approvals.len() == n {
                return approvals;
            }
            assert!(Instant::now() < deadline, "expected {n} approval(s), have {approvals:?}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.cli(&["hub", "stop", "--force"]);
        // SAFETY: probing with signal 0.
        let alive = self.pid > 0 && unsafe { libc::kill(self.pid, 0) } == 0;
        let _ = std::fs::remove_dir_all(&self.home);
        if !std::thread::panicking() {
            assert!(!alive, "the private hub (pid {}) survived hub stop --force", self.pid);
        }
    }
}

struct Helper(Option<Child>);

impl Helper {
    /// Its stdout once it exits (asserting exit 0), or `None` if it is
    /// still running at the deadline.
    fn finish_within(&mut self, limit: Duration) -> Option<String> {
        let deadline = Instant::now() + limit;
        let child = self.0.as_mut().unwrap();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "the helper always exits 0: {status:?}");
                let mut out = String::new();
                child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
                self.0 = None;
                return Some(out);
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.kill();
    }
}

const SID: &str = "11111111-2222-4333-8444-555555555555";
const SID2: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

#[test]
fn approve_and_deny_through_the_api() {
    let hub = Hub::start();
    let sleeper = Sleeper::new();
    hub.register(&sleeper, SID, "busy", now_ms() - 30_000.0);
    let state = hub.state();
    assert_eq!(state["protocol"], 3);
    assert_eq!(state["approvalsSupported"], true);
    let entry = hub.entry(SID).expect("the registered session");
    assert_eq!(entry["approvals"], json!([]));
    assert_eq!(entry["autoApprove"], Value::Null);
    assert_eq!(entry["status"], "busy");

    let mut helper = hub.helper(SID, "rm -rf build\necho done");
    let approvals = hub.wait_for_approvals(SID, 1);
    let a = &approvals[0];
    assert_eq!(a["tool"], "Bash");
    assert_eq!(a["summary"], "Bash: rm -rf build echo done");
    assert_eq!(a["detail"], "Bash: rm -rf build\necho done");
    let received = a["receivedAt"].as_f64().unwrap();
    assert!((received - now_ms()).abs() < 10_000.0, "receivedAt in ms");
    assert_eq!(hub.entry(SID).unwrap()["status"], "waiting", "a session with a request is waiting");
    assert_eq!(helper.finish_within(Duration::from_millis(300)), None, "the helper blocks");

    let id = a["id"].as_str().unwrap().to_string();
    assert_eq!(hub.http("POST", "/api/approve", Some(json!({"id": id, "allow": true}))), (200, json!({"ok": true})));
    assert_eq!(helper.finish_within(5 * SECOND).as_deref(), Some(format!("{ALLOW}\n").as_str()));
    assert_eq!(
        hub.http("POST", "/api/approve", Some(json!({"id": id, "allow": true}))),
        (404, json!({"error": "no such approval"})),
        "answered once"
    );
    assert!(hub.approvals(SID).is_empty(), "gone from the state at once");

    let mut helper = hub.helper(SID, "ls");
    let id = hub.wait_for_approvals(SID, 1)[0]["id"].as_str().unwrap().to_string();
    assert_eq!(hub.http("POST", "/api/approve", Some(json!({"id": id, "allow": false}))).0, 200);
    assert_eq!(helper.finish_within(5 * SECOND).as_deref(), Some(format!("{DENY}\n").as_str()));

    assert_eq!(hub.http("POST", "/api/approve", Some(json!({"id": "x"}))).0, 400, "allow is required");
    assert_eq!(hub.http("GET", "/api/approve", None).0, 404, "a GET is a file request, as for the other POST routes");

    // The helper goes away (Claude Code killed it): the request goes too.
    let mut helper = hub.helper(SID, "sleep 1");
    hub.wait_for_approvals(SID, 1);
    helper.kill();
    hub.wait_for_approvals(SID, 0);
}

#[test]
fn no_hub_means_no_decision() {
    let home = scratch("n");
    let mut c = Command::new(CLAUDESHIP);
    c.arg("permission-hook").stdin(Stdio::piped()).stdout(Stdio::piped());
    private(&mut c, &home);
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"tool_name":"Bash"}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert!(!home.join("hub.sock").exists(), "the helper never starts a hub");
    // Garbage on stdin, likewise.
    let mut c = Command::new(CLAUDESHIP);
    c.arg("permission-hook").stdin(Stdio::piped()).stdout(Stdio::piped());
    private(&mut c, &home);
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"not json").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success() && out.stdout.is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn the_socket_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let hub = Hub::start();
    let mode = std::fs::metadata(hub.home.join("approvals.sock")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn standing_rules_answer_pending_and_new_requests() {
    let hub = Hub::start();
    let (one, two) = (Sleeper::new(), Sleeper::new());
    hub.register(&one, SID, "busy", now_ms() - 30_000.0);
    hub.register(&two, SID2, "busy", now_ms() - 30_000.0);

    let mut first = hub.helper(SID, "one");
    let mut second = hub.helper(SID, "two");
    let mut other = hub.helper(SID2, "other");
    hub.wait_for_approvals(SID, 2);
    hub.wait_for_approvals(SID2, 1);

    assert_eq!(
        hub.http("POST", "/api/auto-approve", Some(json!({"sessionId": SID, "rule": "5m"}))),
        (200, json!({"ok": true}))
    );
    for helper in [&mut first, &mut second] {
        assert_eq!(helper.finish_within(5 * SECOND).as_deref(), Some(format!("{ALLOW}\n").as_str()));
    }
    assert_eq!(other.finish_within(Duration::from_millis(300)), None, "another session's request waits");
    let rule = &hub.entry(SID).unwrap()["autoApprove"];
    let until = rule["until"].as_f64().expect("{until: ms}");
    assert!((until - (now_ms() + 300_000.0)).abs() < 10_000.0, "five minutes from now: {rule}");
    assert_eq!(hub.entry(SID2).unwrap()["autoApprove"], Value::Null);

    // A new request for that session is answered on arrival.
    let mut third = hub.helper(SID, "three");
    assert_eq!(third.finish_within(5 * SECOND).as_deref(), Some(format!("{ALLOW}\n").as_str()));

    // For the session; then off.
    assert_eq!(hub.http("POST", "/api/auto-approve", Some(json!({"sessionId": SID2, "rule": "session"}))).0, 200);
    assert_eq!(other.finish_within(5 * SECOND).as_deref(), Some(format!("{ALLOW}\n").as_str()));
    assert_eq!(hub.entry(SID2).unwrap()["autoApprove"], json!({"session": true}));
    assert_eq!(hub.http("POST", "/api/auto-approve", Some(json!({"sessionId": SID2, "rule": "off"}))).0, 200);
    assert_eq!(hub.entry(SID2).unwrap()["autoApprove"], Value::Null);
    let mut fourth = hub.helper(SID2, "four");
    hub.wait_for_approvals(SID2, 1);
    assert_eq!(fourth.finish_within(Duration::from_millis(100)), None, "held again");

    assert_eq!(hub.http("POST", "/api/auto-approve", Some(json!({"sessionId": SID, "rule": "forever"}))).0, 400);
    assert_eq!(hub.http("POST", "/api/auto-approve", Some(json!({"rule": "5m"}))).0, 400);
    assert_eq!(
        hub.http("POST", "/api/auto-approve", Some(json!({"sessionId": "not-a-session", "rule": "5m"}))).0,
        400,
        "a sessionId must be a UUID"
    );

    // The session leaves the registry: its rule goes with it.
    drop(one);
    let deadline = Instant::now() + 5 * SECOND;
    while hub.entry(SID).is_some() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(200));
    }
    let one = Sleeper::new();
    hub.register(&one, SID, "busy", now_ms() - 30_000.0);
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(hub.entry(SID).unwrap()["autoApprove"], Value::Null, "the rule was pruned");
    let mut fifth = hub.helper(SID, "five");
    hub.wait_for_approvals(SID, 1);
    assert_eq!(fifth.finish_within(Duration::from_millis(100)), None, "and no longer answers");
}

#[test]
fn a_session_rule_ends_with_its_session_even_with_no_screen_polling() {
    let hub = Hub::start();
    let one = Sleeper::new();
    hub.register(&one, SID, "busy", now_ms() - 30_000.0);
    hub.entry(SID).expect("registered");
    assert_eq!(hub.http("POST", "/api/auto-approve", Some(json!({"sessionId": SID, "rule": "session"}))).0, 200);
    // The session ends and, with nobody looking, comes back under the same
    // id (`claude --resume`): the hub's own ticker must have dropped the rule.
    drop(one);
    std::thread::sleep(Duration::from_millis(4500));
    let again = Sleeper::new();
    hub.register(&again, SID, "busy", now_ms() - 30_000.0);
    let mut helper = hub.helper(SID, "after resume");
    assert_eq!(helper.finish_within(Duration::from_millis(1500)), None, "not answered by the old rule");
    assert_eq!(hub.wait_for_approvals(SID, 1).len(), 1, "held for a screen");
}

#[test]
fn answered_in_the_terminal_hangs_up_without_a_verdict() {
    let hub = Hub::start();
    let sleeper = Sleeper::new();
    // At arrival the session reads `busy` from before the prompt: kept.
    hub.register(&sleeper, SID, "busy", now_ms() - 30_000.0);
    let mut helper = hub.helper(SID, "make");
    hub.wait_for_approvals(SID, 1);
    // The prompt itself: waiting, newer. Kept.
    hub.register(&sleeper, SID, "waiting", now_ms());
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(hub.approvals(SID).len(), 1, "still waiting on the prompt");
    assert_eq!(helper.finish_within(Duration::ZERO), None);
    // Answered in the terminal: busy again, newer than the request. No
    // screen polls from here on — the hub notices by itself.
    hub.register(&sleeper, SID, "busy", now_ms() + 1.0);
    assert_eq!(
        helper.finish_within(6 * SECOND).as_deref(),
        Some(""),
        "the helper is hung up on with no decision"
    );
    assert!(hub.approvals(SID).is_empty());
}

#[test]
fn a_request_without_a_session_is_dropped_after_ten_seconds() {
    let hub = Hub::start();
    let mut helper = hub.helper("99999999-2222-4333-8444-555555555555", "orphan");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(helper.finish_within(Duration::from_secs(8)), None, "kept through the grace period");
    assert_eq!(helper.finish_within(Duration::from_secs(8)).as_deref(), Some(""), "then dropped, unanswered");
}

#[test]
fn a_change_reaches_the_next_poll_however_recent_the_cache() {
    let hub = Hub::start();
    let sleeper = Sleeper::new();
    hub.register(&sleeper, SID, "busy", now_ms() - 30_000.0);
    for _ in 0..3 {
        // Freshly cached; a request arrives; the next poll, well inside the
        // cache's second, shows it.
        let cached = Instant::now();
        assert!(hub.entry(SID).is_some());
        let mut helper = hub.helper(SID, "x");
        std::thread::sleep(Duration::from_millis(250));
        let approvals = hub.approvals(SID);
        if cached.elapsed() > Duration::from_millis(900) {
            continue; // a slow machine: the cache would have expired anyway
        }
        assert_eq!(approvals.len(), 1, "a new request invalidates the cache");
        // The helper goes away; the next poll no longer shows it.
        let cached = Instant::now();
        hub.state();
        helper.kill();
        std::thread::sleep(Duration::from_millis(250));
        let approvals = hub.approvals(SID);
        if cached.elapsed() > Duration::from_millis(900) {
            continue;
        }
        assert!(approvals.is_empty(), "a closed request invalidates the cache");
        return;
    }
    panic!("never managed a poll inside the cache's second");
}

// MARK: The hook installer

fn run(home: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(CLAUDESHIP);
    command.args(args);
    private(&mut command, home);
    command.output().unwrap()
}

#[test]
fn install_hook_merges_into_the_settings() {
    let home = scratch("k");
    let settings = home.join("claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let swift = json!({"type": "command", "command": "/Applications/ClaudeShip.app/Contents/MacOS/ClaudeShip --permission-hook", "timeout": 86400});
    let theirs = json!({"type": "command", "command": "/usr/local/bin/audit", "timeout": 5});
    let original = json!({
        "model": "opus",
        "permissions": {"allow": ["Bash(ls)"]},
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}],
            "PermissionRequest": [
                {"matcher": "Bash", "hooks": [theirs.clone()]},
                {"hooks": [swift]},
            ],
        },
    });
    std::fs::write(&settings, serde_json::to_string_pretty(&original).unwrap()).unwrap();

    let out = run(&home, &["hub", "install-hook"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("installed in") && text.contains(settings.to_str().unwrap()), "{text}");
    let after: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    let ours = format!("{} permission-hook", std::fs::canonicalize(CLAUDESHIP).unwrap().display());
    assert_eq!(after["model"], "opus");
    assert_eq!(after["permissions"], original["permissions"]);
    assert_eq!(after["hooks"]["Stop"], original["hooks"]["Stop"]);
    assert_eq!(
        after["hooks"]["PermissionRequest"],
        json!([
            {"matcher": "Bash", "hooks": [theirs.clone()]},
            {"hooks": [{"type": "command", "command": ours, "timeout": 86400}]},
        ]),
        "the Swift helper replaced, the other hook untouched"
    );

    let out = run(&home, &["hub", "install-hook"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Nothing to do"), "idempotent");

    let out = run(&home, &["hub", "uninstall-hook"]);
    assert!(out.status.success());
    let removed: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(removed["hooks"]["PermissionRequest"], json!([{"matcher": "Bash", "hooks": [theirs]}]));
    assert_eq!(removed["hooks"]["Stop"], original["hooks"]["Stop"]);
    let out = run(&home, &["hub", "uninstall-hook"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Nothing to do"));

    // Not an object: refused, untouched.
    std::fs::write(&settings, "[]").unwrap();
    let out = run(&home, &["hub", "install-hook"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), "[]");

    // No settings at all: created.
    std::fs::remove_file(&settings).unwrap();
    assert!(run(&home, &["hub", "install-hook"]).status.success());
    let created: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(created["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"], 86400);
    let _ = std::fs::remove_dir_all(&home);
}

// MARK: The login service (written, never loaded)

#[test]
fn install_service_writes_the_unit() {
    let home = scratch("s");
    std::fs::create_dir_all(home.join("h")).unwrap();
    let binary = std::fs::canonicalize(CLAUDESHIP).unwrap().display().to_string();
    let out = run(&home, &["hub", "install-service", "--no-load"]);
    assert!(out.status.success(), "{out:?}");
    let printed = String::from_utf8_lossy(&out.stdout).into_owned();
    if cfg!(target_os = "macos") {
        let label = format!("test.claudeship.{}.hub", std::process::id());
        let path = home.join(format!("h/Library/LaunchAgents/{label}.plist"));
        assert!(printed.contains(&format!("wrote {}", path.display())), "{printed}");
        assert!(printed.contains("not loaded"), "{printed}");
        let plist = std::fs::read_to_string(&path).unwrap();
        assert!(plist.contains(&format!("<string>{label}</string>")));
        assert!(plist.contains(&format!("<string>{binary}</string>\n\t\t<string>hub</string>\n\t\t<string>run</string>")));
        assert!(plist.contains(&format!("<string>{}</string>", home.join("hub.log").display())));
        assert!(plist.contains(&format!("<key>CLAUDESHIP_HOME</key>\n\t\t<string>{}</string>", home.display())));
        if Command::new("plutil").arg("-lint").arg(&path).output().is_ok_and(|o| !o.status.success()) {
            panic!("plutil rejects the plist");
        }

        // `hub stop` under the service says what it means.
        // Never port 0: that falls back to 7433, the real hub's.
        let config = json!({"port": free_port(), "root": home.join("h")});
        std::fs::write(home.join("config.json"), config.to_string()).unwrap();
        assert!(run(&home, &["hub", "start"]).status.success());
        struct Stop<'a>(&'a Path);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                let _ = run(self.0, &["hub", "stop", "--force"]);
            }
        }
        let _stop = Stop(&home);
        let stopped = run(&home, &["hub", "stop"]);
        assert!(stopped.status.success(), "{stopped:?}");
        assert!(String::from_utf8_lossy(&stopped.stdout).contains("starts it again"), "{stopped:?}");

        let out = run(&home, &["hub", "uninstall-service", "--no-load"]);
        assert!(out.status.success());
        assert!(!path.exists());

        // Without BUNDLE_ID in the environment: the one built in, else the
        // example label.
        let mut command = Command::new(CLAUDESHIP);
        command.args(["hub", "install-service", "--no-load"]);
        private(&mut command, &home);
        command.env_remove("BUNDLE_ID");
        assert!(command.output().unwrap().status.success());
        let built_in = option_env!("BUNDLE_ID")
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .unwrap_or("com.example.claudeship");
        let default = home.join(format!("h/Library/LaunchAgents/{built_in}.hub.plist"));
        assert!(default.exists());
        let mut command = Command::new(CLAUDESHIP);
        command.args(["hub", "uninstall-service", "--no-load"]);
        private(&mut command, &home);
        command.env_remove("BUNDLE_ID");
        assert!(command.output().unwrap().status.success());
        assert!(!default.exists());
    } else {
        let path = home.join("h/.config/systemd/user/claudeship-hub.service");
        let unit = std::fs::read_to_string(&path).unwrap();
        assert!(unit.contains(&format!("ExecStart={binary} hub run")));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(printed.contains("loginctl enable-linger"));
        assert!(run(&home, &["hub", "uninstall-service", "--no-load"]).status.success());
        assert!(!path.exists());
    }
    let _ = std::fs::remove_dir_all(&home);
}
