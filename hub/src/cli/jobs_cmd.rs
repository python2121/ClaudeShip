//! `claudeship run | ask | jobs …`: jobs (docs/jobs.md) from the command
//! line, on this hub or — with `--host` — on another hub of its swarm.
//!
//! Talks to this machine's hub over loopback HTTP with its own pairing
//! secret (from the socket's `link` op), exactly as a paired client: the
//! hub relays a request naming another host there.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::client::{fail, request};
use crate::jobs::parse_event;
use crate::swarm::client::parse_response;
use crate::token::COOKIE_NAME;

const USAGE: &str = "\
usage: claudeship run [--host <name|id>] [--cwd <dir>] [--mode <mode>] [--max-seconds <n>] -- <argv…>
           start a job; prints its id
       claudeship ask [--host <name|id>] [--cwd <dir>] [--mode <mode>] [--resume <uuid>] [--max-seconds <n>] [-v] \"<prompt>\"
           run claude -p there, wait, print its final text (the session id goes to stderr)
       claudeship jobs [--host <name|id>]                      list the jobs
       claudeship jobs wait|output|kill <id> [--host <name|id>]";

/// The longest a single long-poll asks the hub to hold.
const WAIT: u64 = 60;

pub fn run(command: &str, args: &[String]) -> ! {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        std::process::exit(0);
    }
    match command {
        "run" => run_job(args),
        "ask" => ask(args),
        _ => jobs(args),
    }
}

// MARK: Options

#[derive(Default, Debug, PartialEq, Eq)]
struct Options {
    host: Option<String>,
    cwd: Option<String>,
    mode: Option<String>,
    resume: Option<String>,
    max_seconds: Option<u64>,
    verbose: bool,
    /// What's left: argv, the prompt's words, or `jobs`' subcommand and id.
    rest: Vec<String>,
}

/// Flags anywhere before `--` (`run` takes everything after it as argv
/// untouched; without one, the first word that isn't a flag starts it).
fn parse_options(args: &[String], stop_at_first_word: bool) -> Result<Options, String> {
    let mut o = Options::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let mut value = |name: &str| -> Result<String, String> {
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--" => {
                o.rest.extend(args[i + 1..].iter().cloned());
                return Ok(o);
            }
            "--host" => o.host = Some(value("--host")?),
            "--cwd" => o.cwd = Some(value("--cwd")?),
            "--mode" => o.mode = Some(value("--mode")?),
            "--resume" => o.resume = Some(value("--resume")?),
            "--max-seconds" => {
                let v = value("--max-seconds")?;
                o.max_seconds = Some(v.parse().map_err(|_| format!("--max-seconds: not a number: {v}"))?);
            }
            "-v" | "--verbose" => o.verbose = true,
            flag if flag.starts_with("--") && flag.len() > 2 => return Err(format!("unknown option {flag}")),
            _ => {
                if stop_at_first_word {
                    o.rest.extend(args[i..].iter().cloned());
                    return Ok(o);
                }
                o.rest.push(arg.clone());
            }
        }
        i += 1;
    }
    Ok(o)
}

fn options(args: &[String], stop_at_first_word: bool) -> Options {
    parse_options(args, stop_at_first_word).unwrap_or_else(|e| {
        eprintln!("claudeship: {e}\n{USAGE}");
        std::process::exit(2)
    })
}

// MARK: Talking to the hub

struct Hub {
    authority: String,
    token: String,
}

impl Hub {
    /// This machine's hub, over loopback.
    fn local() -> Hub {
        let link = request(json!({"op": "link"}));
        let port = link.get("port").and_then(Value::as_i64).unwrap_or(0);
        let token = link.get("token").and_then(Value::as_str).unwrap_or("").to_string();
        if token.is_empty() || port <= 0 {
            fail("the hub has no web server or pairing secret; see claudeship hub status");
        }
        Hub { authority: format!("127.0.0.1:{port}"), token }
    }

    /// One request: the status and the body as JSON. A hub that can't be
    /// reached ends this process.
    fn call(&self, method: &str, target: &str, body: Option<&Value>, wait: Duration) -> (u16, Value) {
        let attempt = || -> Result<(u16, Value), String> {
            let address = self.authority.parse().map_err(|_| "bad hub address".to_string())?;
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).map_err(|e| e.to_string())?;
            stream.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
            let payload = body.map(Value::to_string).unwrap_or_default();
            let mut head = format!(
                "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nCookie: {COOKIE_NAME}={}\r\n",
                self.authority, self.token
            );
            if body.is_some() {
                // Same-origin, as the page's own requests are.
                head.push_str(&format!(
                    "Origin: http://{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
                    self.authority,
                    payload.len()
                ));
            }
            head.push_str("\r\n");
            stream.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
            stream.write_all(payload.as_bytes()).map_err(|e| e.to_string())?;
            let mut raw = Vec::new();
            stream.take(64 << 20).read_to_end(&mut raw).map_err(|e| e.to_string())?;
            let answer = parse_response(&raw).ok_or("the hub's answer was cut off")?;
            Ok((answer.status, serde_json::from_slice(&answer.body).unwrap_or(Value::Null)))
        };
        attempt().unwrap_or_else(|e| fail(&format!("the hub at {}: {e}", self.authority)))
    }

    /// The answer of a request that must succeed.
    fn ok(&self, method: &str, target: &str, body: Option<&Value>, wait: Duration, what: &str) -> Value {
        let (status, value) = self.call(method, target, body, wait);
        if status != 200 {
            let error = value.get("error").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("HTTP {status}"));
            let mut message = format!("{what}: {error}");
            if let Some(host) = value.get("host").and_then(Value::as_str) {
                message.push_str(&format!(" (host {host})"));
            }
            fail(&message);
        }
        value
    }

    /// `--host`: an id, a name, or an id prefix from `hosts[]`. `None` for
    /// this hub. Also the host's home, the default directory there.
    fn resolve(&self, wanted: Option<&str>) -> (Option<String>, Option<String>) {
        let Some(wanted) = wanted.filter(|w| !w.is_empty()) else {
            return (None, None);
        };
        let state = self.ok("GET", "/api/state", None, Duration::from_secs(10), "the hub's state");
        let hosts = state.get("hosts").and_then(Value::as_array).cloned().unwrap_or_default();
        let lower = wanted.to_lowercase();
        let matches = |test: &dyn Fn(&Value) -> bool| -> Vec<Value> { hosts.iter().filter(|h| test(h)).cloned().collect() };
        let text = |h: &Value, k: &str| h.get(k).and_then(Value::as_str).unwrap_or("").to_lowercase();
        let mut found = matches(&|h| text(h, "id") == lower);
        if found.is_empty() {
            found = matches(&|h| text(h, "name") == lower);
        }
        if found.is_empty() {
            found = matches(&|h| text(h, "id").starts_with(&lower));
        }
        match found.as_slice() {
            [host] if host.get("local") == Some(&Value::Bool(true)) => (None, None),
            [host] => (
                host.get("id").and_then(Value::as_str).map(str::to_string),
                host.get("home").and_then(Value::as_str).map(str::to_string),
            ),
            [] => fail(&format!("no host named {wanted} in this hub's swarm (see claudeship hub peers)")),
            _ => fail(&format!("{wanted} names more than one host; use its id")),
        }
    }
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn host_query(host: &Option<String>) -> String {
    host.as_ref().map(|h| format!("&host={}", encode(h))).unwrap_or_default()
}

/// The directory a job runs in: `--cwd`; else here, this one; there, its home.
fn job_cwd(o: &Options, host: &Option<String>, home: Option<String>) -> Option<String> {
    if let Some(cwd) = &o.cwd {
        if host.is_none() && !cwd.starts_with('/') && !cwd.starts_with('~') {
            return std::env::current_dir().ok().map(|d| d.join(cwd).to_string_lossy().into_owned());
        }
        return Some(cwd.clone());
    }
    if host.is_some() {
        return home;
    }
    std::env::current_dir().ok().and_then(|d| d.to_str().map(str::to_string))
}

fn start(hub: &Hub, host: &Option<String>, mut body: Map<String, Value>) -> String {
    if let Some(host) = host {
        body.insert("host".into(), host.clone().into());
    }
    let answer = hub.ok("POST", "/api/jobs", Some(&Value::Object(body)), Duration::from_secs(30), "the job was refused");
    answer.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| fail("the hub gave the job no id"))
}

/// One long-poll: the job, with stdout from `since`.
fn poll(hub: &Hub, host: &Option<String>, id: &str, since: u64, wait: u64) -> Value {
    let target = format!("/api/jobs/{}?since={since}&wait={wait}{}", encode(id), host_query(host));
    hub.ok("GET", &target, None, Duration::from_secs(wait + 30), &format!("job {id}"))
}

/// Polls until the job ends, handing each new piece of stdout to `out`.
/// The final state.
fn follow(hub: &Hub, host: &Option<String>, id: &str, mut out: impl FnMut(&str)) -> Value {
    let mut since = 0;
    let mut said_waiting: Option<String> = None;
    loop {
        let job = poll(hub, host, id, since, WAIT);
        let start = job.get("since").and_then(Value::as_u64).unwrap_or(since);
        if start > since {
            eprintln!("claudeship: {} bytes of the job's output were dropped before they were read", start - since);
        }
        if let Some(text) = job.get("stdout").and_then(Value::as_str) {
            out(text);
        }
        since = job.get("next").and_then(Value::as_u64).unwrap_or(since);
        let waiting = job.get("waitingFor").and_then(Value::as_str).map(str::to_string);
        if waiting.is_some() && waiting != said_waiting {
            eprintln!(
                "claudeship: job {id} is waiting for {} (answer it from the web page or the phone)",
                waiting.as_deref().unwrap_or("")
            );
        }
        said_waiting = waiting;
        if job.get("running") == Some(&Value::Bool(false)) {
            return job;
        }
    }
}

fn exit_with(job: &Value) -> ! {
    if job.get("timedOut") == Some(&Value::Bool(true)) {
        eprintln!(
            "claudeship: the job ran past its {} s and was stopped",
            job.get("maxSeconds").and_then(Value::as_u64).unwrap_or(0)
        );
    }
    let _ = std::io::stdout().flush();
    std::process::exit(job.get("exitCode").and_then(Value::as_i64).unwrap_or(1) as i32);
}

fn print_stderr(job: &Value) {
    if let Some(text) = job.get("stderr").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        eprint!("{text}");
        if !text.ends_with('\n') {
            eprintln!();
        }
    }
}

// MARK: Commands

fn common_body(o: &Options, cwd: Option<String>) -> Map<String, Value> {
    let mut body = Map::new();
    if let Some(cwd) = cwd {
        body.insert("cwd".into(), cwd.into());
    }
    if let Some(mode) = &o.mode {
        body.insert("permissionMode".into(), mode.clone().into());
    }
    if let Some(seconds) = o.max_seconds {
        body.insert("maxSeconds".into(), seconds.into());
    }
    body
}

fn run_job(args: &[String]) -> ! {
    let o = options(args, true);
    if o.rest.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let hub = Hub::local();
    let (host, home) = hub.resolve(o.host.as_deref());
    let mut body = common_body(&o, job_cwd(&o, &host, home));
    body.insert("argv".into(), o.rest.clone().into());
    println!("{}", start(&hub, &host, body));
    std::process::exit(0);
}

fn ask(args: &[String]) -> ! {
    let o = options(args, false);
    if o.rest.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let hub = Hub::local();
    let (host, home) = hub.resolve(o.host.as_deref());
    let mut body = common_body(&o, job_cwd(&o, &host, home));
    body.insert("prompt".into(), o.rest.join(" ").into());
    if let Some(resume) = &o.resume {
        body.insert("resume".into(), resume.clone().into());
    }
    let id = start(&hub, &host, body);
    if o.verbose {
        eprintln!("claudeship: job {id}");
    }
    let mut line = String::new();
    let mut result: Option<String> = None;
    let mut session: Option<String> = None;
    let mut take = |text: &str| {
        if o.verbose {
            eprintln!("{text}");
        }
        let (s, r) = parse_event(text.as_bytes());
        session = s.or(session.take());
        result = r.or(result.take());
    };
    let job = follow(&hub, &host, &id, |text| {
        line.push_str(text);
        while let Some(end) = line.find('\n') {
            let complete: String = line.drain(..=end).collect();
            take(complete.trim_end_matches(['\n', '\r']));
        }
    });
    if !line.is_empty() {
        take(&line);
    }
    let result = result.or_else(|| job.get("result").and_then(Value::as_str).map(str::to_string));
    let session = session.or_else(|| job.get("claudeSessionId").and_then(Value::as_str).map(str::to_string));
    if let Some(result) = result {
        println!("{result}");
    }
    if o.verbose || job.get("exitCode").and_then(Value::as_i64) != Some(0) {
        print_stderr(&job);
    }
    if let Some(session) = session {
        eprintln!("session: {session}");
    }
    exit_with(&job);
}

fn jobs(args: &[String]) -> ! {
    let o = options(args, false);
    let hub = Hub::local();
    let (host, _) = hub.resolve(o.host.as_deref());
    let sub = o.rest.first().map(String::as_str).unwrap_or("list");
    let id = || {
        o.rest.get(1).cloned().unwrap_or_else(|| {
            eprintln!("{USAGE}");
            std::process::exit(2)
        })
    };
    match sub {
        "list" | "ls" => {
            let target = format!("/api/jobs?{}", host_query(&host).trim_start_matches('&'));
            let answer = hub.ok("GET", target.trim_end_matches('?'), None, Duration::from_secs(30), "jobs");
            print_list(answer.get("jobs").and_then(Value::as_array).cloned().unwrap_or_default());
        }
        "wait" => {
            let id = id();
            let job = follow(&hub, &host, &id, |text| {
                print!("{text}");
                let _ = std::io::stdout().flush();
            });
            print_stderr(&job);
            exit_with(&job);
        }
        "output" => {
            let job = poll(&hub, &host, &id(), 0, 0);
            print!("{}", job.get("stdout").and_then(Value::as_str).unwrap_or(""));
            let _ = std::io::stdout().flush();
            print_stderr(&job);
            if job.get("truncated") == Some(&Value::Bool(true)) {
                eprintln!("claudeship: the start of the output was dropped (only the last 4 MB are kept)");
            }
            std::process::exit(0);
        }
        "kill" => {
            let id = id();
            let mut body = Map::new();
            if let Some(host) = &host {
                body.insert("host".into(), host.clone().into());
            }
            hub.ok(
                "POST",
                &format!("/api/jobs/{}/kill", encode(&id)),
                Some(&Value::Object(body)),
                Duration::from_secs(30),
                &format!("job {id}"),
            );
            println!("job {id} told to end");
            std::process::exit(0);
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn print_list(jobs: Vec<Value>) -> ! {
    if jobs.is_empty() {
        println!("no jobs");
        std::process::exit(0);
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let home = crate::config::home_dir().to_string_lossy().into_owned();
    for job in jobs {
        let text = |k: &str| job.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let state = if job.get("running") == Some(&Value::Bool(true)) {
            "running".to_string()
        } else if job.get("timedOut") == Some(&Value::Bool(true)) {
            "timed out".to_string()
        } else {
            format!("exit {}", job.get("exitCode").and_then(Value::as_i64).unwrap_or(-1))
        };
        let started = job.get("startedAt").and_then(Value::as_i64).unwrap_or(now);
        let argv: Vec<String> = job
            .get("argv")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let mut command = argv.join(" ");
        if command.chars().count() > 60 {
            command = command.chars().take(59).collect::<String>() + "…";
        }
        println!(
            "  {}  {:<9}  {:>6} ago  {}  {}",
            text("id"),
            state,
            super::hub_cmd::compact_age((now - started) / 1000),
            super::hub_cmd::abbreviate_home(&text("cwd"), &home),
            command
        );
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn options_and_argv() {
        let o = parse_options(&words("--host steam --cwd /x -- make test --host y"), true).unwrap();
        assert_eq!(o.host.as_deref(), Some("steam"));
        assert_eq!(o.cwd.as_deref(), Some("/x"));
        assert_eq!(o.rest, words("make test --host y"), "argv after -- is untouched");
        let o = parse_options(&words("--mode plan ls -la --host z"), true).unwrap();
        assert_eq!(o.rest, words("ls -la --host z"), "argv starts at the first word");
        let o = parse_options(&words("-v what is --max-seconds 9 this"), false).unwrap();
        assert!(o.verbose);
        assert_eq!(o.max_seconds, Some(9));
        assert_eq!(o.rest, words("what is this"));
        assert!(parse_options(&words("--host"), false).is_err());
        assert!(parse_options(&words("--max-seconds x"), false).is_err());
        assert!(parse_options(&words("--nope"), false).is_err());
    }

    #[test]
    fn query_encoding() {
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
        assert_eq!(host_query(&Some("x y".into())), "&host=x%20y");
        assert_eq!(host_query(&None), "");
    }
}
