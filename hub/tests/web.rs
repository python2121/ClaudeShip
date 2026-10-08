//! The hub's web face end to end: a private hub (`CLAUDESHIP_HOME` in a
//! short scratch directory, its own port, `HOME` and the project root in
//! the scratch directory too, `CLAUDESHIP_CMD` = the stand-in,
//! `CLAUDESHIP_WEB` = the repo's `web/`), driven with hand-written HTTP
//! requests and a hand-written WebSocket client — what a browser sends,
//! byte for byte, and nothing a library would smooth over.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CLAUDESHIP: &str = env!("CARGO_BIN_EXE_claudeship");
const STAND_IN: &str = env!("CARGO_BIN_EXE_claudeship-stand-in");
const PROTOCOL: u32 = 3;
const SECOND: Duration = Duration::from_secs(1);

struct WebHub {
    home: PathBuf,
    port: u16,
    pid: i32,
}

impl WebHub {
    fn start() -> WebHub {
        static N: AtomicUsize = AtomicUsize::new(0);
        // Short: the socket path must fit in sun_path (104 bytes on macOS).
        let home = PathBuf::from(format!(
            "/tmp/cs-w{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&home);
        for dir in ["root/proj", "root/.hidden", "h"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        // A login shell in an empty home must not stop to ask about setup.
        for rc in [".zshenv", ".zshrc", ".zprofile", ".bash_profile", ".bashrc"] {
            std::fs::write(home.join("h").join(rc), "").unwrap();
        }
        // A port that was free a moment ago can be taken before the hub
        // binds it (another test's hub, an outgoing connection), and the
        // hub then retries only every 10 s: start again on another.
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
            let mut hub = WebHub { home: home.clone(), port, pid: 0 };
            let started = hub.cli(&["hub", "start"]);
            assert!(started.status.success(), "hub start: {started:?}");
            hub.pid = hub.status()["pid"].as_i64().unwrap() as i32;
            if hub.web_listening() {
                return hub;
            }
            assert!(attempt < 5, "the web server never listened (5 ports tried)");
            let _ = hub.cli(&["hub", "stop", "--force"]);
            // Not dropped: that would remove the home.
            std::mem::forget(hub);
        }
        unreachable!()
    }

    /// Whether this hub (not whoever else might hold the port) listens,
    /// within 5 s.
    fn web_listening(&self) -> bool {
        let deadline = Instant::now() + 5 * SECOND;
        while Instant::now() < deadline {
            if self.status()["webListening"] == true {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(CLAUDESHIP)
            .args(args)
            .env("CLAUDESHIP_HOME", &self.home)
            .env("CLAUDESHIP_CMD", STAND_IN)
            .env(
                "CLAUDESHIP_WEB",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../web"),
            )
            .env("HOME", self.home.join("h"))
            .output()
            .unwrap()
    }

    fn status(&self) -> Value {
        let out = self.cli(&["hub", "status", "--json"]);
        assert!(out.status.success(), "status: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.home.join("token"))
            .unwrap()
            .trim()
            .to_string()
    }

    fn cookie(&self) -> String {
        format!("claude_ship={}", self.token())
    }

    fn host(&self) -> String {
        format!("localhost:{}", self.port)
    }

    fn origin(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// One request, as raw lines (each without its CRLF), and the answer.
    fn http(&self, method: &str, target: &str, headers: &[String], body: &str) -> Response {
        let mut request = format!("{method} {target} HTTP/1.1\r\n");
        for h in headers {
            request.push_str(h);
            request.push_str("\r\n");
        }
        if !body.is_empty() {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        request.push_str(body);
        self.raw(request.as_bytes())
    }

    /// A request as bytes, which need not be text.
    fn raw(&self, request: &[u8]) -> Response {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.set_read_timeout(Some(5 * SECOND)).unwrap();
        stream.write_all(request).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        Response::parse(&raw)
    }

    fn get(&self, target: &str, paired: bool) -> Response {
        let mut headers = vec![format!("Host: {}", self.host())];
        if paired {
            headers.push(format!("Cookie: {}", self.cookie()));
        }
        self.http("GET", target, &headers, "")
    }

    /// A same-origin JSON POST, as the page sends it.
    fn post(&self, path: &str, body: Value) -> Response {
        self.http(
            "POST",
            path,
            &[
                format!("Host: {}", self.host()),
                format!("Cookie: {}", self.cookie()),
                format!("Origin: {}", self.origin()),
                "Content-Type: application/json".into(),
            ],
            &body.to_string(),
        )
    }

    fn state(&self) -> Value {
        let r = self.get("/api/state", true);
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()
    }

    /// A session started over the Unix socket (as `claudeship` would),
    /// left running once its terminal detaches. Returns its hub id.
    fn launch(&self, args: &[&str]) -> String {
        let mut stream = UnixStream::connect(self.home.join("hub.sock")).unwrap();
        stream.set_read_timeout(Some(5 * SECOND)).unwrap();
        let hello = json!({
            "op": "launch", "protocol": PROTOCOL, "cwd": "/tmp", "args": args,
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
        // Let the program start before the terminal goes.
        std::thread::sleep(Duration::from_millis(200));
        attached["id"].as_str().unwrap().to_string()
    }

    fn clients(&self, id: &str) -> Option<i64> {
        self.status()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .and_then(|s| s["clients"].as_i64())
    }

    fn ws(&self, query: &str) -> Ws {
        self.ws_with(query, &[], None).expect("an upgrade")
    }

    /// The upgrade, with the page's headers plus `extra`; `Err` is the
    /// refusal. `recv_buffer` shrinks the socket's receive buffer.
    fn ws_with(&self, query: &str, extra: &[String], recv_buffer: Option<usize>) -> Result<Ws, Response> {
        let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        if let Some(size) = recv_buffer {
            socket.set_recv_buffer_size(size).unwrap();
        }
        let address: std::net::SocketAddr = ([127, 0, 0, 1], self.port).into();
        socket.connect(&address.into()).unwrap();
        let mut stream: TcpStream = socket.into();
        stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut request = format!(
            "GET /ws/term?{query} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n",
            self.host()
        );
        let mut headers = vec![
            format!("Cookie: {}", self.cookie()),
            format!("Origin: {}", self.origin()),
        ];
        for h in extra {
            let name = h.split(':').next().unwrap().to_lowercase();
            headers.retain(|x| x.split(':').next().unwrap().to_lowercase() != name);
            if !h.ends_with(':') {
                headers.push(h.clone());
            }
        }
        for h in headers {
            request.push_str(&h);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        let deadline = Instant::now() + 5 * SECOND;
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
            // A plain answer: read the rest of it.
            let _ = stream.set_read_timeout(Some(2 * SECOND));
            let _ = stream.read_to_end(&mut raw);
            return Err(Response::parse(&raw));
        }
        assert!(
            head.to_lowercase()
                .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="),
            "{head}"
        );
        // Apple's URLSession (the phone's WebSocket) refuses a 101 whose
        // Connection header also says close — what hyper's
        // `keep_alive(false)` used to append. Browsers let it pass, so
        // only the test and the phone would notice a regression.
        let connection = head
            .to_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("connection:"))
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        assert_eq!(connection, "upgrade", "the 101's Connection header: {head}");
        Ok(Ws {
            stream,
            pending: raw[end..].to_vec(),
            output: Vec::new(),
            texts: Vec::new(),
            close_code: None,
            closed: false,
        })
    }
}

impl Drop for WebHub {
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
        let _ = std::fs::remove_dir_all(&self.home);
        if !std::thread::panicking() {
            assert!(!alive, "the private hub (pid {}) survived hub stop --force", self.pid);
        }
    }
}

#[derive(Debug)]
struct Response {
    status: u16,
    /// Names lowercased.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn parse(raw: &[u8]) -> Response {
        let end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("no response head in {:?}", String::from_utf8_lossy(raw)));
        let head = String::from_utf8_lossy(&raw[..end]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.to_lowercase(), v.trim().to_string()))
            .collect();
        Response {
            status,
            headers,
            body: raw[end + 4..].to_vec(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.text()))
    }
}

/// A browser's end of a terminal WebSocket.
struct Ws {
    stream: TcpStream,
    pending: Vec<u8>,
    /// Every binary payload so far.
    output: Vec<u8>,
    /// Every text message so far, parsed.
    texts: Vec<Value>,
    close_code: Option<u16>,
    /// The server hung up (EOF or error).
    closed: bool,
}

impl Ws {
    fn send(&mut self, opcode: u8, payload: &[u8]) {
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
        self.stream.write_all(&frame).unwrap();
    }

    fn text(&mut self, value: Value) {
        self.send(1, value.to_string().as_bytes());
    }

    fn input(&mut self, bytes: &[u8]) {
        self.send(2, bytes);
    }

    /// The next message's opcode (payload filed into `output`/`texts`).
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

    /// The first text message (from now on, or already received and not yet
    /// taken) matching `want`.
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

    fn drain_for(&mut self, period: Duration) {
        let deadline = Instant::now() + period;
        while Instant::now() < deadline {
            if self.next(deadline.saturating_duration_since(Instant::now())).is_none() && self.closed {
                return;
            }
        }
    }

    /// Read until the server hangs up; false if it doesn't within `timeout`.
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

/// A server frame (never masked): opcode, payload, bytes used.
fn parse_frame(buffer: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    if buffer.len() < 2 {
        return None;
    }
    assert_eq!(buffer[1] & 0x80, 0, "server frames are not masked");
    assert_ne!(buffer[0] & 0x80, 0, "the hub doesn't fragment");
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

// MARK: Tests

#[test]
fn pairing_sets_the_cookie_and_redirects() {
    let hub = WebHub::start();
    let token = hub.token();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(hub.home.join("token")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "the secret is the owner's only");

    let r = hub.get("/auth?k=0123", false);
    assert_eq!(r.status, 403);
    assert_eq!(r.text(), "That pairing link is not valid for this hub.");
    assert!(r.header("set-cookie").is_none());

    let r = hub.get(&format!("/auth?k={token}"), false);
    assert_eq!(r.status, 303);
    assert_eq!(r.header("location"), Some("/"));
    assert_eq!(
        r.header("set-cookie"),
        Some(format!("claude_ship={token}; Path=/; Max-Age=31536000; HttpOnly; SameSite=Strict").as_str())
    );
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(r.header("connection"), Some("close"));

    // `hub link` hands out the same secret.
    let link = String::from_utf8_lossy(&hub.cli(&["hub", "link"]).stdout).into_owned();
    assert!(
        link.contains(&format!("http://localhost:{}/auth?k={token}", hub.port)),
        "{link}"
    );
}

#[test]
fn the_api_needs_the_cookie_and_an_honest_host() {
    let hub = WebHub::start();
    let r = hub.get("/api/state", false);
    assert_eq!(r.status, 401);
    assert_eq!(r.json(), json!({"error": "not paired"}));
    assert_eq!(r.header("content-type"), Some("application/json"));
    let r = hub.http(
        "GET",
        "/api/state",
        &[format!("Host: {}", hub.host()), format!("Cookie: claude_ship={}", "0".repeat(64))],
        "",
    );
    assert_eq!(r.status, 401, "a wrong secret is no secret");

    let state = hub.state();
    assert_eq!(state["protocol"], PROTOCOL);
    let root = hub.home.join("root").to_string_lossy().into_owned();
    assert_eq!(state["root"], root.as_str());
    assert_eq!(state["home"], hub.home.join("h").to_string_lossy().as_ref());
    assert_eq!(state["defaultPermissionMode"], "auto");
    let names: Vec<&str> = state["projects"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["proj"], "folders under the root, hidden ones not");

    // DNS names are refused whatever the cookie: DNS can lie.
    for host in ["evil.example", "evil.example:80", "localhost.evil.example", "localhost:80x"] {
        let r = hub.http(
            "GET",
            "/api/state",
            &[format!("Host: {host}"), format!("Cookie: {}", hub.cookie())],
            "",
        );
        assert_eq!(r.status, 403, "{host}");
        assert_eq!(r.text(), "This hub answers only to localhost or its Tailscale IP address.");
    }
    let r = hub.http("GET", "/api/state", &[format!("Cookie: {}", hub.cookie())], "");
    assert_eq!(r.status, 403, "no Host at all");
    for host in [format!("127.0.0.1:{}", hub.port), format!("[::1]:{}", hub.port), "100.100.1.2".into()] {
        let r = hub.http("GET", "/api/state", &[format!("Host: {host}"), format!("Cookie: {}", hub.cookie())], "");
        assert_eq!(r.status, 200, "{host}");
    }

    // Other methods and paths.
    let r = hub.http("POST", "/api/state", &[format!("Host: {}", hub.host()), format!("Cookie: {}", hub.cookie())], "");
    assert_eq!((r.status, r.text().as_str()), (405, "method not allowed"));
    assert_eq!(hub.get("/api/nothing", true).status, 404);
    assert_eq!(hub.get("/api/nothing", false).status, 401);
}

#[test]
fn posts_must_be_same_origin_json() {
    let hub = WebHub::start();
    let host = format!("Host: {}", hub.host());
    let cookie = format!("Cookie: {}", hub.cookie());
    let json_type = "Content-Type: application/json".to_string();
    let body = json!({"id": "zzzzzz"}).to_string();
    let cases = [
        (vec![host.clone(), cookie.clone(), json_type.clone(), "Origin: http://evil.example".into()], "cross-origin"),
        (vec![host.clone(), cookie.clone(), json_type.clone(), format!("Origin: http://127.0.0.1:{}", hub.port)], "another of our names is another origin"),
        (vec![host.clone(), cookie.clone(), "Content-Type: text/plain".into(), format!("Origin: {}", hub.origin())], "not JSON"),
        (vec![host.clone(), cookie.clone(), "Content-Type: application/x-www-form-urlencoded".into()], "a form"),
    ];
    for (headers, why) in cases {
        let r = hub.http("POST", "/api/kill", &headers, &body);
        assert_eq!(r.status, 403, "{why}");
        assert_eq!(r.json(), json!({"error": "refused"}), "{why}");
    }
    let r = hub.http("POST", "/api/kill", &[host.clone(), cookie.clone(), json_type.clone()], "[1]");
    assert_eq!(r.status, 403, "a JSON array is not a request");
    // Not a browser (no Origin) but paired and JSON: allowed.
    let r = hub.http("POST", "/api/kill", &[host.clone(), cookie.clone(), json_type.clone()], &body);
    assert_eq!((r.status, r.json()), (404, json!({"error": "no such session"})));
    let r = hub.http("POST", "/api/kill", &[host, json_type], &body);
    assert_eq!(r.status, 401, "unpaired");
}

#[test]
fn an_unreadable_or_doubled_origin_is_not_ours() {
    let hub = WebHub::start();
    let body = json!({"id": "zzzzzz"}).to_string();
    let head = |origins: &[&[u8]]| {
        let mut request = format!(
            "POST /api/kill HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            hub.host(),
            hub.cookie(),
            body.len()
        )
        .into_bytes();
        for origin in origins {
            request.extend_from_slice(b"Origin: ");
            request.extend_from_slice(origin);
            request.extend_from_slice(b"\r\n");
        }
        request.extend_from_slice(b"\r\n");
        request.extend_from_slice(body.as_bytes());
        request
    };
    let ours = hub.origin();
    let r = hub.raw(&head(&[ours.as_bytes()]));
    assert_eq!(r.status, 404, "ours, once: allowed");
    let r = hub.raw(&head(&[b"http://evil.example\xff"]));
    assert_eq!((r.status, r.json()), (403, json!({"error": "refused"})), "not text");
    let r = hub.raw(&head(&[ours.as_bytes(), b"http://evil.example"]));
    assert_eq!((r.status, r.json()), (403, json!({"error": "refused"})), "two of them");
    // And a head far past 64 KB (hyper enforces its limit per read, so
    // not to the byte) is refused rather than served.
    let mut stream = TcpStream::connect(("127.0.0.1", hub.port)).unwrap();
    stream.set_read_timeout(Some(5 * SECOND)).unwrap();
    let request = format!("GET / HTTP/1.1\r\nHost: {}\r\nX-Pad: {}\r\n\r\n", hub.host(), "a".repeat(200_000));
    let _ = stream.write_all(request.as_bytes());
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    assert!(
        raw.is_empty() || raw.starts_with(b"HTTP/1.1 431"),
        "{:?}",
        String::from_utf8_lossy(&raw[..raw.len().min(80)])
    );
}

#[test]
fn launch_kill_and_settings() {
    let hub = WebHub::start();
    let root = hub.home.join("root");
    let home = hub.home.join("h");
    for (path, why) in [
        ("/tmp".to_string(), "outside the root"),
        (root.join(".hidden").to_string_lossy().into_owned(), "hidden"),
        (root.to_string_lossy().into_owned(), "the root itself"),
        (root.join("proj/../..").to_string_lossy().into_owned(), "above the root"),
        (root.join("missing").to_string_lossy().into_owned(), "missing"),
    ] {
        let r = hub.post("/api/launch", json!({"path": path}));
        assert_eq!((r.status, r.json()), (400, json!({"error": "not a project directory"})), "{why}");
    }
    let proj = root.join("proj").to_string_lossy().into_owned();
    let r = hub.post("/api/launch", json!({"path": proj, "permissionMode": "yolo"}));
    assert_eq!((r.status, r.json()), (400, json!({"error": "unknown permission mode"})));
    let r = hub.post("/api/launch", json!({"path": proj, "resume": "x; rm -rf ~"}));
    assert_eq!((r.status, r.json()), (400, json!({"error": "bad conversation id"})));

    // The home directory (the quick "+") and a project, under the login shell.
    let r = hub.post("/api/launch", json!({"path": home.to_string_lossy()}));
    assert_eq!(r.status, 200, "{}", r.text());
    let quick = r.json()["id"].as_str().unwrap().to_string();
    let r = hub.post("/api/launch", json!({"path": proj, "permissionMode": "plan"}));
    assert_eq!(r.status, 200, "{}", r.text());
    let id = r.json()["id"].as_str().unwrap().to_string();

    // Straight into the directory (the cache was invalidated): Claude
    // hasn't registered them, so they are "starting", and attachable.
    let state = hub.state();
    let project = &state["projects"][0];
    assert_eq!(project["name"], "proj");
    let entry = &project["sessions"][0];
    assert_eq!(entry["key"], format!("h:{id}"));
    assert_eq!(entry["hubId"], id.as_str());
    assert_eq!(entry["status"], "starting");
    assert_eq!(entry["attachable"], true);
    assert_eq!(entry["mode"], "plan");
    assert_eq!(entry["viewers"], 0);
    assert!(entry["startedAt"].as_i64().unwrap() > 0);
    let elsewhere = &state["elsewhere"][0];
    assert_eq!(elsewhere["hubId"], quick.as_str());
    assert_eq!(elsewhere["mode"], "auto", "the default mode");
    let sessions = hub.status()["sessions"].as_array().unwrap().clone();
    assert!(sessions.iter().all(|s| s["origin"] == "web"));

    // The program runs under the login shell: attach and see it.
    let mut ws = hub.ws(&format!("id={id}&rows=30&cols=100"));
    ws.wait_for("READY");

    let r = hub.post("/api/kill", json!({"id": id}));
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    let exit = ws.wait_text(is_type("exit"));
    assert_eq!(exit["code"], 129, "hung up");
    let r = hub.post("/api/kill", json!({"id": quick}));
    assert_eq!(r.status, 200);

    // Settings.
    let r = hub.post("/api/settings", json!({"defaultPermissionMode": "nope"}));
    assert_eq!((r.status, r.json()), (400, json!({"error": "unknown permission mode"})));
    let r = hub.post("/api/settings", json!({"defaultPermissionMode": "plan"}));
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    assert_eq!(hub.state()["defaultPermissionMode"], "plan");
    let saved: Value = serde_json::from_slice(&std::fs::read(hub.home.join("config.json")).unwrap()).unwrap();
    assert_eq!(saved["defaultPermissionMode"], "plan");
    assert_eq!(saved["port"], hub.port, "the rest of the config kept");
}

#[test]
fn attach_gets_the_replay_then_live_bytes() {
    let hub = WebHub::start();
    let id = hub.launch(&["echo"]);
    let mut ws = hub.ws(&format!("id={id}&rows=30&cols=100"));
    // Size first (it is the owner now: nobody else is attached), then
    // what the program said before this screen arrived.
    let size = ws.wait_text(is_type("size"));
    assert_eq!(size, json!({"type": "size", "rows": 30, "cols": 100, "owner": true}));
    ws.wait_for("READY");
    ws.input(b"hello");
    ws.wait_for("hello");
    assert_eq!(hub.clients(&id), Some(1));
    // A second screen gets the same replay, the typing included.
    let mut other = hub.ws(&format!("id={id}&rows=30&cols=100"));
    other.wait_for("READY\r\nhello");
    ws.text(json!({"type": "ping"}));
    ws.wait_text(is_type("pong"));
    // Nonsense is ignored, not fatal.
    ws.text(json!({"type": "dance"}));
    ws.send(1, b"not json");
    ws.input(b"!");
    ws.wait_for("hello!");
    // A WebSocket ping is answered by a pong frame.
    ws.send(9, b"hi");
    let deadline = Instant::now() + 5 * SECOND;
    loop {
        assert!(Instant::now() < deadline, "no pong frame");
        if ws.next(SECOND) == Some(10) {
            break;
        }
    }
}

#[test]
fn spectators_and_who_owns_the_size() {
    let hub = WebHub::start();
    let id = hub.launch(&["size"]);
    let mut a = hub.ws(&format!("id={id}&rows=30&cols=100"));
    assert_eq!(a.wait_text(is_type("size"))["owner"], true);
    a.wait_for("SIZE 30 100");

    // claim=0: joins without resizing, and is told whose size it is.
    let mut b = hub.ws(&format!("id={id}&rows=40&cols=120&claim=0"));
    assert_eq!(
        b.wait_text(is_type("size")),
        json!({"type": "size", "rows": 30, "cols": 100, "owner": false})
    );
    // A spectator's fit is remembered, not applied.
    b.text(json!({"type": "fit", "rows": 41, "cols": 121}));
    a.drain_for(SECOND);
    assert!(!a.output_text().contains("SIZE 4"), "{}", a.output_text());
    assert!(a.texts.iter().all(|t| t["type"] != "size"), "{:?}", a.texts);

    // A resize makes the sender the owner, and everyone is told.
    b.text(json!({"type": "resize", "rows": 40, "cols": 120}));
    assert_eq!(
        b.wait_text(is_type("size")),
        json!({"type": "size", "rows": 40, "cols": 120, "owner": true})
    );
    assert_eq!(
        a.wait_text(is_type("size")),
        json!({"type": "size", "rows": 40, "cols": 120, "owner": false})
    );
    a.wait_for("SIZE 40 120");
    // The owner's fit applies at once.
    b.text(json!({"type": "fit", "rows": 42, "cols": 122}));
    a.wait_for("SIZE 42 122");
    // Out-of-range sizes are ignored.
    b.text(json!({"type": "resize", "rows": 1, "cols": 5}));
    a.drain_for(Duration::from_millis(500));
    assert!(!a.output_text().contains("SIZE 1 "));
    // Typing claims it back for a.
    a.input(b"x");
    assert_eq!(a.wait_text(|t| t["type"] == "size" && t["owner"] == true)["rows"], 30);
}

#[test]
fn exit_then_close_and_gone() {
    let hub = WebHub::start();
    let id = hub.launch(&["echo"]);
    let mut ws = hub.ws(&format!("id={id}&rows=24&cols=80"));
    ws.wait_for("READY");
    ws.input(b"q");
    assert_eq!(ws.wait_text(is_type("exit")), json!({"type": "exit", "code": 0}));
    assert!(ws.closes_within(5 * SECOND));
    assert_eq!(ws.close_code, Some(1000));

    // Ended a moment ago: a late screen still gets its output and the code.
    let mut late = hub.ws(&format!("id={id}&rows=24&cols=80"));
    late.wait_for("READY");
    assert_eq!(late.wait_text(is_type("exit"))["code"], 0);
    assert!(late.closes_within(5 * SECOND));

    for query in ["id=zzzzzz&rows=24&cols=80".to_string(), format!("id={id}x&rows=24&cols=80")] {
        let mut gone = hub.ws(&query);
        assert_eq!(gone.wait_text(is_type("gone")), json!({"type": "gone"}));
        assert!(gone.closes_within(5 * SECOND));
        assert_eq!(gone.close_code, Some(1000));
    }
    // A size out of range is no attachment either.
    let live = hub.launch(&["echo"]);
    let mut gone = hub.ws(&format!("id={live}&rows=1&cols=80"));
    gone.wait_text(is_type("gone"));
    assert!(gone.closes_within(5 * SECOND));
}

#[test]
fn upgrades_refused_unless_ours_same_origin_and_paired() {
    let hub = WebHub::start();
    let id = hub.launch(&["echo"]);
    let query = format!("id={id}&rows=24&cols=80");
    let refusals: [(Vec<String>, &str); 3] = [
        (vec!["Origin: http://evil.example".into()], "cross-origin"),
        (vec!["Cookie:".into()], "unpaired"),
        (vec![format!("Cookie: claude_ship={}", "1".repeat(64))], "wrong secret"),
    ];
    for (extra, why) in refusals {
        let Err(r) = hub.ws_with(&query, &extra, None) else {
            panic!("{why}: upgraded");
        };
        assert_eq!((r.status, r.text().as_str()), (403, "websocket refused"), "{why}");
    }
    // An upgrade anywhere else.
    let r = hub.http(
        "GET",
        "/api/state",
        &[
            format!("Host: {}", hub.host()),
            format!("Cookie: {}", hub.cookie()),
            "Upgrade: websocket".into(),
            "Connection: Upgrade".into(),
            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==".into(),
            "Sec-WebSocket-Version: 13".into(),
        ],
        "",
    );
    assert_eq!((r.status, r.text().as_str()), (403, "websocket refused"));
    // No Origin (not a browser) but paired: fine.
    let mut ws = hub.ws_with(&query, &["Origin:".into()], None).expect("upgraded");
    ws.wait_for("READY");
    assert_eq!(hub.clients(&id), Some(1));
}

#[test]
fn unlink_drops_open_terminals_and_the_old_cookie() {
    let hub = WebHub::start();
    let id = hub.launch(&["echo"]);
    let old = hub.cookie();
    let mut ws = hub.ws(&format!("id={id}&rows=24&cols=80"));
    ws.wait_for("READY");
    let out = hub.cli(&["hub", "unlink"]);
    assert!(out.status.success(), "{out:?}");
    assert!(ws.closes_within(3 * SECOND), "the terminal was hung up");
    assert_ne!(hub.cookie(), old, "a new secret");
    let r = hub.http("GET", "/api/state", &[format!("Host: {}", hub.host()), format!("Cookie: {old}")], "");
    assert_eq!(r.status, 401);
    assert_eq!(hub.get("/api/state", true).status, 200);
    let deadline = Instant::now() + 3 * SECOND;
    while hub.clients(&id) != Some(0) {
        assert!(Instant::now() < deadline, "the hub still counts the browser");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_pages_files() {
    let hub = WebHub::start();
    let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("../web");
    let r = hub.get("/", false);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(r.body, std::fs::read(web.join("index.html")).unwrap());
    let port = hub.port;
    assert_eq!(
        r.header("content-security-policy").map(str::to_string),
        Some(format!(
            "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             connect-src 'self' ws://localhost:{port} wss://localhost:{port}; frame-ancestors 'none'; base-uri 'none'"
        ))
    );
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let r = hub.get("/app.js", false);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("text/javascript; charset=utf-8"));
    assert_eq!(r.body, std::fs::read(web.join("app.js")).unwrap());
    let r = hub.get("/vendor/xterm.css", false);
    assert_eq!(r.header("content-type"), Some("text/css; charset=utf-8"));
    assert_eq!(hub.get("/manifest.webmanifest", false).header("content-type"), Some("application/manifest+json"));
    assert_eq!(hub.get("/icon.svg", false).header("content-type"), Some("image/svg+xml"));
    for path in ["/vendor/XTERM-LICENSE", "/.hidden.js", "/vendor/../app.js", "/vendor/%2e%2e/app.js", "/missing.js", "/x//app.js"] {
        let r = hub.get(path, false);
        assert_eq!((r.status, r.text().as_str()), (404, "not found"), "{path}");
        assert!(r.header("content-security-policy").is_none());
    }
    let r = hub.http("DELETE", "/", &[format!("Host: {}", hub.host())], "");
    assert_eq!(r.status, 405);
    let r = hub.http("GET", "/", &["Host: evil.example".into()], "");
    assert_eq!(r.status, 403, "even the page needs an honest Host");
}

#[test]
fn a_request_that_never_finishes_is_dropped_at_15s() {
    let hub = WebHub::start();
    let mut stream = TcpStream::connect(("127.0.0.1", hub.port)).unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n").unwrap();
    stream.set_read_timeout(Some(30 * SECOND)).unwrap();
    let started = Instant::now();
    let mut buffer = [0u8; 1024];
    let n = stream.read(&mut buffer).unwrap_or(0);
    let waited = started.elapsed();
    assert_eq!(n, 0, "closed without an answer");
    assert!(
        waited >= 14 * SECOND && waited <= 20 * SECOND,
        "dropped after {waited:?}"
    );
}

#[test]
fn a_terminal_that_stops_reading_is_dropped_after_a_minute() {
    let hub = WebHub::start();
    let id = hub.launch(&["echo"]);
    let attached = Instant::now();
    // A small receive window and never a read: what a frozen tab does.
    let stalled = hub
        .ws_with(&format!("id={id}&rows=24&cols=80"), &[], Some(4096))
        .expect("upgraded");
    // Output for it to fall behind on, from a screen that keeps up.
    let mut typist = hub.ws(&format!("id={id}&rows=24&cols=80&claim=0"));
    typist.wait_for("READY");
    let chunk = vec![b'a'; 64 * 1024];
    let (mut counted, mut echoed) = (0, 0);
    for i in 0..64 {
        typist.input(&chunk);
        let want = (i + 1) * chunk.len();
        let deadline = Instant::now() + 10 * SECOND;
        while echoed < want {
            assert!(Instant::now() < deadline, "echo stalled at {i}");
            typist.next(Duration::from_millis(200));
            echoed += typist.output[counted..].iter().filter(|&&b| b == b'a').count();
            counted = typist.output.len();
        }
    }
    let flooded = Instant::now();
    assert_eq!(hub.clients(&id), Some(2));
    // Not before a minute has passed since it could first have stalled...
    let wait = (attached + 50 * SECOND).saturating_duration_since(Instant::now());
    std::thread::sleep(wait);
    assert_eq!(hub.clients(&id), Some(2), "dropped too soon");
    // ...but within one of the flood (plus a little).
    let deadline = flooded + 70 * SECOND;
    while hub.clients(&id) != Some(1) {
        assert!(Instant::now() < deadline, "the stalled terminal was never dropped");
        std::thread::sleep(SECOND);
    }
    // The other screen is unaffected.
    typist.input(b"z");
    typist.wait_for("z");
    drop(stalled.stream.shutdown(std::net::Shutdown::Both));
}
