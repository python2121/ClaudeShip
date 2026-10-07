//! Being the hub (`claudeship hub run`): the single-instance lock, the
//! Unix socket terminal clients connect to, and a reader and a writer task
//! per connection. One connection is either one attached terminal or one
//! request/reply.

use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, watch};

use crate::config::HubConfig;
use crate::frame::{self, FrameDecoder, Kind, PROTOCOL};
use crate::hub::{self, Command, Hub, clamp_size, log};
use crate::paths;
use crate::session::{ClientId, Event, Sink, SinkReceiver};

/// Client ids, shared with the web server's terminals.
pub static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

/// Become the hub: take the single-instance lock, open the socket, and
/// never return.
pub fn run_forever() -> ! {
    // SAFETY: called first thing, single-threaded.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    // Everything below runs from "/", so a relative home must be pinned
    // down first (and children inherit the absolute one).
    let home = paths::home();
    if home.is_relative()
        && let Ok(cwd) = std::env::current_dir()
    {
        // SAFETY: still single-threaded; no runtime yet.
        unsafe { std::env::set_var("CLAUDESHIP_HOME", cwd.join(home)) };
    }
    paths::ensure_home();
    let lock_path = paths::lock();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path);
    // SAFETY: flock on a descriptor we own; it is held for the process's
    // life (the file is never closed).
    let mut locked = lock
        .as_ref()
        .is_ok_and(|f| unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0);
    // Under the login service, exiting would only have launchd (KeepAlive)
    // start us again every ten seconds: wait for the hub that has the lock
    // to stop, and take over then.
    if !locked
        && let Ok(file) = &lock
        && std::env::var_os(crate::service::SERVICE_ENV).is_some_and(|v| v == "1")
    {
        log(&format!(
            "another hub holds {}; the service's hub takes over when it stops",
            lock_path.display()
        ));
        // SAFETY: as above, blocking.
        locked = loop {
            let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break r == 0;
            }
        };
    }
    if !locked {
        log(&format!(
            "another hub already holds {}; exiting",
            lock_path.display()
        ));
        std::process::exit(0);
    }
    let _ = std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600));
    std::mem::forget(lock);
    // Don't pin whatever directory the first client happened to be in.
    let _ = std::env::set_current_dir("/");
    raise_fd_limit(4096);

    let supervisor = match paths::self_command() {
        Ok(path) => path,
        Err(e) => {
            log(&format!("cannot locate own executable: {e}"));
            std::process::exit(1);
        }
    };
    let config = HubConfig::load(&paths::config());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            log(&format!("runtime: {e}"));
            std::process::exit(1)
        });
    runtime.block_on(async move {
        let (tx, rx) = mpsc::unbounded_channel();
        let socket = paths::socket();
        let approvals_socket = paths::approvals_socket();
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(&approvals_socket);
        // Owner-only from the moment they exist: a chmod after the bind
        // alone leaves a moment in which anyone could connect (when the
        // home directory isn't already owner-only). The mask is restored at
        // once — sessions inherit it.
        // SAFETY: umask has no preconditions.
        let mask = unsafe { libc::umask(0o177) };
        let bound = tokio::net::UnixListener::bind(&socket);
        let approvals = tokio::net::UnixListener::bind(&approvals_socket);
        // SAFETY: as above.
        unsafe { libc::umask(mask) };
        let listener = match bound {
            Ok(listener) => listener,
            Err(e) => {
                log(&format!("bind {}: {e}", socket.display()));
                std::process::exit(1);
            }
        };
        if let Err(e) = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)) {
            log(&format!("chmod {}: {e}", socket.display()));
            std::process::exit(1);
        }
        log(&format!(
            "hub up: pid {}, socket {}, web port {}, root {}",
            std::process::id(),
            socket.display(),
            config.port,
            config.root
        ));
        // Without it the hub still runs; permission prompts are then
        // answered in the terminal only.
        match approvals {
            Ok(listener) => {
                let _ = std::fs::set_permissions(
                    &approvals_socket,
                    std::fs::Permissions::from_mode(0o600),
                );
                tokio::spawn(crate::approvals::serve(listener, tx.clone()));
            }
            Err(e) => log(&format!("bind {}: {e} (no remote approvals)", approvals_socket.display())),
        }
        let hub = Hub::new(config, tx.clone(), supervisor);
        tokio::spawn(crate::web::server::run(hub.web.shared.clone()));
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
                        serve(stream, id, tx.clone());
                    }
                    Err(e) => {
                        // Out of descriptors, most likely: don't spin.
                        log(&format!("accept: {e}"));
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });
        hub::run(hub, rx).await
    })
}

/// Every session and every viewer is a descriptor or three; the default
/// soft limit of 256 is not many.
fn raise_fd_limit(wanted: u64) {
    // SAFETY: getrlimit/setrlimit with a struct on this frame.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
            limit.rlim_cur = (wanted as libc::rlim_t).min(limit.rlim_max);
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}

/// Start the reader and writer tasks for one connection.
fn serve(stream: UnixStream, client: ClientId, hub: mpsc::UnboundedSender<Command>) {
    let (read, write) = stream.into_split();
    let (sink, events, abort) = Sink::new();
    if hub
        .send(Command::Connected {
            client,
            sink: sink.clone(),
        })
        .is_err()
    {
        return;
    }
    tokio::spawn(writer(write, events, sink));
    tokio::spawn(reader(read, client, hub, abort));
}

async fn reader(
    mut read: OwnedReadHalf,
    client: ClientId,
    hub: mpsc::UnboundedSender<Command>,
    mut abort: watch::Receiver<bool>,
) {
    let mut decoder = FrameDecoder::new();
    let mut buffer = vec![0u8; 65_536];
    'connection: loop {
        let n = tokio::select! {
            n = read.read(&mut buffer) => n,
            _ = abort.wait_for(|&a| a) => break,
        };
        let Ok(n) = n else { break };
        if n == 0 {
            break;
        }
        let Some(frames) = decoder.feed(&buffer[..n]) else {
            break;
        };
        for (kind, payload) in frames {
            let command = match kind {
                Kind::Hello => Command::Hello {
                    client,
                    request: frame::json(&payload),
                },
                Kind::Input => Command::Input {
                    client,
                    data: payload,
                },
                Kind::Resize => {
                    let request = frame::json(&payload);
                    let int = |k: &str| request.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
                    let Some((rows, cols)) = clamp_size(int("rows"), int("cols")) else {
                        continue;
                    };
                    Command::Resize { client, rows, cols }
                }
                _ => continue,
            };
            if hub.send(command).is_err() {
                break 'connection;
            }
        }
    }
    let _ = hub.send(Command::Closed { client });
}

/// Writes block on a stalled terminal (a suspended client, a full socket),
/// which is why they happen here and not in the hub. `sink` is this
/// connection's own: aborting it when done ends the reader too, which
/// tells the hub.
async fn writer(mut write: OwnedWriteHalf, mut events: SinkReceiver, sink: Sink) {
    let mut abort = sink.aborted();
    loop {
        let event = tokio::select! {
            event = events.rx.recv() => event,
            _ = abort.wait_for(|&a| a) => None,
        };
        let Some(event) = event else { break };
        let (bytes, last) = match &event {
            Event::Output(data) => (frame::encode(Kind::Output, data), false),
            // A terminal client draws whatever arrives; it has no use for this.
            Event::Size { .. } => (Vec::new(), false),
            // Only browsers are told this.
            Event::Gone => (Vec::new(), true),
            Event::Attached { id, pid } => (
                frame::encode_json(
                    Kind::Attached,
                    &json!({"id": id, "pid": pid, "protocol": PROTOCOL}),
                ),
                false,
            ),
            Event::Exit(code) => (frame::encode_json(Kind::Exit, &json!({"code": code})), true),
            Event::Error(message) => (
                frame::encode_json(Kind::Error, &json!({"message": message})),
                true,
            ),
            Event::Reply(value, _) => (frame::encode_json(Kind::Reply, value), true),
        };
        let ok = bytes.is_empty()
            || tokio::select! {
                r = write.write_all(&bytes) => r.is_ok(),
                _ = abort.wait_for(|&a| a) => false,
            };
        events.written(&event);
        if let Event::Reply(_, Some(done)) = event {
            // The hub's shutdown, for `stop`: only once the reply is out.
            let _ = write.shutdown().await;
            let _ = done.send(());
        }
        if !ok || last {
            break;
        }
    }
    let _ = write.shutdown().await;
    sink.abort();
}
