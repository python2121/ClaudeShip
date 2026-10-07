//! The peer side of proxying: a client's action or terminal that another
//! hub of the swarm relays to this one (`proxy.rs` there). The gate in
//! `router.rs` has checked the Host header and the bearer; a POST is JSON
//! with no foreign Origin; the terminal's upgrade needs no Origin (a peer
//! is not a browser).
//!
//! **Local only**, like `peer.rs`: the state is `LocalOnly` (the hub's
//! command channel, the state cache, the book — no peer client), so what a
//! peer asks for happens here and is never forwarded again. A request that
//! names a `host` is refused (400) rather than read as "local".
//!
//! | Route | |
//! |---|---|
//! | `POST /peer/api/launch`, `kill`, `settings`, `approve`, `auto-approve` | the same body and answer as `/api/…` |
//! | `GET /peer/ws/term?id&rows&cols&claim` | the same attachment as `/ws/term` |
//! | `GET\|POST /peer/api/jobs…` | the same as `/api/jobs…` (`jobs.rs`), run here |

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{Method, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::json;

use super::peer::LocalOnly;
use super::router::{MAX_MESSAGE, attach_from, gate_of, json_response, local_action, read_json_object, text};
use super::{ConnectionSlot, Patience, ws};
use std::sync::Arc;
use tokio::sync::watch;

pub fn router<S: Clone + Send + Sync + 'static>(local: LocalOnly) -> Router<S> {
    Router::new()
        .route("/peer/api/launch", any(api))
        .route("/peer/api/kill", any(api))
        .route("/peer/api/settings", any(api))
        .route("/peer/api/approve", any(api))
        .route("/peer/api/auto-approve", any(api))
        .route("/peer/api/jobs", any(jobs_api))
        .route("/peer/api/jobs/{*rest}", any(jobs_api))
        .route("/peer/ws/term", any(terminal))
        .with_state(local)
}

fn names_a_host() -> Response {
    json_response(400, json!({"error": "a peer's request can't name a host"}))
}

async fn api(State(local): State<LocalOnly>, request: Request) -> Response {
    if request.method() != Method::POST {
        return text(405, "method not allowed");
    }
    let path = gate_of(&request).path;
    let Some(body) = read_json_object(request).await else {
        return json_response(400, json!({"error": "bad request"}));
    };
    if body.contains_key("host") {
        return names_a_host();
    }
    let action = path.strip_prefix("/peer").unwrap_or(&path);
    let response = local_action(&local.hub, action, &body).await;
    // The relaying hub fetches our state right after: let it be fresh.
    local.state.invalidate();
    response
}

/// A peer's relayed jobs request: run here, never forwarded (a `host` in
/// the body or the query is refused).
async fn jobs_api(State(local): State<LocalOnly>, request: Request) -> Response {
    let gate = gate_of(&request);
    let method = request.method().clone();
    let patience = request.extensions().get::<Patience>().cloned();
    if gate.query.contains_key("host") {
        return names_a_host();
    }
    let body = match method {
        Method::GET => None,
        Method::POST => {
            let Some(body) = read_json_object(request).await else {
                return json_response(400, json!({"error": "bad request"}));
            };
            if body.contains_key("host") {
                return names_a_host();
            }
            Some(body)
        }
        _ => return text(405, "method not allowed"),
    };
    let rest = gate.path.strip_prefix("/peer/api/jobs").unwrap_or("");
    let id = local.book.id();
    super::jobs::local(&local.jobs, &id, &method, rest, &gate.query, body, patience.as_ref()).await
}

async fn terminal(State(local): State<LocalOnly>, request: Request) -> Response {
    let gate = gate_of(&request);
    let upgrade = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    if !upgrade || request.method() != Method::GET {
        return text(405, "method not allowed");
    }
    if gate.query.contains_key("host") {
        return names_a_host();
    }
    // Admitted under the swarm secret as it is now: subscribed, then the
    // bearer checked again (the gate's check came before), so a new secret
    // at any point from here on hangs this terminal up — a peer the swarm
    // has moved on from keeps no session open here.
    let mut swarm_epoch = local.book.secret_epoch();
    let still_a_peer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .is_some_and(|(_, secret)| local.book.check(secret.trim()));
    if !still_a_peer {
        return json_response(401, json!({"error": "not a peer"}));
    }
    let slot = request.extensions().get::<Arc<ConnectionSlot>>().cloned();
    let (mut parts, _) = request.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let attach = attach_from(&gate.query);
    // One stop signal for `run_on`: a new pairing secret (as every web
    // connection) or a new swarm secret. The watcher ends with the terminal.
    let mut pairing_epoch = gate.epoch.clone();
    let (stop, epoch) = watch::channel(0u64);
    tokio::spawn(async move {
        tokio::select! {
            _ = pairing_epoch.changed() => {}
            _ = swarm_epoch.changed() => {}
            _ = stop.closed() => return,
        }
        let _ = stop.send(1);
    });
    let hub = local.hub.clone();
    upgrade
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| ws::run_on(socket, hub, attach, slot, epoch))
}
