//! Talking to the hub over its Unix socket, and starting it.

use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::frame::{self, FrameDecoder, Kind};
use crate::paths;

/// The hub's log is rotated when it has grown past this at a start.
const LOG_ROTATE_BYTES: u64 = 2 << 20;

pub fn fail(message: &str) -> ! {
    eprintln!("claudeship: {message}");
    std::process::exit(1);
}

/// Blocking write of the whole buffer (waiting out a non-blocking
/// descriptor); false if the other end is gone.
pub fn write_all_fd(fd: RawFd, data: &[u8]) -> bool {
    let mut offset = 0;
    while offset < data.len() {
        // SAFETY: writing from a live slice.
        let n = unsafe { libc::write(fd, data[offset..].as_ptr().cast(), data.len() - offset) };
        if n > 0 {
            offset += n as usize;
            continue;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: one pollfd on this frame.
                unsafe { libc::poll(&mut pfd, 1, 1000) };
            }
            _ => return false,
        }
    }
    true
}

pub fn try_connect() -> Option<UnixStream> {
    UnixStream::connect(paths::socket()).ok()
}

pub fn connect(start_if_needed: bool) -> UnixStream {
    if let Some(stream) = try_connect() {
        return stream;
    }
    if !start_if_needed {
        fail("the hub is not running (start it with: claudeship hub start)");
    }
    start_hub();
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if let Some(stream) = try_connect() {
            return stream;
        }
    }
    fail(&format!(
        "the hub did not start — see {}",
        paths::log().display()
    ));
}

/// Launch the hub as its own session, detached from this terminal, so
/// closing the window it was first started from doesn't take it down.
fn start_hub() {
    paths::ensure_home();
    let log = paths::log();
    // The log is append-only; start a fresh one when it has grown.
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let mut old = log.clone().into_os_string();
        old.push(".1");
        let _ = std::fs::rename(&log, old);
    }
    let exe = paths::self_command().unwrap_or_else(|_| fail("cannot locate own executable"));
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)
        .unwrap_or_else(|e| fail(&format!("cannot open {}: {e}", log.display())));
    let err = out
        .try_clone()
        .unwrap_or_else(|e| fail(&format!("cannot open {}: {e}", log.display())));
    let mut command = Command::new(exe);
    command
        .args(["hub", "run"])
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    let limit = crate::pty::descriptor_limit();
    // SAFETY: setsid and fcntl are async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Whatever this terminal's shell left open would otherwise be
            // held by the hub for its whole life.
            crate::pty::cloexec_above_stdio(limit);
            Ok(())
        });
    }
    if let Err(e) = command.spawn() {
        fail(&format!("cannot start the hub: {e}"));
    }
}

/// Read frames until a reply or an error. `Err` carries the hub's refusal,
/// or `None` when the connection broke.
fn await_reply(stream: &mut UnixStream) -> Result<Map<String, Value>, Option<String>> {
    let mut decoder = FrameDecoder::new();
    let mut buffer = vec![0u8; 65_536];
    loop {
        let n = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return Err(None),
            Ok(n) => n,
        };
        let frames = decoder.feed(&buffer[..n]).ok_or(None)?;
        for (kind, payload) in frames {
            match kind {
                Kind::Reply => return Ok(frame::json(&payload)),
                Kind::Error => {
                    let message = frame::json(&payload)
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("request failed")
                        .to_string();
                    return Err(Some(message));
                }
                _ => {}
            }
        }
    }
}

/// One request, one reply; any failure ends this process.
pub fn request(hello: Value) -> Map<String, Value> {
    let mut stream = connect(false);
    if !write_all_fd(stream.as_raw_fd(), &frame::encode_json(Kind::Hello, &hello)) {
        fail("lost the hub");
    }
    match await_reply(&mut stream) {
        Ok(reply) => reply,
        Err(Some(message)) => fail(&message),
        Err(None) => fail("lost the hub"),
    }
}

/// The hub's live sessions (id and supervisor pid). Empty when no hub is
/// running or it doesn't answer within a second; never starts one.
pub fn live_sessions() -> Vec<(String, i32)> {
    let Some(mut stream) = try_connect() else {
        return Vec::new();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let hello = frame::encode_json(Kind::Hello, &serde_json::json!({"op": "status"}));
    if !write_all_fd(stream.as_raw_fd(), &hello) {
        return Vec::new();
    }
    let Ok(reply) = await_reply(&mut stream) else {
        return Vec::new();
    };
    reply
        .get("sessions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|s| {
                    Some((
                        s.get("id")?.as_str()?.to_string(),
                        i32::try_from(s.get("pid")?.as_i64()?).ok()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}
