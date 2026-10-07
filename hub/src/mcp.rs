//! `claudeship mcp`: an MCP server on stdio that gives Claude Code the
//! swarm and the jobs engine as tools (plan phase 11, docs/jobs.md "MCP").
//! Registered once with `claude mcp add claudeship -- claudeship mcp`.
//!
//! Dependency-free on purpose: JSON-RPC 2.0, one message per line
//! (protocol version 2024-11-05), answering `initialize`, `ping`,
//! `tools/list` and `tools/call`; notifications are taken silently and any
//! other request is "method not found". Stdout is the protocol — every log
//! line goes to stderr.
//!
//! The tools are thin: each is one or a few HTTP requests to the local hub
//! over loopback (`http://127.0.0.1:<port>` from `config.json`, the pairing
//! secret from `token`, both read from the hub home on every call so a hub
//! paired after this server started still works) — exactly what the Mac
//! app's `HubClient` does. The hub does the routing to other machines: a
//! tool's `host` (a name or id from `ship_hosts`) becomes the `host` the
//! jobs endpoints take. The only state kept here is each job's read offset,
//! so `ship_wait` answers with the output that is new since the last call.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::config::{HubConfig, PERMISSION_MODES};
use crate::paths;
use crate::swarm::client::parse_response;
use crate::token::COOKIE_NAME;

/// The MCP revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";
/// How long `ship_ask` (and the longest `ship_wait`) holds one tool call
/// before handing back the job: well inside an MCP client's patience.
const CALL_BUDGET: Duration = Duration::from_secs(50);
const MAX_WAIT_SECONDS: u64 = 50;
const DEFAULT_WAIT_SECONDS: u64 = 30;
const STATE_TIMEOUT: Duration = Duration::from_secs(1);
/// A start or kill may be relayed to a peer (3 s per address there).
const ACTION_TIMEOUT: Duration = Duration::from_secs(10);
/// Extra time a long-poll gets beyond its own `wait`.
const WAIT_SLACK: Duration = Duration::from_secs(5);
/// The most output one answer carries; the rest is a `ship_output` away.
const OUTPUT_CAP: usize = 48 << 10;
const STDERR_CAP: usize = 4 << 10;
/// The fallback "result" when a run printed no `result` event.
const RESULT_TAIL: usize = 4 << 10;
/// The largest answer read from the hub.
const MAX_ANSWER: usize = 32 << 20;
/// The longest message line taken from the client; a longer one is
/// skipped to its newline and answered with a parse error.
const MAX_LINE: usize = 8 << 20;

/// `claudeship mcp`: serve stdin/stdout until the client hangs up.
pub fn run() -> ! {
    let mut server = Server::new(Endpoint::Home);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let code = match serve(stdin.lock(), stdout.lock(), &mut server) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("claudeship mcp: {e}");
            1
        }
    };
    std::process::exit(code)
}

/// Read newline-delimited JSON-RPC from `input`, answer on `output`.
pub fn serve(mut input: impl BufRead, mut output: impl Write, server: &mut Server) -> std::io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if (&mut input).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        let answer = if line.len() > MAX_LINE {
            // Not held in memory: the rest of it is read and dropped.
            if line.last() != Some(&b'\n') {
                skip_line(&mut input)?;
            }
            Some(error(Value::Null, -32700, &format!("parse error: a message over {} MB", MAX_LINE >> 20)))
        } else if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        } else {
            server.handle_line(&line)
        };
        if let Some(answer) = answer {
            let mut text = answer.to_string();
            text.push('\n');
            output.write_all(text.as_bytes())?;
            output.flush()?;
        }
    }
}

/// Reads up to and including the next newline, keeping none of it.
fn skip_line(input: &mut impl BufRead) -> std::io::Result<()> {
    loop {
        let buffer = input.fill_buf()?;
        if buffer.is_empty() {
            return Ok(());
        }
        match buffer.iter().position(|&b| b == b'\n') {
            Some(i) => {
                input.consume(i + 1);
                return Ok(());
            }
            None => {
                let n = buffer.len();
                input.consume(n);
            }
        }
    }
}

/// Where the hub is.
pub enum Endpoint {
    /// The hub home's `config.json` port and `token`, read per request.
    Home,
    /// A fixed address and secret (tests).
    #[allow(dead_code)]
    Fixed { authority: String, token: String },
}

impl Endpoint {
    fn locate(&self) -> Result<(String, String), String> {
        match self {
            Endpoint::Fixed { authority, token } => Ok((authority.clone(), token.clone())),
            Endpoint::Home => {
                let config = HubConfig::load(&paths::config());
                let token = crate::token::read().filter(|t| !t.is_empty()).ok_or_else(|| {
                    format!(
                        "the local ClaudeShip hub has no pairing token in {} (is it installed? `claudeship hub start`)",
                        paths::home().display()
                    )
                })?;
                Ok((format!("127.0.0.1:{}", config.port), token))
            }
        }
    }
}

/// One blocking HTTP/1 exchange with the hub: status and JSON body.
fn exchange(endpoint: &Endpoint, method: &str, target: &str, body: Option<&Value>, budget: Duration) -> Result<(u16, Value), String> {
    let (authority, token) = endpoint.locate()?;
    let unreachable = |e: &dyn std::fmt::Display| {
        format!("cannot reach the local ClaudeShip hub at {authority}: {e} (is it running? `claudeship hub start`)")
    };
    let address: SocketAddr = authority.parse().map_err(|e| unreachable(&e))?;
    let deadline = Instant::now() + budget;
    let mut stream = TcpStream::connect_timeout(&address, budget.min(Duration::from_secs(2))).map_err(|e| unreachable(&e))?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nCookie: {COOKIE_NAME}={token}\r\n"
    );
    if body.is_some() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(payload.as_bytes()))
        .map_err(|e| unreachable(&e))?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 16 << 10];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("the local ClaudeShip hub at {authority} did not answer within {} s", budget.as_secs()));
        }
        let _ = stream.set_read_timeout(Some(left));
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > MAX_ANSWER {
                    return Err(format!("the hub at {authority} sent an answer over {} MB", MAX_ANSWER >> 20));
                }
                if complete(&raw) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err(format!("the local ClaudeShip hub at {authority} did not answer within {} s", budget.as_secs()));
            }
            Err(e) => return Err(unreachable(&e)),
        }
    }
    let answer = parse_response(&raw).ok_or_else(|| format!("{authority} did not answer like a ClaudeShip hub"))?;
    let json = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
    Ok((answer.status, json))
}

/// Whether `raw` holds a whole response with a `Content-Length` (one
/// without is read to the close).
fn complete(raw: &[u8]) -> bool {
    let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&raw[..end]) else {
        return false;
    };
    head.split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .is_some_and(|length| raw.len() - end - 4 >= length)
}

/// A query-string value, percent-encoded.
fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A host the tools act on: its id for the hub (`None` = this hub) and a
/// name for messages.
#[derive(Clone, Debug)]
struct Host {
    id: Option<String>,
    name: String,
}

/// What `ship_wait` remembers per job: the next byte to read and the
/// unfinished last line (so a `result` event split across two reads is
/// still found).
#[derive(Default)]
struct Cursor {
    offset: u64,
    partial: String,
    /// The last `result` event read: a Claude run prints it and exits a
    /// moment later, so the read that sees the finish often has no text.
    result: Option<Value>,
}

/// The MCP server's state.
pub struct Server {
    endpoint: Endpoint,
    cursors: HashMap<(String, String), Cursor>,
    budget: Duration,
}

impl Server {
    pub fn new(endpoint: Endpoint) -> Server {
        Server {
            endpoint,
            cursors: HashMap::new(),
            budget: CALL_BUDGET,
        }
    }

    /// Shorten `ship_ask`'s hold on a call (tests).
    #[cfg(test)]
    fn with_budget(mut self, budget: Duration) -> Server {
        self.budget = budget;
        self
    }

    /// One line from the client → the answer, if it gets one.
    pub fn handle_line(&mut self, line: &[u8]) -> Option<Value> {
        let message: Value = match serde_json::from_slice(line) {
            Ok(m) => m,
            Err(e) => return Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        match message {
            Value::Array(batch) if batch.is_empty() => Some(error(Value::Null, -32600, "empty batch")),
            Value::Array(batch) => {
                let answers: Vec<Value> = batch.into_iter().filter_map(|m| self.handle(m)).collect();
                (!answers.is_empty()).then_some(Value::Array(answers))
            }
            message => self.handle(message),
        }
    }

    fn handle(&mut self, message: Value) -> Option<Value> {
        let Value::Object(message) = message else {
            return Some(error(Value::Null, -32600, "invalid request: not an object"));
        };
        let id = message.get("id").cloned();
        let valid_id = matches!(id, None | Some(Value::String(_) | Value::Number(_) | Value::Null));
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to something we never asked: ignore it.
            if message.contains_key("result") || message.contains_key("error") {
                return None;
            }
            return Some(error(id.unwrap_or(Value::Null), -32600, "invalid request: no method"));
        };
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !valid_id {
            let id = if valid_id { id.unwrap_or(Value::Null) } else { Value::Null };
            return Some(error(id, -32600, "invalid request: not JSON-RPC 2.0"));
        }
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        // A notification (no id) is never answered.
        let id = id?;
        let outcome = match method {
            "initialize" => Ok(initialize()),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.call(&params),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match outcome {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, text)) => error(id, code, &text),
        })
    }

    /// `tools/call`: an unknown tool is a protocol error; anything that
    /// goes wrong inside a tool is a result with `isError`, which the model
    /// reads.
    fn call(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err((-32602, "tools/call needs a tool name".into()));
        };
        let empty = Map::new();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(a)) => a,
            Some(_) => return Err((-32602, "tool arguments must be an object".into())),
        };
        let outcome = match name {
            "ship_hosts" => self.ship_hosts(),
            "ship_sessions" => self.ship_sessions(args),
            "ship_run" => self.ship_run(args),
            "ship_ask" => self.ship_ask(args),
            "ship_wait" => self.ship_wait(args),
            "ship_output" => self.ship_output(args),
            "ship_kill" => self.ship_kill(args),
            _ => return Err((-32602, format!("unknown tool: {name}"))),
        };
        Ok(match outcome {
            Ok(value) => json!({"content": [{"type": "text", "text": value.to_string()}]}),
            Err(text) => {
                eprintln!("claudeship mcp: {name}: {text}");
                json!({"content": [{"type": "text", "text": text}], "isError": true})
            }
        })
    }

    // MARK: the hub

    fn state(&self) -> Result<Value, String> {
        let (status, body) = exchange(&self.endpoint, "GET", "/api/state", None, STATE_TIMEOUT)?;
        if status != 200 {
            return Err(refusal("this machine", status, &body));
        }
        Ok(body)
    }

    /// The swarm's hosts, or this hub alone (one from before the swarm).
    fn hosts(state: &Value) -> Vec<Value> {
        match state.get("hosts").and_then(Value::as_array) {
            Some(hosts) => hosts.clone(),
            None => {
                let mut local = state.as_object().cloned().unwrap_or_default();
                local.insert("name".into(), "this machine".into());
                local.insert("local".into(), true.into());
                local.insert("reachable".into(), true.into());
                vec![Value::Object(local)]
            }
        }
    }

    /// `host` as given (a name, an id, an id prefix, or nothing for this
    /// machine) → the host.
    fn resolve(&self, args: &Map<String, Value>) -> Result<(Host, Value), String> {
        let given = match args.get("host") {
            None | Some(Value::Null) => None,
            Some(Value::String(h)) if h.trim().is_empty() => None,
            Some(Value::String(h)) => Some(h.trim().to_string()),
            Some(_) => return Err("host must be a string: a host name or id from ship_hosts".into()),
        };
        let state = self.state()?;
        let hosts = Self::hosts(&state);
        let host_of = |h: &Value| Host {
            id: h.get("id").and_then(Value::as_str).map(String::from),
            name: h
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("this machine")
                .to_string(),
        };
        let local = || {
            hosts
                .iter()
                .find(|h| h.get("local").and_then(Value::as_bool) == Some(true))
                .map(host_of)
                .unwrap_or(Host {
                    id: None,
                    name: "this machine".into(),
                })
        };
        let Some(given) = given else {
            return Ok((local(), state));
        };
        if ["local", "here", "localhost", "this machine"].contains(&given.to_ascii_lowercase().as_str()) {
            return Ok((local(), state));
        }
        let field = |h: &Value, k: &str| h.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let by_id: Vec<&Value> = hosts.iter().filter(|h| field(h, "id") == given).collect();
        let by_name: Vec<&Value> = hosts
            .iter()
            .filter(|h| field(h, "name").eq_ignore_ascii_case(&given))
            .collect();
        let by_prefix: Vec<&Value> = if given.len() >= 4 {
            hosts.iter().filter(|h| field(h, "id").starts_with(&given)).collect()
        } else {
            Vec::new()
        };
        for matches in [by_id, by_name, by_prefix] {
            match matches.len() {
                0 => continue,
                1 => return Ok((host_of(matches[0]), state)),
                _ => {
                    let ids: Vec<String> = matches.iter().map(|h| field(h, "id")).collect();
                    return Err(format!(
                        "host \"{given}\" is ambiguous: it matches {}; name one by id",
                        ids.join(", ")
                    ));
                }
            }
        }
        let known: Vec<String> = hosts
            .iter()
            .map(|h| format!("{} ({})", field(h, "name"), field(h, "id")))
            .collect();
        Err(format!(
            "no host \"{given}\" in the swarm; known hosts: {}",
            known.join(", ")
        ))
    }

    /// A jobs request to `host`: the answer on 2xx, else an error naming it.
    fn jobs(&self, host: &Host, method: &str, target: &str, body: Option<Value>, budget: Duration) -> Result<Value, String> {
        let body = body.map(|mut b| {
            if let (Some(id), Some(obj)) = (&host.id, b.as_object_mut()) {
                obj.insert("host".into(), id.clone().into());
            }
            b
        });
        let (status, answer) = exchange(&self.endpoint, method, target, body.as_ref(), budget)
            .map_err(|e| format!("{} (acting on host {})", e, host.name))?;
        if (200..300).contains(&status) {
            Ok(answer)
        } else {
            Err(refusal(&host.name, status, &answer))
        }
    }

    fn job_target(host: &Host, job: &str, query: &[(&str, String)]) -> String {
        let mut target = format!("/api/jobs/{job}");
        let mut pairs: Vec<String> = Vec::new();
        if let Some(id) = &host.id {
            pairs.push(format!("host={}", encode(id)));
        }
        pairs.extend(query.iter().map(|(k, v)| format!("{k}={}", encode(v))));
        if !pairs.is_empty() {
            target.push('?');
            target.push_str(&pairs.join("&"));
        }
        target
    }

    // MARK: tools

    fn ship_hosts(&mut self) -> Result<Value, String> {
        let state = self.state()?;
        let hosts: Vec<Value> = Self::hosts(&state)
            .iter()
            .map(|h| {
                let sessions = sessions_of(h).len();
                let mut out = pick(
                    h,
                    &["name", "id", "local", "reachable", "protocol", "root", "home", "defaultPermissionMode", "lastSeen"],
                );
                out.insert("sessions".into(), sessions.into());
                Value::Object(out)
            })
            .collect();
        Ok(json!({ "hosts": hosts }))
    }

    fn ship_sessions(&mut self, args: &Map<String, Value>) -> Result<Value, String> {
        let only = match args.get("host") {
            None | Some(Value::Null) => None,
            Some(Value::String(h)) if h.trim().is_empty() => None,
            Some(_) => Some(self.resolve(args)?.0),
        };
        let state = self.state()?;
        let mut sessions = Vec::new();
        let mut unreachable = Vec::new();
        for h in Self::hosts(&state) {
            let id = h.get("id").and_then(Value::as_str).map(String::from);
            let name = h.get("name").and_then(Value::as_str).unwrap_or("this machine").to_string();
            if let Some(only) = &only
                && (only.id != id || (only.id.is_none() && only.name != name))
            {
                continue;
            }
            if h.get("reachable").and_then(Value::as_bool) == Some(false) {
                unreachable.push(Value::String(name.clone()));
            }
            for (project, session) in sessions_of(&h) {
                let mut out = Map::new();
                out.insert("host".into(), name.clone().into());
                out.insert("hostId".into(), id.clone().map_or(Value::Null, Value::from));
                out.insert("project".into(), project.map_or(Value::Null, Value::from));
                out.extend(pick(
                    session,
                    &[
                        "status", "waitingFor", "cwd", "name", "title", "branch", "sessionId", "hubId", "pid",
                        "background", "mode", "jobId", "startedAt", "since",
                    ],
                ));
                let pending = session.get("approvals").and_then(Value::as_array).map_or(0, Vec::len);
                if pending > 0 {
                    out.insert("pendingApprovals".into(), pending.into());
                }
                sessions.push(Value::Object(out));
            }
        }
        let mut answer = json!({ "sessions": sessions });
        if !unreachable.is_empty() {
            answer["unreachableHosts"] = Value::Array(unreachable);
        }
        Ok(answer)
    }

    fn ship_run(&mut self, args: &Map<String, Value>) -> Result<Value, String> {
        let cwd = cwd_arg(args)?;
        let argv: Vec<String> = match args.get("argv") {
            Some(Value::Array(items)) if !items.is_empty() => items
                .iter()
                .map(|v| v.as_str().map(String::from))
                .collect::<Option<_>>()
                .ok_or("argv must be an array of strings")?,
            _ => return Err("argv must be a non-empty array of strings, e.g. [\"cargo\", \"test\"]".into()),
        };
        let mut body = json!({ "cwd": cwd, "argv": argv });
        if let Some(mode) = mode_arg(args)? {
            body["permissionMode"] = mode.into();
        }
        if let Some(seconds) = max_seconds_arg(args)? {
            body["maxSeconds"] = seconds.into();
        }
        let (host, _) = self.resolve(args)?;
        let answer = self.jobs(&host, "POST", "/api/jobs", Some(body), ACTION_TIMEOUT)?;
        let job = job_id_of(&answer, &host)?;
        Ok(json!({ "job": job, "host": host.name, "hostId": host.id }))
    }

    fn ship_ask(&mut self, args: &Map<String, Value>) -> Result<Value, String> {
        let started = Instant::now();
        let cwd = cwd_arg(args)?;
        let prompt = match args.get("prompt") {
            Some(Value::String(p)) if !p.trim().is_empty() => p.clone(),
            _ => return Err("prompt must be a non-empty string".into()),
        };
        let mut body = json!({
            "cwd": cwd,
            "prompt": prompt,
            "permissionMode": mode_arg(args)?.unwrap_or_else(|| "auto".into()),
        });
        match args.get("resume") {
            None | Some(Value::Null) => {}
            Some(Value::String(r)) if r.trim().is_empty() => {}
            Some(Value::String(r)) => body["resume"] = r.trim().into(),
            Some(_) => return Err("resume must be a session id (the sessionId an earlier ship_ask returned)".into()),
        }
        if let Some(seconds) = max_seconds_arg(args)? {
            body["maxSeconds"] = seconds.into();
        }
        let (host, _) = self.resolve(args)?;
        let answer = self.jobs(&host, "POST", "/api/jobs", Some(body), ACTION_TIMEOUT)?;
        let job = job_id_of(&answer, &host)?;
        let mut seen = String::new();
        let mut result = None;
        let mut last = Value::Null;
        loop {
            let wait = self.budget.saturating_sub(started.elapsed()).as_secs().min(MAX_WAIT_SECONDS);
            if wait == 0 {
                let mut out = json!({
                    "job": job,
                    "host": host.name,
                    "running": true,
                    "stdoutSoFar": tail(&seen, OUTPUT_CAP),
                    "note": "still running: call ship_wait with this host and job to keep waiting (it returns only output newer than this)",
                });
                if let Some(w) = last.get("waitingFor").filter(|w| !w.is_null()) {
                    out["waitingFor"] = w.clone();
                }
                return Ok(out);
            }
            let asked = Instant::now();
            let poll = self.poll(&host, &job, wait)?;
            if poll.new.is_empty() && running(&poll.status) && asked.elapsed() < Duration::from_millis(200) {
                // A hub that answers a long-poll at once with nothing new
                // isn't hammered for the rest of the budget.
                std::thread::sleep(Duration::from_millis(250));
            }
            seen.push_str(&poll.new);
            if poll.result.is_some() {
                result = poll.result;
            }
            if !running(&poll.status) {
                return Ok(finished(&job, &host, &poll.status, result.as_ref(), &seen));
            }
            last = poll.status;
        }
    }

    fn ship_wait(&mut self, args: &Map<String, Value>) -> Result<Value, String> {
        let job = job_arg(args)?;
        let wait = match args.get("timeoutSeconds") {
            None | Some(Value::Null) => DEFAULT_WAIT_SECONDS,
            Some(v) => v
                .as_f64()
                .filter(|s| *s >= 0.0)
                .ok_or("timeoutSeconds must be a number of seconds, 0 to 50")?
                .ceil() as u64,
        }
        .min(MAX_WAIT_SECONDS);
        let (host, _) = self.resolve(args)?;
        let poll = self.poll(&host, &job, wait)?;
        let mut out = report(&job, &host, &poll.status);
        let (text, skipped) = capped_tail(&poll.new, OUTPUT_CAP);
        out.insert("stdout".into(), text.into());
        if skipped > 0 {
            let start = start_offset(&poll.status)
                .or_else(|| next_offset(&poll.status).map(|n| n.saturating_sub(poll.new.len() as u64)))
                .unwrap_or(0);
            out.insert(
                "stdoutSkipped".into(),
                format!("{skipped} earlier bytes of this output are not shown; ship_output with since={start} reads them")
                    .into(),
            );
        }
        if !running(&poll.status)
            && let Some(event) = &poll.result
        {
            out.extend(result_fields(event));
        }
        Ok(Value::Object(out))
    }

    fn ship_output(&mut self, args: &Map<String, Value>) -> Result<Value, String> {
        let job = job_arg(args)?;
        let since = match args.get("since") {
            None | Some(Value::Null) => 0,
            Some(v) => v.as_u64().ok_or("since must be a byte offset (a whole number, 0 for the start)")?,
        };
        let (host, _) = self.resolve(args)?;
        let status = self.fetch(&host, &job, since, None)?;
        let stdout = status.get("stdout").and_then(Value::as_str).unwrap_or("");
        let mut out = report(&job, &host, &status);
        let shown = head(stdout, OUTPUT_CAP);
        let more = shown.len() < stdout.len();
        let start = start_offset(&status).unwrap_or(since);
        let next = if more {
            start + shown.len() as u64
        } else {
            next_offset(&status).unwrap_or(start + stdout.len() as u64)
        };
        out.insert("stdout".into(), shown.into());
        out.insert("more".into(), more.into());
        out.insert("nextSince".into(), next.into());
        if !running(&status)
            && let Some(event) = last_result_event(stdout).or_else(|| hub_result(&status))
        {
            out.extend(result_fields(&event));
        }
        Ok(Value::Object(out))
    }

    fn ship_kill(&mut self, args: &Map<String, Value>) -> Result<Value, String> {
        let job = job_arg(args)?;
        let (host, _) = self.resolve(args)?;
        let answer = self.jobs(
            &host,
            "POST",
            &format!("/api/jobs/{job}/kill"),
            Some(json!({})),
            ACTION_TIMEOUT,
        )?;
        Ok(json!({
            "ok": answer.get("ok").and_then(Value::as_bool).unwrap_or(true),
            "job": job,
            "host": host.name,
        }))
    }

    /// One `GET /api/jobs/<id>`, long-polling `wait` seconds when given.
    fn fetch(&self, host: &Host, job: &str, since: u64, wait: Option<u64>) -> Result<Value, String> {
        let mut query = vec![("since", since.to_string())];
        if let Some(wait) = wait {
            query.push(("wait", wait.to_string()));
        }
        let budget = Duration::from_secs(wait.unwrap_or(0)) + WAIT_SLACK;
        self.jobs(host, "GET", &Self::job_target(host, job, &query), None, budget)
    }

    /// A long-poll from this job's cursor, which it moves on.
    fn poll(&mut self, host: &Host, job: &str, wait: u64) -> Result<Poll, String> {
        let key = (host.id.clone().unwrap_or_default(), job.to_string());
        let since = self.cursors.get(&key).map_or(0, |c| c.offset);
        let status = self.fetch(host, job, since, Some(wait))?;
        let new = status.get("stdout").and_then(Value::as_str).unwrap_or("").to_string();
        let cursor = self.cursors.entry(key).or_default();
        cursor.offset = next_offset(&status).unwrap_or(since + new.len() as u64);
        let mut text = std::mem::take(&mut cursor.partial);
        text.push_str(&new);
        if running(&status) {
            // Hold the unfinished last line back for the next read.
            let cut = text.rfind('\n').map_or(0, |i| i + 1);
            cursor.partial = text.split_off(cut);
        }
        if let Some(event) = last_result_event(&text) {
            cursor.result = Some(event);
        }
        let result = cursor.result.clone().or_else(|| hub_result(&status));
        Ok(Poll { status, new, result })
    }
}

/// One long-poll's answer: the job's status, the output new to this
/// cursor, and the last `result` event among its finished lines.
struct Poll {
    status: Value,
    new: String,
    result: Option<Value>,
}

/// Where the next read of a job's stdout starts: the hub's `next`. (The
/// text's own length is not it: bytes dropped before `since`, or ones that
/// weren't UTF-8, make the two differ.)
fn next_offset(status: &Value) -> Option<u64> {
    status.get("next").and_then(Value::as_u64)
}

/// The byte offset a job answer's `stdout` starts at (later than asked
/// when the front was dropped).
fn start_offset(status: &Value) -> Option<u64> {
    status.get("since").and_then(Value::as_u64)
}

/// The `result` the hub kept for a finished Claude job, as an event.
fn hub_result(status: &Value) -> Option<Value> {
    let text = status.get("result").and_then(Value::as_str)?;
    let mut event = json!({"type": "result", "result": text});
    if let Some(id) = status.get("claudeSessionId").and_then(Value::as_str) {
        event["session_id"] = id.into();
    }
    Some(event)
}

fn running(status: &Value) -> bool {
    status.get("running").and_then(Value::as_bool) == Some(true)
}

/// The status fields every job answer carries.
fn report(job: &str, host: &Host, status: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("job".into(), job.into());
    out.insert("host".into(), host.name.clone().into());
    out.insert("running".into(), running(status).into());
    out.extend(pick(
        status,
        &["exitCode", "timedOut", "waitingFor", "truncated", "startedAt", "finishedAt"],
    ));
    if let Some(next) = next_offset(status) {
        out.insert("stdoutOffset".into(), next.into());
    }
    if let Some(id) = status.get("claudeSessionId").filter(|v| !v.is_null()) {
        out.insert("sessionId".into(), id.clone());
    }
    let stderr = status.get("stderr").and_then(Value::as_str).unwrap_or("");
    if !stderr.is_empty() {
        out.insert("stderr".into(), tail(stderr, STDERR_CAP).into());
    }
    out
}

/// `ship_ask`'s answer for a finished job.
fn finished(job: &str, host: &Host, status: &Value, event: Option<&Value>, seen: &str) -> Value {
    let mut out = Map::new();
    match event {
        Some(event) => out.extend(result_fields(event)),
        None => {
            out.insert("result".into(), tail(seen.trim_end(), RESULT_TAIL).into());
        }
    }
    let session = status
        .get("claudeSessionId")
        .filter(|v| !v.is_null())
        .cloned()
        .or_else(|| out.get("sessionId").cloned())
        .unwrap_or(Value::Null);
    out.insert("sessionId".into(), session);
    out.insert("exitCode".into(), status.get("exitCode").cloned().unwrap_or(Value::Null));
    out.insert(
        "timedOut".into(),
        status.get("timedOut").cloned().unwrap_or(false.into()),
    );
    out.insert("job".into(), job.into());
    out.insert("host".into(), host.name.clone().into());
    let stderr = status.get("stderr").and_then(Value::as_str).unwrap_or("");
    let failed = status.get("exitCode").and_then(Value::as_i64).is_some_and(|c| c != 0);
    if failed && !stderr.is_empty() {
        out.insert("stderr".into(), tail(stderr, STDERR_CAP).into());
    }
    Value::Object(out)
}

/// `result`, `sessionId`, and `isError` from a stream-json `result` event.
fn result_fields(event: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    let text = match event.get("result").and_then(Value::as_str) {
        Some(text) => text.to_string(),
        None => format!(
            "(no result text: the run ended with {})",
            event.get("subtype").and_then(Value::as_str).unwrap_or("an error")
        ),
    };
    out.insert("result".into(), text.into());
    if let Some(id) = event.get("session_id").and_then(Value::as_str) {
        out.insert("sessionId".into(), id.into());
    }
    if event.get("is_error").and_then(Value::as_bool) == Some(true) {
        out.insert("isError".into(), true.into());
    }
    out
}

/// The last stream-json `result` event among `text`'s lines.
fn last_result_event(text: &str) -> Option<Value> {
    text.lines().rev().find_map(|line| {
        let line = line.trim();
        if !line.starts_with('{') || !line.contains("\"result\"") {
            return None;
        }
        let event: Value = serde_json::from_str(line).ok()?;
        (event.get("type").and_then(Value::as_str) == Some("result")).then_some(event)
    })
}

/// The last `cap` bytes of `text` (on a character boundary).
fn tail(text: &str, cap: usize) -> &str {
    capped_tail(text, cap).0
}

/// The last `cap` bytes of `text` and how many bytes were left off.
fn capped_tail(text: &str, cap: usize) -> (&str, usize) {
    if text.len() <= cap {
        return (text, 0);
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (&text[start..], start)
}

/// The first `cap` bytes of `text` (on a character boundary).
fn head(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The named fields of `value` that are present and not null.
fn pick(value: &Value, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|k| {
            let v = value.get(*k)?;
            (!v.is_null()).then(|| ((*k).to_string(), v.clone()))
        })
        .collect()
}

/// A host's live sessions with their project's name (`None` for one
/// outside the root).
fn sessions_of(host: &Value) -> Vec<(Option<String>, &Value)> {
    let mut out = Vec::new();
    for project in host.get("projects").and_then(Value::as_array).into_iter().flatten() {
        let name = project.get("name").and_then(Value::as_str).map(String::from);
        for session in project.get("sessions").and_then(Value::as_array).into_iter().flatten() {
            out.push((name.clone(), session));
        }
    }
    for session in host.get("elsewhere").and_then(Value::as_array).into_iter().flatten() {
        out.push((None, session));
    }
    out
}

fn cwd_arg(args: &Map<String, Value>) -> Result<String, String> {
    match args.get("cwd").and_then(Value::as_str).map(str::trim) {
        Some(cwd) if cwd.starts_with('/') => Ok(cwd.to_string()),
        _ => Err("cwd must be an absolute directory on that host, e.g. /Users/me/code/project (ship_hosts shows each host's root and home)".into()),
    }
}

fn mode_arg(args: &Map<String, Value>) -> Result<Option<String>, String> {
    match args.get("mode") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(m)) if PERMISSION_MODES.contains(&m.as_str()) => Ok(Some(m.clone())),
        Some(_) => Err(format!("mode must be one of {}", PERMISSION_MODES.join(", "))),
    }
}

fn max_seconds_arg(args: &Map<String, Value>) -> Result<Option<u64>, String> {
    match args.get("maxSeconds") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|s| *s >= 1.0 && s.fract() == 0.0)
            .map(|s| Some(s as u64))
            .ok_or_else(|| "maxSeconds must be a whole number of seconds, at least 1".into()),
    }
}

fn job_arg(args: &Map<String, Value>) -> Result<String, String> {
    match args.get("job").and_then(Value::as_str).map(str::trim) {
        Some(job)
            if !job.is_empty()
                && job.len() <= 64
                && job.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Ok(job.to_string())
        }
        _ => Err("job must be the job id that ship_run or ship_ask returned".into()),
    }
}

fn job_id_of(answer: &Value, host: &Host) -> Result<String, String> {
    answer
        .get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("host {} started no job: the hub's answer had no id", host.name))
}

/// A non-2xx answer from the hub → a message for the model, naming the host.
fn refusal(host: &str, status: u16, body: &Value) -> String {
    let error = body.get("error").and_then(Value::as_str).unwrap_or("");
    let error = if error.is_empty() { String::new() } else { format!(": {error}") };
    match status {
        401 => format!(
            "the local ClaudeShip hub refused this machine's pairing token (HTTP 401{error}) while acting on host {host}"
        ),
        403 => format!(
            "host {host} refused (HTTP 403{error}). Jobs run only on a hub whose config.json has \"jobs\": true — its owner turns that on"
        ),
        404 => format!(
            "host {host}: not found (HTTP 404{error}). The host must be one from ship_hosts, and a finished job is kept for an hour"
        ),
        409 => {
            let theirs = body.get("theirs").map(Value::to_string).unwrap_or_else(|| "?".into());
            let ours = body.get("ours").map(Value::to_string).unwrap_or_else(|| "?".into());
            format!(
                "host {host} speaks another hub protocol (HTTP 409{error}; theirs {theirs}, ours {ours}): update the older ClaudeShip"
            )
        }
        502 => format!("host {host} is unreachable (HTTP 502{error}): it may be asleep or off the tailnet"),
        _ => format!("host {host}: the hub answered HTTP {status}{error}"),
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "claudeship", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "ClaudeShip's swarm: the machines paired with this one. ship_hosts lists them; \
ship_ask runs Claude headless on one (in a directory there) and returns its answer plus a sessionId to \
pass as resume for a follow-up in the same conversation; ship_run runs any command there. Long jobs come \
back as {running: true, job}: call ship_wait until running is false. ship_sessions shows what is already \
running on each machine.",
    })
}

/// The tool list, with schemas a model can use unaided.
fn tools() -> Vec<Value> {
    let host = json!({
        "type": "string",
        "description": "The machine to act on: a host name or id from ship_hosts (an id prefix of 4+ characters works too). Omit for this machine.",
    });
    let cwd = json!({
        "type": "string",
        "description": "Absolute path of the working directory on that host — inside its root (see ship_hosts) or its home directory, e.g. /home/me/code/project.",
    });
    let mode = json!({
        "type": "string",
        "enum": PERMISSION_MODES,
        "description": "Claude Code permission mode for the run. A headless run cannot answer prompts itself: under manual or plan a person must approve from the ClaudeShip phone app or web page while the job waits (ship_wait shows waitingFor). Default auto.",
    });
    let max_seconds = json!({
        "type": "integer",
        "minimum": 1,
        "description": "Wall-clock cap for the job, in seconds (hub default 1800, at most 14400 unless that hub's config raises it). On expiry the job is killed with exitCode 124 and timedOut true; its output so far is kept.",
    });
    let job = json!({
        "type": "string",
        "description": "The job id ship_run or ship_ask returned.",
    });
    vec![
        json!({
            "name": "ship_hosts",
            "description": "List the machines in this ClaudeShip swarm: name, id, whether this is the local one, whether it is reachable, its hub protocol, its project root and home directory, its default permission mode, and how many Claude sessions it has running. Use the name or id as the host of the other tools.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
        }),
        json!({
            "name": "ship_sessions",
            "description": "List the live Claude Code sessions on every machine (or one), each with its host, project, cwd, status (busy, idle, waiting, shell, starting), title, branch, and sessionId. Use it to see whether something is already running there before starting a job.",
            "inputSchema": {"type": "object", "properties": {"host": host}, "additionalProperties": false},
        }),
        json!({
            "name": "ship_ask",
            "description": "Ask Claude on another machine to do something: runs `claude -p <prompt>` headless in cwd there and waits up to about 50 seconds. Finished: returns {result (Claude's final answer), sessionId, exitCode, timedOut, job}. Not finished: returns {job, host, running: true, stdoutSoFar} — then call ship_wait(host, job) until running is false; its answer then carries result and sessionId. Pass a sessionId as resume to continue that remote conversation with its context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": host,
                    "cwd": cwd,
                    "prompt": {"type": "string", "description": "What the remote Claude should do. It sees only this prompt (and, with resume, its own earlier conversation), not yours: be self-contained."},
                    "mode": mode,
                    "resume": {"type": "string", "description": "The sessionId of an earlier ship_ask on the same host, to continue that conversation."},
                    "maxSeconds": max_seconds,
                },
                "required": ["host", "cwd", "prompt"],
                "additionalProperties": false,
            }
        }),
        json!({
            "name": "ship_run",
            "description": "Run a command (argv, no shell — use [\"sh\", \"-c\", \"...\"] for pipes) in cwd on a machine and return at once with {job, host}. Follow it with ship_wait to collect output and the exit code, ship_output to reread output, ship_kill to stop it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": host,
                    "cwd": cwd,
                    "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "The program and its arguments, e.g. [\"cargo\", \"test\"]."},
                    "mode": mode,
                    "maxSeconds": max_seconds,
                },
                "required": ["host", "cwd", "argv"],
                "additionalProperties": false,
            }
        }),
        json!({
            "name": "ship_wait",
            "description": "Wait for a job to finish or print more, up to timeoutSeconds, and return its status (running, exitCode, timedOut, waitingFor when it is blocked on a permission prompt) with only the output that is new since the previous ship_wait or ship_ask for this job. When a Claude job has finished, result and sessionId are included. Call again while running is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": host,
                    "job": job,
                    "timeoutSeconds": {"type": "number", "minimum": 0, "maximum": 50, "description": "How long to wait for a change, 0 to 50 seconds. Default 30."},
                },
                "required": ["host", "job"],
                "additionalProperties": false,
            }
        }),
        json!({
            "name": "ship_output",
            "description": "Read a job's stdout from a byte offset (default 0, the start) without waiting, up to 48 KB per call; when more is true, call again with since=nextSince. Also returns status, exitCode, and stderr. Does not move ship_wait's position.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": host,
                    "job": job,
                    "since": {"type": "integer", "minimum": 0, "description": "Byte offset into stdout to read from. Default 0."},
                },
                "required": ["host", "job"],
                "additionalProperties": false,
            }
        }),
        json!({
            "name": "ship_kill",
            "description": "Stop a running job (its whole process group). Its output so far stays readable with ship_output.",
            "inputSchema": {
                "type": "object",
                "properties": {"host": host, "job": job},
                "required": ["host", "job"],
                "additionalProperties": false,
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    //! The server against a stand-in hub: a tiny HTTP server on a thread
    //! that answers with canned bodies and records every request.

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{Endpoint, Server, serve};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const STEAM: &str = "aaaaaaaa-1111-4222-8333-444444444444";
    const LOCAL: &str = "bbbbbbbb-1111-4222-8333-444444444444";

    #[derive(Clone, Debug)]
    struct Request {
        method: String,
        path: String,
        query: Vec<(String, String)>,
        body: Value,
        cookie: Option<String>,
    }

    impl Request {
        fn param(&self, key: &str) -> Option<&str> {
            self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
        }
    }

    type Handler = dyn Fn(&Request) -> (u16, Value) + Send + Sync;

    struct FakeHub {
        port: u16,
        requests: Arc<Mutex<Vec<Request>>>,
    }

    impl FakeHub {
        fn start(handler: impl Fn(&Request) -> (u16, Value) + Send + Sync + 'static) -> FakeHub {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
            let handler: Arc<Handler> = Arc::new(handler);
            let log = requests.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (handler, log) = (handler.clone(), log.clone());
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        let mut parts = line.split(' ');
                        let method = parts.next().unwrap_or("").to_string();
                        let target = parts.next().unwrap_or("").to_string();
                        let (mut length, mut cookie) = (0, None);
                        loop {
                            let mut header = String::new();
                            reader.read_line(&mut header).unwrap();
                            let header = header.trim_end();
                            if header.is_empty() {
                                break;
                            }
                            let (k, v) = header.split_once(':').unwrap();
                            match k.to_ascii_lowercase().as_str() {
                                "content-length" => length = v.trim().parse().unwrap(),
                                "cookie" => cookie = Some(v.trim().to_string()),
                                _ => {}
                            }
                        }
                        let mut body = vec![0; length];
                        reader.read_exact(&mut body).unwrap();
                        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                        let request = Request {
                            method,
                            path: path.to_string(),
                            query: query
                                .split('&')
                                .filter_map(|p| p.split_once('='))
                                .map(|(k, v)| (k.to_string(), v.replace("%2D", "-")))
                                .collect(),
                            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                            cookie,
                        };
                        log.lock().unwrap().push(request.clone());
                        let (status, answer) = handler(&request);
                        let text = answer.to_string();
                        let mut stream = stream;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}",
                            text.len()
                        );
                    });
                }
            });
            FakeHub { port, requests }
        }

        fn server(&self) -> Server {
            Server::new(Endpoint::Fixed {
                authority: format!("127.0.0.1:{}", self.port),
                token: TOKEN.into(),
            })
        }

        fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }

        fn jobs_requests(&self) -> Vec<Request> {
            self.requests().into_iter().filter(|r| r.path.starts_with("/api/jobs")).collect()
        }
    }

    fn state() -> Value {
        json!({
            "protocol": 3,
            "hosts": [
                {"id": LOCAL, "name": "mac", "local": true, "reachable": true, "protocol": 3,
                 "root": "/Users/me/code", "home": "/Users/me",
                 "projects": [{"name": "ship", "path": "/Users/me/code/ship", "sessions": [
                     {"pid": 10, "cwd": "/Users/me/code/ship", "status": "busy", "title": "Refactor", "key": "k", "viewers": 0,
                      "approvals": [{"id": "x"}]}
                 ]}],
                 "elsewhere": []},
                {"id": STEAM, "name": "steam", "local": false, "reachable": true, "protocol": 3,
                 "root": "/home/me/code", "home": "/home/me",
                 "projects": [], "elsewhere": [{"pid": 20, "cwd": "/tmp", "status": "idle"}]},
                {"id": "cccccccc-0000-4000-8000-000000000000", "name": "deny", "reachable": true, "protocol": 3, "projects": []},
                {"id": "dddddddd-0000-4000-8000-000000000000", "name": "gone", "reachable": true, "protocol": 3, "projects": []},
                {"id": "eeeeeeee-0000-4000-8000-000000000000", "name": "old", "reachable": true, "protocol": 2, "projects": []},
                {"id": "ffffffff-0000-4000-8000-000000000000", "name": "asleep", "reachable": false, "protocol": 3, "projects": []},
            ]
        })
    }

    /// Run `lines` through `serve` and return the answers.
    fn converse(server: &mut Server, lines: &[Value]) -> Vec<Value> {
        let mut input = String::new();
        for line in lines {
            input.push_str(&line.to_string());
            input.push('\n');
        }
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output, server).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn call(server: &mut Server, tool: &str, arguments: Value) -> (bool, Value) {
        let answers = converse(
            server,
            &[json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": arguments}})],
        );
        let result = &answers[0]["result"];
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        let error = result["isError"].as_bool() == Some(true);
        let value = if error { Value::String(text) } else { serde_json::from_str(&text).unwrap() };
        (error, value)
    }

    fn base(request: &Request) -> Option<(u16, Value)> {
        (request.path == "/api/state").then(|| (200, state()))
    }

    #[test]
    fn a_scripted_conversation() {
        let hub = FakeHub::start(|r| base(r).unwrap_or((404, json!({"error": "no"}))));
        let mut server = hub.server();
        let mut input = String::new();
        for line in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}).to_string(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
            String::new(),
            json!({"jsonrpc": "2.0", "id": "two", "method": "tools/list"}).to_string(),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "ship_hosts", "arguments": {}}}).to_string(),
            json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}).to_string(),
            json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"}).to_string(),
            "{not json".to_string(),
            json!({"id": 6, "method": "ping"}).to_string(),
            json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "ship_nothing"}}).to_string(),
            json!([{"jsonrpc": "2.0", "id": 8, "method": "ping"}, {"jsonrpc": "2.0", "method": "notifications/cancelled"}]).to_string(),
            json!({"jsonrpc": "2.0", "id": 9, "result": {}}).to_string(),
        ] {
            input.push_str(&line);
            input.push_str("\r\n");
        }
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output, &mut server).unwrap();
        let output = String::from_utf8(output).unwrap();
        let answers: Vec<Value> = output.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(answers.len(), 9, "one line per request, none for notifications or responses: {output}");

        assert_eq!(answers[0]["id"], 1);
        assert_eq!(answers[0]["result"]["protocolVersion"], "2024-11-05");
        assert!(answers[0]["result"]["capabilities"]["tools"].is_object());
        assert_eq!(answers[0]["result"]["serverInfo"]["name"], "claudeship");

        assert_eq!(answers[1]["id"], "two");
        let names: Vec<&str> = answers[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["ship_hosts", "ship_sessions", "ship_ask", "ship_run", "ship_wait", "ship_output", "ship_kill"]
        );
        for tool in answers[1]["result"]["tools"].as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(tool["description"].as_str().unwrap().len() > 40);
        }

        assert_eq!(answers[2]["id"], 3);
        assert!(answers[2]["result"]["isError"].is_null());
        let hosts: Value = serde_json::from_str(answers[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(hosts["hosts"][0]["name"], "mac");
        assert_eq!(hosts["hosts"][0]["sessions"], 1);
        assert_eq!(hosts["hosts"][1]["name"], "steam");
        assert_eq!(hosts["hosts"][1]["root"], "/home/me/code");
        assert!(hosts["hosts"][0].get("projects").is_none(), "hosts are a summary");

        assert_eq!(answers[3], json!({"jsonrpc": "2.0", "id": 4, "result": {}}));
        assert_eq!(answers[4]["error"]["code"], -32601, "unknown method");
        assert_eq!(answers[5]["error"]["code"], -32700, "malformed line");
        assert_eq!(answers[5]["id"], Value::Null);
        assert_eq!(answers[6]["error"]["code"], -32600, "no jsonrpc 2.0");
        assert_eq!(answers[7]["error"]["code"], -32602, "unknown tool");
        assert_eq!(answers[8], json!([{"jsonrpc": "2.0", "id": 8, "result": {}}]), "a batch");

        let requests = hub.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].cookie.as_deref(), Some(&*format!("claude_ship={TOKEN}")));
    }

    #[test]
    fn sessions_flatten_with_host_names() {
        let hub = FakeHub::start(|r| base(r).unwrap());
        let mut server = hub.server();
        let (error, all) = call(&mut server, "ship_sessions", json!({}));
        assert!(!error);
        let sessions = all["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0]["host"], "mac");
        assert_eq!(sessions[0]["project"], "ship");
        assert_eq!(sessions[0]["pendingApprovals"], 1);
        assert!(sessions[0].get("key").is_none());
        assert_eq!(sessions[1]["host"], "steam");
        assert_eq!(sessions[1]["project"], Value::Null);
        assert_eq!(all["unreachableHosts"], json!(["asleep"]));
        let (_, steam) = call(&mut server, "ship_sessions", json!({"host": "STEAM"}));
        assert_eq!(steam["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(steam["sessions"][0]["hostId"], STEAM);
    }

    const RESULT_LINE: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"All 12 tests pass.","session_id":"5e55-1"}"#;

    #[test]
    fn ask_completes_and_finds_a_split_result_line() {
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = polls.clone();
        let hub = FakeHub::start(move |r| {
            if let Some(answer) = base(r) {
                return answer;
            }
            match (r.method.as_str(), r.path.as_str()) {
                ("POST", "/api/jobs") => (200, json!({"id": "abc123", "host": STEAM})),
                ("GET", "/api/jobs/abc123") => {
                    let (first, second) = RESULT_LINE.split_at(30);
                    let lead = "{\"type\":\"system\",\"subtype\":\"init\"}\n";
                    if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                        let out = format!("{lead}{first}");
                        (200, json!({"id": "abc123", "running": true, "stdout": out, "next": out.len(), "stderr": ""}))
                    } else {
                        (200, json!({"id": "abc123", "running": false, "exitCode": 0, "timedOut": false,
                                     "claudeSessionId": "5e55-1", "stdout": format!("{second}\n"),
                                     "next": lead.len() + RESULT_LINE.len() + 1, "stderr": ""}))
                    }
                }
                _ => (404, json!({"error": "no"})),
            }
        });
        let mut server = hub.server();
        let (error, answer) = call(
            &mut server,
            "ship_ask",
            json!({"host": "steam", "cwd": "/home/me/code/ship", "prompt": "run the tests", "resume": "old-1"}),
        );
        assert!(!error, "{answer}");
        assert_eq!(answer["result"], "All 12 tests pass.");
        assert_eq!(answer["sessionId"], "5e55-1");
        assert_eq!(answer["exitCode"], 0);
        assert_eq!(answer["timedOut"], false);
        assert_eq!(answer["job"], "abc123");
        let jobs = hub.jobs_requests();
        assert_eq!(jobs[0].body["host"], STEAM, "a name resolves to the id the hub routes by");
        assert_eq!(jobs[0].body["permissionMode"], "auto");
        assert_eq!(jobs[0].body["resume"], "old-1");
        assert_eq!(jobs[0].body["prompt"], "run the tests");
        assert_eq!(jobs[1].param("since"), Some("0"));
        assert_eq!(jobs[1].param("host"), Some(STEAM));
        assert!(jobs[1].param("wait").is_some());
        let lead = "{\"type\":\"system\",\"subtype\":\"init\"}\n".len() + 30;
        assert_eq!(jobs[2].param("since"), Some(&*lead.to_string()));
    }

    #[test]
    fn ask_hands_back_the_job_at_its_budget_and_wait_continues() {
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finish = done.clone();
        let hub = FakeHub::start(move |r| {
            if let Some(answer) = base(r) {
                return answer;
            }
            match (r.method.as_str(), r.path.as_str()) {
                ("POST", "/api/jobs") => (200, json!({"id": "f00d01", "host": LOCAL})),
                ("GET", "/api/jobs/f00d01") => {
                    let since: usize = r.param("since").unwrap().parse().unwrap();
                    if finish.load(Ordering::SeqCst) {
                        let out = format!("{RESULT_LINE}\n");
                        return (200, json!({"running": false, "exitCode": 0, "timedOut": false, "stdout": out,
                                             "next": since + out.len(), "stderr": ""}));
                    }
                    let wait: u64 = r.param("wait").map_or(0, |w| w.parse().unwrap());
                    std::thread::sleep(Duration::from_millis(400).min(Duration::from_secs(wait)));
                    (200, json!({"running": true, "stdout": "tick\n", "next": since + 5, "stderr": "",
                                 "waitingFor": "Bash"}))
                }
                _ => (404, json!({"error": "no"})),
            }
        });
        let mut server = hub.server().with_budget(Duration::from_secs(2));
        let started = std::time::Instant::now();
        let (error, answer) = call(&mut server, "ship_ask", json!({"cwd": "/Users/me/code/ship", "prompt": "hi", "mode": "manual"}));
        assert!(!error, "{answer}");
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(answer["running"], true);
        assert_eq!(answer["job"], "f00d01");
        assert_eq!(answer["host"], "mac");
        assert_eq!(answer["waitingFor"], "Bash");
        assert!(answer["stdoutSoFar"].as_str().unwrap().starts_with("tick\n"));
        let polls = hub.jobs_requests().len() - 1;
        assert!(polls >= 2, "it long-polled until the budget: {polls}");
        assert!(hub.jobs_requests()[0].body.get("host").is_some());
        assert_eq!(hub.jobs_requests()[0].body["permissionMode"], "manual");

        // ship_wait picks up where ship_ask stopped, and caps its wait.
        let (_, waited) = call(&mut server, "ship_wait", json!({"host": "mac", "job": "f00d01", "timeoutSeconds": 1}));
        assert_eq!(waited["running"], true);
        assert_eq!(waited["stdout"], "tick\n");
        let last = hub.jobs_requests().last().unwrap().clone();
        assert_eq!(last.param("since"), Some(&*(5 * polls).to_string()));
        assert_eq!(last.param("wait"), Some("1"));

        done.store(true, Ordering::SeqCst);
        let (_, finished) = call(&mut server, "ship_wait", json!({"job": "f00d01", "timeoutSeconds": 500}));
        assert_eq!(finished["running"], false);
        assert_eq!(finished["result"], "All 12 tests pass.");
        assert_eq!(finished["sessionId"], "5e55-1");
        assert_eq!(finished["stdoutOffset"], 5 * (polls + 1) + RESULT_LINE.len() + 1);
        let last = hub.jobs_requests().last().unwrap().clone();
        assert_eq!(last.param("wait"), Some("50"), "a wait is capped at 50 s");
        assert_eq!(last.param("since"), Some(&*(5 * (polls + 1)).to_string()));

        // ship_output rereads from anywhere without moving the cursor.
        let (_, output) = call(&mut server, "ship_output", json!({"job": "f00d01", "since": 3}));
        assert_eq!(output["result"], "All 12 tests pass.");
        assert_eq!(output["more"], false);
        assert_eq!(hub.jobs_requests().last().unwrap().param("since"), Some("3"));
        assert!(hub.jobs_requests().last().unwrap().param("wait").is_none());
    }

    #[test]
    fn a_job_without_a_result_event_answers_with_its_output() {
        let hub = FakeHub::start(|r| {
            base(r).unwrap_or_else(|| match r.method.as_str() {
                "POST" if r.path == "/api/jobs/beef01/kill" => (200, json!({"ok": true})),
                "POST" => (200, json!({"id": "beef01"})),
                _ => (200, json!({"running": false, "exitCode": 2, "timedOut": true, "stdout": "partial\n",
                                  "next": 8, "stderr": "boom"})),
            })
        });
        let mut server = hub.server();
        let (_, answer) = call(&mut server, "ship_ask", json!({"host": "steam", "cwd": "/x", "prompt": "p", "maxSeconds": 60}));
        assert_eq!(answer["result"], "partial");
        assert_eq!(answer["exitCode"], 2);
        assert_eq!(answer["timedOut"], true);
        assert_eq!(answer["stderr"], "boom");
        assert_eq!(hub.jobs_requests()[0].body["maxSeconds"], 60);

        let (error, run) = call(&mut server, "ship_run", json!({"host": "aaaaaaaa", "cwd": "/x", "argv": ["make", "test"]}));
        assert!(!error);
        assert_eq!(run, json!({"job": "beef01", "host": "steam", "hostId": STEAM}));
        let post = hub.jobs_requests().into_iter().filter(|r| r.method == "POST").nth(1).unwrap();
        assert_eq!(post.body["argv"], json!(["make", "test"]));
        assert!(post.body.get("permissionMode").is_none());

        let (error, killed) = call(&mut server, "ship_kill", json!({"host": "steam", "job": "beef01"}));
        assert!(!error);
        assert_eq!(killed["ok"], true);
        let kill = hub.jobs_requests().pop().unwrap();
        assert_eq!(kill.path, "/api/jobs/beef01/kill");
        assert_eq!(kill.body["host"], STEAM);
    }

    #[test]
    fn refusals_name_the_host() {
        let hub = FakeHub::start(|r| {
            base(r).unwrap_or_else(|| match r.body.get("host").and_then(Value::as_str).unwrap_or("") {
                h if h.starts_with("cccc") => (403, json!({"error": "jobs are disabled"})),
                h if h.starts_with("dddd") => (404, json!({"error": "no such job"})),
                h if h.starts_with("eeee") => (409, json!({"error": "protocol mismatch", "theirs": 2, "ours": 3})),
                h if h.starts_with("ffff") => (502, json!({"error": "unreachable"})),
                _ => (500, json!({})),
            })
        });
        let mut server = hub.server();
        for (host, status, words) in [
            ("deny", "403", "\"jobs\": true"),
            ("gone", "404", "no such job"),
            ("old", "409", "theirs 2, ours 3"),
            ("asleep", "502", "unreachable"),
        ] {
            let (error, text) = call(&mut server, "ship_run", json!({"host": host, "cwd": "/x", "argv": ["true"]}));
            let text = text.as_str().unwrap();
            assert!(error, "{host}");
            assert!(text.contains(&format!("host {host}")), "{text}");
            assert!(text.contains(status) && text.contains(words), "{text}");
        }
        let (error, text) = call(&mut server, "ship_kill", json!({"host": "nowhere", "job": "abc"}));
        assert!(error);
        assert!(text.as_str().unwrap().contains("no host \"nowhere\"") && text.as_str().unwrap().contains("steam"));
        for (tool, args, words) in [
            ("ship_run", json!({"cwd": "relative", "argv": ["x"]}), "absolute"),
            ("ship_run", json!({"cwd": "/x", "argv": []}), "argv"),
            ("ship_ask", json!({"cwd": "/x", "prompt": "p", "mode": "yolo"}), "mode must be one of"),
            ("ship_wait", json!({"job": "../etc"}), "job must be"),
            ("ship_ask", json!({"cwd": "/x", "prompt": "p", "maxSeconds": 0}), "maxSeconds"),
        ] {
            let (error, text) = call(&mut server, tool, args);
            assert!(error && text.as_str().unwrap().contains(words), "{tool}: {text}");
        }
        assert!(hub.jobs_requests().len() == 4, "invalid arguments never reach the hub");
    }

    #[test]
    fn no_hub_is_a_tool_error() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut server = Server::new(Endpoint::Fixed {
            authority: format!("127.0.0.1:{port}"),
            token: TOKEN.into(),
        });
        let (error, text) = call(&mut server, "ship_hosts", json!({}));
        assert!(error);
        assert!(text.as_str().unwrap().contains("cannot reach the local ClaudeShip hub"), "{text}");
    }

    #[test]
    fn an_oversized_message_is_skipped_not_kept() {
        let hub = FakeHub::start(|r| base(r).unwrap());
        let mut server = hub.server();
        let mut input = vec![b'x'; super::MAX_LINE + 10];
        input.extend_from_slice(b"\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");
        let mut output = Vec::new();
        serve(&input[..], &mut output, &mut server).unwrap();
        let answers: Vec<Value> = String::from_utf8(output).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0]["error"]["code"], -32700);
        assert_eq!(answers[1], json!({"jsonrpc": "2.0", "id": 2, "result": {}}), "the next message is still read");
    }

    /// The hub's real answer shape: `stdout` from `since` (later than asked
    /// when the front was dropped) and `next`; the `result` event read
    /// while the run was still exiting, then a finish with no new text.
    #[test]
    fn offsets_follow_the_hub_and_the_result_outlives_its_read() {
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = polls.clone();
        let hub = FakeHub::start(move |r| {
            if let Some(answer) = base(r) {
                return answer;
            }
            match (r.method.as_str(), r.path.as_str()) {
                ("POST", "/api/jobs") => (200, json!({"id": "c0ffee", "host": STEAM})),
                ("GET", "/api/jobs/c0ffee") => match seen.fetch_add(1, Ordering::SeqCst) {
                    // 1000 bytes were dropped before the first read; the
                    // text kept has a replaced (non-UTF-8) byte, so it is
                    // longer than the bytes it stands for.
                    0 => (200, json!({"running": true, "stdout": "\u{fffd}a\n", "since": 1000, "next": 1003, "stderr": ""})),
                    1 => (200, json!({"running": true, "stdout": format!("{RESULT_LINE}\n"), "since": 1003,
                                      "next": 1003 + RESULT_LINE.len() + 1, "stderr": ""})),
                    _ => (200, json!({"running": false, "exitCode": 0, "timedOut": false, "stdout": "",
                                      "since": 1003 + RESULT_LINE.len() + 1, "next": 1003 + RESULT_LINE.len() + 1,
                                      "claudeSessionId": "5e55-1", "result": "All 12 tests pass.", "stderr": ""})),
                },
                _ => (404, json!({"error": "no"})),
            }
        });
        let mut server = hub.server();
        let (_, run) = call(&mut server, "ship_run", json!({"host": "steam", "cwd": "/x", "argv": ["claude"]}));
        assert_eq!(run["job"], "c0ffee");
        let (_, first) = call(&mut server, "ship_wait", json!({"host": "steam", "job": "c0ffee", "timeoutSeconds": 0}));
        assert_eq!(first["stdoutOffset"], 1003);
        let (_, second) = call(&mut server, "ship_wait", json!({"host": "steam", "job": "c0ffee", "timeoutSeconds": 0}));
        assert_eq!(second["running"], true);
        let (_, done) = call(&mut server, "ship_wait", json!({"host": "steam", "job": "c0ffee", "timeoutSeconds": 0}));
        assert_eq!(done["running"], false);
        assert_eq!(done["result"], "All 12 tests pass.", "{done}");
        assert_eq!(done["sessionId"], "5e55-1");
        let since: Vec<String> = hub.jobs_requests().iter().filter_map(|r| r.param("since").map(String::from)).collect();
        assert_eq!(since, ["0", "1003", &(1003 + RESULT_LINE.len() + 1).to_string()], "each read starts at the last next");
        // ship_output pages from where the text really starts.
        let (_, output) = call(&mut server, "ship_output", json!({"host": "steam", "job": "c0ffee"}));
        assert_eq!(output["nextSince"], 1003 + RESULT_LINE.len() + 1);
        assert_eq!(output["result"], "All 12 tests pass.", "the hub's kept result");
    }

    #[test]
    fn output_helpers() {
        assert_eq!(super::tail("héllo", 4), "llo");
        assert_eq!(super::head("héllo", 2), "h");
        assert_eq!(super::capped_tail("abcdef", 4), ("cdef", 2));
        assert_eq!(super::encode("a b/ü"), "a%20b%2F%C3%BC");
        assert!(super::complete(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"));
        assert!(!super::complete(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{}"));
        let text = format!("{RESULT_LINE}\n{{\"type\":\"assistant\"}}\nnot json\n");
        assert_eq!(super::last_result_event(&text).unwrap()["result"], "All 12 tests pass.");
        assert!(super::last_result_event("{\"type\":\"system\",\"result\":1}").is_none());
    }
}
