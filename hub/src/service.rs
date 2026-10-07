//! `claudeship hub install-service` / `uninstall-service`: the hub as a
//! login service, so it (and with it remote approval) is up whenever the
//! user is. macOS: a LaunchAgent; Linux: a systemd user unit.
//!
//! The unit runs `<this binary> hub run` with `CLAUDESHIP_SERVICE=1`, which
//! makes a hub that finds the lock taken wait for it instead of exiting —
//! otherwise a hub started by hand (or by `claudeship` on first use) would
//! have launchd respawning the service's every ten seconds.
//!
//! `--no-load` writes the file and skips `launchctl` / `systemctl` (the
//! tests use it; so can anyone who wants to load it themselves).
//!
//! **Inside a distrobox** (SteamOS and other immutable hosts) there is no
//! systemd of its own: the unit goes to the host's `systemctl --user`
//! (`$HOME` is shared, so the same `~/.config/systemd/user` path), reached
//! through `distrobox-host-exec`, and its `ExecStart` re-enters the box with
//! `distrobox-enter -n <box> -- env CLAUDESHIP_SERVICE=1 … <binary> hub run`
//! — the binary was built in the box and only ever runs there.
//! `distrobox-enter` starts a stopped box, and doesn't reliably forward the
//! environment, hence `env` in the command rather than `Environment=`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::home_dir;
use crate::paths;

/// The environment variable the unit sets (see `local::run_forever`).
pub const SERVICE_ENV: &str = "CLAUDESHIP_SERVICE";

/// `<BUNDLE_ID>.hub`: from the environment when set, else the `BUNDLE_ID`
/// the binary was built with (`install.sh` exports `.env` before `cargo
/// build`), else `com.example.claudeship.hub`. The build-time value is what
/// lets a later `claudeship hub stop --force` or `uninstall-service`, run
/// from a shell that never sourced `.env`, find the service it installed.
pub fn label() -> String {
    let bundle = std::env::var("BUNDLE_ID")
        .ok()
        .filter(|b| !b.trim().is_empty())
        .or_else(|| option_env!("BUNDLE_ID").filter(|b| !b.trim().is_empty()).map(str::to_string))
        .unwrap_or_else(|| "com.example.claudeship".into());
    format!("{}.hub", bundle.trim())
}

#[cfg(target_os = "macos")]
pub fn unit_path() -> PathBuf {
    home_dir()
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label()))
}

#[cfg(not(target_os = "macos"))]
pub fn unit_path() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"));
    config.join("systemd/user/claudeship-hub.service")
}

/// Whether the hub this command talks to is the service's: its unit file
/// exists and runs a hub in this `CLAUDESHIP_HOME` (the default home when
/// neither sets one). A private hub (a dev build, a test) is not the
/// service's, so `hub stop --force` there must never unload the real one.
pub fn installed() -> bool {
    std::fs::read_to_string(unit_path()).is_ok_and(|unit| runs_home(&unit, explicit_home().as_deref()))
}

/// Whether a unit this module wrote runs the hub in `home` (`None`: the
/// default home, so no `CLAUDESHIP_HOME` in it).
fn runs_home(unit: &str, home: Option<&str>) -> bool {
    let Some(home) = home else {
        return !unit.contains("CLAUDESHIP_HOME");
    };
    #[cfg(target_os = "macos")]
    {
        let line = format!("<key>CLAUDESHIP_HOME</key>\n\t\t<string>{}</string>\n", xml_escape(home));
        unit.contains(&line)
    }
    #[cfg(not(target_os = "macos"))]
    systemd_runs_home(unit, home)
}

/// `runs_home` for a systemd unit: `Environment=` in a plain one; one word
/// of the `env …` in a distrobox-wrapped `ExecStart`, anywhere in the line.
/// Whole words only, so `/tmp/dev` never matches `/tmp/dev/x`.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn systemd_runs_home(unit: &str, home: &str) -> bool {
    let word = systemd_quote(&format!("CLAUDESHIP_HOME={home}"));
    unit.lines().any(|line| {
        line == format!("Environment={word}")
            || line
                .strip_prefix("ExecStart=")
                .is_some_and(|exec| format!(" {exec} ").contains(&format!(" {word} ")))
    })
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// The LaunchAgent. `home` is set only when `CLAUDESHIP_HOME` is, so the
/// service's hub uses the same directory as the command that installed it.
pub fn plist(label: &str, binary: &str, log: &str, home: Option<&str>) -> String {
    let mut env = format!("\t\t<key>{SERVICE_ENV}</key>\n\t\t<string>1</string>\n");
    if let Some(home) = home {
        env.push_str(&format!(
            "\t\t<key>CLAUDESHIP_HOME</key>\n\t\t<string>{}</string>\n",
            xml_escape(home)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{binary}</string>
		<string>hub</string>
		<string>run</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
{env}	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
        binary = xml_escape(binary),
        log = xml_escape(log),
    )
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
/// systemd quoting for one word of `ExecStart` / a value of `Environment`.
fn systemd_quote(word: &str) -> String {
    if !word.is_empty() && word.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+:@=,".contains(&b)) {
        word.to_string()
    } else {
        format!(
            "\"{}\"",
            word.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%")
        )
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
/// The systemd user unit. The hub writes its own log (`hub.log` via
/// stderr is the journal here), so nothing is redirected. `container`:
/// the distrobox this binary lives in, when the unit runs on its host.
pub fn systemd_unit(binary: &str, home: Option<&str>, container: Option<&ContainerInfo>) -> String {
    let mut vars = vec![format!("{SERVICE_ENV}=1")];
    if let Some(home) = home {
        vars.push(systemd_quote(&format!("CLAUDESHIP_HOME={home}")));
    }
    let (description, exec, env) = match container {
        None => (
            "ClaudeShip session hub".to_string(),
            format!("{} hub run", systemd_quote(binary)),
            vars.iter().map(|v| format!("Environment={v}\n")).collect::<String>(),
        ),
        Some(c) => (
            format!("ClaudeShip session hub (in distrobox {})", c.name),
            format!(
                "{DISTROBOX_ENTER} -n {} -- env {} {} hub run",
                systemd_quote(&c.name),
                vars.join(" "),
                systemd_quote(binary)
            ),
            String::new(),
        ),
    };
    format!(
        "[Unit]\n\
         Description={description}\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         {env}\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// The host-side command that runs something in a box. A bare name: the
/// host's PATH resolves it (SteamOS ships it in `/usr/bin`).
#[cfg_attr(target_os = "macos", allow(dead_code))]
const DISTROBOX_ENTER: &str = "distrobox-enter";

/// The distrobox (or other podman/docker container) this binary runs in.
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    pub name: String,
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
const NAMELESS: &str = "running inside a container, but its name is unknown (no CONTAINER_ID, \
     no name=\"…\" in /run/.containerenv): set CONTAINER_ID to the distrobox's name and run this again";

/// The container from what the probes found: `in_container` (`/run/.containerenv`
/// or `/.dockerenv` exists), `$CONTAINER_ID`, and `/run/.containerenv`'s text.
/// `Err` when in one whose name can't be found.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn container_from(
    in_container: bool,
    container_id: Option<&str>,
    containerenv: Option<&str>,
) -> Result<Option<ContainerInfo>, String> {
    if !in_container {
        return Ok(None);
    }
    container_id
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| containerenv.and_then(containerenv_name))
        .map(|name| Some(ContainerInfo { name }))
        .ok_or_else(|| NAMELESS.to_string())
}

/// The `name="…"` line of `/run/.containerenv` (podman writes one per box).
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn containerenv_name(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("name=\"")?.strip_suffix('"'))
        .find(|n| !n.is_empty())
        .map(str::to_string)
}

/// Where this process runs.
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Host,
    /// A distrobox: the host's systemd can run the hub through it.
    Distrobox,
    /// A plain docker/podman container: no service manager at all.
    Plain,
}

/// Container markers present: a distrobox when `distrobox-host-exec` is on
/// PATH or `$CONTAINER_ID` is set, otherwise a plain container.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn place_from(in_container: bool, container_id: Option<&str>, has_host_exec: bool) -> Place {
    if !in_container {
        Place::Host
    } else if has_host_exec || container_id.map(str::trim).is_some_and(|n| !n.is_empty()) {
        Place::Distrobox
    } else {
        Place::Plain
    }
}

#[cfg(not(target_os = "macos"))]
fn place() -> Place {
    let host_exec = std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("distrobox-host-exec").is_file()));
    let id = std::env::var("CONTAINER_ID").ok();
    place_from(in_container(), id.as_deref(), host_exec)
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
const PLAIN_INSTALL: &str = "no service manager in a plain container: run `claudeship hub run` as the \
     container's command (foreground), keep $HOME on a persistent volume, and check it with \
     `claudeship hub status`";

/// This process's container, from `/run/.containerenv`, `/.dockerenv` and
/// `$CONTAINER_ID`; fails (with a hint) when in one with no name.
#[cfg(not(target_os = "macos"))]
fn container() -> Option<ContainerInfo> {
    let containerenv = std::fs::read_to_string("/run/.containerenv").ok();
    let in_container = containerenv.is_some()
        || Path::new("/run/.containerenv").exists()
        || Path::new("/.dockerenv").exists();
    let id = std::env::var("CONTAINER_ID").ok();
    container_from(in_container, id.as_deref(), containerenv.as_deref())
        .unwrap_or_else(|e| crate::cli::fail(&e))
}

/// Whether this process is in a container (the unit then belongs to the
/// host's systemd). No failure for a nameless one: that's for installing.
#[cfg(not(target_os = "macos"))]
fn in_container() -> bool {
    Path::new("/run/.containerenv").exists() || Path::new("/.dockerenv").exists()
}

/// `program args…`, or on a box's host `distrobox-host-exec program args…`,
/// as a command line for messages.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn host_command_line(in_box: bool, program: &str, args: &str) -> String {
    if in_box {
        format!("distrobox-host-exec {program} {args}")
    } else {
        format!("{program} {args}")
    }
}

/// How to start the service's hub by hand (for `hub stop`'s message).
#[cfg(not(target_os = "macos"))]
pub fn start_hint() -> String {
    match place() {
        Place::Plain => "claudeship hub run".to_string(),
        p => host_command_line(p == Place::Distrobox, "systemctl", "--user start claudeship-hub"),
    }
}

/// Run `systemctl --user …` on the host: directly, or from inside a box
/// through `distrobox-host-exec`.
#[cfg(not(target_os = "macos"))]
fn systemctl(in_box: bool, args: &[&str]) -> bool {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    if in_box {
        let mut wrapped = vec!["systemctl"];
        wrapped.extend_from_slice(&full);
        run("distrobox-host-exec", &wrapped)
    } else {
        run("systemctl", &full)
    }
}

fn binary() -> String {
    paths::self_command()
        .unwrap_or_else(|e| crate::cli::fail(&format!("cannot locate own executable: {e}")))
        .to_string_lossy()
        .into_owned()
}

/// `CLAUDESHIP_HOME` as an absolute path, when set.
fn explicit_home() -> Option<String> {
    let home = std::env::var_os("CLAUDESHIP_HOME").filter(|h| !h.is_empty())?;
    let path = PathBuf::from(home);
    let path = if path.is_relative() {
        std::env::current_dir().ok()?.join(path)
    } else {
        path
    };
    Some(path.to_string_lossy().into_owned())
}

fn write_unit(path: &Path, text: &str) {
    if let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        crate::cli::fail(&format!("cannot create {}: {e}", dir.display()));
    }
    if let Err(e) = std::fs::write(path, text) {
        crate::cli::fail(&format!("cannot write {}: {e}", path.display()));
    }
}

fn run(program: &str, args: &[&str]) -> bool {
    match Command::new(program).args(args).status() {
        Ok(status) => status.success(),
        Err(e) => {
            eprintln!("claudeship: {program}: {e}");
            false
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn quiet(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(target_os = "macos")]
fn domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Unload the LaunchAgent if it is loaded (this stops its hub). macOS only;
/// elsewhere false.
pub fn bootout() -> bool {
    #[cfg(target_os = "macos")]
    {
        let target = format!("{}/{}", domain(), label());
        if quiet("launchctl", &["print", &target]) {
            return quiet("launchctl", &["bootout", &target]);
        }
    }
    false
}

pub fn install(load: bool) -> ! {
    #[cfg(not(target_os = "macos"))]
    if place() == Place::Plain {
        println!("{PLAIN_INSTALL}");
        std::process::exit(0);
    }
    let path = unit_path();
    let binary = binary();
    let home = explicit_home();
    #[cfg(target_os = "macos")]
    {
        paths::ensure_home();
        let log = paths::log().to_string_lossy().into_owned();
        write_unit(&path, &plist(&label(), &binary, &log, home.as_deref()));
        println!("wrote {} (runs {binary} hub run at login, kept alive by launchd)", path.display());
        if load {
            let target = format!("{}/{}", domain(), label());
            if quiet("launchctl", &["print", &target]) {
                let _ = quiet("launchctl", &["bootout", &target]);
            }
            if !run("launchctl", &["bootstrap", &domain(), &path.to_string_lossy()]) {
                crate::cli::fail(&format!("launchctl bootstrap {} {} failed", domain(), path.display()));
            }
            println!("loaded it: launchctl print {target}");
        } else {
            println!("not loaded (--no-load): launchctl bootstrap {} {}", domain(), path.display());
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let container = container();
        let in_box = container.is_some();
        write_unit(&path, &systemd_unit(&binary, home.as_deref(), container.as_ref()));
        match &container {
            Some(c) => println!(
                "wrote {} (the host's systemd runs {binary} hub run inside distrobox {})",
                path.display(),
                c.name
            ),
            None => println!("wrote {} (runs {binary} hub run)", path.display()),
        }
        let sc = |args: &str| host_command_line(in_box, "systemctl", args);
        if load {
            if !systemctl(in_box, &["daemon-reload"])
                || !systemctl(in_box, &["enable", "--now", "claudeship-hub.service"])
            {
                crate::cli::fail(&format!("{} failed", sc("--user enable --now claudeship-hub.service")));
            }
            println!("enabled and started it: {}", sc("--user status claudeship-hub"));
        } else {
            println!(
                "not loaded (--no-load): {} && {}",
                sc("--user daemon-reload"),
                sc("--user enable --now claudeship-hub")
            );
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "$USER".into());
        if in_box {
            println!(
                "To keep it running while you are logged out (and start it at boot), enable lingering \
                 on the host: distrobox-host-exec loginctl enable-linger {user}"
            );
        } else {
            println!(
                "To keep it running while you are logged out (and start it at boot): loginctl enable-linger {user}"
            );
        }
    }
    println!(
        "A hub that is already running keeps running (and keeps its sessions); the service's hub \
         takes over when it stops: claudeship hub stop"
    );
    std::process::exit(0);
}

pub fn uninstall(load: bool) -> ! {
    #[cfg(not(target_os = "macos"))]
    if place() == Place::Plain {
        println!("Nothing to do — a plain container has no service manager, so no service was installed");
        std::process::exit(0);
    }
    let path = unit_path();
    if !path.exists() {
        println!("Nothing to do — {} does not exist", path.display());
        std::process::exit(0);
    }
    #[cfg(target_os = "macos")]
    if load && bootout() {
        println!("unloaded {} (its hub stopped)", label());
    }
    #[cfg(not(target_os = "macos"))]
    let in_box = place() == Place::Distrobox;
    #[cfg(not(target_os = "macos"))]
    if load {
        let _ = systemctl(in_box, &["disable", "--now", "claudeship-hub.service"]);
    }
    if let Err(e) = std::fs::remove_file(&path) {
        crate::cli::fail(&format!("cannot remove {}: {e}", path.display()));
    }
    #[cfg(not(target_os = "macos"))]
    if load {
        let _ = systemctl(in_box, &["daemon-reload"]);
    }
    println!("removed {}", path.display());
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_agent() {
        let text = plist("com.example.claudeship.hub", "/Users/me/.local/bin/claudeship", "/x/hub.log", Some("/tmp/a&b"));
        assert!(text.contains("<string>com.example.claudeship.hub</string>"));
        assert!(text.contains(
            "<array>\n\t\t<string>/Users/me/.local/bin/claudeship</string>\n\t\t<string>hub</string>\n\t\t<string>run</string>\n\t</array>"
        ));
        assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(text.contains("<key>StandardOutPath</key>\n\t<string>/x/hub.log</string>"));
        assert!(text.contains("<key>StandardErrorPath</key>\n\t<string>/x/hub.log</string>"));
        assert!(text.contains("<string>/tmp/a&amp;b</string>"), "escaped");
        assert!(text.contains("<key>CLAUDESHIP_SERVICE</key>"));
        assert!(!plist("l", "/b", "/l", None).contains("CLAUDESHIP_HOME"));
    }

    #[test]
    fn whose_hub_the_unit_runs() {
        #[cfg(target_os = "macos")]
        let unit = |home| plist("l", "/b", "/l", home);
        #[cfg(not(target_os = "macos"))]
        let unit = |home| systemd_unit("/b", home, None);
        assert!(runs_home(&unit(None), None), "the default home");
        assert!(!runs_home(&unit(None), Some("/tmp/dev")), "a private hub is not the service's");
        assert!(!runs_home(&unit(Some("/tmp/dev")), None));
        assert!(runs_home(&unit(Some("/tmp/a&b c%")), Some("/tmp/a&b c%")));
        assert!(!runs_home(&unit(Some("/tmp/dev2")), Some("/tmp/dev")));
        assert!(!runs_home(&unit(Some("/tmp/dev/x")), Some("/tmp/dev")), "not a prefix match");
    }

    #[test]
    fn systemd() {
        let text = systemd_unit("/home/me/.local/bin/claudeship", None, None);
        assert!(text.contains("\nExecStart=/home/me/.local/bin/claudeship hub run\n"));
        assert!(text.contains("\nRestart=on-failure\n"));
        assert!(text.contains("\nWantedBy=default.target\n"));
        assert!(text.contains("\nEnvironment=CLAUDESHIP_SERVICE=1\n"));
        assert!(text.contains("\nDescription=ClaudeShip session hub\n"));
        assert!(!text.contains("distrobox"));
        let spaced = systemd_unit("/home/a b/claudeship", Some("/tmp/x 1%"), None);
        assert!(spaced.contains("ExecStart=\"/home/a b/claudeship\" hub run"));
        assert!(spaced.contains("Environment=\"CLAUDESHIP_HOME=/tmp/x 1%%\""));
    }

    fn arch_box() -> ContainerInfo {
        ContainerInfo { name: "arch-box".into() }
    }

    #[test]
    fn systemd_in_a_distrobox() {
        let text = systemd_unit("/home/me/.local/bin/claudeship", None, Some(&arch_box()));
        assert!(text.contains(
            "\nExecStart=distrobox-enter -n arch-box -- env CLAUDESHIP_SERVICE=1 /home/me/.local/bin/claudeship hub run\n"
        ));
        assert!(text.contains("\nDescription=ClaudeShip session hub (in distrobox arch-box)\n"));
        assert!(text.contains("\nRestart=on-failure\n"));
        assert!(text.contains("\nWantedBy=default.target\n"));
        assert!(!text.contains("Environment="), "distrobox-enter doesn't forward it; env does");
        let homed = systemd_unit("/home/a b/claudeship", Some("/tmp/x 1%"), Some(&arch_box()));
        assert!(homed.contains(
            "ExecStart=distrobox-enter -n arch-box -- env CLAUDESHIP_SERVICE=1 \"CLAUDESHIP_HOME=/tmp/x 1%%\" \"/home/a b/claudeship\" hub run\n"
        ));
    }

    #[test]
    fn whose_hub_the_wrapped_unit_runs() {
        let unit = |home| systemd_unit("/home/me/.local/bin/claudeship", home, Some(&arch_box()));
        assert!(systemd_runs_home(&unit(Some("/tmp/dev")), "/tmp/dev"), "the home inside the env words");
        assert!(systemd_runs_home(&unit(Some("/tmp/a&b c%")), "/tmp/a&b c%"));
        assert!(!systemd_runs_home(&unit(Some("/tmp/dev/x")), "/tmp/dev"), "not a prefix match");
        assert!(!systemd_runs_home(&unit(Some("/tmp/dev")), "/tmp/dev/x"));
        assert!(!systemd_runs_home(&unit(None), "/tmp/dev"));
        // And the plain unit, through the same matcher.
        assert!(systemd_runs_home(&systemd_unit("/b", Some("/tmp/a b"), None), "/tmp/a b"));
        assert!(!systemd_runs_home(&systemd_unit("/b", Some("/tmp/dev/x"), None), "/tmp/dev"));
        #[cfg(not(target_os = "macos"))]
        {
            assert!(runs_home(&unit(None), None), "the default home");
            assert!(!runs_home(&unit(None), Some("/tmp/dev")));
            assert!(!runs_home(&unit(Some("/tmp/dev")), None));
            assert!(runs_home(&unit(Some("/tmp/dev")), Some("/tmp/dev")));
        }
    }

    #[test]
    fn containerenv_names() {
        let text = "engine=\"podman-5.2.2\"\nname=\"arch-box\"\nid=\"0123abcd\"\nimage=\"quay.io/toolbx/arch-toolbox:latest\"\nrootless=1\n";
        assert_eq!(containerenv_name(text).as_deref(), Some("arch-box"));
        assert_eq!(containerenv_name("engine=\"podman\"\nname=\"\"\n"), None);
        assert_eq!(containerenv_name(""), None);
    }

    #[test]
    fn which_container() {
        let env = "engine=\"podman\"\nname=\"arch-box\"\n";
        assert_eq!(container_from(false, Some("x"), Some(env)), Ok(None), "not in one");
        assert_eq!(container_from(true, Some("dev-box"), Some(env)), Ok(Some(ContainerInfo { name: "dev-box".into() })), "CONTAINER_ID first");
        assert_eq!(container_from(true, Some(""), Some(env)), Ok(Some(arch_box())));
        assert_eq!(container_from(true, None, Some(env)), Ok(Some(arch_box())));
        let err = container_from(true, None, Some("engine=\"podman\"\n")).unwrap_err();
        assert!(err.contains("CONTAINER_ID"), "{err}");
        assert!(container_from(true, None, None).unwrap_err().contains("CONTAINER_ID"), "/.dockerenv only");
    }

    #[test]
    fn where_it_runs() {
        assert_eq!(place_from(false, Some("x"), true), Place::Host);
        assert_eq!(place_from(true, None, true), Place::Distrobox, "host-exec on PATH");
        assert_eq!(place_from(true, Some("arch-box"), false), Place::Distrobox, "CONTAINER_ID");
        assert_eq!(place_from(true, Some(" "), false), Place::Plain, "blank id");
        assert_eq!(place_from(true, None, false), Place::Plain);
    }

    #[test]
    fn host_commands() {
        assert_eq!(host_command_line(false, "systemctl", "--user start x"), "systemctl --user start x");
        assert_eq!(
            host_command_line(true, "systemctl", "--user start x"),
            "distrobox-host-exec systemctl --user start x"
        );
    }
}
