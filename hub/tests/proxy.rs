//! Proxying end to end (plan phase 10, part B): two private hubs on this
//! machine peering over loopback (`CLAUDESHIP_ADVERTISE_LOOPBACK=1`), a
//! client of A acting on B's sessions by naming B's host id — launch,
//! attach (replay, typing, resize, close both ways), kill, approve,
//! auto-approve, settings — and A's refusals: 404 unknown host, 409 a peer
//! on another protocol, 502 a peer that doesn't answer; B's refusal of a
//! relayed request that names a host. Requests are hand-written, as in
//! `web.rs` and `swarm.rs`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const CLAUDESHIP: &str = env!("CARGO_BIN_EXE_claudeship");
const STAND_IN: &str = env!("CARGO_BIN_EXE_claudeship-stand-in");
const PROTOCOL: u32 = 3;
const SECOND: Duration = Duration::from_secs(1);
/// Several gossip rounds (every 2 s, 3 s timeout).
const GOSSIP: Duration = Duration::from_secs(20);
const ALLOW: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
const SID: &str = "11111111-2222-4333-8444-555555555555";

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as f64
}

struct Hub {
    home: PathBuf,
    port: u16,
    pid: i32,
    env: Vec<(&'static str, String)>,
}

impl Hub {
    fn start() -> Hub {
        Hub::start_with(&[])
    }

    fn start_with(env: &[(&'static str, &str)]) -> Hub {
        static N: AtomicUsize = AtomicUsize::new(0);
        let home = PathBuf::from(format!(
            "/tmp/cs-p{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&home);
        for dir in ["root/proj", "h", "claude/sessions"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        for rc in [".zshenv", ".zshrc", ".zprofile", ".bash_profile", ".bashrc"] {
            std::fs::write(home.join("h").join(rc), "").unwrap();
        }
        let mut all = vec![("CLAUDESHIP_ADVERTISE_LOOPBACK", "1".to_string())];
        all.extend(env.iter().map(|(k, v)| (*k, v.to_string())));
        for attempt in 1.. {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            std::fs::write(
                home.join("config.json"),
                json!({"port": port, "root": home.join("root")}).to_string(),
            )
            .unwrap();
            let mut hub = Hub { home: home.clone(), port, pid: 0, env: all.clone() };
            if hub.boot() {
                return hub;
            }
            assert!(attempt < 5, "the web server never listened (5 ports tried)");
            let _ = hub.cli(&["hub", "stop", "--force"]);
            std::mem::forget(hub);
        }
        unreachable!()
    }

    fn boot(&mut self) -> bool {
        let started = self.cli(&["hub", "start"]);
        assert!(started.status.success(), "hub start: {started:?}");
        self.pid = self.status()["pid"].as_i64().unwrap() as i32;
        let deadline = Instant::now() + 5 * SECOND;
        while Instant::now() < deadline {
            if self.status()["webListening"] == true {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn stop(&self) {
        let out = self.cli(&["hub", "stop", "--force"]);
        assert!(out.status.success(), "{out:?}");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(CLAUDESHIP);
        command
            .args(args)
            .env("CLAUDESHIP_HOME", &self.home)
            .env("CLAUDESHIP_CMD", STAND_IN)
            .env("HOME", self.home.join("h"))
            .env("CLAUDE_CONFIG_DIR", self.home.join("claude"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME");
        for (k, v) in &self.env {
            command.env(k, v);
        }
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn status(&self) -> Value {
        let out = self.cli(&["hub", "status", "--json"]);
        assert!(out.status.success(), "status: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn id(&self) -> String {
        self.status()["swarmId"].as_str().unwrap().to_string()
    }

    fn peer_requests(&self) -> u64 {
        self.status()["peerRequests"].as_u64().unwrap()
    }

    /// The web clients attached to session `id` (`None`: no such session).
    fn clients(&self, id: &str) -> Option<i64> {
        self.status()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .and_then(|s| s["clients"].as_i64())
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.home.join("token")).unwrap().trim().to_string()
    }

    fn secret(&self) -> String {
        std::fs::read_to_string(self.home.join("swarm.secret")).unwrap().trim().to_string()
    }

    fn http(&self, method: &str, target: &str, headers: &[String], body: &str) -> Response {
        let mut request = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n", self.port);
        for h in headers {
            request.push_str(h);
            request.push_str("\r\n");
        }
        if !body.is_empty() {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        request.push_str(body);
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.set_read_timeout(Some(15 * SECOND)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        Response::parse(&raw)
    }

    /// A paired client's same-origin JSON POST.
    fn post(&self, path: &str, body: Value) -> Response {
        self.http(
            "POST",
            path,
            &[
                format!("Cookie: claude_ship={}", self.token()),
                format!("Origin: http://127.0.0.1:{}", self.port),
                "Content-Type: application/json".into(),
            ],
            &body.to_string(),
        )
    }

    /// A peer's JSON POST, with this hub's swarm secret as the bearer.
    fn peer_post(&self, path: &str, body: Value) -> Response {
        self.http(
            "POST",
            path,
            &[
                format!("Authorization: Bearer {}", self.secret()),
                "Content-Type: application/json".into(),
            ],
            &body.to_string(),
        )
    }

    fn state(&self) -> Value {
        let r = self.http("GET", "/api/state", &[format!("Cookie: claude_ship={}", self.token())], "");
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()
    }

    fn host(&self, id: &str) -> Option<Value> {
        self.state()["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == id)
            .cloned()
    }

    fn reaches(&self, id: &str) -> bool {
        self.host(id).is_some_and(|h| h["reachable"] == true)
    }

    /// Join `member`'s swarm, as the phone does, and wait until it
    /// reaches us.
    fn join(&self, member: &Hub) {
        let swarm = member.post("/api/swarm", json!({}));
        assert_eq!(swarm.status, 200, "{}", swarm.text());
        let swarm = swarm.json();
        let r = self.post(
            "/api/swarm/join",
            json!({"secret": swarm["secret"], "peers": swarm["peers"]}),
        );
        assert_eq!(r.status, 200, "{}", r.text());
        let mine = self.id();
        eventually(GOSSIP, "the member reaches the joiner", || member.reaches(&mine));
    }

    /// A session started over the Unix socket running the stand-in in
    /// `mode`, left running. Its hub id.
    fn launch(&self, mode: &str) -> String {
        let mut stream = UnixStream::connect(self.home.join("hub.sock")).unwrap();
        stream.set_read_timeout(Some(5 * SECOND)).unwrap();
        let hello = json!({
            "op": "launch", "protocol": PROTOCOL, "cwd": "/tmp", "args": [mode],
            "env": {"PATH": "/usr/bin:/bin"}, "rows": 24, "cols": 80,
        })
        .to_string();
        let mut frame = vec![b'H'];
        frame.extend_from_slice(&(hello.len() as u32).to_be_bytes());
        frame.extend_from_slice(hello.as_bytes());
        stream.write_all(&frame).unwrap();
        let mut head = [0u8; 5];
        stream.read_exact(&mut head).unwrap();
        assert_eq!(head[0], b'A', "attached");
        let mut payload = vec![0u8; u32::from_be_bytes(head[1..5].try_into().unwrap()) as usize];
        stream.read_exact(&mut payload).unwrap();
        let attached: Value = serde_json::from_slice(&payload).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        attached["id"].as_str().unwrap().to_string()
    }

    /// A paired browser's terminal on this hub.
    fn ws(&self, query: &str) -> Ws {
        self.upgrade(
            &format!("/ws/term?{query}"),
            &[
                format!("Cookie: claude_ship={}", self.token()),
                format!("Origin: http://127.0.0.1:{}", self.port),
            ],
        )
        .unwrap_or_else(|r| panic!("refused {}: {}", r.status, r.text()))
    }

    /// An upgrade to `target`; `Err` is the refusal.
    fn upgrade(&self, target: &str, headers: &[String]) -> Result<Ws, Response> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut request = format!(
            "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n",
            self.port
        );
        for h in headers {
            request.push_str(h);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        let deadline = Instant::now() + 15 * SECOND;
        let end = loop {
            if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
            assert!(Instant::now() < deadline, "no answer to the upgrade");
            let mut buffer = [0u8; 4096];
            match stream.read(&mut buffer) {
                Ok(0) => break raw.len(),
                Ok(n) => raw.extend_from_slice(&buffer[..n]),
                Err(_) => {}
            }
        };
        let head = String::from_utf8_lossy(&raw[..end]).into_owned();
        if !head.starts_with("HTTP/1.1 101") {
            let _ = stream.set_read_timeout(Some(2 * SECOND));
            let _ = stream.read_to_end(&mut raw);
            return Err(Response::parse(&raw));
        }
        assert!(
            head.to_lowercase()
                .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="),
            "{head}"
        );
        Ok(Ws {
            stream,
            pending: raw[end..].to_vec(),
            output: Vec::new(),
            texts: Vec::new(),
            close_code: None,
            closed: false,
        })
    }

    /// Claude's registry entry for a session, naming a live process.
    fn register(&self, sleeper: &Child, session_id: &str, status: &str) {
        let path = self.home.join(format!("claude/sessions/{}.json", sleeper.id()));
        let entry = json!({
            "pid": sleeper.id(), "sessionId": session_id,
            "cwd": self.home.join("root/proj"), "status": status,
            "startedAt": now_ms() - 60_000.0, "statusUpdatedAt": now_ms() - 30_000.0,
        });
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, entry.to_string()).unwrap();
        std::fs::rename(temp, path).unwrap();
    }

    /// `claudeship permission-hook` against this hub, as Claude Code runs it.
    fn helper(&self, session_id: &str, command: &str) -> Child {
        let mut child = self
            .command(&["permission-hook"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = json!({
            "session_id": session_id, "cwd": "/tmp", "hook_event_name": "PermissionRequest",
            "tool_name": "Bash", "tool_input": {"command": command, "description": "test"},
        });
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        child
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let leaders: Vec<i32> = serde_json::from_slice::<Value>(&self.cli(&["hub", "status", "--json"]).stdout)
            .ok()
            .and_then(|v| {
                v["sessions"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|s| s["pid"].as_i64()).map(|p| p as i32).collect())
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
        for _ in 0..10 {
            let _ = std::fs::remove_dir_all(&self.home);
            if !self.home.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if !std::thread::panicking() {
            assert!(!alive, "the private hub (pid {}) survived hub stop --force", self.pid);
        }
    }
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

impl Response {
    fn parse(raw: &[u8]) -> Response {
        let end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("no response head in {:?}", String::from_utf8_lossy(raw)));
        let head = String::from_utf8_lossy(&raw[..end]).into_owned();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        Response { status, body: raw[end + 4..].to_vec() }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.text()))
    }
}

/// A browser's end of a terminal WebSocket (as in `web.rs`).
struct Ws {
    stream: TcpStream,
    pending: Vec<u8>,
    output: Vec<u8>,
    texts: Vec<Value>,
    close_code: Option<u16>,
    closed: bool,
}

impl Ws {
    fn send(&mut self, opcode: u8, payload: &[u8]) {
        self.stream.write_all(&client_frame(opcode, payload)).unwrap();
    }

    fn text(&mut self, value: Value) {
        self.send(1, value.to_string().as_bytes());
    }

    fn input(&mut self, bytes: &[u8]) {
        self.send(2, bytes);
    }

    fn next(&mut self, timeout: Duration) -> Option<u8> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some((opcode, payload, used)) = parse_frame(&self.pending) {
                self.pending.drain(..used);
                match opcode {
                    1 => self.texts.push(serde_json::from_slice(&payload).unwrap()),
                    2 => self.output.extend_from_slice(&payload),
                    8 if payload.len() >= 2 => {
                        self.close_code = Some(u16::from_be_bytes([payload[0], payload[1]]))
                    }
                    _ => {}
                }
                return Some(opcode);
            }
            if self.closed || Instant::now() > deadline {
                return None;
            }
            let mut buffer = [0u8; 65_536];
            match self.stream.read(&mut buffer) {
                Ok(0) => self.closed = true,
                Ok(n) => self.pending.extend_from_slice(&buffer[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => self.closed = true,
            }
        }
    }

    fn output_text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + 5 * SECOND;
        while !self.output_text().contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || (self.next(left).is_none() && self.closed) {
                panic!("never saw {needle:?}; got {:?}", self.output_text());
            }
        }
    }

    fn wait_text(&mut self, want: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + 5 * SECOND;
        loop {
            if let Some(i) = self.texts.iter().position(&want) {
                return self.texts.remove(i);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || (self.next(left).is_none() && self.closed) {
                panic!("no such text message; got {:?}", self.texts);
            }
        }
    }

    fn closes_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.closed {
            if Instant::now() > deadline {
                return false;
            }
            self.next(deadline.saturating_duration_since(Instant::now()));
        }
        true
    }
}

/// A browser's (masked) frame.
fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x80 | opcode];
    let n = payload.len();
    if n < 126 {
        frame.push(0x80 | n as u8);
    } else if n <= 0xffff {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let mask = [0x12, 0x34, 0x56, 0x78];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    frame
}

fn parse_frame(buffer: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    if buffer.len() < 2 {
        return None;
    }
    assert_eq!(buffer[1] & 0x80, 0, "server frames are not masked");
    let (len, header) = match buffer[1] & 0x7f {
        126 if buffer.len() >= 4 => (u16::from_be_bytes([buffer[2], buffer[3]]) as usize, 4),
        127 if buffer.len() >= 10 => (u64::from_be_bytes(buffer[2..10].try_into().unwrap()) as usize, 10),
        126 | 127 => return None,
        n => (n as usize, 2),
    };
    if buffer.len() < header + len {
        return None;
    }
    Some((buffer[0] & 0x0f, buffer[header..header + len].to_vec(), header + len))
}

fn is_type(kind: &'static str) -> impl Fn(&Value) -> bool {
    move |v| v["type"] == kind
}

fn eventually(limit: Duration, why: &str, mut what: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !what() {
        assert!(Instant::now() < deadline, "never happened: {why}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Every session entry (projects and elsewhere) of a state or `hosts` entry.
fn sessions(state: &Value) -> Vec<Value> {
    let mut all: Vec<Value> = state["elsewhere"].as_array().cloned().unwrap_or_default();
    for project in state["projects"].as_array().into_iter().flatten() {
        all.extend(project["sessions"].as_array().into_iter().flatten().cloned());
    }
    all
}

// MARK: Tests

#[test]
fn launch_attach_resize_and_kill_through_the_home_hub() {
    let a = Hub::start();
    let b = Hub::start();
    b.join(&a);
    let bid = b.id();

    // Launch on B through A: B's answer, and B's session in A's hosts[]
    // at once (A fetched B's state after forwarding).
    let proj = b.home.join("root/proj").to_string_lossy().into_owned();
    let r = a.post("/api/launch", json!({"host": bid, "path": proj}));
    assert_eq!(r.status, 200, "{}", r.text());
    let id = r.json()["id"].as_str().unwrap().to_string();
    assert!(b.clients(&id).is_some(), "the session runs on B");
    assert!(a.clients(&id).is_none(), "not on A");
    let listed = sessions(&a.host(&bid).unwrap());
    assert!(listed.iter().any(|s| s["hubId"] == id.as_str()), "{listed:?}");
    // B's own refusals come back verbatim.
    let r = a.post("/api/launch", json!({"host": bid, "path": "/tmp"}));
    assert_eq!((r.status, r.json()), (400, json!({"error": "not a project directory"})));
    // The local id, or no host, is A itself.
    let r = a.post("/api/kill", json!({"host": a.id(), "id": id}));
    assert_eq!((r.status, r.json()), (404, json!({"error": "no such session"})));

    // Attach through A: the size, the replay, then live bytes.
    let mut ws = a.ws(&format!("host={bid}&id={id}&rows=30&cols=100"));
    assert_eq!(ws.wait_text(is_type("size")), json!({"type": "size", "rows": 30, "cols": 100, "owner": true}));
    ws.wait_for("READY");
    ws.input(b"hello");
    ws.wait_for("hello");
    ws.text(json!({"type": "ping"}));
    ws.wait_text(is_type("pong"));
    assert_eq!(b.clients(&id), Some(1));

    // Kill through A: the relayed screen is told and closed.
    let r = a.post("/api/kill", json!({"host": bid, "id": id}));
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    assert_eq!(ws.wait_text(is_type("exit"))["code"], 129, "hung up");
    assert!(ws.closes_within(5 * SECOND));
    assert_eq!(ws.close_code, Some(1000));
    let r = a.post("/api/kill", json!({"host": bid, "id": id}));
    assert_eq!(r.status, 404, "{}", r.text());
    eventually(5 * SECOND, "the killed session leaves A's view of B", || {
        !sessions(&a.host(&bid).unwrap()).iter().any(|s| s["hubId"] == id.as_str())
    });

    // A relayed resize claims the size on B.
    let sized = b.launch("size");
    let mut direct = b.ws(&format!("id={sized}&rows=30&cols=100"));
    assert_eq!(direct.wait_text(is_type("size"))["owner"], true);
    direct.wait_for("SIZE 30 100");
    let mut relayed = a.ws(&format!("host={bid}&id={sized}&rows=30&cols=100&claim=0"));
    assert_eq!(
        relayed.wait_text(is_type("size")),
        json!({"type": "size", "rows": 30, "cols": 100, "owner": false})
    );
    relayed.text(json!({"type": "resize", "rows": 40, "cols": 120}));
    assert_eq!(
        relayed.wait_text(is_type("size")),
        json!({"type": "size", "rows": 40, "cols": 120, "owner": true})
    );
    direct.wait_for("SIZE 40 120");
    relayed.wait_for("SIZE 40 120");
    assert_eq!(b.clients(&sized), Some(2));

    // The client closes: B's attachment goes with it.
    relayed.send(8, &1000u16.to_be_bytes());
    assert!(relayed.closes_within(5 * SECOND));
    eventually(5 * SECOND, "B's relayed attachment is released", || b.clients(&sized) == Some(1));

    // The session exits on B: the relayed screen sees it and closes.
    let echo = b.launch("echo");
    let mut ws = a.ws(&format!("host={bid}&id={echo}&rows=24&cols=80"));
    ws.wait_for("READY");
    ws.input(b"q");
    assert_eq!(ws.wait_text(is_type("exit")), json!({"type": "exit", "code": 0}));
    assert!(ws.closes_within(5 * SECOND));
    assert_eq!(ws.close_code, Some(1000));
    // An id B doesn't have: B's `gone`, relayed.
    let mut gone = a.ws(&format!("host={bid}&id=zzzzzz&rows=24&cols=80"));
    gone.wait_text(is_type("gone"));
    assert!(gone.closes_within(5 * SECOND));
    drop(direct);
    eventually(5 * SECOND, "no attachment left on B", || {
        b.status()["sessions"].as_array().unwrap().iter().all(|s| s["clients"] == 0)
    });
}

#[test]
fn approve_auto_approve_and_settings_through_the_home_hub() {
    let a = Hub::start();
    let b = Hub::start();
    b.join(&a);
    let bid = b.id();
    let mut sleeper = Command::new("sleep").arg("600").spawn().unwrap();
    b.register(&sleeper, SID, "busy");

    let helper = b.helper(SID, "rm -rf build");
    let mut pending = Vec::new();
    eventually(GOSSIP, "B's request in A's view of B", || {
        pending = sessions(&a.host(&bid).unwrap())
            .into_iter()
            .find(|s| s["sessionId"] == SID)
            .and_then(|s| s["approvals"].as_array().cloned())
            .unwrap_or_default();
        pending.len() == 1
    });
    let approval = pending[0]["id"].as_str().unwrap().to_string();
    let r = a.post("/api/approve", json!({"host": bid, "id": approval, "allow": true}));
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    let out = helper.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{ALLOW}\n"));
    let r = a.post("/api/approve", json!({"host": bid, "id": approval, "allow": true}));
    assert_eq!((r.status, r.json()), (404, json!({"error": "no such approval"})), "answered once");

    let r = a.post("/api/auto-approve", json!({"host": bid, "sessionId": SID, "rule": "5m"}));
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    let entry = sessions(&b.state()).into_iter().find(|s| s["sessionId"] == SID).unwrap();
    assert!(entry["autoApprove"]["until"].as_f64().is_some(), "{entry}");

    let r = a.post("/api/settings", json!({"host": bid, "defaultPermissionMode": "plan"}));
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    assert_eq!(b.state()["defaultPermissionMode"], "plan");
    assert_ne!(a.state()["defaultPermissionMode"], "plan", "A's own settings untouched");
    assert_eq!(a.host(&bid).unwrap()["defaultPermissionMode"], "plan");

    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

#[test]
fn refusals_unknown_host_unreachable_and_protocol_mismatch() {
    let mut a = Hub::start();
    // B polls nobody, so any peer request it makes would be the relay's.
    let b = Hub::start_with(&[("CLAUDESHIP_GOSSIP", "0")]);
    b.join(&a);
    let bid = b.id();

    // An id nobody has.
    let unknown = "00000000-0000-4000-8000-000000000000";
    let r = a.post("/api/kill", json!({"host": unknown, "id": "x"}));
    assert_eq!((r.status, r.json()), (404, json!({"error": "no such host"})));
    let r = a.post("/api/kill", json!({"host": 7, "id": "x"}));
    assert_eq!(r.status, 404);
    let refused = a
        .upgrade(
            &format!("/ws/term?host={unknown}&id=x&rows=24&cols=80"),
            &[
                format!("Cookie: claude_ship={}", a.token()),
                format!("Origin: http://127.0.0.1:{}", a.port),
            ],
        )
        .err()
        .expect("refused");
    assert_eq!((refused.status, refused.json()), (404, json!({"error": "no such host"})));

    // B's side: a relayed request never names a host, and never fans out.
    let before = b.peer_requests();
    let proj = b.home.join("root/proj").to_string_lossy().into_owned();
    let r = b.peer_post("/peer/api/launch", json!({"path": proj, "host": a.id()}));
    assert_eq!((r.status, r.json()), (400, json!({"error": "a peer's request can't name a host"})));
    let refused = b
        .upgrade(
            &format!("/peer/ws/term?host={}&id=x&rows=24&cols=80", a.id()),
            &[format!("Authorization: Bearer {}", b.secret())],
        )
        .err()
        .expect("refused");
    assert_eq!(refused.status, 400);
    // And needs the bearer.
    let refused = b.upgrade("/peer/ws/term?id=x&rows=24&cols=80", &[]).err().expect("refused");
    assert_eq!(refused.status, 401);
    let r = b.http("POST", "/peer/api/kill", &["Content-Type: application/json".into()], "{}");
    assert_eq!(r.status, 401);
    // A peer's terminal works without an Origin.
    let mut ws = b
        .upgrade(
            "/peer/ws/term?id=zzzzzz&rows=24&cols=80",
            &[format!("Authorization: Bearer {}", b.secret())],
        )
        .unwrap_or_else(|r| panic!("refused {}", r.status));
    ws.wait_text(is_type("gone"));
    assert_eq!(b.peer_requests(), before, "a peer's request made no peer request");

    // B stopped: unreachable.
    b.stop();
    let r = a.post("/api/kill", json!({"host": bid, "id": "x"}));
    assert_eq!((r.status, r.json()), (502, json!({"error": "unreachable", "host": bid})));
    let refused = a
        .upgrade(
            &format!("/ws/term?host={bid}&id=x&rows=24&cols=80"),
            &[
                format!("Cookie: claude_ship={}", a.token()),
                format!("Origin: http://127.0.0.1:{}", a.port),
            ],
        )
        .err()
        .expect("refused");
    assert_eq!((refused.status, refused.json()), (502, json!({"error": "unreachable", "host": bid})));

    // B recorded on another protocol (A restarted with no poller, so
    // nothing corrects it): refused before any connection.
    a.stop();
    let path = a.home.join("peers.json");
    let mut peers: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for record in peers["peers"].as_array_mut().unwrap() {
        if record["id"] == bid.as_str() {
            record["protocol"] = json!(PROTOCOL + 1);
        }
    }
    std::fs::write(&path, peers.to_string()).unwrap();
    a.env.push(("CLAUDESHIP_GOSSIP", "0".into()));
    assert!(a.boot());
    let before = a.peer_requests();
    let r = a.post("/api/launch", json!({"host": bid, "path": "/tmp"}));
    assert_eq!(
        (r.status, r.json()),
        (409, json!({"error": "protocol mismatch", "host": bid, "theirs": PROTOCOL + 1, "ours": PROTOCOL}))
    );
    let refused = a
        .upgrade(
            &format!("/ws/term?host={bid}&id=x&rows=24&cols=80"),
            &[
                format!("Cookie: claude_ship={}", a.token()),
                format!("Origin: http://127.0.0.1:{}", a.port),
            ],
        )
        .err()
        .expect("refused");
    assert_eq!(refused.status, 409);
    assert_eq!(a.peer_requests(), before, "checked before connecting");
}

/// Point A's record of `bid` at `addresses` only, and restart A with no
/// poller (so nothing corrects it).
fn reroute(a: &mut Hub, bid: &str, addresses: Value) {
    a.stop();
    let path = a.home.join("peers.json");
    let mut peers: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for record in peers["peers"].as_array_mut().unwrap() {
        if record["id"] == bid {
            record["addresses"] = addresses.clone();
        }
    }
    std::fs::write(&path, peers.to_string()).unwrap();
    a.env.push(("CLAUDESHIP_GOSSIP", "0".into()));
    assert!(a.boot());
}

#[test]
fn a_relay_carries_both_directions_at_once() {
    use tokio_tungstenite::tungstenite::{self, Message};
    // A peer that writes a lot of output before it reads again — as a
    // hub does while it delivers output to a slow relay — while the
    // client pastes something big. One loop doing both directions would
    // block writing the paste to the peer while the peer blocks writing
    // to it, and hold both for the stall minute.
    const OUTPUT: usize = 24 << 20;
    const PASTE: usize = 6 << 20;
    let mut a = Hub::start();
    let b = Hub::start_with(&[("CLAUDESHIP_GOSSIP", "0")]);
    b.join(&a);
    let bid = b.id();
    let fake = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    reroute(&mut a, &bid, json!([fake.local_addr().unwrap().to_string()]));
    let peer = std::thread::spawn(move || {
        let (stream, _) = fake.accept().unwrap();
        let mut ws = tungstenite::accept(stream).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        for _ in 0..OUTPUT / (256 << 10) {
            ws.send(Message::binary(vec![b'o'; 256 << 10])).unwrap();
        }
        let mut pasted = 0;
        while pasted < PASTE {
            if let Message::Binary(data) = ws.read().unwrap() {
                pasted += data.len();
            }
        }
        // A close with a code of its own: relayed as it is.
        let _ = ws.close(Some(tungstenite::protocol::CloseFrame {
            code: 4001.into(),
            reason: "".into(),
        }));
        let _ = ws.get_ref().set_read_timeout(Some(2 * SECOND));
        while ws.read().is_ok() {}
        pasted
    });
    let mut ws = a.ws(&format!("host={bid}&id=x&rows=24&cols=80"));
    let mut writer = ws.stream.try_clone().unwrap();
    let paste = std::thread::spawn(move || writer.write_all(&client_frame(2, &vec![b'p'; PASTE])).is_ok());
    let deadline = Instant::now() + 20 * SECOND;
    while ws.output.len() < OUTPUT {
        assert!(
            Instant::now() < deadline,
            "the relay stalled: {} of {OUTPUT} bytes of output",
            ws.output.len()
        );
        ws.next(SECOND);
        assert!(!ws.closed, "closed after {} bytes", ws.output.len());
    }
    assert!(paste.join().unwrap());
    assert_eq!(peer.join().unwrap(), PASTE, "the paste reached the peer whole");
    assert!(ws.closes_within(5 * SECOND));
    assert_eq!(ws.close_code, Some(4001), "the peer's close code");
}

#[test]
fn a_new_swarm_secret_hangs_up_peer_terminals() {
    // B's terminal for a peer, admitted under B's swarm secret; then B
    // joins another swarm (a new secret, the pairing token unchanged).
    let b = Hub::start();
    let c = Hub::start();
    let echo = b.launch("echo");
    let mut ws = b
        .upgrade(
            &format!("/peer/ws/term?id={echo}&rows=24&cols=80"),
            &[format!("Authorization: Bearer {}", b.secret())],
        )
        .unwrap_or_else(|r| panic!("refused {}", r.status));
    ws.wait_for("READY");
    assert_eq!(b.clients(&echo), Some(1));
    let swarm = c.post("/api/swarm", json!({}));
    assert_eq!(swarm.status, 200, "{}", swarm.text());
    let swarm = swarm.json();
    let r = b.post("/api/swarm/join", json!({"secret": swarm["secret"], "peers": swarm["peers"]}));
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(ws.closes_within(5 * SECOND), "the old secret's terminal hung up");
    eventually(5 * SECOND, "its attachment released", || b.clients(&echo) == Some(0));
}

#[test]
fn a_forwarded_post_goes_to_one_address_only() {
    let mut a = Hub::start();
    let b = Hub::start_with(&[("CLAUDESHIP_GOSSIP", "0")]);
    b.join(&a);
    let bid = b.id();

    // B's first address takes the connection and never answers; its
    // second is B itself.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let silent_address = silent.local_addr().unwrap().to_string();
    let heard = std::thread::spawn(move || {
        let (mut stream, _) = silent.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(1500))).unwrap();
        let mut raw = Vec::new();
        let mut buffer = [0u8; 4096];
        while let Ok(n) = stream.read(&mut buffer) {
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buffer[..n]);
        }
        // Held open past A's patience.
        std::thread::sleep(4 * SECOND);
        String::from_utf8_lossy(&raw).into_owned()
    });
    let mut addresses = vec![json!(silent_address)];
    let peers: Value = serde_json::from_slice(&std::fs::read(a.home.join("peers.json")).unwrap()).unwrap();
    let record = peers["peers"].as_array().unwrap().iter().find(|r| r["id"] == bid.as_str()).unwrap();
    let loopback = record["addresses"].as_array().unwrap().iter().find(|a| a.as_str().unwrap().starts_with("127."));
    addresses.push(loopback.unwrap_or_else(|| panic!("{record}")).clone());
    reroute(&mut a, &bid, json!(addresses));

    // The launch may have run at the silent address: not sent again.
    let proj = b.home.join("root/proj").to_string_lossy().into_owned();
    let r = a.post("/api/launch", json!({"host": bid, "path": proj}));
    assert_eq!((r.status, r.json()), (502, json!({"error": "unreachable", "host": bid})));
    assert!(b.status()["sessions"].as_array().unwrap().is_empty(), "launched once at most");

    // What the peer was sent: the bearer, never the client's cookie or
    // Origin, and the body without `host`.
    let request = heard.join().unwrap();
    let (head, body) = request.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("POST /peer/api/launch HTTP/1.1\r\n"), "{head}");
    let lower = head.to_lowercase();
    assert!(lower.contains(&format!("authorization: bearer {}", b.secret()).to_lowercase()), "{head}");
    assert!(!lower.contains("cookie") && !lower.contains("origin"), "{head}");
    assert!(!head.contains(&a.token()), "{head}");
    assert_eq!(serde_json::from_str::<Value>(body).unwrap(), json!({"path": proj}));
}
