//! The `/peer/*` routes: what one hub of a swarm asks another. The gate in
//! `router.rs` has already checked the Host header and the bearer (the
//! swarm secret, in constant time), and that a POST is JSON with no foreign
//! Origin; the network gate in `server.rs` is the same as for everything.
//!
//! **Local only.** These handlers answer from this hub's own state and
//! never contact a peer: their state is `LocalOnly`, which holds the hub's
//! command channel, the state cache, and the swarm's `Book` — and nothing
//! that can open a connection (the peer client lives in `Swarm`, which
//! only client-facing handlers and the poller get). A request from a peer
//! therefore can't fan out, loop, or be relayed onwards.
//!
//! | Route | |
//! |---|---|
//! | `POST /peer/hello {record}` | record the newcomer → `{record: ours, peers}`; 410 if it was unpaired |
//! | `POST /peer/rotate {newSecret, peers}` | the bearer is the old secret → `{ok}` |
//! | `GET /peer/state` | the local `/api/state` (same 1 s cache) plus `record` and `peers` |

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::Method;
use axum::response::Response;
use axum::routing::any;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::router::{json_response, read_json_object, text};
use super::state;
use crate::hub::Command;
use crate::swarm::{Book, PeerRecord, is_secret, now_ms};

/// Everything a peer's request may use. No peer client here, on purpose.
#[derive(Clone)]
pub struct LocalOnly {
    pub hub: mpsc::UnboundedSender<Command>,
    pub state: Arc<state::Cache>,
    pub book: Arc<Book>,
}

pub fn router<S: Clone + Send + Sync + 'static>(local: LocalOnly) -> Router<S> {
    Router::new()
        .route("/peer/hello", any(hello))
        .route("/peer/rotate", any(rotate))
        .route("/peer/state", any(peer_state))
        .with_state(local)
}

fn records_value(book: &Book) -> Value {
    Value::Array(book.records().iter().map(PeerRecord::to_value).collect())
}

async fn hello(State(local): State<LocalOnly>, request: Request) -> Response {
    if request.method() != Method::POST {
        return text(405, "method not allowed");
    }
    let Some(body) = read_json_object(request).await else {
        return json_response(400, json!({"error": "bad request"}));
    };
    let Some(mut record) = PeerRecord::from_value(body.get("record")) else {
        return json_response(400, json!({"error": "hello needs a record"}));
    };
    if record.id == local.book.id() {
        return json_response(400, json!({"error": "that is this hub"}));
    }
    if record.is_tombstoned() {
        return json_response(400, json!({"error": "a hello can't be a tombstone"}));
    }
    // We hear it now, by our clock.
    record.last_seen = now_ms();
    local.book.merge(vec![record.clone()]);
    if local.book.members().iter().all(|r| r.id != record.id) {
        // Tombstoned here: it was unpaired, and stays out.
        return json_response(410, json!({"error": "this hub was unpaired from the swarm"}));
    }
    json_response(
        200,
        json!({"record": local.book.me().to_value(), "peers": records_value(&local.book)}),
    )
}

async fn rotate(State(local): State<LocalOnly>, request: Request) -> Response {
    if request.method() != Method::POST {
        return text(405, "method not allowed");
    }
    let Some(body) = read_json_object(request).await else {
        return json_response(400, json!({"error": "bad request"}));
    };
    let Some(secret) = body
        .get("newSecret")
        .and_then(Value::as_str)
        .filter(|s| is_secret(s))
    else {
        return json_response(400, json!({"error": "rotate needs a newSecret"}));
    };
    local.book.adopt_secret(secret);
    local.book.merge(PeerRecord::list_from(body.get("peers")));
    json_response(200, json!({"ok": true}))
}

async fn peer_state(State(local): State<LocalOnly>, request: Request) -> Response {
    if request.method() != Method::GET {
        return text(405, "method not allowed");
    }
    let bytes = local.state.get(&local.hub).await;
    let mut object: Map<String, Value> = match serde_json::from_slice(&bytes) {
        Ok(Value::Object(object)) => object,
        _ => Map::new(),
    };
    object.insert("record".into(), local.book.me().to_value());
    object.insert("peers".into(), records_value(&local.book));
    json_response(200, Value::Object(object))
}
