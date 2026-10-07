//! The HTTP side: the gate every request passes, the pairing link, the
//! JSON API, the terminal WebSocket's upgrade, and the page's files. Paths,
//! status codes, and bodies are the Swift hub's, so the page and the phone
//! work against either.
//!
//! | Route | Gate | |
//! |---|---|---|
//! | `GET /auth?k=` | host | the secret, once → 303 `/` + cookie |
//! | `GET /ws/term?id&rows&cols&claim` | host, same-origin, paired | a terminal (`ws`) |
//! | `GET /api/state` | host, paired | the directory, cached 1 s |
//! | `POST /api/launch`, `/api/kill`, `/api/settings`, `/api/approve`, `/api/auto-approve` | host, same-origin, paired, JSON | |
//! | `POST /api/swarm`, `/api/swarm/join`, `/api/swarm/invite`, `/api/swarm/unpair` | host, same-origin, paired, JSON | the swarm (`swarm.rs`) |
//! | `GET /api/swarm/peers` | host, paired | what `hub peers` prints |
//! | `GET /api/jobs…`, `POST /api/jobs…` | host, paired; POSTs same-origin JSON | jobs (`jobs.rs`); `config.jobs` off → 403 |
//! | `/peer/*` | host, swarm bearer, POSTs JSON with no foreign Origin | `peer.rs` |
//! | `/peer/api/*`, `GET /peer/ws/term` | same (the upgrade needs no Origin) | `peer_api.rs`: a peer's relayed action or terminal |
//! | `GET /*` | host | the page's files |
//!
//! A client's action or terminal naming another hub of the swarm (`host`)
//! is forwarded there by `proxy.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state, map_response};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot, watch};

use super::security::{
    constant_time_equals, cookie, is_allowed_host, is_same_origin, percent_decode,
};
use super::{ConnectionSlot, Patience, Shared, assets, jobs, peer, peer_api, proxy, ws};
use crate::approvals::Rule;
use crate::config::HubConfig;
use crate::hub::{Command, clamp_size};
use crate::swarm::PeerRecord;
use crate::token::COOKIE_NAME;

/// Request bodies past this are refused (the API's are a few dozen bytes).
const MAX_BODY: usize = 1 << 20;
/// A browser terminal's largest message (a paste).
pub(super) const MAX_MESSAGE: usize = 8 << 20;

/// What the gate found out about a request, for the handlers.
#[derive(Clone)]
pub(super) struct Gate {
    /// The Host header, already known to be one we answer to.
    host: String,
    same_origin: bool,
    /// The request carries this hub's pairing secret.
    paired: bool,
    /// Percent-decoded.
    pub(super) path: String,
    pub(super) query: HashMap<String, String>,
    /// Subscribed before the secret was checked: a terminal that passed
    /// the check still hangs up if the secret changed at any point after.
    pub(super) epoch: watch::Receiver<u64>,
}

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/auth", any(auth))
        .route("/ws/term", any(terminal))
        .route("/api/state", any(api_state))
        .route("/api/launch", any(api_post))
        .route("/api/kill", any(api_post))
        .route("/api/settings", any(api_post))
        .route("/api/approve", any(api_post))
        .route("/api/auto-approve", any(api_post))
        .route("/api/swarm", any(api_post))
        .route("/api/swarm/join", any(api_post))
        .route("/api/jobs", any(api_jobs))
        .route("/api/jobs/{*rest}", any(api_jobs))
        .route("/api/swarm/invite", any(api_post))
        .route("/api/swarm/unpair", any(api_post))
        .route("/api/swarm/peers", any(api_swarm_peers))
        .merge(peer::router(peer::LocalOnly {
            hub: shared.hub.clone(),
            state: shared.state.clone(),
            book: shared.swarm.book.clone(),
            jobs: shared.jobs.clone(),
        }))
        .merge(peer_api::router(peer::LocalOnly {
            hub: shared.hub.clone(),
            state: shared.state.clone(),
            book: shared.swarm.book.clone(),
            jobs: shared.jobs.clone(),
        }))
        .fallback(fallback)
        .layer(from_fn_with_state(shared.clone(), gate))
        .layer(map_response(common_headers))
        .with_state(shared)
}

// MARK: Responses

fn respond(status: u16, content_type: &str, body: impl Into<Body>) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

pub(super) fn text(status: u16, body: &'static str) -> Response {
    respond(status, "text/plain; charset=utf-8", body)
}

pub(super) fn json_response(status: u16, value: Value) -> Response {
    respond(status, "application/json", value.to_string())
}

/// On every answer but a WebSocket's 101: one request per connection, and
/// nothing cached, sniffed, or referred.
async fn common_headers(mut response: Response) -> Response {
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let headers = response.headers_mut();
        headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
    }
    response
}

// MARK: The gate

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// `a=1&b=%20` as the Swift hub read it: pieces split on `&` (empty ones
/// dropped), percent-decoded, a key that doesn't decode skipped, a value
/// that doesn't decode empty. `+` is not a space.
fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (pair, None),
        };
        let Some(key) = percent_decode(key) else {
            continue;
        };
        let value = value.map(|v| percent_decode(v).unwrap_or_default());
        out.insert(key, value.unwrap_or_default());
    }
    out
}

/// Before any route: the Host header must be a name DNS can't fake; an
/// upgrade must be the terminal's, same-origin and paired; the API needs
/// the pairing secret. Only the pairing link itself goes without it.
async fn gate(State(shared): State<Arc<Shared>>, mut request: Request, next: Next) -> Response {
    let Some(path) = percent_decode(request.uri().path()).filter(|p| p.starts_with('/')) else {
        return text(400, "bad request");
    };
    let headers = request.headers();
    let Some(host) = header_text(headers, header::HOST)
        .filter(|h| is_allowed_host(h, &shared.allowed_hosts))
        .map(str::to_string)
    else {
        return text(
            403,
            "This hub answers only to localhost or its Tailscale IP address.",
        );
    };
    // An Origin that isn't readable text, or more than one, is not ours:
    // only a missing one means "not a browser".
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let same_origin = match (origins.next(), origins.next()) {
        (None, _) => is_same_origin(None, Some(&host)),
        (Some(origin), None) => origin
            .to_str()
            .is_ok_and(|origin| is_same_origin(Some(origin), Some(&host))),
        (Some(_), Some(_)) => false,
    };
    let epoch = shared.epoch();
    let paired = shared.token().is_some_and(|token| {
        cookie(COOKIE_NAME, header_text(headers, header::COOKIE))
            .is_some_and(|offered| constant_time_equals(&offered, &token))
    });
    let get = request.method() == Method::GET;
    let is_auth = get && path == "/auth";
    let upgrade = header_text(headers, header::UPGRADE)
        .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    if !is_auth && upgrade {
        let key = headers.contains_key(header::SEC_WEBSOCKET_KEY);
        // A peer's relayed terminal: the bearer below is its credential,
        // and a peer is not a browser (no Origin to check).
        let peer_terminal = get && path == "/peer/ws/term" && key;
        let browser_terminal = get && path == "/ws/term" && same_origin && paired && key;
        if !(browser_terminal || peer_terminal) {
            return text(403, "websocket refused");
        }
    }
    if !is_auth && path.starts_with("/api/") && !paired {
        return json_response(401, json!({"error": "not paired"}));
    }
    // A peer: the swarm secret as a bearer, compared in constant time. A
    // peer is not a browser; a POST must still be JSON, and an Origin (a
    // browser's) must be ours.
    if path.starts_with("/peer/") {
        let offered = header_text(headers, header::AUTHORIZATION)
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, secret)| secret.trim());
        if !offered.is_some_and(|secret| shared.swarm.book.check(secret)) {
            return json_response(401, json!({"error": "not a peer"}));
        }
        let json_type = header_text(headers, header::CONTENT_TYPE)
            .is_some_and(|t| t.to_lowercase().starts_with("application/json"));
        if request.method() == Method::POST && !(same_origin && json_type) {
            return json_response(403, json!({"error": "refused"}));
        }
    }
    let query = parse_query(request.uri().query().unwrap_or(""));
    request.extensions_mut().insert(Gate {
        host,
        same_origin,
        paired,
        path,
        query,
        epoch,
    });
    next.run(request).await
}

pub(super) fn gate_of(request: &Request) -> Gate {
    request
        .extensions()
        .get::<Gate>()
        .cloned()
        .expect("the gate runs before every route")
}

// MARK: Routes

/// Any GET the routes don't claim is a file; anything else is refused.
async fn fallback(request: Request) -> Response {
    let gate = gate_of(&request);
    other(request.method(), &gate)
}

fn other(method: &Method, gate: &Gate) -> Response {
    if method == Method::GET {
        asset(&gate.path, &gate.host)
    } else {
        text(405, "method not allowed")
    }
}

fn asset(path: &str, host: &str) -> Response {
    let Some((data, kind)) = assets::lookup(path) else {
        return text(404, "not found");
    };
    let mut response = respond(200, kind, data);
    // Everything the page loads is ours; xterm.js injects <style>
    // elements, hence the inline-style allowance. The WebSocket origin is
    // spelled out because Safari doesn't count ws: under 'self'; `host`
    // has passed `is_allowed_host`, so it is safe to echo.
    let csp = format!(
        "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
         connect-src 'self' ws://{host} wss://{host}; frame-ancestors 'none'; base-uri 'none'"
    );
    if let Ok(value) = HeaderValue::from_str(&csp) {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

/// Pairing: the link from `claudeship hub link` carries the secret once;
/// from then on the browser holds it as a cookie.
async fn auth(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let gate = gate_of(&request);
    if request.method() != Method::GET {
        return other(request.method(), &gate);
    }
    let token = shared.token();
    let Some(token) = token.filter(|token| {
        gate.query
            .get("k")
            .is_some_and(|offered| constant_time_equals(offered, token))
    }) else {
        return text(403, "That pairing link is not valid for this hub.");
    };
    let mut response = respond(303, "text/plain; charset=utf-8", Body::empty());
    let headers = response.headers_mut();
    headers.insert(header::LOCATION, HeaderValue::from_static("/"));
    if let Ok(value) = HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={token}; Path=/; Max-Age=31536000; HttpOnly; SameSite=Strict"
    )) {
        headers.insert(header::SET_COOKIE, value);
    }
    response
}

/// A browser terminal. The gate has already refused any upgrade that isn't
/// this one, same-origin, and paired.
async fn terminal(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let gate = gate_of(&request);
    let upgrade = header_text(request.headers(), header::UPGRADE)
        .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    if !upgrade {
        return other(request.method(), &gate);
    }
    let slot = request.extensions().get::<Arc<ConnectionSlot>>().cloned();
    let (mut parts, _) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let epoch = gate.epoch.clone();
    let upgrade = upgrade.max_message_size(MAX_MESSAGE).max_frame_size(MAX_MESSAGE);
    // Another hub's session: relayed there.
    match proxy::target(&shared, gate.query.get("host").map(|h| Value::String(h.clone())).as_ref()) {
        Err(refusal) => return refusal.response(),
        Ok(Some(route)) => return proxy::terminal(shared, route, &gate.query, upgrade, slot, epoch).await,
        Ok(None) => {}
    }
    let attach = attach_from(&gate.query);
    upgrade.on_upgrade(move |socket| ws::run(socket, shared, attach, slot, epoch))
}

/// What a terminal's upgrade query asks for.
pub(super) fn attach_from(query: &HashMap<String, String>) -> ws::Attach {
    let number = |k: &str| query.get(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    ws::Attach {
        id: query.get("id").cloned().unwrap_or_default(),
        size: clamp_size(number("rows"), number("cols")),
        // `claim=0`: a screen that was only mirroring reconnects as a
        // spectator rather than resizing the session under its user.
        claim: query.get("claim").map(String::as_str) != Some("0"),
    }
}

/// The local directory (cached), plus `hosts`: this hub and every peer's
/// last state, from the book — nothing here waits on a peer.
async fn api_state(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let gate = gate_of(&request);
    if request.method() != Method::GET {
        return other(request.method(), &gate);
    }
    let local = shared.state.get(&shared.hub).await;
    let Ok(Value::Object(mut state)) = serde_json::from_slice::<Value>(&local) else {
        return respond(200, "application/json", local);
    };
    let hosts = shared.swarm.book.hosts(&Value::Object(state.clone()));
    state.insert("hosts".into(), Value::Array(hosts));
    json_response(200, Value::Object(state))
}

/// `GET /api/swarm/peers`: this hub's record and every peer's (tombstones
/// included), with whether the last poll reached it — `hub peers`.
async fn api_swarm_peers(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let gate = gate_of(&request);
    if request.method() != Method::GET {
        return other(request.method(), &gate);
    }
    json_response(200, shared.swarm.book.view())
}

/// `/api/jobs…` (`jobs.rs`): here, or relayed to the hub named by `host`
/// (the body's for a POST, the query's for a GET). A relayed GET gets its
/// `wait` plus 5 s to answer; this connection, its wait plus 10.
async fn api_jobs(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let gate = gate_of(&request);
    let method = request.method().clone();
    let patience = request.extensions().get::<Patience>().cloned();
    let rest = gate.path.strip_prefix("/api/jobs").unwrap_or("").to_string();
    let body = match method {
        Method::GET => None,
        Method::POST => {
            let json_type = header_text(request.headers(), header::CONTENT_TYPE)
                .is_some_and(|t| t.to_lowercase().starts_with("application/json"));
            let body = read_json_object(request).await;
            let Some(body) = body.filter(|_| gate.same_origin && json_type && gate.paired) else {
                return json_response(403, json!({"error": "refused"}));
            };
            Some(body)
        }
        _ => return text(405, "method not allowed"),
    };
    let host = match &body {
        Some(body) => body.get("host").cloned(),
        None => gate.query.get("host").map(|h| Value::String(h.clone())),
    };
    match proxy::target(&shared, host.as_ref()) {
        Err(refusal) => refusal.response(),
        Ok(Some(route)) => match body {
            Some(body) => proxy::forward(&shared, route, &gate.path, body).await,
            None => {
                let wait = jobs::wait_seconds(&gate.query);
                if let Some(patience) = &patience {
                    patience.extend(wait + 10);
                }
                let query = proxy::query_without_host(&gate.query);
                let target = if query.is_empty() { gate.path.clone() } else { format!("{}?{query}", gate.path) };
                proxy::forward_get(&shared, route, &target, std::time::Duration::from_secs(wait + 5)).await
            }
        },
        Ok(None) => {
            let id = shared.swarm.book.id();
            jobs::local(&shared.jobs, &id, &method, &rest, &gate.query, body, patience.as_ref()).await
        }
    }
}

/// A request's body as a JSON object (at most `MAX_BODY`).
pub(super) async fn read_json_object(request: Request) -> Option<Map<String, Value>> {
    let body = axum::body::to_bytes(request.into_body(), MAX_BODY).await.ok()?;
    match serde_json::from_slice::<Value>(&body).ok()? {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// Asks the hub and waits for its answer.
async fn ask<T>(hub: &mpsc::UnboundedSender<Command>, make: impl FnOnce(oneshot::Sender<T>) -> Command) -> Option<T> {
    let (tx, rx) = oneshot::channel();
    hub.send(make(tx)).ok()?;
    rx.await.ok()
}

fn hub_gone() -> Response {
    json_response(500, json!({"error": "the hub is stopping"}))
}

async fn api_post(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let gate = gate_of(&request);
    let patience = request.extensions().get::<Patience>().cloned();
    if request.method() != Method::POST {
        return other(request.method(), &gate);
    }
    let json_type = header_text(request.headers(), header::CONTENT_TYPE)
        .is_some_and(|t| t.to_lowercase().starts_with("application/json"));
    let Ok(body) = axum::body::to_bytes(request.into_body(), MAX_BODY).await else {
        return text(400, "bad request");
    };
    // JSON-only and same-origin: a form on another site can send neither,
    // so it can't start or end sessions through the user's browser.
    let object: Option<Map<String, Value>> = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| match v {
            Value::Object(o) => Some(o),
            _ => None,
        });
    let Some(body) = object.filter(|_| gate.same_origin && json_type && gate.paired) else {
        return json_response(403, json!({"error": "refused"}));
    };
    let string = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    match gate.path.as_str() {
        "/api/swarm" => {
            // What a joining hub needs: the secret and who is in the swarm.
            let book = &shared.swarm.book;
            let peers: Vec<Value> = book.records().iter().map(PeerRecord::to_value).collect();
            json_response(200, json!({"secret": book.secret(), "peers": peers}))
        }
        "/api/swarm/join" => {
            let Some(secret) = string("secret") else {
                return json_response(400, json!({"error": "join needs a secret and peers"}));
            };
            let peers = PeerRecord::list_from(body.get("peers"));
            match shared.swarm.join(&secret, peers).await {
                Ok(value) => json_response(200, value),
                Err(e) => json_response(400, json!({"error": e})),
            }
        }
        "/api/swarm/invite" => {
            // Enrol another computer: its pairing link, followed from here
            // (a browser paired with us can't post to it).
            let Some(link) = string("link") else {
                return json_response(400, json!({"error": "invite needs the other computer's pairing link"}));
            };
            // Its three steps may take longer than a plain request is
            // given: don't let the connection be dropped mid-enrolment.
            if let Some(patience) = &patience {
                patience.extend(crate::swarm::INVITE_BUDGET.as_secs() + 5);
            }
            match shared.swarm.invite(&link).await {
                Ok(value) => json_response(200, value),
                Err((status, error)) => json_response(status, json!({"error": error})),
            }
        }
        "/api/swarm/unpair" => {
            let book = &shared.swarm.book;
            let Some(id) = string("id").filter(|id| crate::web::state::is_session_id(id)) else {
                return json_response(400, json!({"error": "unpair needs a hub's id"}));
            };
            if id.eq_ignore_ascii_case(&book.id()) {
                return json_response(400, json!({"error": "that is this hub; it can't remove itself"}));
            }
            match book.unpair(&id) {
                Ok(record) => json_response(200, json!({"ok": true, "record": record.to_value()})),
                Err(e) => json_response(404, json!({"error": e})),
            }
        }
        // Another hub's session or settings: forwarded there.
        path => match proxy::target(&shared, body.get("host")) {
            Err(refusal) => refusal.response(),
            Ok(Some(route)) => proxy::forward(&shared, route, path, body).await,
            Ok(None) => {
                let response = local_action(&shared.hub, path, &body).await;
                shared.state.invalidate();
                response
            }
        },
    }
}

/// `POST /api/launch|kill|approve|auto-approve|settings` on this hub, for
/// a client of ours or (`peer_api.rs`) a peer relaying one of its own.
pub(super) async fn local_action(
    hub: &mpsc::UnboundedSender<Command>,
    path: &str,
    body: &Map<String, Value>,
) -> Response {
    let string = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    match path {
        "/api/launch" => {
            let asked = ask(hub, |reply| Command::WebLaunch {
                path: string("path").unwrap_or_default(),
                mode: string("permissionMode"),
                resume: string("resume"),
                reply,
            })
            .await;
            match asked {
                Some((status, value)) => json_response(status, value),
                None => hub_gone(),
            }
        }
        "/api/kill" => {
            let id = string("id").unwrap_or_default();
            match ask(hub, |reply| Command::WebKill { id, reply }).await {
                Some(true) => json_response(200, json!({"ok": true})),
                Some(false) => json_response(404, json!({"error": "no such session"})),
                None => hub_gone(),
            }
        }
        "/api/approve" => {
            let (Some(id), Some(allow)) = (string("id"), body.get("allow").and_then(Value::as_bool)) else {
                return json_response(400, json!({"error": "approve needs an id and allow"}));
            };
            match ask(hub, |reply| Command::Approve { id, allow, reply }).await {
                Some(true) => json_response(200, json!({"ok": true})),
                Some(false) => json_response(404, json!({"error": "no such approval"})),
                None => hub_gone(),
            }
        }
        "/api/auto-approve" => {
            let session_id = string("sessionId").filter(|id| crate::web::state::is_session_id(id));
            let rule = string("rule").map(|r| Rule::parse(&r, SystemTime::now()));
            let (Some(session_id), Some(Ok(rule))) = (session_id, rule) else {
                return json_response(
                    400,
                    json!({"error": "auto-approve needs a sessionId (a UUID) and a rule: 5m, session, or off"}),
                );
            };
            match ask(hub, |reply| Command::AutoApprove { session_id, rule, reply }).await {
                Some(()) => json_response(200, json!({"ok": true})),
                None => hub_gone(),
            }
        }
        "/api/settings" => {
            let Some(mode) =
                string("defaultPermissionMode").filter(|m| HubConfig::permission_modes().contains(&m.as_str()))
            else {
                return json_response(400, json!({"error": "unknown permission mode"}));
            };
            match ask(hub, |reply| Command::WebSettings { mode, reply }).await {
                Some(()) => json_response(200, json!({"ok": true})),
                None => hub_gone(),
            }
        }
        _ => text(404, "not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_as_the_swift_hub_read_them() {
        let q = parse_query("id=abc&rows=24&claim=0&&k=%41b+c&bad%zz=1&empty&v=%zz");
        assert_eq!(q["id"], "abc");
        assert_eq!(q["rows"], "24");
        assert_eq!(q["claim"], "0");
        assert_eq!(q["k"], "Ab+c", "plus is not a space");
        assert_eq!(q["empty"], "");
        assert_eq!(q["v"], "", "a value that doesn't decode is empty");
        assert!(!q.keys().any(|k| k.starts_with("bad")), "a key that doesn't decode is skipped");
    }
}
