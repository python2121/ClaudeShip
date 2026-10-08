//! The listener and the connection gate. A manual accept loop on a
//! dual-stack socket, because the gate needs both ends' addresses — which
//! of our addresses the peer reached is what tells the tailnet from a LAN
//! that happens to use the same range — and axum alone never sees them.
//! Each admitted connection is served by hyper's HTTP/1 machinery with the
//! router as its service, upgrades enabled for the terminal's WebSocket.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::extract::Request;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

use super::security::{describe, is_allowed_pair};
use super::{ConnectionSlot, Patience, Shared, router};
use crate::hub::log;
use crate::net;

/// Each connection is a descriptor the hub also needs for ptys.
pub const MAX_CONNECTIONS: usize = 128;
/// A plain request not answered in this long is holding one of a limited
/// number of slots (they take milliseconds); it is dropped — unless it is
/// a job's long-poll, which asks for its wait's worth more (`Patience`).
const UNANSWERED: Duration = Duration::from_secs(15);
/// The most a request line and headers may take (hyper's floor is 8 KB).
const MAX_HEAD: usize = 64 * 1024;
const LISTEN_RETRY: Duration = Duration::from_secs(10);

/// One socket for both families: IPv6 with v6only off takes IPv4 too
/// (as `::ffff:a.b.c.d`, which the gate canonicalizes). Where there is no
/// IPv6 at all, IPv4 alone.
fn listen(port: u16) -> std::io::Result<TcpListener> {
    let socket = match Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)) {
        Ok(socket) => {
            socket.set_only_v6(false)?;
            socket.set_reuse_address(true)?;
            socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
            socket
        }
        Err(_) => {
            let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
            socket.set_reuse_address(true)?;
            socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
            socket
        }
    };
    socket.listen(32)?;
    socket.set_nonblocking(true)?;
    TcpListener::from_std(socket.into())
}

/// Listen (retrying every 10 s while the port is taken, saying so once)
/// and serve until the hub exits.
pub async fn run(shared: Arc<Shared>) {
    let port = shared.port;
    let mut said = false;
    let listener = loop {
        match listen(port) {
            Ok(listener) => break listener,
            Err(e) => {
                if !said {
                    log(&format!(
                        "web: cannot listen on port {port}: {e}; retrying quietly"
                    ));
                    said = true;
                }
                tokio::time::sleep(LISTEN_RETRY).await;
            }
        }
    };
    shared.listening.store(true, Ordering::Relaxed);
    log(&format!("web: listening on port {port}"));
    let mut last_refusal: Option<Instant> = None;
    loop {
        let (stream, remote) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                // Out of descriptors, most likely: don't spin.
                log(&format!("web: accept: {e}"));
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if shared.connections.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            continue;
        }
        // The gate: who it is and which of our addresses they reached.
        let local = stream.local_addr().ok();
        let tunnel = net::tunnel_addresses(&shared.tunnel_interfaces);
        if !local.is_some_and(|local| is_allowed_pair(local.ip(), remote.ip(), &tunnel)) {
            // Said once a minute, not per attempt: a refusal is the one
            // thing a person debugging "can't connect" must see.
            if last_refusal.is_none_or(|at| at.elapsed() > Duration::from_secs(60)) {
                last_refusal = Some(Instant::now());
                log(&format!(
                    "web: refused connection from {} to {} (not loopback-to-loopback or tailnet-to-tunnel)",
                    describe(remote.ip()),
                    local.map_or("?".into(), |l| describe(l.ip())),
                ));
            }
            continue;
        }
        let socket = SockRef::from(&stream);
        let _ = socket.set_tcp_nodelay(true);
        // A phone that walks out of range never says goodbye; keepalive is
        // what eventually notices and frees its attachment.
        let keepalive = TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10))
            .with_retries(4);
        let _ = socket.set_tcp_keepalive(&keepalive);
        let slot = Arc::new(ConnectionSlot::take(&shared));
        tokio::spawn(serve(stream, shared.clone(), slot));
    }
}

/// One connection: a single request (every response says
/// `Connection: close`), or a WebSocket upgrade that outlives this task —
/// the terminal holds its own reference to the connection's slot.
async fn serve(stream: TcpStream, shared: Arc<Shared>, slot: Arc<ConnectionSlot>) {
    let answered = Arc::new(AtomicBool::new(false));
    let patience = Patience::default();
    let router = router::router(shared.clone());
    let service = {
        let answered = answered.clone();
        let slot = slot.clone();
        let patience = patience.clone();
        tower::service_fn(move |mut request: Request<Incoming>| {
            request.extensions_mut().insert(slot.clone());
            request.extensions_mut().insert(patience.clone());
            let router = router.clone();
            let answered = answered.clone();
            async move {
                let response = router.oneshot(request).await;
                answered.store(true, Ordering::Relaxed);
                response
            }
        })
    };
    // One request per connection comes from the router's `Connection: close`
    // on every answer (hyper closes after a response that says so), not
    // from `keep_alive(false)`: that makes hyper append `close` to the 101
    // too (`Connection: upgrade, close`), and Apple's URLSession — the
    // phone's WebSocket — rejects such a handshake as a bad response.
    // Browsers tolerate it, which is why only the phone's terminal broke.
    let connection = hyper::server::conn::http1::Builder::new()
        // A request's head past this is refused (431), as the Swift hub's.
        .max_buf_size(MAX_HEAD)
        .serve_connection(TokioIo::new(stream), TowerToHyperService::new(service))
        .with_upgrades();
    tokio::pin!(connection);
    let mut epoch = shared.epoch();
    let deadline = tokio::time::sleep(UNANSWERED);
    tokio::pin!(deadline);
    let mut checked = false;
    loop {
        tokio::select! {
            _ = connection.as_mut() => break,
            // A request that hasn't been answered yet; one that is
            // mid-response (a page's files over a thin link) is left to
            // finish.
            // A job's long-poll asked for more time first (once).
            _ = &mut deadline, if !checked => {
                if !answered.load(Ordering::Relaxed) {
                    let extra = patience.take();
                    if extra == 0 {
                        break;
                    }
                    deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(extra));
                    continue;
                }
                checked = true;
            }
            // The pairing secret changed: everyone out.
            _ = epoch.changed() => break,
        }
    }
    drop(slot);
}
