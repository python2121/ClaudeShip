//! The pairing secret: a random token only this user can read
//! (`<home>/token`, 0600). A browser presents it once (the link from
//! `claudeship hub link`) and holds it from then on as a cookie. Network
//! position alone is not a login — other users of this machine, a
//! sandboxed app, or anything forwarding a port can all reach 127.0.0.1.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use crate::paths;

pub const COOKIE_NAME: &str = "claude_ship";

/// The existing token, or a newly minted one. `None` only if the hub's
/// directory can't be written (or there is no randomness), in which case
/// nothing can authenticate.
pub fn load_or_create() -> Option<String> {
    if let Some(token) = read()
        && token.len() >= 32
    {
        return Some(token);
    }
    create()
}

/// The token on disk, trimmed.
pub fn read() -> Option<String> {
    std::fs::read_to_string(paths::token())
        .ok()
        .map(|t| t.trim().to_string())
}

/// A fresh token (64 hex characters), replacing any on disk.
pub fn create() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    paths::ensure_home();
    let path = paths::token();
    // Written whole or not at all: a reader never sees half a secret.
    let mut staging = path.as_os_str().to_owned();
    staging.push(".new");
    let staging = std::path::PathBuf::from(staging);
    let _ = std::fs::remove_file(&staging);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)
        .and_then(|mut f| f.write_all(token.as_bytes()));
    if written.is_err() || std::fs::rename(&staging, &path).is_err() {
        let _ = std::fs::remove_file(&staging);
        return None;
    }
    Some(token)
}
