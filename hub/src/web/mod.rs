//! The hub's web face: the project directory page, its JSON API, and one
//! WebSocket per browser terminal.
//!
//! `server` is the listener and the connection gate, `router` the HTTP
//! routes, `ws` a browser terminal's attachment, `assets` the page,
//! `state` the directory (`GET /api/state`), `security` the rules behind
//! the gate. Everything that touches a session goes through the hub task
//! as a `Command`, like a terminal client's frames do.

pub mod assets;
pub mod jobs;
pub mod peer;
pub mod peer_api;
pub mod proxy;
pub mod router;
pub mod security;
pub mod server;
pub mod state;
pub mod ws;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::{mpsc, watch};

use crate::config::HubConfig;
use crate::hub::Command;

/// What the web server's tasks share with each other and the hub.
pub struct Shared {
    pub hub: mpsc::UnboundedSender<Command>,
    /// Read once at start; neither changes while the hub runs.
    pub port: u16,
    pub allowed_hosts: Vec<String>,
    pub tunnel_interfaces: Vec<String>,
    token: RwLock<Option<String>>,
    pub listening: AtomicBool,
    /// Bumped when the pairing secret changes: every web connection, open
    /// terminals included, hangs up when it sees a new value.
    epoch: watch::Sender<u64>,
    /// Open connections (a WebSocket counts until it closes).
    pub connections: AtomicUsize,
    pub state: Arc<state::Cache>,
    /// The swarm: its book (which `/peer/*` gets alone) and the peer client.
    pub swarm: Arc<crate::swarm::Swarm>,
    /// Jobs (`jobs.rs`), which `/peer/api/jobs…` gets too.
    pub jobs: Arc<crate::jobs::Jobs>,
}

impl Shared {
    pub fn new(config: &HubConfig, hub: mpsc::UnboundedSender<Command>) -> Arc<Shared> {
        Arc::new(Shared {
            hub,
            port: config.port,
            allowed_hosts: config.allowed_hosts.clone(),
            tunnel_interfaces: config.tunnel_interfaces.clone(),
            token: RwLock::new(crate::token::load_or_create()),
            listening: AtomicBool::new(false),
            epoch: watch::channel(0).0,
            connections: AtomicUsize::new(0),
            state: Arc::default(),
            swarm: crate::swarm::Swarm::new(config.port),
            jobs: crate::jobs::Jobs::new(config),
        })
    }

    pub fn token(&self) -> Option<String> {
        self.token.read().map(|t| t.clone()).unwrap_or(None)
    }

    /// A new pairing secret: every browser paired so far is out, and every
    /// web connection open now is dropped.
    pub fn rotate_token(&self) -> bool {
        let token = crate::token::create();
        let ok = token.is_some();
        if let Ok(mut slot) = self.token.write() {
            *slot = token;
        }
        self.epoch.send_modify(|e| *e += 1);
        ok
    }

    /// Changes when every web connection must go.
    pub fn epoch(&self) -> watch::Receiver<u64> {
        self.epoch.subscribe()
    }

    pub fn is_listening(&self) -> bool {
        self.listening.load(Ordering::Relaxed)
    }
}

/// How much longer than the usual 15 s a connection's request may go
/// unanswered: a job's long-poll `wait` says so before it starts waiting
/// (`server.rs` sweeps unanswered requests).
#[derive(Clone, Default)]
pub struct Patience(Arc<AtomicU64>);

impl Patience {
    pub fn extend(&self, seconds: u64) {
        self.0.fetch_max(seconds, Ordering::Relaxed);
    }

    pub fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

/// One accepted connection's place under the cap; released when the last
/// holder (the connection task, or the WebSocket it became) lets go.
pub struct ConnectionSlot(Arc<Shared>);

impl ConnectionSlot {
    pub fn take(shared: &Arc<Shared>) -> ConnectionSlot {
        shared.connections.fetch_add(1, Ordering::Relaxed);
        ConnectionSlot(shared.clone())
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::Relaxed);
    }
}
