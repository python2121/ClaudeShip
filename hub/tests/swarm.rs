//! The swarm end to end: two or three private hubs on this machine, each
//! with its own short home, port, `HOME`, and project root, peering over
//! loopback (`CLAUDESHIP_ADVERTISE_LOOPBACK=1` — the gate allows
//! loopback-to-loopback, so nothing here needs a tailnet). Requests are
//! hand-written, as in `web.rs`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CLAUDESHIP: &str = env!("CARGO_BIN_EXE_claudeship");
const STAND_IN: &str = env!("CARGO_BIN_EXE_claudeship-stand-in");
const PROTOCOL: u32 = 3;
const SECOND: Duration = Duration::from_secs(1);
/// Several gossip rounds (every 2 s, 3 s timeout).
const GOSSIP: Duration = Duration::from_secs(20);

struct Hub {
    home: PathBuf,
    port: u16,
    pid: i32,
    env: Vec<(&'static str, String)>,
}

impl Hub {
    fn start() -> Hub {
        Hub::start_with(&[], None)
    }

    /// A private hub; `env` is added to its environment, `peers` written
    /// as its `peers.json` before it starts.
    fn start_with(env: &[(&'static str, &str)], peers: Option<Value>) -> Hub {
        static N: AtomicUsize = AtomicUsize::new(0);
        let home = PathBuf::from(format!(
            "/tmp/cs-s{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&home);
        for dir in ["root/proj", "h"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        for rc in [".zshenv", ".zshrc", ".zprofile", ".bash_profile", ".bashrc"] {
            std::fs::write(home.join("h").join(rc), "").unwrap();
        }
        if let Some(peers) = peers {
            std::fs::write(home.join("peers.json"), peers.to_string()).unwrap();
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

    /// `hub start` and wait for the web server; whether it listens.
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

    /// Stop the hub, keeping its home (and port) for `restart`.
    fn stop(&self) {
        let out = self.cli(&["hub", "stop", "--force"]);
        assert!(out.status.success(), "{out:?}");
    }

    fn restart(&mut self) {
        assert!(self.boot(), "the hub came back on its port");
    }

    fn cli(&self, args: &[&str]) -> Output {
        let mut command = Command::new(CLAUDESHIP);
        command
            .args(args)
            .env("CLAUDESHIP_HOME", &self.home)
            .env("CLAUDESHIP_CMD", STAND_IN)
            .env("HOME", self.home.join("h"));
        for (k, v) in &self.env {
            command.env(k, v);
        }
        command.output().unwrap()
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

    fn token(&self) -> String {
        std::fs::read_to_string(self.home.join("token")).unwrap().trim().to_string()
    }

    fn secret(&self) -> String {
        std::fs::read_to_string(self.home.join("swarm.secret")).unwrap().trim().to_string()
    }

    fn link(&self) -> String {
        format!("http://localhost:{}/auth?k={}", self.port, self.token())
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
        stream.set_read_timeout(Some(10 * SECOND)).unwrap();
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

    /// A peer's request, with `secret` as the bearer.
    fn peer(&self, method: &str, path: &str, secret: &str, body: Option<Value>) -> Response {
        let mut headers = vec![format!("Authorization: Bearer {secret}")];
        if body.is_some() {
            headers.push("Content-Type: application/json".into());
        }
        self.http(method, path, &headers, &body.map(|b| b.to_string()).unwrap_or_default())
    }

    fn state(&self) -> Value {
        let r = self.http("GET", "/api/state", &[format!("Cookie: claude_ship={}", self.token())], "");
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()
    }

    /// The `hosts` entry for hub `id`, if listed.
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

    /// Join the swarm `member` is in, as the phone does: its `/api/swarm`,
    /// posted to our `/api/swarm/join`.
    fn join(&self, member: &Hub) -> Value {
        let swarm = member.post("/api/swarm", json!({}));
        assert_eq!(swarm.status, 200, "{}", swarm.text());
        let swarm = swarm.json();
        let r = self.post(
            "/api/swarm/join",
            json!({"secret": swarm["secret"], "peers": swarm["peers"]}),
        );
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()
    }

    /// A session started over the Unix socket, left running. Its hub id.
    fn launch(&self) -> String {
        let mut stream = UnixStream::connect(self.home.join("hub.sock")).unwrap();
        stream.set_read_timeout(Some(5 * SECOND)).unwrap();
        let hello = json!({
            "op": "launch", "protocol": PROTOCOL, "cwd": "/tmp", "args": ["echo"],
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
        // A session's shell can still be on its way out, writing into
        // `h` as the directory goes: try again once it is.
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

/// Wait for `what` to hold, up to `limit`.
fn eventually(limit: Duration, why: &str, mut what: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !what() {
        assert!(Instant::now() < deadline, "never happened: {why}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Hold for `span`: `what` must stay true throughout.
fn throughout(span: Duration, why: &str, mut what: impl FnMut() -> bool) {
    let deadline = Instant::now() + span;
    while Instant::now() < deadline {
        assert!(what(), "stopped holding: {why}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn mentions(value: &Value, text: &str) -> bool {
    value.to_string().contains(text)
}

// MARK: Joining, gossip, and the aggregated state

#[test]
fn join_gossip_and_the_hosts_list() {
    let a = Hub::start();
    let b = Hub::start();
    let c = Hub::start();
    let (a_id, b_id, c_id) = (a.id(), b.id(), c.id());

    // Alone, a hub is a swarm of one: hosts is itself.
    let state = a.state();
    let hosts = state["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0]["id"], a_id.as_str());
    assert_eq!(hosts[0]["local"], true);
    assert_eq!(hosts[0]["now"], state["now"]);
    assert_eq!(state["protocol"], PROTOCOL, "additive: the protocol stays");

    // /api/swarm needs the pairing cookie.
    let r = a.http("POST", "/api/swarm", &["Content-Type: application/json".into()], "{}");
    assert_eq!(r.status, 401);

    // Phone-style: A's swarm handed to B.
    let joined = b.join(&a);
    assert_eq!(joined["hello"][0]["ok"], true, "{joined}");
    assert_eq!(b.secret(), a.secret(), "B took the swarm's secret");
    eventually(GOSSIP, "A and B list each other, reachable", || {
        a.reaches(&b_id) && b.reaches(&a_id)
    });

    // B's sessions, seen from A, with B's clock.
    let session = b.launch();
    eventually(GOSSIP, "A shows B's session", || {
        a.host(&b_id).is_some_and(|h| mentions(&h["elsewhere"], &session))
    });
    let host = a.host(&b_id).unwrap();
    assert_eq!(host["local"], false);
    assert_eq!(host["protocol"], PROTOCOL);
    assert!(host["now"].as_i64().unwrap() > 0, "B's now");
    for key in ["root", "rootDisplay", "home", "defaultPermissionMode", "projects", "approvalsSupported", "lastSeen"] {
        assert!(!host[key].is_null(), "hosts entry has {key}: {host}");
    }
    assert!(host["root"].as_str().unwrap().contains(&b.home.file_name().unwrap().to_string_lossy().to_string()));
    let hosts = a.state()["hosts"].as_array().unwrap().clone();
    assert_eq!(hosts[0]["id"], a_id.as_str(), "the local host first");
    assert!(!mentions(&a.state()["elsewhere"], &session), "top level stays A's own");

    // C joins through B with the command; A hears of C by gossip.
    let out = c.cli(&["hub", "pair", &b.link()]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("joined the swarm"), "{out:?}");
    eventually(GOSSIP, "A reaches C, and C reaches A", || a.reaches(&c_id) && c.reaches(&a_id));
    assert_eq!(c.secret(), a.secret());

    // `hub peers` lists the other two, reachable.
    let out = a.cli(&["hub", "peers"]);
    let table = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{out:?}");
    assert!(table.starts_with("NAME"), "{table}");
    assert!(table.contains("(this hub)"), "{table}");
    assert_eq!(table.lines().filter(|l| l.contains("127.0.0.1:")).count(), 3, "{table}");
    assert_eq!(table.matches("  yes  ").count(), 2, "{table}");
}

#[test]
fn an_unreachable_peer_keeps_its_last_state() {
    let a = Hub::start();
    let b = Hub::start();
    let b_id = b.id();
    b.join(&a);
    let session = b.launch();
    eventually(GOSSIP, "A shows B's session", || {
        a.host(&b_id).is_some_and(|h| h["reachable"] == true && mentions(&h["elsewhere"], &session))
    });
    b.stop();
    eventually(GOSSIP, "A marks B unreachable", || {
        a.host(&b_id).is_some_and(|h| h["reachable"] == false)
    });
    let host = a.host(&b_id).unwrap();
    assert!(mentions(&host["elsewhere"], &session), "the last state stays: {host}");
    assert!(host["lastSeen"].as_u64().unwrap() > 0);
}

// MARK: Leaving

#[test]
fn unpair_propagates_and_the_peer_stays_gone() {
    let a = Hub::start();
    let b = Hub::start();
    let mut c = Hub::start();
    let (a_id, b_id, c_id) = (a.id(), b.id(), c.id());
    b.join(&a);
    c.join(&a);
    eventually(GOSSIP, "everyone reaches everyone", || {
        a.reaches(&b_id) && a.reaches(&c_id) && b.reaches(&c_id) && c.reaches(&b_id)
    });

    // C is away when it is unpaired.
    c.stop();
    let out = a.cli(&["hub", "unpair", &c_id]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("is unpaired"), "{out:?}");
    assert!(a.host(&c_id).is_none(), "gone from A at once");
    eventually(GOSSIP, "the tombstone reaches B", || b.host(&c_id).is_none());
    let peers = String::from_utf8_lossy(&b.cli(&["hub", "peers"]).stdout).into_owned();
    assert!(peers.lines().any(|l| l.contains(" ago") && l.trim_end().ends_with(" ago") && !l.contains("this hub")), "{peers}");
    // Unpairing yourself, or nobody, is refused.
    assert!(!a.cli(&["hub", "unpair", &a_id]).status.success());
    assert!(!a.cli(&["hub", "unpair", "no-such-hub"]).status.success());

    // C comes back: it hears its tombstone and leaves, and stays out.
    c.restart();
    eventually(GOSSIP, "C leaves the swarm", || c.id() != c_id);
    assert_ne!(c.secret(), a.secret(), "C has a secret of its own again");
    let state = c.state();
    assert_eq!(state["hosts"].as_array().unwrap().len(), 1, "C is a swarm of one: {state}");
    let c_new = c.id();
    throughout(6 * SECOND, "neither C stays listed on A or B", || {
        [&a, &b].iter().all(|h| h.host(&c_id).is_none() && h.host(&c_new).is_none())
    });
    assert!(a.reaches(&b_id), "A and B are unaffected");
}

#[test]
fn unlink_rotates_the_swarm_secret() {
    let a = Hub::start();
    let b = Hub::start();
    let (a_id, b_id) = (a.id(), b.id());
    b.join(&a);
    eventually(GOSSIP, "paired", || a.reaches(&b_id) && b.reaches(&a_id));
    let old = a.secret();

    let out = a.cli(&["hub", "unlink"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("claudeship hub pair"), "{text}");
    assert_ne!(a.secret(), old, "a new swarm secret");
    assert_eq!(a.state()["hosts"].as_array().unwrap().len(), 1, "A has no peers");

    // The old secret is refused; B, still holding it, is refused.
    assert_eq!(a.peer("GET", "/peer/state", &old, None).status, 401);
    assert_eq!(a.peer("GET", "/peer/state", &a.secret(), None).status, 200);
    let a_now = a.id();
    eventually(GOSSIP, "B finds A refusing it", || {
        b.host(&a_id).is_some_and(|h| h["reachable"] == false)
    });
    let peers = String::from_utf8_lossy(&b.cli(&["hub", "peers"]).stdout).into_owned();
    assert!(peers.contains("no (refused)"), "{peers}");

    // Pairing again brings them back together.
    let out = b.cli(&["hub", "pair", &a.link()]);
    assert!(out.status.success(), "{out:?}");
    eventually(GOSSIP, "re-paired", || a.reaches(&b_id) && b.reaches(&a_now));
    assert_eq!(b.secret(), a.secret());
    assert_eq!(a_now, a_id, "the same host to everyone: its id stays");
    let hosts = b.state()["hosts"].as_array().unwrap().len();
    assert_eq!(hosts, 2, "no stale entry for A");
}

// MARK: The peer routes' rules

#[test]
fn peer_routes_need_the_bearer() {
    let a = Hub::start();
    let secret = a.secret();
    assert_eq!(secret.len(), 64);
    let mode = std::fs::metadata(a.home.join("swarm.secret")).unwrap();
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777, 0o600);

    let r = a.peer("GET", "/peer/state", &secret, None);
    assert_eq!(r.status, 200);
    let state = r.json();
    assert_eq!(state["record"]["id"], a.id().as_str());
    assert!(state["projects"].is_array() && state["now"].is_i64(), "the local state");
    assert!(state.get("hosts").is_none(), "local only: no hosts");
    assert!(state["record"]["addresses"].as_array().unwrap().contains(&json!(format!("127.0.0.1:{}", a.port))));

    // Missing, wrong (same length, one character off), other schemes, the cookie.
    let mut wrong = secret.clone().into_bytes();
    wrong[63] = if wrong[63] == b'0' { b'1' } else { b'0' };
    let wrong = String::from_utf8(wrong).unwrap();
    for (headers, why) in [
        (vec![], "no bearer"),
        (vec![format!("Authorization: Bearer {wrong}")], "wrong secret"),
        (vec![format!("Authorization: Basic {secret}")], "not a bearer"),
        (vec![format!("Authorization: Bearer {}", &secret[..32])], "a prefix"),
        (vec![format!("Cookie: claude_ship={}", a.token())], "a browser's cookie"),
    ] {
        let r = a.http("GET", "/peer/state", &headers, "");
        assert_eq!((r.status, r.json()), (401, json!({"error": "not a peer"})), "{why}");
        let r = a.http("POST", "/peer/hello", &headers, "{}");
        assert_eq!(r.status, 401, "{why}");
    }
    // A POST must be JSON, and not from a foreign page.
    let record = json!({"record": {"id": "11111111-2222-4333-8444-555555555555", "name": "x", "addresses": []}});
    let r = a.http("POST", "/peer/hello", &[format!("Authorization: Bearer {secret}")], &record.to_string());
    assert_eq!(r.status, 403, "not JSON");
    let r = a.http(
        "POST",
        "/peer/hello",
        &[
            format!("Authorization: Bearer {secret}"),
            "Content-Type: application/json".into(),
            "Origin: http://evil.example".into(),
        ],
        &record.to_string(),
    );
    assert_eq!(r.status, 403, "foreign origin");
    // A DNS name in Host is refused before anything else.
    let mut stream = TcpStream::connect(("127.0.0.1", a.port)).unwrap();
    stream
        .write_all(format!("GET /peer/state HTTP/1.1\r\nHost: evil.example:{}\r\nAuthorization: Bearer {secret}\r\n\r\n", a.port).as_bytes())
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    assert_eq!(Response::parse(&raw).status, 403);
}

/// A non-loopback, non-tailnet IPv4 address of this machine (the LAN's),
/// if it has one.
fn lan_address() -> Option<std::net::Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) => {
            let tailnet = ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64;
            (!ip.is_loopback() && !ip.is_unspecified() && !tailnet).then_some(ip)
        }
        _ => None,
    }
}

#[test]
fn peer_routes_are_refused_off_the_tailnet_like_everything_else() {
    let Some(lan) = lan_address() else {
        eprintln!("skipped: no LAN address on this machine");
        return;
    };
    let a = Hub::start();
    let secret = a.secret();
    // Sanity: the same request over loopback is answered.
    assert_eq!(a.peer("GET", "/peer/state", &secret, None).status, 200);
    let mut stream = TcpStream::connect((lan, a.port)).unwrap();
    stream.set_read_timeout(Some(5 * SECOND)).unwrap();
    let _ = stream.write_all(
        format!("GET /peer/state HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {secret}\r\n\r\n", a.port)
            .as_bytes(),
    );
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    assert!(raw.is_empty(), "LAN to LAN is dropped unanswered: {:?}", String::from_utf8_lossy(&raw));
}

#[test]
fn a_peer_request_never_makes_a_peer_request() {
    // A peer that would be contacted if anything fanned out: a closed port.
    let bogus = json!({"peers": [{
        "id": "99999999-9999-4999-8999-999999999999", "name": "bogus",
        "addresses": ["127.0.0.1:9"], "protocol": PROTOCOL, "build": "x", "lastSeen": 1,
    }]});
    // The counter counts: a hub with its poller on contacts the bogus peer.
    let polling = Hub::start_with(&[], Some(bogus.clone()));
    eventually(GOSSIP, "the poller's requests are counted", || polling.peer_requests() > 0);
    drop(polling);

    let a = Hub::start_with(&[("CLAUDESHIP_GOSSIP", "0")], Some(bogus));
    assert_eq!(a.peer_requests(), 0);
    assert!(a.state()["hosts"].as_array().unwrap().iter().any(|h| h["name"] == "bogus"));
    let secret = a.secret();
    for _ in 0..3 {
        assert_eq!(a.peer("GET", "/peer/state", &secret, None).status, 200);
    }
    let newcomer = json!({"record": {
        "id": "12345678-1234-4234-8234-123456789abc", "name": "newcomer",
        "addresses": ["127.0.0.1:10"], "protocol": PROTOCOL, "build": "x", "lastSeen": 1,
    }});
    let r = a.peer("POST", "/peer/hello", &secret, Some(newcomer));
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(mentions(&r.json()["peers"], "newcomer"));
    let next = "ab".repeat(32);
    let r = a.peer(
        "POST",
        "/peer/rotate",
        &secret,
        Some(json!({"newSecret": next, "peers": [{"id": "abcdefab-cdef-4abc-8def-abcdefabcdef", "name": "third", "addresses": ["127.0.0.1:11"]}]})),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(a.secret(), next, "rotated");
    assert_eq!(a.peer("GET", "/peer/state", &secret, None).status, 401, "the old secret is out");
    assert_eq!(a.peer("GET", "/peer/state", &next, None).status, 200);
    let _ = a.state();
    std::thread::sleep(SECOND);
    assert_eq!(a.peer_requests(), 0, "no request from us to any peer");
    // A tombstoned hub's hello is refused, and it stays out.
    let out = a.cli(&["hub", "unpair", "newcomer"]);
    assert!(out.status.success(), "{out:?}");
    let again = json!({"record": {"id": "12345678-1234-4234-8234-123456789abc", "name": "newcomer", "addresses": []}});
    assert_eq!(a.peer("POST", "/peer/hello", &next, Some(again)).status, 410);
    assert!(!mentions(&a.state()["hosts"], "newcomer"));
    assert_eq!(a.peer_requests(), 0);
}

#[test]
fn a_join_or_hello_naming_an_outside_address_sends_nothing_there() {
    // A record naming any address outside the tailnet (and loopback, under
    // the test knob) is stripped of it: otherwise a join, a hello, or a
    // gossiped record could make this hub send the swarm secret anywhere.
    let a = Hub::start_with(&[("CLAUDESHIP_GOSSIP", "0")], None);
    let stranger = "11111111-2222-4333-8444-555555555555";
    let outside = ["8.8.8.8:7433", "192.168.1.1:7433", "[2001:4860:4860::8888]:7433", "10.1.2.3:80"];
    let r = a.post(
        "/api/swarm/join",
        json!({"secret": "ab".repeat(32), "peers": [{"id": stranger, "name": "s", "addresses": outside, "lastSeen": now_ms()}]}),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    let joined = r.json();
    assert_eq!(joined["hello"][0]["ok"], false, "{joined}");
    assert_eq!(a.peer_requests(), 0, "no connection was made: {joined}");
    let peers: Value =
        serde_json::from_slice(&std::fs::read(a.home.join("peers.json")).unwrap()).unwrap();
    let stored = peers["peers"].as_array().unwrap().iter().find(|p| p["id"] == stranger).cloned().unwrap();
    assert_eq!(stored["addresses"], json!([]), "{stored}");

    // A peer's hello can't plant one either.
    let other = "22222222-2222-4333-8444-555555555555";
    let r = a.peer(
        "POST",
        "/peer/hello",
        &a.secret(),
        Some(json!({"record": {"id": other, "name": "o\u{1b}[31m", "addresses": outside}})),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    let view = a.cli(&["hub", "peers"]);
    let text = String::from_utf8_lossy(&view.stdout).to_string();
    assert!(!outside.iter().any(|o| text.contains(o)), "{text}");
    assert!(!text.contains('\u{1b}'), "no escape sequences from a peer reach the terminal");
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}
