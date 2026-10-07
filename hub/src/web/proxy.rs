//! Proxying (plan phase 10): a client of this hub acting on a session of
//! another hub in the swarm. `POST /api/launch|kill|settings|approve|
//! auto-approve` with a `host`, and `/ws/term?host=…`, name a host id from
//! `hosts[]`; absent, or this hub's own id, means here.
//!
//! For a peer: its route from the book (404 `no such host` for an unknown
//! or tombstoned id), its recorded protocol against ours (409 `protocol
//! mismatch`, before any connection), then its addresses in order, 3 s
//! each (502 `unreachable` when none answers).
//!
//! - A POST goes to the peer's `/peer/api/<same path>` with the bearer and
//!   the body minus `host`; the first address that takes the connection
//!   settles it, answer or not (the request may have run there, so it is
//!   never sent twice); its status and body come back verbatim (but a
//!   401 — the peer no longer takes our secret — reads as unreachable: to
//!   the client a 401 means *it* isn't paired). The peer's state is then
//!   fetched once, so `hosts[]` shows the change on the next poll instead
//!   of after the next gossip round.
//! - A terminal: the WebSocket handshake to the peer's `/peer/ws/term`
//!   done by hand over the counted raw stream, and only then the client's
//!   upgrade; from there messages are relayed one for one, binary and text
//!   alike, never parsed. Each direction is its own pump holding one
//!   message at a time: a client that stops reading stalls the peer's
//!   socket (whose own queue cap and stall sweep then apply) and is itself
//!   dropped after the same minute `ws.rs` gives a browser, while the
//!   other direction keeps moving. A close on either side closes the
//!   other, with its code.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self as tung};

use super::router::{MAX_MESSAGE, json_response};
use super::{ConnectionSlot, Shared};
use crate::frame::PROTOCOL;
use crate::swarm::client::PeerClient;
use crate::swarm::{Heard, PeerRecord, Route};

/// One address's chance: connecting, and the whole exchange or handshake.
const PER_ADDRESS: Duration = Duration::from_secs(3);
/// The state fetch after a forwarded action.
const REFRESH: Duration = Duration::from_secs(1);
/// The most a peer's handshake answer may take.
const MAX_HEAD: usize = 64 * 1024;
/// As `ws.rs`: a side that takes no message for this long is gone.
const STALL: Duration = Duration::from_secs(60);
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// Why a request naming a host isn't forwarded.
pub enum Refusal {
    NoSuchHost,
    Mismatch { host: String, theirs: u32 },
}

impl Refusal {
    pub fn response(&self) -> Response {
        match self {
            Refusal::NoSuchHost => json_response(404, json!({"error": "no such host"})),
            Refusal::Mismatch { host, theirs } => json_response(
                409,
                json!({"error": "protocol mismatch", "host": host, "theirs": theirs, "ours": PROTOCOL}),
            ),
        }
    }
}

/// Where a request naming `host` goes: `None` here, `Some` a peer.
pub fn target(shared: &Shared, host: Option<&Value>) -> Result<Option<Route>, Refusal> {
    let id = match host {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(id)) => id,
        Some(_) => return Err(Refusal::NoSuchHost),
    };
    if id.is_empty() || *id == shared.swarm.book.id() {
        return Ok(None);
    }
    let route = shared.swarm.book.route(id).ok_or(Refusal::NoSuchHost)?;
    if route.protocol != PROTOCOL {
        return Err(Refusal::Mismatch { host: route.id, theirs: route.protocol });
    }
    Ok(Some(route))
}

fn unreachable(route: &Route) -> Response {
    json_response(502, json!({"error": "unreachable", "host": route.id}))
}

// MARK: POSTs

/// `path` (`/api/…`) with `body` on the peer: its answer, verbatim.
pub async fn forward(shared: &Arc<Shared>, route: Route, path: &str, mut body: Map<String, Value>) -> Response {
    body.remove("host");
    let body = Value::Object(body);
    let peer_path = format!("/peer{path}");
    let client = &shared.swarm.client;
    for address in &route.addresses {
        // An address that can't be connected to never saw the request: the
        // next one may. Once connected, this address settles it, answer or
        // not — the request may have run there (a launch with its answer
        // lost), and a retry elsewhere would run it twice.
        let Ok(Ok(stream)) = tokio::time::timeout(PER_ADDRESS, client.connect(address)).await else {
            continue;
        };
        let answer = tokio::time::timeout(
            PER_ADDRESS,
            PeerClient::exchange(stream, address, "POST", &peer_path, &route.secret, Some(&body)),
        )
        .await;
        let Ok(Ok((status, value))) = answer else {
            return unreachable(&route);
        };
        if status == 401 {
            shared.swarm.book.missed(&route.secret, &route.id, true);
            return unreachable(&route);
        }
        let _ = tokio::time::timeout(REFRESH, refresh(shared, &route, address)).await;
        return json_response(status, value);
    }
    unreachable(&route)
}

/// One `/peer/state` from `address`, folded into the book as a gossip
/// poll's answer is.
async fn refresh(shared: &Shared, route: &Route, address: &str) {
    let answer = shared
        .swarm
        .client
        .request(address, "GET", "/peer/state", &route.secret, None, REFRESH)
        .await;
    let Ok((200, Value::Object(mut state))) = answer else {
        return;
    };
    let record = PeerRecord::from_value(state.get("record"));
    let mut news = PeerRecord::list_from(state.get("peers"));
    state.remove("record");
    state.remove("peers");
    // Someone else at that address now is not this peer (as gossip).
    let same = record.as_ref().is_some_and(|r| r.id == route.id);
    news.extend(record.clone());
    let heard = same.then(|| Heard {
        id: route.id.clone(),
        address: address.to_string(),
        own: record,
        state: Some(Value::Object(state)),
    });
    shared.swarm.book.answered_by(&route.secret, news, heard);
}

// MARK: Terminals

/// `/ws/term?host=<peer>`: connect to the peer first (so a refusal is
/// still an HTTP answer the client can read), then upgrade and relay.
pub async fn terminal(
    shared: Arc<Shared>,
    route: Route,
    query: &HashMap<String, String>,
    upgrade: WebSocketUpgrade,
    slot: Option<Arc<ConnectionSlot>>,
    epoch: watch::Receiver<u64>,
) -> Response {
    let mut target = format!("/peer/ws/term?id={}", encode(query.get("id").map_or("", String::as_str)));
    for key in ["rows", "cols", "claim"] {
        if let Some(value) = query.get(key) {
            target.push_str(&format!("&{key}={}", encode(value)));
        }
    }
    let mut opened = None;
    for address in &route.addresses {
        if let Ok(Ok(found)) = tokio::time::timeout(
            PER_ADDRESS,
            open(&shared, address, &target, &route.secret),
        )
        .await
        {
            opened = Some(found);
            break;
        }
    }
    let Some((stream, leftover)) = opened else {
        return unreachable(&route);
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let peer = WebSocketStream::from_partially_read(stream, leftover, Role::Client, Some(config)).await;
    upgrade.on_upgrade(move |socket| relay(socket, peer, slot, epoch))
}

/// The handshake to the peer: a 101 with the right accept key, and
/// whatever was read past its head (the start of the first frames).
async fn open(shared: &Shared, address: &str, target: &str, secret: &str) -> Result<(TcpStream, Vec<u8>), ()> {
    let mut stream = shared.swarm.client.connect(address).await.map_err(|_| ())?;
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10));
    let _ = SockRef::from(&stream).set_tcp_keepalive(&keepalive);
    let key = generate_key();
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {secret}\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.map_err(|_| ())?;
    let mut raw = Vec::new();
    let end = loop {
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        if raw.len() > MAX_HEAD {
            return Err(());
        }
        let mut buffer = [0u8; 4096];
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => raw.extend_from_slice(&buffer[..n]),
        }
    };
    let head = std::str::from_utf8(&raw[..end]).map_err(|_| ())?;
    let mut lines = head.split("\r\n");
    if lines.next().and_then(|l| l.split(' ').nth(1)) != Some("101") {
        return Err(());
    }
    let accept = derive_accept_key(key.as_bytes());
    let accepted = lines.any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("sec-websocket-accept") && value.trim() == accept
        })
    });
    if !accepted {
        return Err(());
    }
    Ok((stream, raw[end + 4..].to_vec()))
}

/// How a relay ended.
enum End {
    /// The client closed (its frame, for the peer).
    ClientClosed(Option<tung::protocol::CloseFrame>),
    /// The peer closed (its frame, for the client).
    PeerClosed(Option<CloseFrame>),
    /// The client's socket failed, ended, or took nothing for `STALL`.
    ClientGone,
    /// The peer's socket failed, ended, or took nothing for `STALL`.
    PeerGone,
    /// Every web connection must go (a new pairing secret).
    Stop,
}

/// Messages both ways until either side closes (or stalls, or every web
/// connection must go). Each direction is its own pump, so a big paste
/// going up never waits on output coming down (or the other way round):
/// with one loop, a relay blocked writing to a peer that is itself
/// blocked writing to the relay would hold both for the full minute.
async fn relay(
    client: WebSocket,
    peer: WebSocketStream<TcpStream>,
    slot: Option<Arc<ConnectionSlot>>,
    mut epoch: watch::Receiver<u64>,
) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut peer_tx, mut peer_rx) = peer.split();
    let up = async {
        loop {
            let message = match client_rx.next().await {
                Some(Ok(Message::Binary(data))) => tung::Message::Binary(data),
                Some(Ok(Message::Text(body))) => tung::Message::Text(body.as_str().into()),
                Some(Ok(Message::Close(frame))) => {
                    return End::ClientClosed(frame.map(|f| tung::protocol::CloseFrame {
                        code: f.code.into(),
                        reason: f.reason.as_str().into(),
                    }));
                }
                // Protocol pings are answered by the library, hop by hop.
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return End::ClientGone,
            };
            if !matches!(tokio::time::timeout(STALL, peer_tx.send(message)).await, Ok(Ok(()))) {
                return End::PeerGone;
            }
        }
    };
    let down = async {
        loop {
            let message = match peer_rx.next().await {
                Some(Ok(tung::Message::Binary(data))) => Message::Binary(data),
                Some(Ok(tung::Message::Text(body))) => Message::Text(body.as_str().into()),
                Some(Ok(tung::Message::Close(frame))) => {
                    return End::PeerClosed(frame.map(|f| CloseFrame {
                        code: f.code.into(),
                        reason: f.reason.as_str().into(),
                    }));
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return End::PeerGone,
            };
            if !matches!(tokio::time::timeout(STALL, client_tx.send(message)).await, Ok(Ok(()))) {
                return End::ClientGone;
            }
        }
    };
    let end = tokio::select! {
        end = up => end,
        end = down => end,
        _ = epoch.changed() => End::Stop,
    };
    let (Ok(mut client), Ok(mut peer)) = (client_tx.reunite(client_rx), peer_tx.reunite(peer_rx)) else {
        drop(slot);
        return;
    };
    match end {
        End::ClientClosed(frame) => {
            // Flush the library's answer to the client; tell the peer.
            let _ = tokio::time::timeout(CLOSE_GRACE, client.close()).await;
            close_peer(&mut peer, frame).await;
        }
        End::PeerClosed(frame) => {
            // The session ended (or the peer is going): flush the
            // library's answer, then close the client with the peer's code.
            let _ = tokio::time::timeout(CLOSE_GRACE, peer.close(None)).await;
            close_client(&mut client, frame).await;
        }
        End::ClientGone | End::Stop => close_peer(&mut peer, None).await,
        End::PeerGone => close_client(&mut client, None).await,
    }
    drop(slot);
}

/// A close to the peer (the client's frame, if any), and a moment for its
/// answer.
async fn close_peer(peer: &mut WebSocketStream<TcpStream>, frame: Option<tung::protocol::CloseFrame>) {
    let _ = tokio::time::timeout(CLOSE_GRACE, async {
        let _ = peer.close(frame).await;
        while let Some(Ok(_)) = peer.next().await {}
    })
    .await;
}

/// A close to the client: the peer's frame, else a normal close (1000) as
/// `ws.rs` sends one.
async fn close_client(client: &mut WebSocket, frame: Option<CloseFrame>) {
    let frame = frame.unwrap_or(CloseFrame {
        code: 1000,
        reason: "".into(),
    });
    let sent = tokio::time::timeout(CLOSE_GRACE, client.send(Message::Close(Some(frame)))).await;
    if !matches!(sent, Ok(Ok(()))) {
        return;
    }
    let _ = tokio::time::timeout(CLOSE_GRACE, async {
        while let Some(Ok(_)) = client.recv().await {}
    })
    .await;
}

/// Percent-encoding for a query value: unreserved characters as they are.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(super::encode("ab-9_.~"), "ab-9_.~");
        assert_eq!(super::encode("a b&c=%"), "a%20b%26c%3D%25");
    }
}
