//! The swarm: hubs that know each other (plan phase 10). One secret per
//! swarm (`swarm.secret`), shared by every member; each hub's `peers.json`
//! holds its own record and every peer's, tombstones included. Peers talk
//! over the web port's `/peer/*` routes with the secret as a bearer token,
//! behind the same network gate and Host check as everything else.
//!
//! This module is the book — the secret, the records, the merge rules, and
//! each peer's last state — and it never talks to the network: the
//! `/peer/*` handlers get the book and nothing that can reach a peer (see
//! `web::peer::LocalOnly`), so a peer's request can't make us call a peer.
//! The outgoing side is `client` (the HTTP client) and `gossip` (the poller,
//! join, rotation), reached only through `Swarm`, which only client-facing
//! handlers and the poller hold.
//!
//! Merge rules: union by id; of two live records the newer `lastSeen` wins;
//! a tombstone beats a record (whatever its `lastSeen`), and tombstones are
//! kept 7 days, then dropped. A hub that finds its own id tombstoned was
//! unpaired by a peer: it leaves (new id, new secret, no peers), so it
//! stays gone and a later `hub pair` makes it a new member.

pub mod client;
pub mod gossip;

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::frame::PROTOCOL;
use crate::hub::log;
use crate::config::HubConfig;
use crate::web::security::{constant_time_equals, is_loopback, is_tailnet};
use crate::{net, paths};

pub use gossip::{INVITE_BUDGET, Swarm};

/// How long a tombstone is carried before it is forgotten.
pub const TOMBSTONE_KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// What a record says about the binary, beside `protocol`.
pub const BUILD: &str = env!("CARGO_PKG_VERSION");
/// Test knob: advertise `127.0.0.1:<port>` too, so hubs on one machine can
/// peer over loopback (which the gate allows loopback-to-loopback).
pub const ADVERTISE_LOOPBACK_ENV: &str = "CLAUDESHIP_ADVERTISE_LOOPBACK";
/// Persisting `lastSeen` alone (no membership change) at most this often.
const PERSIST_EVERY: Duration = Duration::from_secs(60);
/// Bounds on what a peer may tell us.
const MAX_ADDRESSES: usize = 16;
const MAX_RECORDS: usize = 256;

/// Whether loopback addresses may be advertised and dialed (the test knob).
fn loopback_allowed() -> bool {
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    *ALLOWED.get_or_init(|| std::env::var_os(ADVERTISE_LOOPBACK_ENV).is_some_and(|v| v == "1"))
}

/// The interfaces Tailscale lives on (the config's `tunnelInterfaces`),
/// read once.
fn tunnel_interfaces() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| HubConfig::load(&paths::config()).tunnel_interfaces)
}

/// An address a peer record may name: a tailnet address, or loopback under
/// the test knob. Anything else is refused, or a record could make this
/// hub send the swarm secret to an arbitrary host.
pub fn is_peer_address(address: SocketAddr) -> bool {
    is_tailnet(address.ip()) || (loopback_allowed() && is_loopback(address.ip()))
}

/// Whether this hub may dial `address` now: a peer address, and for a
/// tailnet one, Tailscale is up here (a tunnel interface holds a tailnet
/// address). With it down, 100.64/10 is ordinary CGNAT space routed to
/// whatever network we are on, and the bearer would go to a stranger.
pub fn is_dialable(address: SocketAddr) -> bool {
    if !is_peer_address(address) {
        return false;
    }
    if is_loopback(address.ip()) {
        return true;
    }
    net::tunnel_addresses(tunnel_interfaces())
        .into_iter()
        .any(|ip| is_tailnet(ip) && ip.is_ipv4() == address.is_ipv4())
}

/// A pairing link to enrol another hub from here (`POST /api/swarm/invite`):
/// exactly `http://<ip literal>:<port>/auth?k=<key>` — no DNS name (whoever
/// runs the network's DNS could point one anywhere, and this hub is about
/// to hand over the swarm secret), no other scheme or path, an explicit
/// port, a key of letters and digits. The address must be a peer address
/// (`is_peer_address`: tailnet, or loopback under the test knob); whether
/// it is dialable now is the caller's question.
pub fn parse_invite_link(link: &str) -> Result<(SocketAddr, String), &'static str> {
    const SHAPE: &str = "that is not a pairing link (http://<address>:<port>/auth?k=…, from claudeship hub link)";
    let rest = link.trim().strip_prefix("http://").ok_or(SHAPE)?;
    let (authority, target) = rest.split_once('/').ok_or(SHAPE)?;
    let query = target.strip_prefix("auth?").ok_or(SHAPE)?;
    // The key and nothing else: no second `k`, no other parameter.
    let key = query
        .strip_prefix("k=")
        .filter(|k| !k.is_empty() && k.len() <= 256 && k.bytes().all(|b| b.is_ascii_alphanumeric()))
        .ok_or(SHAPE)?;
    let address: SocketAddr = authority
        .parse()
        .map_err(|_| "the link must name the other computer by its Tailscale IP address and port, not a name")?;
    // A port to dial, and no IPv6 zone (`%en0`): the address must print
    // back as the Host the other hub accepts.
    let zoned = matches!(address, SocketAddr::V6(v6) if v6.scope_id() != 0 || v6.flowinfo() != 0);
    if address.port() == 0 || zoned {
        return Err(SHAPE);
    }
    if !is_peer_address(address) {
        return Err("the link must name the other computer's Tailscale address (100.x.y.z or fd7a:115c:a1e0::…)");
    }
    Ok((address, key.to_string()))
}

/// Text from a peer, for logs and terminals: no control characters.
fn printable(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// One hub as the swarm knows it. `lastSeen` and `tombstone` are epoch ms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerRecord {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `ip:port`, as `SocketAddr` prints it.
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub last_seen: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<u64>,
}

impl PeerRecord {
    /// A record from the wire, trimmed to sane bounds; `None` if its id
    /// isn't a UUID.
    pub fn sanitized(mut self) -> Option<PeerRecord> {
        if !crate::web::state::is_session_id(&self.id) {
            return None;
        }
        self.id = self.id.to_lowercase();
        self.name = printable(&self.name, 255);
        self.build = printable(&self.build, 64);
        self.addresses
            .retain(|a| a.parse::<SocketAddr>().is_ok_and(is_peer_address));
        self.addresses.truncate(MAX_ADDRESSES);
        Some(self)
    }

    /// Records from a JSON array, bad ones skipped.
    pub fn list_from(value: Option<&Value>) -> Vec<PeerRecord> {
        value
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .take(MAX_RECORDS)
                    .filter_map(|v| serde_json::from_value::<PeerRecord>(v.clone()).ok())
                    .filter_map(PeerRecord::sanitized)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn from_value(value: Option<&Value>) -> Option<PeerRecord> {
        serde_json::from_value::<PeerRecord>(value?.clone())
            .ok()?
            .sanitized()
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn is_tombstoned(&self) -> bool {
        self.tombstone.is_some()
    }
}

/// What a merge did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Merged {
    /// A record was added or tombstoned, or a member's addresses, name,
    /// protocol, or build changed: worth writing down.
    pub changed: bool,
    /// The incoming records tombstone us: a peer unpaired this hub.
    pub unpaired_me: bool,
}

/// Fold `incoming` into `known` (by id), by the swarm's rules. `me` is our
/// own id, which is never stored among the peers.
pub fn merge(
    known: &mut BTreeMap<String, PeerRecord>,
    incoming: impl IntoIterator<Item = PeerRecord>,
    me: &str,
) -> Merged {
    let mut out = Merged::default();
    for record in incoming {
        if record.id == me {
            out.unpaired_me |= record.is_tombstoned();
            continue;
        }
        let Some(current) = known.get_mut(&record.id) else {
            if known.len() < MAX_RECORDS {
                known.insert(record.id.clone(), record);
                out.changed = true;
            }
            continue;
        };
        match (current.tombstone, record.tombstone) {
            // Tombstones win; of two, the later one is kept (so it expires
            // last everywhere).
            (Some(a), Some(b)) => {
                if b > a {
                    current.tombstone = Some(b);
                }
            }
            (Some(_), None) => {}
            (None, Some(_)) => {
                *current = record;
                out.changed = true;
            }
            (None, None) => {
                if record.last_seen > current.last_seen {
                    let differs = current.addresses != record.addresses
                        || current.name != record.name
                        || current.protocol != record.protocol
                        || current.build != record.build;
                    *current = record;
                    out.changed |= differs;
                }
            }
        }
    }
    out
}

/// What a peer is heard as: who, at which address, its own record as it
/// sent it, and its state (a poll's).
pub struct Heard {
    pub id: String,
    pub address: String,
    pub own: Option<PeerRecord>,
    pub state: Option<Value>,
}

/// What a peer said of itself, answering us directly, onto our record of
/// it: name, addresses, protocol, build. Whether anything changed.
pub fn take_own(record: &mut PeerRecord, own: PeerRecord) -> bool {
    if own.id != record.id || own.is_tombstoned() || record.is_tombstoned() {
        return false;
    }
    let changed = record.name != own.name
        || record.addresses != own.addresses
        || record.protocol != own.protocol
        || record.build != own.build;
    record.name = own.name;
    record.addresses = own.addresses;
    record.protocol = own.protocol;
    record.build = own.build;
    changed
}

/// The part of `incoming` worth merging at `now_ms` (our clock):
/// - a tombstone already past `TOMBSTONE_KEEP` is dropped, so a peer whose
///   clock lags (or that was away) can't bring back one we forgot — it
///   would evict a peer re-added since, or make us leave a swarm we
///   rejoined;
/// - a `lastSeen` in our future (another machine's clock ahead of ours) is
///   clamped to now, so it can't pin a record against every later update
///   or make a dead peer look recently seen;
/// - a record we don't know whose `lastSeen` is older than
///   `TOMBSTONE_KEEP` is dropped: it may be one whose tombstone we have
///   already forgotten, carried back by a peer that was away all along.
pub fn admissible(
    incoming: Vec<PeerRecord>,
    known: &BTreeMap<String, PeerRecord>,
    now_ms: u64,
) -> Vec<PeerRecord> {
    let keep = TOMBSTONE_KEEP.as_millis() as u64;
    incoming
        .into_iter()
        .filter(|r| match r.tombstone {
            Some(t) => now_ms.saturating_sub(t) < keep,
            None => known.contains_key(&r.id) || now_ms.saturating_sub(r.last_seen) < keep,
        })
        .map(|mut r| {
            r.last_seen = r.last_seen.min(now_ms);
            if let Some(t) = r.tombstone.as_mut() {
                *t = (*t).min(now_ms);
            }
            r
        })
        .collect()
}

/// Forget tombstones older than `TOMBSTONE_KEEP`. Whether any went.
pub fn expire(known: &mut BTreeMap<String, PeerRecord>, now_ms: u64) -> bool {
    let keep = TOMBSTONE_KEEP.as_millis() as u64;
    let before = known.len();
    known.retain(|_, r| r.tombstone.is_none_or(|t| now_ms.saturating_sub(t) < keep));
    known.len() != before
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A random version-4 UUID, lowercase.
pub fn new_id() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // No randomness: a time-and-pid id is still unique enough here.
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            ^ u128::from(std::process::id());
        b = t.to_be_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// A secret as minted: 64 hex characters. Accepted from the wire if it is
/// 32 to 128 hex characters.
pub fn is_secret(text: &str) -> bool {
    (32..=128).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn mint_secret() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Written whole or not at all, owner-only.
fn write_private(path: &Path, data: &[u8]) -> bool {
    paths::ensure_home();
    let mut staging = path.as_os_str().to_owned();
    staging.push(".new");
    let staging = std::path::PathBuf::from(staging);
    let _ = std::fs::remove_file(&staging);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)
        .and_then(|mut f| f.write_all(data));
    if written.is_err() || std::fs::rename(&staging, path).is_err() {
        let _ = std::fs::remove_file(&staging);
        return false;
    }
    true
}

/// What this hub knows of a peer beyond its record: whether the last poll
/// reached it, and the state it answered with.
#[derive(Default, Clone)]
pub struct Live {
    pub reachable: bool,
    /// The peer answered 401: it no longer takes our secret.
    pub refused: bool,
    /// Its `/peer/state` minus `record` and `peers`.
    pub state: Option<Value>,
    /// The address that answered last, tried first next time.
    pub address: Option<String>,
    /// When `state` arrived, to carry its `now` forward.
    pub polled: Option<Instant>,
}

struct Inner {
    me: PeerRecord,
    known: BTreeMap<String, PeerRecord>,
    live: HashMap<String, Live>,
    persisted: Instant,
}

/// Where to reach a peer, for whoever must talk to it (gossip now; part B's
/// proxy too): its addresses (the one that answered last first), the
/// swarm secret, and what it last said its protocol was.
#[derive(Clone, Debug)]
pub struct Route {
    pub id: String,
    pub name: String,
    pub addresses: Vec<String>,
    pub secret: String,
    pub protocol: u32,
}

/// The swarm's local state: no network in here.
pub struct Book {
    port: u16,
    /// What to call this machine in our record (`config.name`, else the
    /// hostname), refreshed with the rest of it.
    name: String,
    advertise_loopback: bool,
    secret: RwLock<String>,
    /// Bumped whenever the secret changes: a peer's terminal admitted
    /// under the old one hangs up (`peer_api.rs`).
    secret_epoch: tokio::sync::watch::Sender<u64>,
    inner: Mutex<Inner>,
}

impl Book {
    /// Load `swarm.secret` and `peers.json` from the hub's home, minting
    /// what is missing (a swarm of one).
    pub fn load(port: u16, name: String) -> Book {
        let secret = std::fs::read_to_string(paths::swarm_secret())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| is_secret(s));
        let secret = match secret {
            Some(secret) => secret,
            None => {
                let minted = mint_secret().unwrap_or_default();
                if minted.is_empty() || !write_private(&paths::swarm_secret(), minted.as_bytes()) {
                    log("swarm: could not write swarm.secret; peers can't authenticate");
                }
                minted
            }
        };
        let saved: Value = std::fs::read(paths::peers())
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or(Value::Null);
        let id = PeerRecord::from_value(saved.get("self"))
            .map(|r| r.id)
            .unwrap_or_else(new_id);
        let mut known = BTreeMap::new();
        // Our own records, however old, are still ours (an unreachable peer
        // stays listed); only expired tombstones and future clocks go.
        let saved_peers = PeerRecord::list_from(saved.get("peers"));
        let ours: BTreeMap<String, PeerRecord> =
            saved_peers.iter().map(|r| (r.id.clone(), r.clone())).collect();
        for record in admissible(saved_peers, &ours, now_ms()) {
            if record.id != id {
                known.insert(record.id.clone(), record);
            }
        }
        expire(&mut known, now_ms());
        let book = Book {
            port,
            name,
            advertise_loopback: std::env::var_os(ADVERTISE_LOOPBACK_ENV).is_some_and(|v| v == "1"),
            secret: RwLock::new(secret),
            secret_epoch: tokio::sync::watch::channel(0).0,
            inner: Mutex::new(Inner {
                me: PeerRecord {
                    id,
                    name: String::new(),
                    addresses: Vec::new(),
                    protocol: PROTOCOL,
                    build: BUILD.into(),
                    last_seen: 0,
                    tombstone: None,
                },
                known,
                live: HashMap::new(),
                persisted: Instant::now(),
            }),
        };
        {
            let mut inner = book.lock();
            book.refresh_me(&mut inner);
            book.persist(&mut inner);
        }
        book
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn secret(&self) -> String {
        self.secret.read().map(|s| s.clone()).unwrap_or_default()
    }

    /// Whether `offered` is the swarm secret, in constant time.
    pub fn check(&self, offered: &str) -> bool {
        let secret = self.secret();
        !secret.is_empty() && constant_time_equals(offered, &secret)
    }

    fn set_secret(&self, secret: String) {
        if !write_private(&paths::swarm_secret(), secret.as_bytes()) {
            log("swarm: could not write swarm.secret");
        }
        if let Ok(mut slot) = self.secret.write() {
            *slot = secret;
        }
        self.secret_epoch.send_modify(|e| *e += 1);
    }

    /// Changes whenever the swarm secret does. Subscribe before checking
    /// a bearer, so a change landing in between still counts.
    pub fn secret_epoch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.secret_epoch.subscribe()
    }

    /// Take the secret a joined swarm (or a rotating peer) gave us. Under
    /// the book's lock, so an answer to a request made with the old secret
    /// (`answered_by`) is either merged before or dropped after.
    pub fn adopt_secret(&self, secret: &str) {
        let _inner = self.lock();
        if self.secret() != secret {
            self.set_secret(secret.to_string());
        }
    }

    fn refresh_me(&self, inner: &mut Inner) {
        let mut addresses: Vec<String> = Vec::new();
        if self.advertise_loopback {
            addresses.push(SocketAddr::from(([127, 0, 0, 1], self.port)).to_string());
        }
        // Only Tailscale's own addresses: a Wi-Fi network handing out
        // 100.64/10 on the LAN puts the same range on a non-tunnel interface.
        for ip in net::tunnel_addresses(tunnel_interfaces()) {
            if is_tailnet(ip) {
                addresses.push(SocketAddr::from((ip, self.port)).to_string());
            }
        }
        inner.me.name = self.name.clone();
        inner.me.addresses = addresses;
        inner.me.protocol = PROTOCOL;
        inner.me.build = BUILD.into();
        inner.me.last_seen = now_ms();
    }

    fn persist(&self, inner: &mut Inner) {
        let data = json!({
            "self": inner.me.to_value(),
            "peers": inner.known.values().map(PeerRecord::to_value).collect::<Vec<_>>(),
        });
        let text = serde_json::to_vec_pretty(&data).unwrap_or_default();
        if !write_private(&paths::peers(), &text) {
            log("swarm: could not write peers.json");
        }
        inner.persisted = Instant::now();
    }

    /// Our own record, addresses and `lastSeen` fresh.
    pub fn me(&self) -> PeerRecord {
        let mut inner = self.lock();
        self.refresh_me(&mut inner);
        inner.me.clone()
    }

    pub fn id(&self) -> String {
        self.lock().me.id.clone()
    }

    /// Ours and every peer's, tombstones included: what we tell a peer.
    pub fn records(&self) -> Vec<PeerRecord> {
        let mut inner = self.lock();
        self.refresh_me(&mut inner);
        std::iter::once(inner.me.clone())
            .chain(inner.known.values().cloned())
            .collect()
    }

    /// The peers to poll: every one not tombstoned.
    pub fn members(&self) -> Vec<PeerRecord> {
        self.lock()
            .known
            .values()
            .filter(|r| !r.is_tombstoned())
            .cloned()
            .collect()
    }

    /// How to reach peer `id` (not tombstoned), if we know it.
    pub fn route(&self, id: &str) -> Option<Route> {
        let inner = self.lock();
        let record = inner.known.get(id).filter(|r| !r.is_tombstoned())?;
        let mut addresses = record.addresses.clone();
        if let Some(last) = inner.live.get(id).and_then(|l| l.address.clone())
            && let Some(at) = addresses.iter().position(|a| *a == last)
        {
            let first = addresses.remove(at);
            addresses.insert(0, first);
        }
        let protocol = inner
            .live
            .get(id)
            .and_then(|l| l.state.as_ref())
            .and_then(|s| s.get("protocol"))
            .and_then(Value::as_u64)
            .map_or(record.protocol, |p| p as u32);
        Some(Route {
            id: record.id.clone(),
            name: record.name.clone(),
            addresses,
            secret: self.secret(),
            protocol,
        })
    }

    /// Fold records a peer sent into ours. If they say we were unpaired,
    /// leave.
    pub fn merge(&self, incoming: Vec<PeerRecord>) -> Merged {
        let mut inner = self.lock();
        self.merge_locked(&mut inner, incoming)
    }

    /// `merge` and `heard` for an answer to a request made with `secret`:
    /// dropped if the swarm's secret has changed since (we left, or
    /// joined another swarm, while it was in flight). `leave` changes the
    /// secret under the same lock, so the two can't interleave.
    /// `heard`: which peer answered, at which address, its own record as
    /// it sent it (taken whatever its `lastSeen`; see `heard_locked`), and
    /// its state.
    pub fn answered_by(&self, secret: &str, incoming: Vec<PeerRecord>, heard: Option<Heard>) {
        let mut inner = self.lock();
        if self.secret() != secret {
            return;
        }
        let merged = self.merge_locked(&mut inner, incoming);
        if merged.unpaired_me {
            return;
        }
        if let Some(heard) = heard {
            self.heard_locked(&mut inner, heard);
        }
    }

    fn merge_locked(&self, inner: &mut Inner, incoming: Vec<PeerRecord>) -> Merged {
        let me = inner.me.id.clone();
        let incoming = admissible(incoming, &inner.known, now_ms());
        let mut merged = merge(&mut inner.known, incoming, &me);
        merged.changed |= expire(&mut inner.known, now_ms());
        let gone: Vec<String> = inner
            .live
            .keys()
            .filter(|id| inner.known.get(*id).is_none_or(PeerRecord::is_tombstoned))
            .cloned()
            .collect();
        for id in gone {
            inner.live.remove(&id);
        }
        if merged.unpaired_me {
            log("swarm: a peer unpaired this hub; leaving the swarm (new id, new secret, no peers)");
            self.leave_locked(inner, true);
        } else if merged.changed {
            self.persist(inner);
        }
        merged
    }

    /// A poll (or hello) reached a peer. What it says of itself — name,
    /// addresses, protocol, build — is taken as it is, whatever its
    /// `lastSeen` (its clock may be behind ours, and our record of it
    /// carries our clock); `lastSeen` becomes our now.
    fn heard_locked(&self, inner: &mut Inner, heard: Heard) {
        let Heard { id, address, own, state } = heard;
        let now = now_ms();
        let Some(record) = inner.known.get_mut(&id).filter(|r| !r.is_tombstoned()) else {
            return;
        };
        let changed = own.is_some_and(|own| take_own(record, own));
        record.last_seen = record.last_seen.max(now);
        let live = inner.live.entry(id).or_default();
        live.reachable = true;
        live.refused = false;
        live.address = Some(address);
        if state.is_some() {
            live.state = state;
            live.polled = Some(Instant::now());
        }
        if changed || inner.persisted.elapsed() > PERSIST_EVERY {
            self.persist(inner);
        }
    }

    /// A poll made with `secret` didn't reach peer `id` (`refused`: it
    /// answered 401). Dropped if the secret has changed since (a rotation
    /// landed while it was in flight). Whether it was taken.
    pub fn missed(&self, secret: &str, id: &str, refused: bool) -> bool {
        let mut inner = self.lock();
        if self.secret() != secret || inner.known.get(id).is_none_or(PeerRecord::is_tombstoned) {
            return false;
        }
        let live = inner.live.entry(id.to_string()).or_default();
        live.reachable = false;
        live.refused = refused;
        true
    }

    /// A new id for this hub (joining a swarm that tombstoned the old one).
    pub fn renew_id(&self) {
        let mut inner = self.lock();
        inner.me.id = new_id();
        self.persist(&mut inner);
    }

    fn leave_locked(&self, inner: &mut Inner, renew_id: bool) {
        if renew_id {
            inner.me.id = new_id();
        }
        inner.known.clear();
        inner.live.clear();
        if let Some(secret) = mint_secret() {
            self.set_secret(secret);
        }
        self.persist(inner);
    }

    /// `hub unlink`: a swarm of one again — a new secret, no peers — told
    /// to nobody, so every old peer is refused until it pairs again. The id
    /// stays (re-paired, we are the same host to everyone); an unpaired
    /// hub's leaving (in `merge`) takes a new one too, so the tombstone of
    /// the old one can't follow it into its next swarm.
    pub fn leave(&self) -> bool {
        let mut inner = self.lock();
        self.leave_locked(&mut inner, false);
        !self.secret().is_empty()
    }

    /// Tombstone the peer whose name (case-insensitive) or id (or id
    /// prefix of 4+ characters) is `target`; gossip carries it.
    pub fn unpair(&self, target: &str) -> Result<PeerRecord, String> {
        let mut inner = self.lock();
        let lower = target.to_lowercase();
        if lower == inner.me.id || (lower.len() >= 4 && inner.me.id.starts_with(&lower)) {
            return Err("that is this hub; to leave the swarm use: claudeship hub unlink".into());
        }
        let matches: Vec<String> = inner
            .known
            .values()
            .filter(|r| !r.is_tombstoned())
            .filter(|r| {
                r.name.to_lowercase() == lower
                    || r.id == lower
                    || (lower.len() >= 4 && r.id.starts_with(&lower))
            })
            .map(|r| r.id.clone())
            .collect();
        let id = match matches.as_slice() {
            [id] => id.clone(),
            [] => return Err(format!("no peer named {target} (see: claudeship hub peers)")),
            many => {
                return Err(format!(
                    "{} peers match {target}; name one by id: {}",
                    many.len(),
                    many.join(", ")
                ));
            }
        };
        let now = now_ms();
        let record = inner.known.get_mut(&id).expect("matched above");
        record.tombstone = Some(now);
        let record = record.clone();
        inner.live.remove(&id);
        self.persist(&mut inner);
        Ok(record)
    }

    /// `hub peers`: our record and every peer's, with what the polls found.
    pub fn view(&self) -> Value {
        let mut inner = self.lock();
        self.refresh_me(&mut inner);
        let peers: Vec<Value> = inner
            .known
            .values()
            .map(|r| {
                let live = inner.live.get(&r.id).cloned().unwrap_or_default();
                json!({
                    "record": r.to_value(),
                    "reachable": live.reachable,
                    "refused": live.refused,
                })
            })
            .collect();
        json!({"self": inner.me.to_value(), "peers": peers})
    }

    /// `/api/state`'s `hosts`: this hub (from `local`, the local state)
    /// first, then every peer not tombstoned by name — an unreachable one
    /// with its last state and `reachable: false`.
    pub fn hosts(&self, local: &Value) -> Vec<Value> {
        let mut inner = self.lock();
        self.refresh_me(&mut inner);
        let mut hosts = vec![host_entry(&inner.me, true, true, Some(local))];
        let mut peers: Vec<&PeerRecord> = inner.known.values().filter(|r| !r.is_tombstoned()).collect();
        peers.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        for record in peers {
            let live = inner.live.get(&record.id);
            let mut entry = host_entry(
                record,
                false,
                live.is_some_and(|l| l.reachable),
                live.and_then(|l| l.state.as_ref()),
            );
            // Its `now` was its clock when it answered: carried forward to
            // this moment, so a client's offset for it stays right.
            let since = live.and_then(|l| l.polled).map(|at| at.elapsed().as_millis() as u64);
            if let (Some(since), Some(then)) = (since, entry.get("now").and_then(Value::as_u64)) {
                entry["now"] = (then + since).into();
            }
            hosts.push(entry);
        }
        hosts
    }
}

/// The keys of a hub's state that make up its `hosts` entry.
const HOST_KEYS: [&str; 9] = [
    "protocol",
    "now",
    "root",
    "rootDisplay",
    "home",
    "defaultPermissionMode",
    "projects",
    "elsewhere",
    "approvalsSupported",
];

fn host_entry(record: &PeerRecord, local: bool, reachable: bool, state: Option<&Value>) -> Value {
    let mut entry = Map::new();
    entry.insert("id".into(), record.id.clone().into());
    let name = match state.and_then(|s| s.get("host")).and_then(Value::as_str) {
        Some(host) if record.name.is_empty() => host.to_string(),
        _ => record.name.clone(),
    };
    entry.insert("name".into(), name.into());
    entry.insert("local".into(), local.into());
    entry.insert("reachable".into(), reachable.into());
    entry.insert(
        "lastSeen".into(),
        if record.last_seen > 0 {
            record.last_seen.into()
        } else {
            Value::Null
        },
    );
    entry.insert("protocol".into(), record.protocol.into());
    entry.insert("projects".into(), json!([]));
    entry.insert("elsewhere".into(), json!([]));
    if let Some(state) = state {
        for key in HOST_KEYS {
            if let Some(v) = state.get(key) {
                entry.insert(key.into(), v.clone());
            }
        }
    }
    Value::Object(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const ME: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";

    fn record(id: &str, seen: u64, address: &str) -> PeerRecord {
        PeerRecord {
            id: id.into(),
            name: "box".into(),
            addresses: vec![address.into()],
            protocol: 3,
            build: "0.1.0".into(),
            last_seen: seen,
            tombstone: None,
        }
    }

    fn tomb(id: &str, at: u64) -> PeerRecord {
        PeerRecord {
            tombstone: Some(at),
            ..record(id, 0, "100.64.0.9:7433")
        }
    }

    #[test]
    fn invite_links() {
        let ok = |l: &str| parse_invite_link(l).map(|(a, k)| (a.to_string(), k));
        assert_eq!(ok(" http://100.64.0.1:7433/auth?k=abc123 "), Ok(("100.64.0.1:7433".into(), "abc123".into())));
        assert_eq!(ok("http://[fd7a:115c:a1e0::1]:7433/auth?k=ff"), Ok(("[fd7a:115c:a1e0::1]:7433".into(), "ff".into())));
        for bad in [
            "https://100.64.0.1:7433/auth?k=ab",
            "http://100.64.0.1:7433/other?k=ab",
            "http://100.64.0.1:7433/auth/?k=ab",
            "http://100.64.0.1:7433/auth?k=",
            "http://100.64.0.1:7433/auth?k=a%20b",
            "http://100.64.0.1/auth?k=ab",
            "http://box.tailnet.ts.net:7433/auth?k=ab",
            "http://localhost:7433/auth?k=ab",
            "http://user@100.64.0.1:7433/auth?k=ab",
            "http://192.168.1.2:7433/auth?k=ab",
            "http://8.8.8.8:7433/auth?k=ab",
            // Loopback only under the test knob, which unit tests don't set.
            "http://127.0.0.1:7433/auth?k=ab",
            "ftp://100.64.0.1:7433/auth?k=ab",
            // Exactly one parameter, the key.
            "http://100.64.0.1:7433/auth?x=1&k=ab",
            "http://100.64.0.1:7433/auth?k=ab&next=http://8.8.8.8/",
            "http://100.64.0.1:7433/auth?k=ab&k=cd",
            "http://100.64.0.1:7433/auth?k=ab#x",
            "http://100.64.0.1:7433/auth/../x?k=ab",
            "http://100.64.0.1:7433//auth?k=ab",
            "http://100.64.0.1:7433@8.8.8.8:80/auth?k=ab",
            "http://100.64.0.1:0/auth?k=ab",
            "http://100.64.0.1:65536/auth?k=ab",
            "http://100.64.0.1:7433:80/auth?k=ab",
            "http://fd7a:115c:a1e0::1:7433/auth?k=ab",
            "http://[fd7a:115c:a1e0::1%25en0]:7433/auth?k=ab",
            "http://[fd7a:115c:a1e0::1%5]:7433/auth?k=ab",
            "http://100.64.0.1:7433\r\nX: y/auth?k=ab",
            "",
        ] {
            assert!(parse_invite_link(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn union_by_id_and_newest_last_seen_wins() {
        let mut known = BTreeMap::new();
        let m = merge(&mut known, [record(A, 10, "100.64.0.1:7433")], ME);
        assert!(m.changed);
        let m = merge(&mut known, [record(B, 5, "100.64.0.2:7433")], ME);
        assert!(m.changed);
        assert_eq!(known.len(), 2, "union");
        // Older news changes nothing.
        let m = merge(&mut known, [record(A, 9, "100.64.0.7:7433")], ME);
        assert_eq!(m, Merged::default());
        assert_eq!(known[A].addresses, ["100.64.0.1:7433"]);
        // Newer news replaces; only a new address counts as a change.
        let m = merge(&mut known, [record(A, 20, "100.64.0.1:7433")], ME);
        assert!(!m.changed);
        assert_eq!(known[A].last_seen, 20);
        let m = merge(&mut known, [record(A, 30, "100.64.0.3:7433")], ME);
        assert!(m.changed);
        assert_eq!(known[A].addresses, ["100.64.0.3:7433"]);
    }

    #[test]
    fn tombstones_beat_records_whatever_their_age() {
        let mut known = BTreeMap::new();
        merge(&mut known, [record(A, 100, "100.64.0.1:7433")], ME);
        let m = merge(&mut known, [tomb(A, 50)], ME);
        assert!(m.changed);
        assert_eq!(known[A].tombstone, Some(50));
        // A live record, however new, doesn't bring it back.
        let m = merge(&mut known, [record(A, 1_000, "100.64.0.1:7433")], ME);
        assert!(!m.changed);
        assert!(known[A].is_tombstoned());
        // Of two tombstones, the later is kept.
        merge(&mut known, [tomb(A, 70)], ME);
        assert_eq!(known[A].tombstone, Some(70));
        merge(&mut known, [tomb(A, 60)], ME);
        assert_eq!(known[A].tombstone, Some(70));
        // An unknown tombstone is carried too.
        let m = merge(&mut known, [tomb(B, 5)], ME);
        assert!(m.changed);
        assert!(known[B].is_tombstoned());
    }

    #[test]
    fn our_own_id_is_never_a_peer_and_its_tombstone_is_news() {
        let mut known = BTreeMap::new();
        let m = merge(&mut known, [record(ME, 5, "127.0.0.1:1")], ME);
        assert_eq!(m, Merged::default());
        assert!(known.is_empty());
        let m = merge(&mut known, [tomb(ME, 5)], ME);
        assert!(m.unpaired_me);
        assert!(known.is_empty());
    }

    #[test]
    fn tombstones_expire_after_seven_days() {
        let day = 24 * 3600 * 1000;
        let now = 100 * day;
        let mut known = BTreeMap::new();
        merge(
            &mut known,
            [tomb(A, now - 7 * day - 1), tomb(B, now - 6 * day), record(ME, 0, "1.2.3.4:5")],
            "ffffffff-ffff-4fff-8fff-ffffffffffff",
        );
        assert_eq!(known.len(), 3);
        assert!(expire(&mut known, now));
        assert!(!known.contains_key(A), "past seven days: forgotten");
        assert!(known.contains_key(B), "within seven days: kept");
        assert!(known.contains_key(ME), "live records never expire");
        assert!(!expire(&mut known, now));
    }

    #[test]
    fn records_from_the_wire_are_checked() {
        let value = json!([
            {"id": A.to_uppercase(), "name": "x\u{1b}[2Jy\n", "lastSeen": 3,
             "addresses": ["100.64.0.1:7433", "nope", "[fd7a:115c:a1e0::1]:7433",
                           "8.8.8.8:7433", "[fd7a::1]:7433", "192.168.1.2:7433", "127.0.0.1:7433", "[::1]:7433"]},
            {"id": "not-a-uuid"},
            {"name": "no id"},
            7,
        ]);
        let list = PeerRecord::list_from(Some(&value));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, A, "lowercased");
        // Tailnet only (loopback only under the test knob, unset here): a
        // record naming any other address would have us send it the secret.
        assert_eq!(list[0].addresses, ["100.64.0.1:7433", "[fd7a:115c:a1e0::1]:7433"]);
        assert_eq!(list[0].name, "x[2Jy", "no control characters");
        assert_eq!(list[0].last_seen, 3);
        let wire = list[0].to_value();
        assert_eq!(wire["lastSeen"], 3);
        assert!(wire.get("tombstone").is_none());
    }

    #[test]
    fn admissible_drops_stale_tombstones_and_clamps_future_clocks() {
        let day = 24 * 3600 * 1000;
        let now = 100 * day;
        let mut known = BTreeMap::new();
        known.insert(A.to_string(), record(A, now - 9 * day, "100.64.0.1:7433"));
        let kept = admissible(
            vec![
                // A tombstone we would already have forgotten: not news.
                tomb(A, now - 8 * day),
                // A live record we know, however old: kept.
                record(A, now - 9 * day, "100.64.0.1:7433"),
                // A record we don't know, unseen for 8 days: maybe one whose
                // tombstone we forgot; dropped.
                record(B, now - 8 * day, "100.64.0.2:7433"),
                // From a clock an hour ahead: clamped to our now.
                record(ME, now + 3_600_000, "100.64.0.3:7433"),
                tomb(B, now + 3_600_000),
            ],
            &known,
            now,
        );
        assert_eq!(kept.len(), 3, "{kept:?}");
        assert!(!kept[0].is_tombstoned());
        assert_eq!(kept[1].last_seen, now);
        assert_eq!(kept[2].tombstone, Some(now));
        // So a record from a fast clock can't pin itself: the next honest
        // update replaces it.
        let mut known = BTreeMap::new();
        let first = admissible(vec![record(A, u64::MAX, "100.64.0.1:7433")], &known, now);
        merge(&mut known, first, ME);
        let later = admissible(vec![record(A, now + 1, "100.64.0.9:7433")], &known, now + 1);
        assert!(merge(&mut known, later, ME).changed);
        assert_eq!(known[A].addresses, ["100.64.0.9:7433"]);
    }

    #[test]
    fn a_peer_answering_directly_is_believed_about_itself() {
        // Our record of A carries our clock (we heard it at 1000); A's
        // clock is behind, so its own record of a new address is "older".
        let mut ours = record(A, 1_000, "100.64.0.1:7433");
        let mut known = BTreeMap::from([(A.to_string(), ours.clone())]);
        let own = PeerRecord { protocol: 4, ..record(A, 400, "100.64.0.5:7433") };
        assert!(!merge(&mut known, [own.clone()], ME).changed, "gossip alone keeps the old");
        assert!(take_own(&mut ours, own.clone()));
        assert_eq!((ours.addresses.as_slice(), ours.protocol), (["100.64.0.5:7433".to_string()].as_slice(), 4));
        assert!(!take_own(&mut ours, own), "nothing new");
        assert!(!take_own(&mut ours, record(B, 9, "100.64.0.7:7433")), "not someone else's");
        assert_eq!(ours.addresses, ["100.64.0.5:7433"]);
    }

    #[test]
    fn peer_addresses() {
        let ok = |a: &str| is_peer_address(a.parse().unwrap());
        assert!(ok("100.64.0.1:7433"));
        assert!(ok("100.127.255.255:1"));
        assert!(ok("[fd7a:115c:a1e0::5]:7433"));
        assert!(!ok("100.128.0.1:7433"));
        assert!(!ok("1.2.3.4:7433"));
        assert!(!ok("[::ffff:8.8.8.8]:7433"));
        assert!(!ok("10.0.0.1:7433"));
        assert!(!ok("127.0.0.1:7433"), "loopback only under the test knob");
        assert!(!is_dialable("8.8.8.8:80".parse().unwrap()));
    }

    #[test]
    fn ids_and_secrets() {
        let id = new_id();
        assert!(crate::web::state::is_session_id(&id), "{id}");
        assert_eq!(&id[14..15], "4");
        assert_ne!(new_id(), id);
        assert!(is_secret(&"a".repeat(64)));
        assert!(!is_secret(&"a".repeat(31)));
        assert!(!is_secret(&"g".repeat(64)));
        assert!(is_secret(&mint_secret().unwrap()));
    }

    #[test]
    fn a_host_entry_takes_the_state_keys() {
        let state = json!({"now": 5, "root": "/r", "projects": [1], "elsewhere": [], "protocol": 3,
                           "approvalsSupported": true, "host": "h", "permissionModes": []});
        let e = host_entry(&record(A, 9, "100.64.0.1:7433"), false, false, Some(&state));
        assert_eq!(e["now"], 5);
        assert_eq!(e["projects"], json!([1]));
        assert_eq!(e["reachable"], false);
        assert_eq!(e["lastSeen"], 9);
        assert!(e.get("permissionModes").is_none());
        let never = host_entry(&record(A, 0, "100.64.0.1:7433"), false, false, None);
        assert_eq!(never["projects"], json!([]));
        assert_eq!(never["lastSeen"], Value::Null);
    }
}
