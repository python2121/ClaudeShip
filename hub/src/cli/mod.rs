//! The `claudeship` command: runs Claude through the hub, so the session
//! lives in the hub rather than in this terminal, and this terminal is one
//! (replaceable) view of it. `claudeship hub …` manages the hub itself.
//!
//! Plain blocking code: no runtime here, only the hub has one.

pub mod args;
mod attach;
mod client;
mod hub_cmd;
mod qr;

use std::ffi::OsString;
use std::os::unix::process::CommandExt;

use serde_json::{Map, Value, json};

use crate::frame::PROTOCOL;
use crate::paths;
use args::{bypasses_hub, resumed_session_id};
pub use client::fail;

/// `claudeship [args]` (everything but `hub …`).
pub fn run(args: Vec<OsString>) -> ! {
    if args.first().is_some_and(|a| a == "hub") {
        hub_cmd::run(&args[1..]);
    }
    // SAFETY: isatty has no preconditions.
    let tty = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    // Arguments travel to the hub as JSON strings; one that isn't UTF-8
    // can't, so that invocation goes to claude untouched instead.
    let strings: Option<Vec<String>> = args.iter().map(|a| a.to_str().map(String::from)).collect();
    let Some(strings) = strings.filter(|s| tty && !bypasses_hub(s)) else {
        exec_program(&args);
    };
    let Some((rows, cols)) = terminal_size() else {
        exec_program(&args);
    };
    // `--resume` of a conversation that is already running in the hub
    // means "show me that one", not "start a copy of it".
    if let Some(session_id) = resumed_session_id(&strings)
        && let Some(hub_id) = hub_session_for(&session_id)
    {
        let stream = client::connect(false);
        attach::run(
            stream,
            json!({"op": "attach", "protocol": PROTOCOL, "id": hub_id, "rows": rows, "cols": cols}),
            true,
        );
    }
    let Some(cwd) = std::env::current_dir()
        .ok()
        .and_then(|d| d.to_str().map(String::from))
    else {
        exec_program(&args);
    };
    // Variables that aren't UTF-8 can't be sent; claude has no use for them.
    let env: Map<String, Value> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, Value::String(v.into_string().ok()?))))
        .collect();
    let stream = client::connect(true);
    attach::run(
        stream,
        json!({
            "op": "launch", "protocol": PROTOCOL, "cwd": cwd, "args": strings,
            "env": env, "rows": rows, "cols": cols,
        }),
        false,
    );
}

/// Run the program in place of this process, arguments untouched.
fn exec_program(args: &[OsString]) -> ! {
    let program = paths::program();
    let error = std::process::Command::new(&program).args(args).exec();
    let reason = error
        .raw_os_error()
        .map(crate::supervisor::strerror)
        .unwrap_or_else(|| error.to_string());
    fail(&format!("cannot run {}: {reason}", program.to_string_lossy()));
}

pub fn terminal_size() -> Option<(u16, u16)> {
    for fd in [1, 0] {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ writes a winsize.
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut size) } == 0
            && size.ws_row > 0
            && size.ws_col > 0
        {
            return Some((size.ws_row, size.ws_col));
        }
    }
    None
}

/// The hub session running a given Claude conversation, if any: the
/// registry names the Claude process, and a hub session's supervisor is
/// one of its ancestors.
fn hub_session_for(session_id: &str) -> Option<String> {
    let hub = client::live_sessions();
    if hub.is_empty() {
        return None;
    }
    let live = claude_sessions::Scanner::new().scan();
    let claude = live
        .iter()
        .find(|s| s.entry.session_id.as_deref() == Some(session_id))?;
    let chain = crate::procs::ancestor_pids(claude.entry.pid as i32, crate::procs::ANCESTRY_LIMIT);
    hub.into_iter()
        .find(|(_, pid)| chain.contains(pid))
        .map(|(id, _)| id)
}
