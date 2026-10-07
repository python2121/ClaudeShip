//! Where the hub keeps its socket, lock, log, config, and pairing secret.
//! `CLAUDESHIP_HOME` relocates all of it, so a test hub never touches the
//! real one (mind the 104-byte `sun_path` limit: keep a test home short).

use std::ffi::OsString;
use std::path::PathBuf;

use crate::config::home_dir;

/// The hub's directory. macOS keeps the Swift hub's location so the Mac app
/// and every paired browser carry on working; Linux follows XDG.
pub fn home() -> PathBuf {
    if let Some(home) = std::env::var_os("CLAUDESHIP_HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    default_home()
}

#[cfg(target_os = "macos")]
fn default_home() -> PathBuf {
    home_dir().join("Library/Application Support/ClaudeShip/hub")
}

#[cfg(not(target_os = "macos"))]
fn default_home() -> PathBuf {
    match std::env::var_os("XDG_STATE_HOME").filter(|h| !h.is_empty()) {
        Some(state) => PathBuf::from(state).join("claudeship"),
        None => home_dir().join(".local/state/claudeship"),
    }
}

pub fn socket() -> PathBuf {
    home().join("hub.sock")
}

/// Where `claudeship permission-hook` finds the hub (`approvals.rs`).
pub fn approvals_socket() -> PathBuf {
    home().join("approvals.sock")
}

pub fn lock() -> PathBuf {
    home().join("hub.lock")
}

pub fn log() -> PathBuf {
    home().join("hub.log")
}

pub fn config() -> PathBuf {
    home().join("config.json")
}

/// The web pairing secret (phase 4 writes it).
pub fn token() -> PathBuf {
    home().join("token")
}

/// Create the home directory, owner-only.
pub fn ensure_home() {
    use std::os::unix::fs::DirBuilderExt;
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home());
}

/// The program a session runs. `CLAUDESHIP_CMD` swaps in a stand-in so
/// tests can exercise the pty path without starting Claude.
pub fn program() -> OsString {
    std::env::var_os("CLAUDESHIP_CMD")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "claude".into())
}

/// This binary, symlinks resolved: what the hub runs as each session's
/// supervisor and what `hub start` runs as the hub. After an install the
/// path holds the new build, so new sessions get the new supervisor while
/// an old hub keeps running.
pub fn self_command() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

#[cfg(test)]
mod tests {
    #[test]
    fn files_live_in_the_home() {
        let home = super::home();
        assert_eq!(super::socket(), home.join("hub.sock"));
        assert_eq!(super::approvals_socket(), home.join("approvals.sock"));
        assert_eq!(super::lock(), home.join("hub.lock"));
        assert_eq!(super::log(), home.join("hub.log"));
        assert_eq!(super::config(), home.join("config.json"));
    }
}
