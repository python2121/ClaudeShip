//! The hub's HTTP client for talking to peers, hand-rolled over a tokio
//! `TcpStream`: one request per connection (`Connection: close`), the
//! answer read to EOF (bounded) — no pool, no proxy, no redirects, no TLS
//! (the tailnet is the transport). Every request carries the swarm secret
//! as a bearer token and is counted (`sent`, shown in `hub status --json`
//! as `peerRequests`), which is how the tests prove a peer's request never
//! makes us call a peer. (hyper's client side would need a dependency
//! version newer than this workspace takes; the hub answers every request
//! with a `Content-Length` and a close, so a reader this small is enough.)

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::token::COOKIE_NAME;

/// The largest answer read from a peer (a big directory is a few hundred KB).
const MAX_ANSWER: usize = 16 << 20;

#[derive(Debug)]
pub enum PeerError {
    /// Not an `ip:port`.
    Address,
    /// Couldn't connect, or the connection failed mid-request.
    Io(String),
    Timeout,
    /// An answer that isn't JSON.
    Body,
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerError::Address => write!(f, "not a reachable tailnet ip:port address"),
            PeerError::Io(e) => write!(f, "{e}"),
            PeerError::Timeout => write!(f, "timed out"),
            PeerError::Body => write!(f, "the answer was not JSON"),
        }
    }
}

/// The peer client. Cheap to clone (the counter is shared).
#[derive(Clone, Default)]
pub struct PeerClient {
    sent: Arc<AtomicU64>,
}

impl PeerClient {
    /// How many peer requests (and connections) this hub has made.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// A TCP connection to a peer, counted. Part B's WebSocket relay starts
    /// here (then speaks HTTP/1 + the upgrade over it, bearer included).
    pub async fn connect(&self, address: &str) -> Result<TcpStream, PeerError> {
        let target: SocketAddr = address.parse().map_err(|_| PeerError::Address)?;
        // Only a tailnet address with Tailscale up (or loopback under the
        // test knob): the request carries the swarm secret.
        if !super::is_dialable(target) {
            return Err(PeerError::Address);
        }
        self.sent.fetch_add(1, Ordering::Relaxed);
        let stream = TcpStream::connect(target)
            .await
            .map_err(|e| PeerError::Io(e.to_string()))?;
        let _ = stream.set_nodelay(true);
        Ok(stream)
    }

    /// One request to `address` (`ip:port`), with the bearer `secret` and
    /// an optional JSON body, answered within `timeout`: the status and the
    /// body as JSON (`Null` for an empty one).
    pub async fn request(
        &self,
        address: &str,
        method: &str,
        path: &str,
        secret: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<(u16, Value), PeerError> {
        tokio::time::timeout(timeout, self.request_inner(address, method, path, secret, body))
            .await
            .map_err(|_| PeerError::Timeout)?
    }

    async fn request_inner(
        &self,
        address: &str,
        method: &str,
        path: &str,
        secret: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), PeerError> {
        let stream = self.connect(address).await?;
        Self::exchange(stream, address, method, path, secret, body).await
    }

    /// `request` over a connection `connect` made: for a caller that must
    /// know whether the request may have reached the peer (from here on,
    /// it may have) — the proxy's forwarded POSTs, which must not run twice.
    pub async fn exchange(
        stream: TcpStream,
        address: &str,
        method: &str,
        path: &str,
        secret: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), PeerError> {
        let auth = format!("Authorization: Bearer {secret}\r\n");
        let Answer { status, body, .. } = Self::send(stream, address, method, path, &auth, body).await?;
        let value = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).map_err(|_| PeerError::Body)?
        };
        Ok((status, value))
    }

    /// One request to a hub that is not (yet) a peer — enrolling it from
    /// here (`Swarm::invite`) — with its pairing `cookie` rather than the
    /// swarm secret, answered within `timeout`: the whole answer, headers
    /// included (the pairing link's 303 carries the cookie). Counted and
    /// held to dialable addresses like every other request.
    pub async fn visit(
        &self,
        address: &str,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Answer, PeerError> {
        // Only a cookie as a hub mints it: nothing that could end the line.
        if cookie.is_some_and(|c| !c.bytes().all(|b| b.is_ascii_alphanumeric())) {
            return Err(PeerError::Body);
        }
        let auth = cookie
            .map(|c| format!("Cookie: {COOKIE_NAME}={c}\r\n"))
            .unwrap_or_default();
        let attempt = async {
            let stream = self.connect(address).await?;
            Self::send(stream, address, method, path, &auth, body).await
        };
        tokio::time::timeout(timeout, attempt)
            .await
            .map_err(|_| PeerError::Timeout)?
    }

    /// Write one request (`auth`: its credential header line, CRLF-ended)
    /// and read the answer to EOF.
    async fn send(
        mut stream: TcpStream,
        address: &str,
        method: &str,
        path: &str,
        auth: &str,
        body: Option<&Value>,
    ) -> Result<Answer, PeerError> {
        let io = |e: std::io::Error| PeerError::Io(e.to_string());
        let payload = body.map(Value::to_string).unwrap_or_default();
        // `SocketAddr` prints an IPv6 literal bracketed: what the peer's
        // Host check takes.
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{auth}\
             Connection: close\r\nContent-Length: {}\r\n",
            payload.len()
        );
        if body.is_some() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await.map_err(io)?;
        stream.write_all(payload.as_bytes()).await.map_err(io)?;
        let mut raw = Vec::new();
        (&mut stream)
            .take(MAX_ANSWER as u64 + 64 * 1024)
            .read_to_end(&mut raw)
            .await
            .map_err(io)?;
        parse_response(&raw).ok_or(PeerError::Body)
    }
}

/// An HTTP/1 response.
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    /// Names lowercased, values trimmed.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// An HTTP/1 response read to EOF: its status, headers, and body (by
/// `Content-Length`, chunked, or to the end). Shared with `hub pair`.
pub fn parse_response(raw: &[u8]) -> Option<Answer> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let rest = &raw[end + 4..];
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let mut length: Option<usize> = None;
    let mut chunked = false;
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        headers.push((name.to_ascii_lowercase(), value.to_string()));
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse().ok()?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }
    if chunked {
        let mut body = Vec::new();
        let mut at = rest;
        loop {
            let line_end = at.windows(2).position(|w| w == b"\r\n")?;
            let size_text = std::str::from_utf8(&at[..line_end]).ok()?;
            let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
            at = &at[line_end + 2..];
            if size == 0 {
                return Some(Answer { status, headers, body });
            }
            body.extend_from_slice(at.get(..size)?);
            at = at.get(size + 2..)?;
        }
    }
    let body = match length {
        Some(n) => rest.get(..n)?.to_vec(),
        None => rest.to_vec(),
    };
    Some(Answer { status, headers, body })
}

#[cfg(test)]
mod tests {
    use super::parse_response;

    fn short(raw: &[u8]) -> (u16, Vec<u8>) {
        let a = parse_response(raw).unwrap();
        (a.status, a.body)
    }

    #[test]
    fn responses() {
        let a = parse_response(b"HTTP/1.1 303 See Other\r\nSet-Cookie: c=1; Path=/\r\ncontent-length: 2\r\n\r\n{}trailing").unwrap();
        assert_eq!((a.status, a.body.as_slice()), (303, b"{}".as_slice()));
        assert_eq!(a.header("set-cookie"), Some("c=1; Path=/"));
        let r = short(b"HTTP/1.1 401 Unauthorized\r\n\r\nabc");
        assert_eq!(r, (401, b"abc".to_vec()));
        let r = short(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{\"\r\n3;x\r\na\":\r\n2\r\n1}\r\n0\r\n\r\n");
        assert_eq!(r, (200, b"{\"a\":1}".to_vec()));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 9\r\n\r\nshort").is_none());
        assert!(parse_response(b"garbage").is_none());
    }
}
