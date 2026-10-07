//! The swarm's outgoing side: the poller that keeps every peer's state and
//! the peer lists in step, joining a swarm, and rotating a swarm's secret.
//! Only client-facing handlers (`/api/swarm/join`) and the poller hold a
//! `Swarm`; the `/peer/*` handlers get the `Book` alone.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinSet;

use super::client::{PeerClient, PeerError};
use super::{Book, Heard, PeerRecord, is_dialable, is_secret, now_ms, parse_invite_link};
use crate::frame::PROTOCOL;
use crate::hub::log;

/// How long each step of enrolling another hub (`invite`) may take; the
/// join itself greets every member, so it gets longer.
const INVITE_STEP: Duration = Duration::from_secs(8);
const INVITE_JOIN: Duration = Duration::from_secs(15);
/// The most `invite` takes, all steps together (the route asks the
/// server for that much patience).
pub const INVITE_BUDGET: Duration = Duration::from_secs(8 + 8 + 15);

/// How often every peer is polled.
pub const POLL_EVERY: Duration = Duration::from_secs(2);
/// How long one peer gets to answer (all its addresses together).
pub const PEER_TIMEOUT: Duration = Duration::from_secs(3);
/// Test knob: `CLAUDESHIP_GOSSIP=0` starts no poller (the no-re-forwarding
/// test needs a hub that makes no requests of its own).
pub const GOSSIP_ENV: &str = "CLAUDESHIP_GOSSIP";

/// The book plus the means to talk to peers.
pub struct Swarm {
    pub book: Arc<Book>,
    pub client: PeerClient,
}

/// What one exchange with a peer came to.
enum Outcome {
    Answered { address: String, status: u16, body: Value },
    Failed(PeerError),
}

impl Swarm {
    pub fn new(port: u16) -> Arc<Swarm> {
        Arc::new(Swarm {
            book: Arc::new(Book::load(port)),
            client: PeerClient::default(),
        })
    }

    /// One request to a peer, trying its addresses in order until one
    /// answers at all (any status), within `PEER_TIMEOUT` overall.
    async fn exchange(
        &self,
        addresses: &[String],
        method: &str,
        path: &str,
        secret: &str,
        body: Option<&Value>,
    ) -> Outcome {
        let attempt = async {
            let mut last = PeerError::Address;
            for address in addresses {
                match self
                    .client
                    .request(address, method, path, secret, body, PEER_TIMEOUT)
                    .await
                {
                    Ok((status, body)) => {
                        return Outcome::Answered {
                            address: address.clone(),
                            status,
                            body,
                        };
                    }
                    Err(e) => last = e,
                }
            }
            Outcome::Failed(last)
        };
        tokio::time::timeout(PEER_TIMEOUT, attempt)
            .await
            .unwrap_or(Outcome::Failed(PeerError::Timeout))
    }

    /// The poller: every peer's `/peer/state` every two seconds.
    pub async fn run(self: Arc<Self>) {
        if std::env::var_os(GOSSIP_ENV).is_some_and(|v| v == "0") {
            return;
        }
        let mut ticker = tokio::time::interval(POLL_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut refused_said: HashSet<String> = HashSet::new();
        loop {
            ticker.tick().await;
            self.poll_once(&mut refused_said).await;
        }
    }

    /// Poll every member at once; returns when all have answered or timed out.
    async fn poll_once(self: &Arc<Self>, refused_said: &mut HashSet<String>) {
        let mut polls = JoinSet::new();
        for record in self.book.members() {
            let Some(route) = self.book.route(&record.id) else {
                continue;
            };
            let swarm = self.clone();
            polls.spawn(async move {
                let outcome = swarm
                    .exchange(&route.addresses, "GET", "/peer/state", &route.secret, None)
                    .await;
                (route, outcome)
            });
        }
        while let Some(joined) = polls.join_next().await {
            let Ok((route, outcome)) = joined else { continue };
            match outcome {
                Outcome::Answered {
                    address,
                    status: 200,
                    body: Value::Object(mut state),
                } => {
                    refused_said.remove(&route.id);
                    let record = PeerRecord::from_value(state.get("record"));
                    let mut news = PeerRecord::list_from(state.get("peers"));
                    state.remove("record");
                    state.remove("peers");
                    // Someone else answers at that address now (a hub that
                    // left and came back as a new member): not this peer.
                    let same = record.as_ref().is_some_and(|r| r.id == route.id);
                    news.extend(record.clone());
                    let heard = same.then(|| Heard {
                        id: route.id.clone(),
                        address,
                        own: record,
                        state: Some(Value::Object(state)),
                    });
                    self.book.answered_by(&route.secret, news, heard);
                    if !same {
                        self.book.missed(&route.secret, &route.id, false);
                    }
                }
                Outcome::Answered { status: 401, .. } => {
                    // Taken only if our secret is still the one we asked
                    // with: a rotation that landed mid-poll is no refusal.
                    if self.book.missed(&route.secret, &route.id, true) && refused_said.insert(route.id.clone()) {
                        log(&format!(
                            "swarm: {} ({}) refuses our secret; it must pair again (claudeship hub pair)",
                            route.name, route.id
                        ));
                    }
                }
                _ => {
                    self.book.missed(&route.secret, &route.id, false);
                }
            }
        }
    }

    /// Tell `peers` (by the old `old_secret`) that the swarm's secret is now
    /// `new_secret`, and who is in it. Returns `(id, name, ok)` per peer.
    pub async fn rotate(
        self: &Arc<Self>,
        peers: Vec<PeerRecord>,
        old_secret: &str,
        new_secret: &str,
        everyone: &[PeerRecord],
    ) -> Vec<(String, String, bool)> {
        let body = json!({
            "newSecret": new_secret,
            "peers": everyone.iter().map(PeerRecord::to_value).collect::<Vec<_>>(),
        });
        let mut calls = JoinSet::new();
        for peer in peers {
            let swarm = self.clone();
            let body = body.clone();
            let old = old_secret.to_string();
            calls.spawn(async move {
                let outcome = swarm
                    .exchange(&peer.addresses, "POST", "/peer/rotate", &old, Some(&body))
                    .await;
                let ok = matches!(outcome, Outcome::Answered { status: 200, .. });
                (peer.id, peer.name, ok)
            });
        }
        let mut out = Vec::new();
        while let Some(Ok(result)) = calls.join_next().await {
            out.push(result);
        }
        out
    }

    /// `POST /api/swarm/join`: adopt the swarm `secret` that `incoming`
    /// (a member's `/api/swarm` answer) belongs to. Peers this hub had in
    /// another swarm are moved along (`/peer/rotate`, with the old secret);
    /// then every member gets a `/peer/hello`.
    pub async fn join(self: &Arc<Self>, secret: &str, incoming: Vec<PeerRecord>) -> Result<Value, String> {
        if !is_secret(secret) {
            return Err("not a swarm secret".into());
        }
        let mut me = self.book.me();
        if incoming.iter().any(|r| r.id == me.id && r.is_tombstoned()) {
            // That swarm unpaired an earlier us: join as a new member.
            self.book.renew_id();
            me = self.book.me();
        }
        let incoming: Vec<PeerRecord> = incoming.into_iter().filter(|r| r.id != me.id).collect();
        if incoming.is_empty() {
            return Err("that answer named no other hub".into());
        }
        let old_secret = self.book.secret();
        let mut rotated = Vec::new();
        if old_secret != secret {
            let ids: HashSet<&str> = incoming.iter().map(|r| r.id.as_str()).collect();
            let ours: Vec<PeerRecord> = self
                .book
                .members()
                .into_iter()
                .filter(|r| !ids.contains(r.id.as_str()))
                .collect();
            if !ours.is_empty() {
                let everyone: Vec<PeerRecord> = std::iter::once(me.clone())
                    .chain(incoming.iter().cloned())
                    .chain(self.book.records().into_iter().skip(1))
                    .collect();
                rotated = self.rotate(ours, &old_secret, secret, &everyone).await;
            }
            self.book.adopt_secret(secret);
        }
        self.book.merge(incoming.clone());
        let secret = secret.to_string();
        let mut hellos = JoinSet::new();
        for peer in incoming.into_iter().filter(|r| !r.is_tombstoned()) {
            let swarm = self.clone();
            let body = json!({"record": me.to_value()});
            let secret = secret.clone();
            hellos.spawn(async move {
                let outcome = swarm
                    .exchange(&peer.addresses, "POST", "/peer/hello", &secret, Some(&body))
                    .await;
                (peer, outcome)
            });
        }
        let mut joined = Vec::new();
        while let Some(Ok((peer, outcome))) = hellos.join_next().await {
            let error = match outcome {
                Outcome::Answered {
                    address,
                    status: 200,
                    body,
                } => {
                    let mut news = PeerRecord::list_from(body.get("peers"));
                    let own = PeerRecord::from_value(body.get("record"));
                    news.extend(own.clone());
                    // Heard only if the hub that answered is the one we
                    // greeted (not another now at that address).
                    let heard = own
                        .as_ref()
                        .is_none_or(|o| o.id == peer.id)
                        .then(|| Heard { id: peer.id.clone(), address, own, state: None });
                    self.book.answered_by(&secret, news, heard);
                    None
                }
                Outcome::Answered { status, body, .. } => Some(
                    body.get("error")
                        .and_then(Value::as_str)
                        .map_or(format!("answered {status}"), str::to_string),
                ),
                Outcome::Failed(e) => Some(e.to_string()),
            };
            joined.push(json!({
                "id": peer.id, "name": peer.name, "ok": error.is_none(), "error": error,
            }));
        }
        let rotated: Vec<Value> = rotated
            .into_iter()
            .map(|(id, name, ok)| json!({"id": id, "name": name, "ok": ok}))
            .collect();
        Ok(json!({"ok": true, "id": me.id, "hello": joined, "rotated": rotated}))
    }

    /// `POST /api/swarm/invite {link}`: enrol the hub whose pairing link
    /// this is into our swarm, driven from here — the page's way to add a
    /// computer, since a browser paired with us can't post to another hub.
    /// Pair with it as a browser would (`GET /auth?k=` → the cookie), read
    /// its record (`POST /api/swarm`: name, protocol), then hand it our
    /// swarm (`POST /api/swarm/join {secret, peers}`); it greets every
    /// member, us included. Its record is merged here too, so we know it
    /// even if its greeting didn't reach us. `Ok(name)`, or the status and
    /// error for the client: 400 a bad link, 403 the link's key refused,
    /// 409 another protocol (or no swarm at all), 502 not reached. The key
    /// is never logged or echoed.
    pub async fn invite(self: &Arc<Self>, link: &str) -> Result<Value, (u16, String)> {
        let (address, key) = parse_invite_link(link).map_err(|e| (400, e.to_string()))?;
        if !is_dialable(address) {
            return Err((502, format!("{address} is a Tailscale address, but Tailscale is not up on this computer")));
        }
        let address = address.to_string();
        let unreachable = |e: PeerError| (502, format!("cannot reach {address}: {e}"));
        // 1. Pair with it.
        let answer = self
            .client
            .visit(&address, "GET", &format!("/auth?k={key}"), None, None, INVITE_STEP)
            .await
            .map_err(unreachable)?;
        if answer.status == 403 {
            return Err((403, format!("{address} refused that pairing link (mistyped, or its link was changed)")));
        }
        let cookie = answer
            .header("set-cookie")
            .and_then(|c| c.split(';').next())
            .and_then(|c| c.strip_prefix(&format!("{}=", crate::token::COOKIE_NAME)))
            .filter(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_alphanumeric()))
            .map(str::to_string);
        let Some(cookie) = cookie.filter(|_| answer.status == 303) else {
            return Err((502, format!("{address} did not answer like a ClaudeShip hub (HTTP {})", answer.status)));
        };
        // 2. Who it is.
        let json = |body: &[u8]| serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
        let answer = self
            .client
            .visit(&address, "POST", "/api/swarm", Some(&cookie), Some(&json!({})), INVITE_STEP)
            .await
            .map_err(unreachable)?;
        let swarm = json(&answer.body);
        let record = PeerRecord::from_value(swarm.get("peers").and_then(|p| p.get(0)));
        let Some(record) = record.filter(|_| answer.status == 200) else {
            return Err((409, format!("{address} has no swarm (HTTP {}); it runs an older build — update it", answer.status)));
        };
        let name = if record.name.is_empty() { address.clone() } else { record.name.clone() };
        let me = self.book.me();
        if record.id == me.id {
            return Err((400, "that is this computer's own link".into()));
        }
        if record.protocol != PROTOCOL {
            return Err((409, format!(
                "{name} runs protocol {} and this hub {PROTOCOL}; update and restart the older one first",
                record.protocol
            )));
        }
        // 3. It joins our swarm.
        let secret = self.book.secret();
        let peers: Vec<Value> = self.book.records().iter().map(PeerRecord::to_value).collect();
        let answer = self
            .client
            .visit(
                &address,
                "POST",
                "/api/swarm/join",
                Some(&cookie),
                Some(&json!({"secret": secret, "peers": peers})),
                INVITE_JOIN,
            )
            .await
            .map_err(unreachable)?;
        let joined = json(&answer.body);
        if answer.status != 200 || joined["ok"] != true {
            let why = joined["error"].as_str().map_or(format!("HTTP {}", answer.status), str::to_string);
            return Err((502, format!("{name} could not join: {why}")));
        }
        // Its id may be new (we had unpaired it before).
        let mut record = record;
        if let Some(id) = joined["id"].as_str().filter(|id| crate::web::state::is_session_id(id)) {
            record.id = id.to_lowercase();
        }
        record.last_seen = now_ms();
        self.book.answered_by(&secret, vec![record.clone()], None);
        log(&format!("swarm: enrolled {name} ({}) from a client", record.id));
        Ok(json!({"ok": true, "name": name, "id": record.id}))
    }
}
