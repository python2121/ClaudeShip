//! The hub end to end: the built `claudeship` binary run as a private hub
//! (`CLAUDESHIP_HOME` in a short scratch directory, `CLAUDESHIP_CMD`
//! pointing at the stand-in), driven over its Unix socket with the same
//! frames a terminal client sends.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CLAUDESHIP: &str = env!("CARGO_BIN_EXE_claudeship");
const STAND_IN: &str = env!("CARGO_BIN_EXE_claudeship-stand-in");
/// `frame::PROTOCOL`; the test can't import from a binary crate.
const PROTOCOL: u32 = 3;

const HELLO: u8 = b'H';
const INPUT: u8 = b'I';
const RESIZE: u8 = b'R';
const ATTACHED: u8 = b'A';
const OUTPUT: u8 = b'O';
const EXIT: u8 = b'X';
const ERROR: u8 = b'E';
const REPLY: u8 = b'L';

fn encode(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![kind];
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Give a private home a config with a free port of its own: the default
/// is the real hub's.
fn own_port(home: &std::path::Path) -> u16 {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::fs::write(home.join("config.json"), json!({"port": port}).to_string()).unwrap();
    port
}

struct PrivateHub {
    home: PathBuf,
    port: u16,
    pid: i32,
    program: String,
}

impl PrivateHub {
    fn start() -> PrivateHub {
        PrivateHub::start_with(STAND_IN)
    }

    /// A hub of its own, running `program` for every session.
    fn start_with(program: &str) -> PrivateHub {
        static N: AtomicUsize = AtomicUsize::new(0);
        // Short: the socket path must fit in sun_path (104 bytes on macOS).
        let home = PathBuf::from(format!(
            "/tmp/cs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let port = own_port(&home);
        let mut hub = PrivateHub {
            port,
            home,
            pid: 0,
            program: program.to_string(),
        };
        let started = hub.cli(&["hub", "start"]);
        assert!(started.status.success(), "hub start: {started:?}");
        hub.pid = hub.status()["pid"].as_i64().unwrap() as i32;
        hub
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.cli_os(args.iter().map(std::ffi::OsStr::new))
    }

    fn cli_os<'a>(&self, args: impl IntoIterator<Item = &'a std::ffi::OsStr>) -> Output {
        Command::new(CLAUDESHIP)
            .args(args)
            .env("CLAUDESHIP_HOME", &self.home)
            .env("CLAUDESHIP_CMD", &self.program)
            .output()
            .unwrap()
    }

    fn status(&self) -> Value {
        let out = self.cli(&["hub", "status", "--json"]);
        assert!(out.status.success(), "status: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn connect(&self) -> Client {
        let stream = UnixStream::connect(self.home.join("hub.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        Client {
            stream,
            pending: Vec::new(),
            output: Vec::new(),
            closed: false,
        }
    }

    /// A client that launched `args` (24×80) and has been told it's attached.
    fn launch(&self, args: &[&str]) -> (Client, String) {
        self.launch_sized(args, 24, 80)
    }

    fn launch_sized(&self, args: &[&str], rows: u16, cols: u16) -> (Client, String) {
        let mut client = self.connect();
        client.hello(json!({
            "op": "launch", "protocol": PROTOCOL, "cwd": "/tmp", "args": args,
            "env": {"PATH": "/usr/bin:/bin", "LANG": "en_US.UTF-8"},
            "rows": rows, "cols": cols,
        }));
        let (kind, payload) = client
            .frame(Duration::from_secs(5))
            .expect("an answer to launch");
        assert_eq!(
            kind,
            ATTACHED,
            "launch: {}",
            String::from_utf8_lossy(&payload)
        );
        let attached: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(attached["protocol"], PROTOCOL);
        (client, attached["id"].as_str().unwrap().to_string())
    }

    fn attach(&self, id: &str, rows: u16, cols: u16) -> Client {
        let mut client = self.connect();
        client.hello(
            json!({"op": "attach", "protocol": PROTOCOL, "id": id, "rows": rows, "cols": cols}),
        );
        client
    }

    fn request(&self, hello: Value) -> (u8, Value) {
        let mut client = self.connect();
        client.hello(hello);
        let (kind, payload) = client.frame(Duration::from_secs(5)).expect("a reply");
        (
            kind,
            serde_json::from_slice(&payload).unwrap_or(Value::Null),
        )
    }
}

impl Drop for PrivateHub {
    fn drop(&mut self) {
        // Leave nothing behind, even when a test failed half-way: stop the
        // hub, then make sure every session it had is gone too (a stand-in
        // that ignores SIGHUP would outlive the hub's hang-up).
        let leaders: Vec<i32> = Command::new(CLAUDESHIP)
            .args(["hub", "status", "--json"])
            .env("CLAUDESHIP_HOME", &self.home)
            .output()
            .ok()
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
            .and_then(|v| {
                v["sessions"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|s| s["pid"].as_i64())
                        .map(|p| p as i32)
                        .collect()
                })
            })
            .unwrap_or_default();
        let _ = self.cli(&["hub", "stop", "--force"]);
        for leader in leaders {
            // SAFETY: signalling process groups this test's hub created.
            unsafe {
                libc::kill(-leader, libc::SIGTERM);
                std::thread::sleep(Duration::from_millis(100));
                libc::kill(-leader, libc::SIGKILL);
            }
        }
        // SAFETY: probing with signal 0.
        let alive = self.pid > 0 && unsafe { libc::kill(self.pid, 0) } == 0;
        let _ = std::fs::remove_dir_all(&self.home);
        if !std::thread::panicking() {
            assert!(
                !alive,
                "the private hub (pid {}) survived hub stop --force",
                self.pid
            );
        }
    }
}

struct Client {
    stream: UnixStream,
    pending: Vec<u8>,
    /// Every output byte received so far.
    output: Vec<u8>,
    closed: bool,
}

impl Client {
    fn send(&mut self, kind: u8, payload: &[u8]) {
        self.stream.write_all(&encode(kind, payload)).unwrap();
    }

    fn hello(&mut self, request: Value) {
        self.send(HELLO, &serde_json::to_vec(&request).unwrap());
    }

    fn input(&mut self, bytes: &[u8]) {
        self.send(INPUT, bytes);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.send(
            RESIZE,
            &serde_json::to_vec(&json!({"rows": rows, "cols": cols})).unwrap(),
        );
    }

    /// The next frame (output is also collected into `output`).
    fn frame(&mut self, timeout: Duration) -> Option<(u8, Vec<u8>)> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.pending.len() >= 5 {
                let n = u32::from_be_bytes(self.pending[1..5].try_into().unwrap()) as usize;
                if self.pending.len() >= 5 + n {
                    let kind = self.pending[0];
                    let payload = self.pending[5..5 + n].to_vec();
                    self.pending.drain(..5 + n);
                    if kind == OUTPUT {
                        self.output.extend_from_slice(&payload);
                    }
                    return Some((kind, payload));
                }
            }
            if self.closed || Instant::now() > deadline {
                return None;
            }
            let mut buffer = [0u8; 65_536];
            match self.stream.read(&mut buffer) {
                Ok(0) => self.closed = true,
                Ok(n) => self.pending.extend_from_slice(&buffer[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => self.closed = true,
            }
        }
    }

    fn output_text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    /// Read until the output contains `needle`; fails the test otherwise.
    fn wait_for(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.output_text().contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || (self.frame(left).is_none() && self.closed) {
                panic!("never saw {needle:?}; got {:?}", self.output_text());
            }
        }
    }

    /// Everything that arrives in the next `period`.
    fn drain_for(&mut self, period: Duration) {
        let deadline = Instant::now() + period;
        while Instant::now() < deadline {
            if self
                .frame(deadline.saturating_duration_since(Instant::now()))
                .is_none()
                && self.closed
            {
                return;
            }
        }
    }

    /// Read until the exit frame; its code.
    fn exit_code(&mut self, timeout: Duration) -> i64 {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.frame(left) {
                Some((EXIT, payload)) => {
                    return serde_json::from_slice::<Value>(&payload).unwrap()["code"]
                        .as_i64()
                        .unwrap();
                }
                Some(_) => {}
                None => panic!("no exit frame; output {:?}", self.output_text()),
            }
        }
    }
}

const SECOND: Duration = Duration::from_secs(1);

#[test]
fn launch_echo_and_exit() {
    let hub = PrivateHub::start();
    let (mut a, id) = hub.launch(&["echo"]);
    assert_eq!(id.len(), 6, "a six-hex-digit id");
    a.wait_for("READY", 5 * SECOND);
    a.input(b"hello");
    a.wait_for("hello", 5 * SECOND);
    let status = hub.status();
    assert_eq!(status["sessions"][0]["id"], id.as_str());
    assert_eq!(status["sessions"][0]["clients"], 1);
    assert_eq!(status["sessions"][0]["origin"], "terminal");
    a.input(b"q");
    assert_eq!(a.exit_code(5 * SECOND), 0);
}

#[test]
fn exit_code_is_the_programs() {
    let hub = PrivateHub::start();
    let (mut a, _) = hub.launch(&["exit", "7"]);
    assert_eq!(a.exit_code(5 * SECOND), 7);
    assert!(a.output_text().contains("BYE"));
}

#[test]
fn late_attach_gets_the_replay_with_modes_restored_and_no_query() {
    let hub = PrivateHub::start();
    let (mut a, id) = hub.launch(&["alt"]);
    a.wait_for("ALT-SCREEN", 5 * SECOND);
    assert!(
        a.output_text().contains("\x1b[c"),
        "live clients get the query untouched"
    );

    let mut b = hub.attach(&id, 24, 80);
    b.wait_for("ALT-SCREEN", 5 * SECOND);
    let replay = b.output_text();
    assert!(
        !replay.contains("PRE"),
        "the wiped screen isn't replayed: {replay:?}"
    );
    assert!(
        replay.contains("\x1b[?2004h"),
        "bracketed paste, set before the wipe, restored: {replay:?}"
    );
    assert!(
        replay.contains("\x1b[?1000h"),
        "mouse mode restored: {replay:?}"
    );
    let alt = replay
        .find("\x1b[?1049h")
        .expect("alt screen in the replay");
    assert!(alt < replay.find("ALT-SCREEN").unwrap());
    assert!(
        !replay.contains("\x1b[c"),
        "a replayed query would be answered twice: {replay:?}"
    );
    a.input(b"q");
    assert_eq!(a.exit_code(5 * SECOND), 0);
    assert_eq!(
        b.exit_code(5 * SECOND),
        0,
        "every attached screen hears the exit"
    );
}

#[test]
fn size_follows_typing_not_a_focus_report() {
    let hub = PrivateHub::start();
    let (mut a, id) = hub.launch_sized(&["size"], 24, 80);
    a.wait_for("SIZE 24 80", 5 * SECOND);
    // Attaching claims the size.
    let mut b = hub.attach(&id, 30, 100);
    a.wait_for("SIZE 30 100", 5 * SECOND);
    b.wait_for("SIZE 30 100", 5 * SECOND);
    // A terminal saying it gained focus is not the person choosing it.
    a.input(b"\x1b[I");
    a.drain_for(SECOND / 2);
    assert_eq!(
        a.output_text().matches("SIZE 24 80").count(),
        1,
        "{:?}",
        a.output_text()
    );
    // Typing is.
    a.input(b"x");
    a.wait_for("x", 5 * SECOND);
    let deadline = Instant::now() + 5 * SECOND;
    while a.output_text().matches("SIZE 24 80").count() < 2 {
        assert!(
            Instant::now() < deadline,
            "typing didn't claim the size: {:?}",
            a.output_text()
        );
        a.frame(SECOND / 10);
    }
    // And the other screen's own resize takes it back.
    b.resize(40, 120);
    a.wait_for("SIZE 40 120", 5 * SECOND);
    a.input(b"q");
    assert_eq!(a.exit_code(5 * SECOND), 0);
}

#[test]
fn an_8k_paste_arrives_whole() {
    let hub = PrivateHub::start();
    let (mut a, _) = hub.launch(&["count", "8192"]);
    // As with Claude: a paste is only safe once the program has put the
    // terminal in raw mode (a cooked line holds about 1 KB).
    a.wait_for("READY", 5 * SECOND);
    let paste: Vec<u8> = (0..8192u32).map(|i| b'a' + (i % 26) as u8).collect();
    let sum: u64 = paste.iter().map(|&b| u64::from(b)).sum();
    // One frame, far more than the pty's input queue holds at once.
    a.input(&paste);
    a.wait_for(&format!("GOT 8192 sum={sum}"), 10 * SECOND);
    assert_eq!(a.exit_code(5 * SECOND), 0);
}

#[test]
fn kill_escalates_past_a_program_that_ignores_sighup() {
    let hub = PrivateHub::start();
    let (mut a, id) = hub.launch(&["ignore-hup"]);
    a.wait_for("IGNORING", 5 * SECOND);
    let started = Instant::now();
    let (kind, reply) = hub.request(json!({"op": "kill", "id": id}));
    assert_eq!((kind, &reply), (REPLY, &json!({"ok": true})));
    // SIGHUP is ignored; five seconds later SIGTERM makes the supervisor
    // SIGKILL its job, whose death is the session's exit status.
    assert_eq!(a.exit_code(12 * SECOND), 128 + 9);
    assert!(
        started.elapsed() >= Duration::from_millis(4500),
        "ended before the grace period"
    );
    assert!(hub.status()["sessions"].as_array().unwrap().is_empty());
}

#[test]
fn stop_leaves_no_session_behind_even_one_that_ignores_sighup() {
    let hub = PrivateHub::start();
    let (mut a, _) = hub.launch(&["ignore-hup"]);
    a.wait_for("IGNORING", 5 * SECOND);
    let supervisor = hub.status()["sessions"][0]["pid"].as_i64().unwrap() as i32;
    let stop = hub.cli(&["hub", "stop", "--force"]);
    assert!(stop.status.success(), "{stop:?}");
    // The hub is gone and can't escalate; the supervisor does, once its
    // grace is up, and leaves with its job.
    let deadline = Instant::now() + 10 * SECOND;
    // SAFETY: probing with signal 0.
    while unsafe { libc::kill(supervisor, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "the supervisor (pid {supervisor}) and its job outlived the hub"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn ended_session_keeps_a_failed_launchs_output_for_a_late_attacher() {
    let hub = PrivateHub::start_with("/nonexistent/claude");
    let (mut a, id) = hub.launch(&[]);
    assert_eq!(a.exit_code(5 * SECOND), 127);
    assert!(
        a.output_text().contains("cannot run /nonexistent/claude"),
        "{:?}",
        a.output_text()
    );
    // Arriving after it ended: the attach still works, shows what the
    // program said, and how it exited.
    let mut late = hub.attach(&id, 24, 80);
    let (kind, _) = late.frame(5 * SECOND).unwrap();
    assert_eq!(kind, ATTACHED);
    assert_eq!(late.exit_code(5 * SECOND), 127);
    assert!(
        late.output_text()
            .contains("cannot run /nonexistent/claude")
    );
    // It isn't a running session, though.
    assert!(hub.status()["sessions"].as_array().unwrap().is_empty());
}

#[test]
fn ctrl_z_is_answered_with_a_continue() {
    let hub = PrivateHub::start();
    let (mut a, _) = hub.launch(&["tstp"]);
    a.wait_for("STOPPING", 5 * SECOND);
    a.wait_for("CONTINUED", 5 * SECOND);
    assert!(!a.output_text().contains("NOT-STOPPED"));
    // And the session survives it.
    a.input(b"still here");
    a.wait_for("still here", 5 * SECOND);
    a.input(b"q");
    assert_eq!(a.exit_code(5 * SECOND), 0);
}

#[test]
fn a_blast_reaches_the_client_whole() {
    let hub = PrivateHub::start();
    let (mut a, _) = hub.launch(&["blast", "20"]);
    assert_eq!(a.exit_code(30 * SECOND), 0);
    assert_eq!(a.output.iter().filter(|&&b| b == b'x').count(), 20 << 20);
    assert!(a.output_text().contains("DONE"));
}

#[test]
fn a_client_that_stops_reading_is_cut_off_not_waited_for() {
    let hub = PrivateHub::start();
    // Never read from: its queue passes 32 MB and the hub drops it.
    let (_stuck, id) = hub.launch(&["blast", "64"]);
    let deadline = Instant::now() + 30 * SECOND;
    loop {
        let started = Instant::now();
        let (kind, status) = hub.request(json!({"op": "status"}));
        assert_eq!(kind, REPLY);
        assert!(
            started.elapsed() < 2 * SECOND,
            "the hub stalled behind a stuck client"
        );
        let sessions = status["sessions"].as_array().unwrap();
        if sessions.is_empty() {
            break; // the blast finished: nothing waited on the stuck client
        }
        assert_eq!(sessions[0]["id"], id.as_str());
        assert!(Instant::now() < deadline, "the session never finished");
        std::thread::sleep(SECOND / 5);
    }
}

#[test]
fn version_mismatch_is_refused_with_instructions() {
    let hub = PrivateHub::start();
    let (kind, error) =
        hub.request(json!({"op": "attach", "protocol": 1, "id": "abc", "rows": 24, "cols": 80}));
    assert_eq!(kind, ERROR);
    assert_eq!(
        error["message"],
        "this claudeship is an older build than the running hub. Run the installed one (a new terminal, or reinstall)."
    );
    let (kind, error) = hub.request(json!({
        "op": "launch", "protocol": PROTOCOL + 1, "cwd": "/tmp", "rows": 24, "cols": 80,
    }));
    assert_eq!(kind, ERROR);
    assert_eq!(
        error["message"],
        "the running hub is an older build than this command. Restart it when its sessions can end: \
         claudeship hub stop, then try again."
    );
    // Management works across builds.
    let (kind, status) = hub.request(json!({"op": "status"}));
    assert_eq!(kind, REPLY);
    assert_eq!(status["protocol"], PROTOCOL);
}

#[test]
fn requests_and_refusals() {
    let hub = PrivateHub::start();
    let (kind, e) = hub.request(
        json!({"op": "attach", "protocol": PROTOCOL, "id": "zzzzzz", "rows": 24, "cols": 80}),
    );
    assert_eq!(
        (kind, e["message"].as_str()),
        (ERROR, Some("no such session: zzzzzz"))
    );
    let (kind, e) = hub
        .request(json!({"op": "attach", "protocol": PROTOCOL, "id": "x", "rows": 1, "cols": 80}));
    assert_eq!(
        (kind, e["message"].as_str()),
        (ERROR, Some("attach needs a terminal size"))
    );
    let (kind, e) = hub.request(json!({"op": "launch", "protocol": PROTOCOL, "cwd": "/nonexistent-dir", "rows": 24, "cols": 80}));
    assert_eq!(
        (kind, e["message"].as_str()),
        (ERROR, Some("not a directory: /nonexistent-dir"))
    );
    let (kind, e) = hub.request(json!({"op": "kill", "id": "zzzzzz"}));
    assert_eq!(
        (kind, e["message"].as_str()),
        (ERROR, Some("no such session: zzzzzz"))
    );
    let (kind, e) = hub.request(json!({"op": "dance"}));
    assert_eq!(
        (kind, e["message"].as_str()),
        (ERROR, Some("unknown request: dance"))
    );
    let (kind, link) = hub.request(json!({"op": "link"}));
    assert_eq!(kind, REPLY);
    assert_eq!(link["port"], hub.port);
    assert_eq!(
        link["token"].as_str().map(str::len),
        Some(64),
        "the hub's pairing secret"
    );

    // The socket is the owner's only.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(hub.home.join("hub.sock"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // Stop refuses while a session runs, unless forced.
    let (mut a, _) = hub.launch(&["echo"]);
    a.wait_for("READY", 5 * SECOND);
    let stop = hub.cli(&["hub", "stop"]);
    assert!(!stop.status.success());
    assert!(String::from_utf8_lossy(&stop.stderr).contains(
        "1 session(s) still running — they end with the hub. Use --force to stop anyway."
    ));
    // A second hub on the same home bows out (the lock).
    let second = Command::new(CLAUDESHIP)
        .args(["hub", "run"])
        .env("CLAUDESHIP_HOME", &hub.home)
        .output()
        .unwrap();
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("another hub already holds"));
    assert_eq!(hub.status()["pid"], hub.pid);
    // Forced, the hub hangs the session up and is gone at once: its screen
    // sees the connection close.
    let stop = hub.cli(&["hub", "stop", "--force"]);
    assert!(stop.status.success(), "{stop:?}");
    assert_eq!(String::from_utf8_lossy(&stop.stdout), "hub stopped\n");
    a.drain_for(2 * SECOND);
    assert!(a.closed);
    assert!(
        !hub.home.join("hub.sock").exists(),
        "the socket goes with the hub"
    );
}

#[test]
fn bypassed_invocations_run_the_program_directly() {
    let hub = PrivateHub::start();
    // Not a terminal on stdin/stdout, so it's not a session to share:
    // the program runs in place of the command, exit code and all.
    let out = hub.cli(&["exit", "3"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stdout).contains("BYE"));
    let out = hub.cli(&["-p", "say hi"]);
    assert!(out.status.success());
    // Arguments reach it byte for byte, non-UTF-8 included.
    use std::os::unix::ffi::OsStrExt;
    let odd = std::ffi::OsStr::from_bytes(b"caf\xe9");
    let out = hub.cli_os([
        std::ffi::OsStr::new("argv"),
        std::ffi::OsStr::new("--model"),
        odd,
    ]);
    assert_eq!(out.stdout, b"--model|caf\xe9");
    assert!(
        hub.status()["sessions"].as_array().unwrap().is_empty(),
        "none of that went through the hub"
    );
}

#[test]
fn cli_status_and_usage() {
    let hub = PrivateHub::start();
    let (mut a, id) = hub.launch(&["echo"]);
    a.wait_for("READY", 5 * SECOND);
    let text = String::from_utf8_lossy(&hub.cli(&["hub", "status"]).stdout).into_owned();
    assert!(
        text.starts_with(&format!("hub: pid {}, up ", hub.pid)),
        "{text}"
    );
    assert!(text.contains(&format!("  {id}  /tmp  up ")), "{text}");
    assert!(text.contains("1 attached, 80x24"), "{text}");
    let out = hub.cli(&["hub", "kill", &id]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("session {id} told to end\n")
    );
    assert_eq!(
        a.exit_code(5 * SECOND),
        129,
        "the hang-up ends it: SIGHUP's status"
    );
    let usage = hub.cli(&["hub", "help"]);
    assert!(usage.status.success());
    assert!(String::from_utf8_lossy(&usage.stdout).contains("claudeship hub attach <id>"));
    assert_eq!(hub.cli(&["hub", "bogus"]).status.code(), Some(2));
    let attach = hub.cli(&["hub", "attach"]);
    assert!(String::from_utf8_lossy(&attach.stderr).contains("usage: claudeship hub attach"));
    // `permission-hook` never says anything a hook would read as a decision.
    let hook = hub.cli(&["permission-hook"]);
    assert!(hook.status.success());
    assert!(hook.stdout.is_empty());
}

#[test]
fn the_program_starts_in_the_foreground() {
    // The job takes the terminal before it execs: a program that sets raw
    // mode at once (Claude Code does) must not do it from the background
    // and be stopped for it.
    let hub = PrivateHub::start();
    for _ in 0..5 {
        let (mut a, _) = hub.launch(&["fg"]);
        assert_eq!(a.exit_code(5 * SECOND), 0);
        assert!(
            a.output_text().contains("FOREGROUND"),
            "{:?}",
            a.output_text()
        );
    }
}

/// A pipe whose write end only `command`'s child inherits (close-on-exec
/// everywhere else, so parallel tests' children don't pick it up).
fn inherit_pipe(command: &mut Command) -> (i32, i32) {
    use std::os::unix::process::CommandExt;
    let mut fds = [0; 2];
    // SAFETY: pipe fills two descriptors; fcntl on them.
    unsafe {
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    let write = fds[1];
    // SAFETY: fcntl is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            libc::fcntl(write, libc::F_SETFD, 0);
            Ok(())
        });
    }
    (fds[0], fds[1])
}

/// Whether the pipe's read end sees EOF within `timeout` (every write end
/// closed) rather than staying open.
fn pipe_closes(read: i32, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd: read,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd on this frame; reading into a local byte.
    unsafe {
        if libc::poll(&mut pfd, 1, timeout.as_millis() as i32) != 1 {
            return false;
        }
        let mut byte = 0u8;
        libc::read(read, (&mut byte as *mut u8).cast(), 1) == 0
    }
}

#[test]
fn hub_start_leaves_the_callers_descriptors_behind() {
    let home = PathBuf::from(format!("/tmp/cs-{}-fd", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let port = own_port(&home);
    let mut command = Command::new(CLAUDESHIP);
    command
        .args(["hub", "start"])
        .env("CLAUDESHIP_HOME", &home)
        .env("CLAUDESHIP_CMD", STAND_IN);
    let (read, write) = inherit_pipe(&mut command);
    let out = command.output().unwrap();
    // SAFETY: closing our own write end.
    unsafe { libc::close(write) };
    let mut hub = PrivateHub {
        home,
        port,
        pid: 0,
        program: STAND_IN.to_string(),
    };
    assert!(out.status.success(), "hub start: {out:?}");
    hub.pid = hub.status()["pid"].as_i64().unwrap() as i32;
    assert!(
        pipe_closes(read, 2 * SECOND),
        "the hub kept a descriptor its starter inherited"
    );
    // SAFETY: closing our own read end.
    unsafe { libc::close(read) };
}

#[test]
fn sessions_get_none_of_the_hubs_descriptors() {
    // A hub run directly, holding a descriptor it inherited without
    // close-on-exec: none of that may reach a session.
    let home = PathBuf::from(format!("/tmp/cs-{}-fe", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let port = own_port(&home);
    // Through a shell that leaves it running, so it isn't this test's
    // child (a zombie would look alive to the cleanup's check).
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "\"$0\" hub run </dev/null >/dev/null 2>&1 &",
            CLAUDESHIP,
        ])
        .env("CLAUDESHIP_HOME", &home)
        .env("CLAUDESHIP_CMD", STAND_IN);
    let (read, write) = inherit_pipe(&mut command);
    assert!(command.status().unwrap().success());
    // SAFETY: closing our own ends; the hub holds its copy of the write end.
    unsafe {
        libc::close(write);
        libc::close(read);
    }
    let deadline = Instant::now() + 5 * SECOND;
    while UnixStream::connect(home.join("hub.sock")).is_err() {
        assert!(Instant::now() < deadline, "the hub never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut hub = PrivateHub {
        home,
        port,
        pid: 0,
        program: STAND_IN.to_string(),
    };
    hub.pid = hub.status()["pid"].as_i64().unwrap() as i32;
    let (mut a, _) = hub.launch(&["fds"]);
    assert_eq!(a.exit_code(5 * SECOND), 0);
    assert!(a.output_text().contains("FDS []"), "{:?}", a.output_text());
}
