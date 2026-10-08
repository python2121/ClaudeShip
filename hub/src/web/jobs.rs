//! The jobs routes (plan phase 11; the engine is `crate::jobs`), shared by
//! a client's `/api/jobs…` handled here and a peer's `/peer/api/jobs…`.
//!
//! | Route | |
//! |---|---|
//! | `POST /api/jobs {host?, cwd?, argv \| prompt, permissionMode?, resume?, maxSeconds?, env?}` | `{id, host}` |
//! | `GET /api/jobs?host=` | `{host, jobs: [summary]}` |
//! | `GET /api/jobs/<id>?host=&wait=<s>&since=<n>&consume=1` | the job, stdout from byte `since`; `wait` long-polls up to 60 s |
//! | `POST /api/jobs/<id>/kill {host?}` | `{ok}` |
//!
//! With jobs off (`config.jobs`, switched live by `POST /api/settings
//! {jobs}`) every route here answers 403 `{error: "jobs
//! disabled on this host"}` — on the hub that would run the job: a home
//! hub relays a request naming another host whatever its own switch.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::Method;
use axum::response::Response;
use serde_json::{Map, Value, json};

use super::Patience;
use super::router::{json_response, text};
use crate::jobs::{Jobs, MAX_WAIT, parse_spec};

pub fn disabled() -> Response {
    json_response(403, json!({"error": "jobs disabled on this host"}))
}

/// `wait` from a query: seconds, at most 60.
pub fn wait_seconds(query: &HashMap<String, String>) -> u64 {
    query
        .get("wait")
        .and_then(|w| w.parse::<f64>().ok())
        .filter(|w| w.is_finite() && *w > 0.0)
        .map_or(0, |w| (w.ceil() as u64).min(MAX_WAIT))
}

/// A jobs request on this hub. `rest` is the path after `/api/jobs`;
/// `host` this hub's id (for the answers); `body` a POST's.
pub async fn local(
    jobs: &Arc<Jobs>,
    host: &str,
    method: &Method,
    rest: &str,
    query: &HashMap<String, String>,
    body: Option<Map<String, Value>>,
    patience: Option<&Patience>,
) -> Response {
    if !jobs.enabled() {
        return disabled();
    }
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    match (method, parts.as_slice()) {
        (&Method::POST, []) => {
            let body = body.unwrap_or_default();
            let home = crate::config::home_dir().to_string_lossy().into_owned();
            let program = crate::paths::program().to_string_lossy().into_owned();
            let spec = match parse_spec(&body, jobs.root(), &home, jobs.ceiling, &program) {
                Ok(spec) => spec,
                Err((status, value)) => return json_response(status, value),
            };
            match jobs.start(spec) {
                Ok(id) => json_response(200, json!({"id": id, "host": host})),
                Err((status, value)) => json_response(status, value),
            }
        }
        (&Method::GET, []) => json_response(200, json!({"host": host, "jobs": jobs.list()})),
        (&Method::GET, [id]) => {
            let wait = wait_seconds(query);
            if let Some(patience) = patience {
                patience.extend(wait + 10);
            }
            let since = query.get("since").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let consume = matches!(query.get("consume").map(String::as_str), Some("1" | "true"));
            // Asked before the wait: a job that goes away while waiting is
            // still answered.
            let Some(mut job) = jobs.get(id, since, Duration::from_secs(wait), consume).await else {
                return json_response(404, json!({"error": "no such job"}));
            };
            job.insert("host".into(), host.into());
            job.insert("waitingFor".into(), waiting_for(jobs, id).await.into());
            json_response(200, Value::Object(job))
        }
        (&Method::POST, [id, "kill"]) => {
            if jobs.kill(id) {
                json_response(200, json!({"ok": true}))
            } else {
                json_response(404, json!({"error": "no such job"}))
            }
        }
        (&Method::GET | &Method::POST, _) => json_response(404, json!({"error": "no such job route"})),
        _ => text(405, "method not allowed"),
    }
}

/// What the job's Claude is waiting for, when it has registered a session
/// (matched by its conversation id) and the registry says it waits: its
/// `waitingFor`, else "permission".
async fn waiting_for(jobs: &Jobs, id: &str) -> Option<String> {
    let conversation = jobs.conversation(id)?;
    tokio::task::spawn_blocking(move || {
        claude_sessions::read_live_entries()
            .into_iter()
            .find(|e| e.session_id.as_deref() == Some(conversation.as_str()))
            .filter(|e| e.status.as_deref() == Some("waiting"))
            .map(|e| e.waiting_for.unwrap_or_else(|| "permission".into()))
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    #[test]
    fn waits_are_capped() {
        let q = |w: &str| std::collections::HashMap::from([("wait".to_string(), w.to_string())]);
        assert_eq!(super::wait_seconds(&q("5")), 5);
        assert_eq!(super::wait_seconds(&q("0.5")), 1);
        assert_eq!(super::wait_seconds(&q("600")), 60);
        assert_eq!(super::wait_seconds(&q("-1")), 0);
        assert_eq!(super::wait_seconds(&q("x")), 0);
        assert_eq!(super::wait_seconds(&Default::default()), 0);
    }
}
