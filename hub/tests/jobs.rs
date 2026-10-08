//! Jobs end to end (plan phase 11, part A): private hubs with `"jobs":
//! true` in their config, the stand-in in place of claude (its `-p` mode
//! prints stream-json), hand-written HTTP as in `proxy.rs`, and the
//! `claudeship run | ask | jobs` commands. Two hubs peer over loopback
//! (`CLAUDESHIP_ADVERTISE_LOOPBACK=1`) for the relayed job.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CLAUDESHIP: &str = env!("CARGO_BIN_EXE_claudeship");
const STAND_IN: &str = env!("CARGO_BIN_EXE_claudeship-stand-in");
const SECOND: Duration = Duration::from_secs(1);
const GOSSIP: Duration = Duration::from_secs(20);

struct Hub {
    home: PathBuf,
    port: u16,
    pid: i32,
    env: Vec<(&'static str, String)>,
}

impl Hub {
    fn start(jobs: bool) -> Hub {
        Hub::start_with(json!({"jobs": jobs}), &[])
    }

    fn start_with(extra: Value, env: &[(&'static str, &str)]) -> Hub {
        static N: AtomicUsize = AtomicUsize::new(0);
        // Short: the socket path must fit in sun_path.
        let home = PathBuf::from(format!("/tmp/cs-j{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&home);
        for dir in ["root/proj/.claude/worktrees/w1", "h", "claude/sessions", "out"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        // A login shell in an empty home must not stop to ask about setup.
        for rc in [".zshenv", ".zshrc", ".zprofile", ".bash_profile", ".bashrc"] {
            std::fs::write(home.join("h").join(rc), "").unwrap();
        }
        let mut all = vec![("CLAUDESHIP_ADVERTISE_LOOPBACK", "1".to_string())];
        all.extend(env.iter().map(|(k, v)| (*k, v.to_string())));
        for attempt in 1.. {
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let mut config = json!({"port": port, "root": home.join("root")});
            for (k, v) in extra.as_object().unwrap() {
                config[k] = v.clone();
            }
            std::fs::write(home.join("config.json"), config.to_string()).unwrap();
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

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(CLAUDESHIP);
        command
            .args(args)
            .current_dir(self.home.join("root/proj"))
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

    fn token(&self) -> String {
        std::fs::read_to_string(self.home.join("token")).unwrap().trim().to_string()
    }

    fn secret(&self) -> String {
        std::fs::read_to_string(self.home.join("swarm.secret")).unwrap().trim().to_string()
    }

    fn path(&self, p: &str) -> String {
        self.home.join(p).to_str().unwrap().to_string()
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
        stream.set_read_timeout(Some(90 * SECOND)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        Response::parse(&raw)
    }

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

    fn get(&self, target: &str) -> Response {
        self.http("GET", target, &[format!("Cookie: claude_ship={}", self.token())], "")
    }

    /// Starts a job: its id.
    fn job(&self, body: Value) -> String {
        let r = self.post("/api/jobs", body);
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["host"], self.id());
        v["id"].as_str().unwrap().to_string()
    }

    fn argv_job(&self, argv: &[&str]) -> String {
        self.job(json!({"cwd": self.path("root/proj"), "argv": argv}))
    }

    /// One read of a job (`query` after `?`).
    fn read(&self, id: &str, query: &str) -> Value {
        let r = self.get(&format!("/api/jobs/{id}?{query}"));
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()
    }

    /// The job once it has finished.
    fn finished(&self, id: &str) -> Value {
        let deadline = Instant::now() + 60 * SECOND;
        loop {
            let job = self.read(id, "wait=30");
            if job["running"] == false {
                return job;
            }
            assert!(Instant::now() < deadline, "job {id} never finished: {job}");
        }
    }

    fn list(&self) -> Vec<Value> {
        let r = self.get("/api/jobs");
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()["jobs"].as_array().unwrap().clone()
    }

    fn join(&self, member: &Hub) {
        let swarm = member.post("/api/swarm", json!({}));
        assert_eq!(swarm.status, 200, "{}", swarm.text());
        let swarm = swarm.json();
        let r = self.post("/api/swarm/join", json!({"secret": swarm["secret"], "peers": swarm["peers"]}));
        assert_eq!(r.status, 200, "{}", r.text());
        let mine = self.id();
        eventually(GOSSIP, "the member reaches the joiner", || {
            let state = member.get("/api/state").json();
            state["hosts"].as_array().unwrap().iter().any(|h| h["id"] == mine && h["reachable"] == true)
        });
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.cli(&["hub", "stop", "--force"]);
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

fn eventually(limit: Duration, why: &str, mut what: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !what() {
        assert!(Instant::now() < deadline, "never happened: {why}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn alive(pid: i64) -> bool {
    // SAFETY: probing with signal 0.
    pid > 0 && unsafe { libc::kill(pid as i32, 0) } == 0
}

fn is_uuid(v: &Value) -> bool {
    v.as_str().is_some_and(|s| s.len() == 36 && s.chars().filter(|&c| c == '-').count() == 4)
}

fn text(out: &[u8]) -> String {
    String::from_utf8_lossy(out).into_owned()
}

// MARK: Tests

#[test]
fn run_wait_output_and_kill() {
    let hub = Hub::start(true);
    let id = hub.argv_job(&[STAND_IN, "ticks", "3", "100"]);
    assert_eq!(id.len(), 6, "six hex digits like a session");
    let job = hub.finished(&id);
    assert_eq!(job["exitCode"], 0);
    assert_eq!(job["stdout"], "TICK 0\nTICK 1\nTICK 2\n");
    assert_eq!(job["truncated"], false);
    assert_eq!(job["timedOut"], false);
    assert_eq!(job["argv"], json!([STAND_IN, "ticks", "3", "100"]));
    assert_eq!(job["cwd"].as_str().unwrap(), std::fs::canonicalize(hub.path("root/proj")).unwrap().to_str().unwrap());
    assert_eq!(job["next"], 21);
    assert!(job["finishedAt"].as_i64().unwrap() >= job["startedAt"].as_i64().unwrap());
    // From an offset.
    assert_eq!(hub.read(&id, "since=14")["stdout"], "TICK 2\n");
    // Its own process group, not the hub's, and stderr kept apart.
    let err = hub.finished(&hub.job(json!({"argv": ["/bin/sh", "-c", "echo out; echo err >&2; exit 7"]})));
    assert_eq!((err["stdout"].as_str(), err["stderr"].as_str(), err["exitCode"].as_i64()), (Some("out\n"), Some("err\n"), Some(7)));

    // Kill: SIGTERM to the group.
    let sleeper = hub.argv_job(&[STAND_IN, "sleep", "30"]);
    let pid = hub.read(&sleeper, "")["pid"].as_i64().unwrap();
    assert!(alive(pid));
    let r = hub.post(&format!("/api/jobs/{sleeper}/kill"), json!({}));
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    let killed = hub.finished(&sleeper);
    assert_eq!(killed["exitCode"], 128 + libc::SIGTERM);
    assert_eq!(killed["timedOut"], false);
    let listed: Vec<String> = hub.list().iter().map(|j| j["id"].as_str().unwrap().to_string()).collect();
    assert!(listed.contains(&id) && listed.contains(&sleeper), "{listed:?}");
    assert_eq!(hub.get("/api/jobs/zzzzzz").status, 404);
    assert_eq!(hub.post("/api/jobs/zzzzzz/kill", json!({})).status, 404);

    // The same from the command line.
    let out = hub.cli(&["run", "--", STAND_IN, "sleep", "0.3"]);
    assert!(out.status.success(), "{out:?}");
    let cli_id = text(&out.stdout).trim().to_string();
    assert_eq!(cli_id.len(), 6, "{cli_id}");
    let out = hub.cli(&["jobs", "wait", &cli_id]);
    assert_eq!((out.status.code(), text(&out.stdout)), (Some(0), "START\nEND\n".to_string()), "{out:?}");
    let out = hub.cli(&["jobs", "output", &cli_id]);
    assert_eq!(text(&out.stdout), "START\nEND\n");
    let out = hub.cli(&["jobs"]);
    assert!(text(&out.stdout).contains(&cli_id) && text(&out.stdout).contains("exit 0"), "{out:?}");
    let out = hub.cli(&["run", "--", STAND_IN, "sleep", "30"]);
    let long = text(&out.stdout).trim().to_string();
    let out = hub.cli(&["jobs", "kill", &long]);
    assert!(out.status.success() && text(&out.stdout).contains("told to end"), "{out:?}");
    let out = hub.cli(&["jobs", "wait", &long]);
    assert_eq!(out.status.code(), Some(128 + libc::SIGTERM));
    let out = hub.cli(&["jobs", "wait", "nope00"]);
    assert!(!out.status.success() && text(&out.stderr).contains("no such job"), "{out:?}");
}

#[test]
fn stream_json_session_ids_and_resume() {
    let hub = Hub::start(true);
    let proj = hub.path("root/proj");
    let first = hub.finished(&hub.job(json!({"cwd": proj, "prompt": "hello there"})));
    assert_eq!(first["exitCode"], 0);
    assert!(is_uuid(&first["claudeSessionId"]), "{first}");
    assert_eq!(first["result"], "echo: hello there");
    assert_eq!(first["permissionMode"], "auto", "the default mode");
    assert_eq!(
        first["argv"],
        json!([STAND_IN, "-p", "hello there", "--verbose", "--output-format", "stream-json", "--permission-mode", "auto"])
    );
    let init: Value = serde_json::from_str(first["stdout"].as_str().unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(init["permissionMode"], "auto");
    let sid = first["claudeSessionId"].as_str().unwrap();
    let second = hub.finished(&hub.job(json!({"cwd": proj, "prompt": "and again", "resume": sid, "permissionMode": "plan"})));
    assert_eq!(second["claudeSessionId"], sid, "the follow-up continues the conversation");
    assert_eq!(second["result"], "echo: and again resumed");
    assert_eq!(second["permissionMode"], "plan");
    assert_eq!(hub.post("/api/jobs", json!({"prompt": "x", "resume": "not-a-uuid"})).status, 400);
    assert_eq!(hub.post("/api/jobs", json!({"prompt": "x", "permissionMode": "yolo"})).status, 400);
    assert_eq!(hub.post("/api/jobs", json!({"argv": []})).status, 400);
    assert_eq!(hub.post("/api/jobs", json!({"cwd": proj})).status, 400);

    // waitingFor: the job's Claude registered a session that waits.
    let id = hub.job(json!({"cwd": proj, "prompt": "sleep 30", "permissionMode": "manual"}));
    let mut job = hub.read(&id, "");
    eventually(10 * SECOND, "the session id is known", || {
        job = hub.read(&id, "");
        job["claudeSessionId"].is_string()
    });
    assert_eq!(job["waitingFor"], Value::Null);
    let pid = job["pid"].as_i64().unwrap();
    let entry = json!({
        "pid": pid, "sessionId": job["claudeSessionId"], "cwd": proj, "status": "waiting",
        "waitingFor": "approve Bash", "startedAt": 1, "statusUpdatedAt": 2,
    });
    std::fs::write(hub.home.join(format!("claude/sessions/{pid}.json")), entry.to_string()).unwrap();
    assert_eq!(hub.read(&id, "")["waitingFor"], "approve Bash");
    hub.post(&format!("/api/jobs/{id}/kill"), json!({}));
}

#[test]
fn ask_end_to_end() {
    let hub = Hub::start(true);
    let out = hub.cli(&["ask", "hi", "there"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "echo: hi there\n");
    let stderr = text(&out.stderr);
    let sid = stderr
        .lines()
        .find_map(|l| l.strip_prefix("session: "))
        .unwrap_or_else(|| panic!("no session line: {stderr}"))
        .to_string();
    assert!(is_uuid(&json!(sid)));
    let out = hub.cli(&["ask", "--resume", &sid, "--mode", "plan", "more"]);
    assert_eq!(text(&out.stdout), "echo: more resumed\n", "{out:?}");
    assert!(text(&out.stderr).contains(&format!("session: {sid}")));
    // -v streams the events to stderr.
    let out = hub.cli(&["ask", "-v", "loud"]);
    assert!(text(&out.stderr).contains("\"type\":\"system\""), "{out:?}");
    assert_eq!(text(&out.stdout), "echo: loud\n");
    // Its exit code, and its stderr when it fails.
    let out = hub.cli(&["ask", "exit 3"]);
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    // The cap.
    let started = Instant::now();
    let out = hub.cli(&["ask", "--max-seconds", "1", "sleep 30"]);
    assert_eq!(out.status.code(), Some(124), "{out:?}");
    assert!(text(&out.stderr).contains("ran past its 1 s"), "{out:?}");
    assert!(started.elapsed() < 10 * SECOND);
    // A directory it may not use.
    let out = hub.cli(&["ask", "--cwd", "/", "x"]);
    assert!(!out.status.success() && text(&out.stderr).contains("cwd must be"), "{out:?}");
}

#[test]
fn output_is_bounded_and_flagged() {
    let hub = Hub::start(true);
    let job = hub.finished(&hub.argv_job(&[STAND_IN, "blast", "6"]));
    assert_eq!(job["exitCode"], 0);
    assert_eq!(job["truncated"], true);
    let total = job["stdoutBytes"].as_u64().unwrap();
    assert!(total > 6 << 20, "{total}");
    let kept = job["stdout"].as_str().unwrap();
    assert_eq!(kept.len(), 4 << 20, "the newest 4 MB");
    assert!(kept.ends_with("DONE\r\n"));
    assert_eq!(job["since"].as_u64().unwrap(), total - (4 << 20));
    assert_eq!(job["next"].as_u64().unwrap(), total);
}

#[test]
fn retention_consume_and_eviction() {
    let hub = Hub::start_with(json!({"jobs": true}), &[("CLAUDESHIP_JOBS_KEEP_SECONDS", "3")]);
    let a = hub.finished(&hub.argv_job(&["/usr/bin/true"]))["id"].as_str().unwrap().to_string();
    // consume: read once, then gone.
    let read = hub.read(&a, "consume=1");
    assert_eq!(read["exitCode"], 0);
    assert_eq!(hub.get(&format!("/api/jobs/{a}")).status, 404);
    // Consuming a running job leaves it.
    let running = hub.argv_job(&[STAND_IN, "sleep", "30"]);
    assert_eq!(hub.read(&running, "consume=1")["running"], true);
    assert_eq!(hub.read(&running, "")["running"], true);
    hub.post(&format!("/api/jobs/{running}/kill"), json!({}));
    hub.finished(&running);
    // Kept for the keep time (an hour; 3 s here), then gone.
    let b = hub.finished(&hub.argv_job(&["/usr/bin/true"]))["id"].as_str().unwrap().to_string();
    assert_eq!(hub.get(&format!("/api/jobs/{b}")).status, 200);
    std::thread::sleep(4 * SECOND);
    assert_eq!(hub.get(&format!("/api/jobs/{b}")).status, 404);
    assert!(hub.list().is_empty());
    drop(hub);
    // At 64, the first finished goes (a hub with the hour's keep time).
    let hub = Hub::start(true);
    let mut ids = Vec::new();
    for _ in 0..65 {
        let id = hub.argv_job(&["/usr/bin/true"]);
        hub.finished(&id);
        ids.push(id);
    }
    let listed: Vec<String> = hub.list().iter().map(|j| j["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(listed.len(), 64);
    assert!(!listed.contains(&ids[0]), "the oldest finished job was evicted");
    assert!(listed.contains(&ids[1]) && listed.contains(&ids[64]));
}

#[test]
fn wait_returns_on_finish_and_on_new_output() {
    let hub = Hub::start(true);
    // New output.
    let ticks = hub.argv_job(&[STAND_IN, "ticks", "3", "1500"]);
    let first = hub.read(&ticks, "wait=30&since=0");
    assert!(first["stdout"].as_str().unwrap().starts_with("TICK 0\n"), "{first}");
    let next = first["next"].as_u64().unwrap();
    let started = Instant::now();
    let second = hub.read(&ticks, &format!("wait=30&since={next}"));
    let waited = started.elapsed();
    assert_eq!(second["stdout"], "TICK 1\n", "{second}");
    assert_eq!(second["running"], true);
    assert!(waited > Duration::from_millis(500) && waited < 5 * SECOND, "{waited:?}");
    // No change: the wait runs out.
    let quiet = hub.argv_job(&[STAND_IN, "sleep", "30"]);
    let at = hub.read(&quiet, "wait=5")["next"].as_u64().unwrap();
    let started = Instant::now();
    let same = hub.read(&quiet, &format!("wait=2&since={at}"));
    assert!(started.elapsed() >= 2 * SECOND - Duration::from_millis(100));
    assert_eq!((same["running"].as_bool(), same["stdout"].as_str()), (Some(true), Some("")));
    hub.post(&format!("/api/jobs/{quiet}/kill"), json!({}));
    // A finish (past the 15 s an ordinary request gets).
    let sleeper = hub.argv_job(&[STAND_IN, "sleep", "17"]);
    let at = hub.read(&sleeper, "wait=5")["next"].as_u64().unwrap();
    let started = Instant::now();
    let woke = hub.read(&sleeper, &format!("wait=60&since={at}"));
    assert_eq!(woke["stdout"], "END\n", "{woke}");
    assert!(started.elapsed() > 14 * SECOND, "{:?}", started.elapsed());
    // Then the finish itself, with nothing new to read.
    let done = hub.read(&sleeper, &format!("wait=60&since={}", woke["next"]));
    assert_eq!((done["running"].as_bool(), done["exitCode"].as_i64(), done["stdout"].as_str()), (Some(false), Some(0), Some("")));
}

#[test]
fn where_a_job_may_run() {
    let hub = Hub::start(true);
    let at = |cwd: String| hub.post("/api/jobs", json!({"cwd": cwd, "argv": ["/usr/bin/true"]}));
    let r = at("/tmp".into());
    assert_eq!(r.status, 400);
    assert!(r.json()["error"].as_str().unwrap().contains("under"), "{}", r.text());
    assert_eq!(at(hub.path("out")).status, 400, "outside the root");
    assert_eq!(at(format!("{}/../out", hub.path("root"))).status, 400);
    assert_eq!(at(hub.path("root/missing")).status, 400);
    assert_eq!(at(hub.path("h")).status, 200, "the home directory");
    assert_eq!(at(hub.path("root/proj/.claude/worktrees/w1")).status, 200, "a worktree");
    assert_eq!(at(hub.path("root")).status, 200, "the root itself");
    let job = hub.finished(&hub.job(json!({"cwd": hub.path("root/proj/.claude/worktrees/w1"), "argv": ["/bin/pwd"]})));
    assert!(job["stdout"].as_str().unwrap().trim_end().ends_with("root/proj/.claude/worktrees/w1"), "{job}");
    // The request's env, but not one that changes the shell's start.
    let job = hub.finished(&hub.job(json!({"argv": ["/bin/sh", "-c", "echo $JOB_X"], "env": {"JOB_X": "y"}})));
    assert_eq!(job["stdout"], "y\n");
    assert_eq!(hub.post("/api/jobs", json!({"argv": ["/usr/bin/true"], "env": {"DYLD_INSERT_LIBRARIES": "x"}})).status, 400);
    // A cross-origin POST is refused like every other.
    let r = hub.http(
        "POST",
        "/api/jobs",
        &[format!("Cookie: claude_ship={}", hub.token()), "Origin: http://evil.example".into(), "Content-Type: application/json".into()],
        &json!({"argv": ["/usr/bin/true"]}).to_string(),
    );
    assert_eq!(r.status, 403);
    assert_eq!(hub.http("GET", "/api/jobs", &[], "").status, 401, "unpaired");
}

#[test]
fn jobs_off_is_403_everywhere() {
    let hub = Hub::start(false);
    let refused = json!({"error": "jobs disabled on this host"});
    for r in [
        hub.post("/api/jobs", json!({"argv": ["/usr/bin/true"]})),
        hub.get("/api/jobs"),
        hub.get("/api/jobs/abcdef?wait=5"),
        hub.post("/api/jobs/abcdef/kill", json!({})),
        hub.http(
            "POST",
            "/peer/api/jobs",
            &[format!("Authorization: Bearer {}", hub.secret()), "Content-Type: application/json".into()],
            &json!({"argv": ["/usr/bin/true"]}).to_string(),
        ),
    ] {
        assert_eq!((r.status, r.json()), (403, refused.clone()));
    }
    assert_eq!(hub.status()["jobs"], false);
    let out = hub.cli(&["hub", "status"]);
    assert!(text(&out.stdout).contains("jobs: disabled"), "{out:?}");
    let out = hub.cli(&["ask", "x"]);
    assert!(!out.status.success() && text(&out.stderr).contains("jobs disabled on this host"), "{out:?}");
    let on = Hub::start(true);
    assert!(text(&on.cli(&["hub", "status"]).stdout).contains("jobs: enabled"));
}

#[test]
fn the_jobs_switch_is_live_and_local() {
    let hub = Hub::start(false);
    let refused = json!({"error": "jobs disabled on this host"});
    assert_eq!(hub.post("/api/jobs", json!({"argv": ["/bin/sleep", "30"]})).status, 403);
    // On, with no restart; the file remembers it for the next one.
    let r = hub.post("/api/settings", json!({"jobs": true}));
    assert_eq!((r.status, r.json()), (200, json!({"ok": true, "jobs": true, "ended": 0})));
    assert_eq!(hub.status()["jobs"], true);
    assert_eq!(hub.get("/api/state").json()["jobs"], true, "the state says so");
    let saved: Value = serde_json::from_slice(&std::fs::read(hub.home.join("config.json")).unwrap()).unwrap();
    assert_eq!(saved["jobs"], true, "persisted");
    let id = hub.post("/api/jobs", json!({"argv": ["/bin/sleep", "30"]})).json()["id"].as_str().unwrap().to_string();
    let pid = hub.get(&format!("/api/jobs/{id}")).json()["pid"].as_i64().unwrap();
    assert!(alive(pid));
    // Off is total: what runs is ended, what comes is refused.
    let r = hub.post("/api/settings", json!({"jobs": false}));
    assert_eq!((r.status, r.json()), (200, json!({"ok": true, "jobs": false, "ended": 1})));
    eventually(Duration::from_secs(5), "the job's process ends", || !alive(pid));
    let r = hub.get("/api/jobs");
    assert_eq!((r.status, r.json()), (403, refused));
    assert_eq!(hub.status()["jobs"], false);
    assert_eq!(hub.get("/api/state").json()["jobs"], false);
    // Only a boolean, only from a client paired with this hub: not a
    // peer's relay, not a request naming another machine.
    assert_eq!(hub.post("/api/settings", json!({"jobs": "yes"})).status, 400);
    let r = hub.http(
        "POST",
        "/peer/api/settings",
        &[format!("Authorization: Bearer {}", hub.secret()), "Content-Type: application/json".into()],
        &json!({"jobs": true}).to_string(),
    );
    assert_eq!((r.status, r.json()), (403, json!({"error": "a peer can't switch jobs on this hub"})));
    let r = hub.post("/api/settings", json!({"jobs": true, "host": "ffffffff-ffff-4fff-8fff-ffffffffffff"}));
    assert_ne!(r.status, 200, "naming another host: {}", r.status);
    assert_eq!(hub.status()["jobs"], false, "neither switched it on");
    // From a terminal on the machine, over the socket.
    assert!(text(&hub.cli(&["hub", "jobs"]).stdout).contains("jobs: disabled"));
    assert!(text(&hub.cli(&["hub", "jobs", "on"]).stdout).contains("jobs: enabled"));
    assert_eq!(hub.status()["jobs"], true);
    assert!(text(&hub.cli(&["hub", "jobs", "off"]).stdout).contains("jobs: disabled"));
    assert_eq!(hub.status()["jobs"], false);
    assert!(!hub.cli(&["hub", "jobs", "maybe"]).status.success());
    let log = std::fs::read_to_string(hub.home.join("hub.log")).unwrap_or_default();
    assert!(
        log.contains("jobs enabled from this machine (a browser on this machine)")
            && log.contains("jobs disabled from this machine (a browser on this machine) — 1 running job(s) ended")
            && log.contains("jobs enabled from this machine (claudeship hub jobs)"),
        "{log}"
    );
}

#[test]
fn hub_stop_kills_running_jobs() {
    let hub = Hub::start(true);
    let a = hub.argv_job(&[STAND_IN, "sleep", "60"]);
    let b = hub.argv_job(&[STAND_IN, "ignore-term"]);
    let pids: Vec<i64> = [&a, &b].iter().map(|id| hub.read(id, "wait=5")["pid"].as_i64().unwrap()).collect();
    assert!(pids.iter().all(|&p| alive(p)));
    let out = hub.cli(&["hub", "stop", "--force"]);
    assert!(out.status.success(), "{out:?}");
    eventually(5 * SECOND, "the jobs are gone with the hub", || pids.iter().all(|&p| !alive(p)));
}

#[test]
fn the_wall_clock_cap() {
    let hub = Hub::start(true);
    // Honours SIGTERM: ends at once.
    let started = Instant::now();
    let polite = hub.finished(&hub.job(json!({"argv": [STAND_IN, "sleep", "60"], "maxSeconds": 1})));
    assert!(started.elapsed() < 6 * SECOND);
    assert_eq!((polite["exitCode"].as_i64(), polite["timedOut"].as_bool()), (Some(124), Some(true)));
    assert_eq!(polite["stdout"], "START\n", "the output so far is kept");
    assert_eq!(polite["maxSeconds"], 1);
    // Ignores it: SIGKILL ten seconds later.
    let started = Instant::now();
    let stubborn = hub.job(json!({"argv": [STAND_IN, "ignore-term"], "maxSeconds": 1}));
    let job = hub.finished(&stubborn);
    let took = started.elapsed();
    assert!(took >= 10 * SECOND && took < 20 * SECOND, "{took:?}");
    assert_eq!((job["exitCode"].as_i64(), job["timedOut"].as_bool()), (Some(124), Some(true)));
    assert_eq!(job["stdout"], "IGNORING-TERM\n");
    assert!(!alive(job["pid"].as_i64().unwrap()));
    // The ceiling: 14 400 s unless the config raises it.
    assert_eq!(hub.post("/api/jobs", json!({"argv": ["/usr/bin/true"], "maxSeconds": 14_401})).status, 400);
    assert_eq!(hub.post("/api/jobs", json!({"argv": ["/usr/bin/true"], "maxSeconds": 14_400})).status, 200);
    assert_eq!(hub.read(&hub.argv_job(&["/usr/bin/true"]), "")["maxSeconds"], 1800, "the default");
    let raised = Hub::start_with(json!({"jobs": true, "jobsMaxSeconds": 20_000}), &[]);
    assert_eq!(raised.post("/api/jobs", json!({"argv": ["/usr/bin/true"], "maxSeconds": 20_000})).status, 200);
}

#[test]
fn a_job_through_a_peer() {
    // A (jobs off: it only relays) and B (jobs on).
    let a = Hub::start(false);
    let b = Hub::start(true);
    b.join(&a);
    let b_id = b.id();
    eventually(GOSSIP, "A reaches B", || {
        a.get("/api/state").json()["hosts"].as_array().unwrap().iter().any(|h| h["id"] == b_id && h["reachable"] == true)
    });
    let r = a.post("/api/jobs", json!({"host": b_id, "cwd": b.path("root/proj"), "argv": [STAND_IN, "sleep", "5"]}));
    assert_eq!(r.status, 200, "{}", r.text());
    let started = r.json();
    assert_eq!(started["host"], b_id);
    let id = started["id"].as_str().unwrap().to_string();
    // B runs it; A has none.
    assert_eq!(b.list().len(), 1);
    assert_eq!(a.get("/api/jobs").status, 403);
    // A long-poll through the relay, past its usual 3 s.
    let first = a.get(&format!("/api/jobs/{id}?host={b_id}&wait=10")).json();
    let at = first["next"].as_u64().unwrap();
    let begun = Instant::now();
    let r = a.get(&format!("/api/jobs/{id}?host={b_id}&wait=30&since={at}"));
    assert_eq!(r.status, 200, "{}", r.text());
    let woke = r.json();
    assert_eq!((woke["stdout"].as_str(), woke["host"].as_str()), (Some("END\n"), Some(b_id.as_str())), "{woke}");
    assert!(begun.elapsed() > 4 * SECOND, "it waited on B");
    let done = a.get(&format!("/api/jobs/{id}?host={b_id}&wait=30&since={}", woke["next"])).json();
    assert_eq!((done["running"].as_bool(), done["exitCode"].as_i64()), (Some(false), Some(0)), "{done}");
    let listed = a.get(&format!("/api/jobs?host={b_id}")).json();
    assert_eq!(listed["jobs"][0]["id"], id.as_str());
    // Kill through A.
    let r = a.post("/api/jobs", json!({"host": b_id, "argv": [STAND_IN, "sleep", "30"]}));
    let long = r.json()["id"].as_str().unwrap().to_string();
    let r = a.post(&format!("/api/jobs/{long}/kill"), json!({"host": b_id}));
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    assert_eq!(b.finished(&long)["exitCode"], 128 + libc::SIGTERM);
    // B's own refusals come back verbatim; a peer's request can't name a host.
    let r = a.post("/api/jobs", json!({"host": b_id, "cwd": "/", "argv": ["/usr/bin/true"]}));
    assert_eq!(r.status, 400, "{}", r.text());
    let r = b.http("GET", &format!("/peer/api/jobs?host={b_id}"), &[format!("Authorization: Bearer {}", b.secret())], "");
    assert_eq!(r.status, 400);
    assert_eq!(a.get("/api/jobs?host=00000000-0000-4000-8000-000000000000").status, 404, "no such host");
    // `ask --host` on A runs on B.
    let out = a.cli(&["ask", "--host", &b_id, "remote", "hello"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "echo: remote hello\n");
    let ran = b.list();
    let asked = ran.iter().find(|j| j["argv"][2] == "remote hello").expect("the ask ran on B");
    assert_eq!(
        asked["cwd"].as_str().unwrap(),
        std::fs::canonicalize(b.path("h")).unwrap().to_str().unwrap(),
        "B's home by default"
    );
}

#[test]
fn a_kill_takes_the_whole_group() {
    // The program honours SIGTERM, but a child of it ignores it: once the
    // program is gone, what is left of its group is killed too (it would
    // otherwise outlive the kill and the wall clock).
    let hub = Hub::start(true);
    for cap in [None, Some(1)] {
        let mut body = json!({"argv": ["/bin/sh", "-c", "(trap '' TERM; exec /bin/sleep 60) & echo $!; wait"]});
        if let Some(cap) = cap {
            body["maxSeconds"] = cap.into();
        }
        let id = hub.job(body);
        let mut child = 0;
        eventually(10 * SECOND, "the child's pid is printed", || {
            child = hub.read(&id, "").get("stdout").and_then(Value::as_str).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            child > 0
        });
        assert!(alive(child));
        if cap.is_none() {
            assert_eq!(hub.post(&format!("/api/jobs/{id}/kill"), json!({})).status, 200);
        }
        let job = hub.finished(&id);
        assert_eq!(job["timedOut"], cap.is_some(), "{job}");
        eventually(5 * SECOND, "the child that ignored SIGTERM is gone", || !alive(child));
    }
}
