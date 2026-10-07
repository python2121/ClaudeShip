//! `claudeship hub …`: managing the hub, and its internal entry points.

use std::ffi::OsString;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::client::{self, fail, request};
use super::{attach, qr, terminal_size};
use crate::frame::PROTOCOL;
use crate::web::state::is_session_id;
use crate::{hook, local, net, paths, procs, service, supervisor};

const USAGE: &str = "\
usage: claudeship [claude arguments]   start Claude in this directory, through the hub
       claudeship hub status [--json]   the hub, its web address, and its sessions
       claudeship hub link              the link (and QR code) that pairs a browser with the hub
       claudeship hub unlink            unpair every browser, phone, and peer (new secrets; pair again)
       claudeship hub pair <link>       join the swarm of the hub whose link (from its hub link) this is
       claudeship hub peers             the other hubs in this one's swarm
       claudeship hub unpair <name>     drop a hub from the swarm (everywhere, within a few polls)
       claudeship hub start             start the hub if it isn't running
       claudeship hub restart [--force] stop the hub and start it again (--force: even with sessions)
       claudeship hub attach <id>       open a running session in this terminal (hub id or Claude session id)
       claudeship hub kill <id>         end a session
       claudeship hub stop [--force]    stop the hub (ends its sessions; under the login service see below)
       claudeship hub install-hook      register the PermissionRequest hook (approve from any screen)
       claudeship hub uninstall-hook    remove it
       claudeship hub install-service [--no-load]    run the hub at login (launchd / systemd --user)
       claudeship hub uninstall-service [--no-load]  remove that

Under the login service, `hub stop` on macOS is a restart: launchd starts the hub
again, from the installed binary. `hub stop --force` unloads the service first, so
the hub stays stopped until the next login (or install-service).";

pub fn run(args: &[OsString]) -> ! {
    let sub = args.first().map(|a| a.to_string_lossy().into_owned());
    // Internal: what the hub runs at the head of each session's pty. Its
    // arguments are claude's, passed on untouched.
    if sub.as_deref() == Some("supervise") {
        supervisor::run(args[1..].to_vec());
    }
    let rest: Vec<String> = args
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let force = rest.iter().any(|a| a == "--force");
    match sub.as_deref().unwrap_or("status") {
        "run" => local::run_forever(),
        "start" => {
            drop(client::connect(true));
            print_status();
        }
        "status" | "ls" => {
            if rest.iter().any(|a| a == "--json") {
                print_status_json();
            }
            print_status();
        }
        "link" => print_link(),
        "unlink" => {
            if request(json!({"op": "unlink"}))
                .get("ok")
                .and_then(Value::as_bool)
                != Some(true)
            {
                fail("could not write a new pairing secret");
            }
            println!("Every paired browser is unpaired. Pair again with: claudeship hub link");
            println!(
                "This hub also left its swarm (a new swarm secret, no peers). To bring the swarm back, \
                 run on any one of them: claudeship hub pair <this hub's link from claudeship hub link>"
            );
            std::process::exit(0);
        }
        "pair" => super::pair::pair(rest.first()),
        "peers" => super::pair::peers(),
        "unpair" => super::pair::unpair(rest.first()),
        "attach" => {
            let Some(target) = rest.first() else {
                fail("usage: claudeship hub attach <id | claude session id>");
            };
            let Some((rows, cols)) = terminal_size() else {
                fail("attach needs a terminal");
            };
            // A Claude conversation id (the UUID in `claude --resume`) is
            // accepted too, for other tools that know sessions that way.
            let mut id = target.clone();
            if is_session_id(&id) {
                id = super::hub_session_for(&id).unwrap_or_else(|| {
                    fail(&format!("no hub session is running conversation {id}"))
                });
            }
            let stream = client::connect(false);
            attach::run(
                stream,
                json!({"op": "attach", "protocol": PROTOCOL, "id": id, "rows": rows, "cols": cols}),
                true,
            );
        }
        "kill" => {
            let Some(id) = rest.first() else {
                fail("usage: claudeship hub kill <id>");
            };
            request(json!({"op": "kill", "id": id}));
            println!("session {id} told to end");
            std::process::exit(0);
        }
        "stop" => stop_command(force),
        "install-hook" => hook::install(),
        "uninstall-hook" => hook::uninstall(),
        "install-service" => service::install(!rest.iter().any(|a| a == "--no-load")),
        "uninstall-service" => service::uninstall(!rest.iter().any(|a| a == "--no-load")),
        "restart" => {
            if client::try_connect().is_some() {
                stop(force);
            }
            drop(client::connect(true));
            print_status();
        }
        other => {
            println!("{USAGE}");
            std::process::exit(if other == "help" || other == "--help" {
                0
            } else {
                2
            });
        }
    }
}

/// `hub stop`, and what it means under the login service.
fn stop_command(force: bool) -> ! {
    let under_service = service::installed();
    let mut unloaded = false;
    if force && under_service && cfg!(target_os = "macos") {
        let pid = client::try_connect()
            .map(|_| request(json!({"op": "status"})))
            .and_then(|s| s.get("pid").and_then(Value::as_i64))
            .unwrap_or(0) as i32;
        if service::bootout() {
            unloaded = true;
            println!(
                "unloaded the login service ({}): the hub stays stopped until the next login \
                 or claudeship hub install-service",
                service::label()
            );
            // launchd's SIGTERM is the hub's shutdown; wait for it.
            for _ in 0..100 {
                // SAFETY: probing with signal 0.
                if pid <= 0 || unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    // A hub not run by the service (started by hand, or before it was
    // installed) is stopped the usual way.
    if !unloaded || client::try_connect().is_some() {
        stop(force);
    }
    println!("hub stopped");
    if under_service && !unloaded {
        if cfg!(target_os = "macos") {
            println!(
                "The login service starts it again in a few seconds, from the installed binary \
                 (a restart with the installed build). To keep it stopped: claudeship hub stop --force"
            );
        } else {
            #[cfg(not(target_os = "macos"))]
            println!(
                "The login service does not restart a hub that stopped cleanly; start it again with: {}",
                service::start_hint()
            );
        }
    }
    std::process::exit(0);
}

/// Ask the hub to stop (refused while sessions run, unless `force`), and
/// wait for it to be gone so a start right after doesn't meet its lock.
fn stop(force: bool) {
    let pid = request(json!({"op": "status"}))
        .get("pid")
        .and_then(Value::as_i64)
        .unwrap_or(0) as i32;
    request(json!({"op": "stop", "force": force}));
    for _ in 0..100 {
        // SAFETY: probing with signal 0.
        if pid <= 0 || unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    fail(&format!(
        "the hub (pid {pid}) said it would stop but is still running"
    ));
}

fn state_name(state: claude_sessions::State) -> &'static str {
    match state {
        claude_sessions::State::Busy => "busy",
        claude_sessions::State::Shell => "shell",
        claude_sessions::State::Idle => "idle",
        claude_sessions::State::Waiting => "waiting",
    }
}

/// `hub status --json`: for other tools. Each hub session is joined to the
/// Claude registry entry it runs (when Claude has registered), so a caller
/// holding a Claude session id or pid can find the hub id.
fn print_status_json() -> ! {
    let status = request(json!({"op": "status"}));
    let live = claude_sessions::Scanner::new().scan();
    let chains: Vec<(Vec<i32>, &claude_sessions::LiveSession)> = live
        .iter()
        .map(|s| {
            (
                procs::ancestor_pids(s.entry.pid as i32, procs::ANCESTRY_LIMIT),
                s,
            )
        })
        .collect();
    let sessions: Vec<Value> = status
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            let mut session: Map<String, Value> = entry.as_object().cloned().unwrap_or_default();
            let pid = entry.get("pid").and_then(Value::as_i64).unwrap_or(0) as i32;
            if let Some((_, claude)) = chains.iter().find(|(chain, _)| chain.contains(&pid)) {
                session.insert("claudePid".into(), claude.entry.pid.into());
                session.insert("sessionId".into(), claude.entry.session_id.clone().into());
                session.insert("status".into(), state_name(claude.state).into());
                session.insert("title".into(), claude.title.clone().into());
            }
            Value::Object(session)
        })
        .collect();
    let tailnet: Vec<String> = net::tailnet_addresses()
        .iter()
        .map(|a| a.to_string())
        .collect();
    let out = json!({
        "pid": status.get("pid").cloned().unwrap_or(json!(0)),
        "protocol": status.get("protocol").cloned().unwrap_or(json!(0)),
        "port": status.get("port").cloned().unwrap_or(json!(0)),
        "webListening": status.get("webListening").cloned().unwrap_or(json!(false)),
        "swarmId": status.get("swarmId").cloned().unwrap_or(Value::Null),
        "peerRequests": status.get("peerRequests").cloned().unwrap_or(json!(0)),
        "tailnetAddresses": tailnet,
        "sessions": sessions,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    std::process::exit(0);
}

/// "12s" / "3m" / "2h 5m".
pub fn compact_age(seconds: i64) -> String {
    let secs = seconds.max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn age_since(epoch_seconds: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    compact_age((now - epoch_seconds) as i64)
}

/// `/Users/me/x` → `~/x`.
pub fn abbreviate_home(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".into();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn print_status() -> ! {
    let status = request(json!({"op": "status"}));
    let number = |k: &str| status.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let port = number("port") as i64;
    println!(
        "hub: pid {}, up {}",
        number("pid") as i64,
        age_since(number("startedAt"))
    );
    if status.get("protocol").and_then(Value::as_i64) != Some(i64::from(PROTOCOL)) {
        println!(
            "     NOTE: the hub is running a different build than this command. New sessions need it"
        );
        println!("     restarted, which ends its current ones: claudeship hub restart --force");
    }
    if status.get("webListening").and_then(Value::as_bool) == Some(true) {
        println!("web: http://localhost:{port}");
        for address in net::tailnet_addresses() {
            println!("     http://{address}:{port}");
        }
        println!("     (a new browser must be paired first: claudeship hub link)");
    } else {
        println!(
            "web: not listening on port {port} (see {})",
            paths::log().display()
        );
    }
    let sessions = status
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if sessions.is_empty() {
        println!("no sessions");
    }
    let home = crate::config::home_dir().to_string_lossy().into_owned();
    for s in sessions {
        let text = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
        let int = |k: &str| s.get(k).and_then(Value::as_i64).unwrap_or(0);
        println!(
            "  {}  {}  up {}, {} attached, {}x{}",
            text("id"),
            abbreviate_home(&text("cwd"), &home),
            age_since(s.get("startedAt").and_then(Value::as_f64).unwrap_or(0.0)),
            int("clients"),
            int("cols"),
            int("rows"),
        );
    }
    std::process::exit(0);
}

/// Print the pairing links: the web app's address with the hub's secret
/// attached. Opening one once gives that browser a cookie; the QR code is
/// the phone's way of opening it.
fn print_link() -> ! {
    drop(client::connect(true));
    // From the hub itself, not the file: what it is actually checking.
    let link = request(json!({"op": "link"}));
    let port = link.get("port").and_then(Value::as_i64).unwrap_or(0);
    let token = link.get("token").and_then(Value::as_str).unwrap_or("");
    if token.is_empty() {
        fail(&format!(
            "the hub has no pairing secret (it could not write {}, or it has no web server)",
            paths::token().display()
        ));
    }
    println!("Open once in each browser to pair it with this hub. Anyone with the link can use");
    println!("the hub from this machine or your tailnet, so treat it like a password.\n");
    println!("  this machine:  http://localhost:{port}/auth?k={token}");
    let addresses = net::tailnet_addresses();
    for address in &addresses {
        println!("  tailnet:       http://{address}:{port}/auth?k={token}");
    }
    match addresses.first() {
        Some(address) => {
            if let Some(code) = qr::render(&format!("http://{address}:{port}/auth?k={token}")) {
                println!("\nScan with the phone (it must be on the tailnet):\n");
                println!("{code}");
            }
        }
        None => {
            println!(
                "\nNo Tailscale address on this machine right now — connect Tailscale and run this"
            );
            println!("again to get the link for a phone.");
        }
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(compact_age(-5), "0s");
        assert_eq!(compact_age(59), "59s");
        assert_eq!(compact_age(60), "1m");
        assert_eq!(compact_age(3 * 3600 + 5 * 60 + 9), "3h 5m");
    }

    #[test]
    fn tilde_paths() {
        assert_eq!(abbreviate_home("/Users/me/code", "/Users/me"), "~/code");
        assert_eq!(abbreviate_home("/Users/me", "/Users/me/"), "~");
        assert_eq!(abbreviate_home("/Users/meow", "/Users/me"), "/Users/meow");
        assert_eq!(abbreviate_home("/tmp", ""), "/tmp");
    }
}
