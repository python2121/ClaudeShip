//! Claude Code's side of remote approval: the PermissionRequest hook helper
//! (`claudeship permission-hook`) and its installer (`claudeship hub
//! install-hook` / `uninstall-hook`). Ports of the Swift app's
//! `PermissionHook` and `HookInstaller`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::approvals::{MAX_LINE, decision_json, parse_response, request_line};
use crate::paths;

// MARK: - The helper

/// Claude Code spawns this with the request JSON on stdin. Forward it to
/// the hub and block — with no deadline: the terminal prompt is live at the
/// same time, so waiting hides nothing; the hub hangs up when the prompt is
/// settled elsewhere — for one verdict.
///
/// The prime directive: print a decision ONLY on an explicit allow or deny.
/// Every failure (no hub, garbage input, hang-up, malformed reply) exits 0
/// with nothing on stdout, which Claude Code reads as "no decision" and
/// leaves the prompt to the terminal. Never starts a hub.
pub fn run_helper() -> ! {
    // A panic is a failure like any other: exit 0, nothing on stdout (the
    // default would be exit 101, which Claude Code reports as a hook error).
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(std::io::stderr(), "claudeship permission-hook: {info}");
        std::process::exit(0);
    }));
    if let Some(allow) = ask_hub(&paths::approvals_socket()) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", decision_json(allow));
        let _ = out.flush();
    }
    std::process::exit(0);
}

fn ask_hub(socket: &Path) -> Option<bool> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).ok()?;
    let request = request_line(&input)?;
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.write_all(&request).ok()?;
    let mut line = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let n = match stream.read(&mut buffer) {
            Ok(0) => return None, // hung up without a verdict
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        line.extend_from_slice(&buffer[..n]);
        if let Some(end) = line.iter().position(|&b| b == b'\n') {
            return parse_response(&line[..end]);
        }
        if line.len() >= MAX_LINE {
            return None;
        }
    }
}

// MARK: - The installer

/// Ours: `<this binary> permission-hook`.
pub const MARKER: &str = "permission-hook";
/// The Swift app's helper (`ClaudeShip --permission-hook`), replaced by ours.
pub const OLD_MARKER: &str = "--permission-hook";
/// Seconds Claude Code waits before killing the hook. The helper blocks for
/// as long as the prompt is unanswered, so this is a far-off leak backstop.
pub const TIMEOUT: u64 = 86_400;

/// `~/.claude/settings.json`, or under `CLAUDE_CONFIG_DIR`.
pub fn settings_path() -> PathBuf {
    claude_sessions::claude_root().join("settings.json")
}

/// The command written into the settings: this binary's absolute path
/// (single-quoted for the shell when it needs to be) and `permission-hook`.
pub fn hook_command() -> String {
    let exe = paths::self_command()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "claudeship".into());
    format!("{} {MARKER}", shell_quote(&exe))
}

fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+,:@%".contains(&b));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

fn command_of(entry: &Value) -> Option<&str> {
    entry.get("command").and_then(Value::as_str)
}

/// `<path to claudeship…> <marker>`: the marker as the command's last word,
/// after a program whose file name is ours (`claudeship`, `ClaudeShip`,
/// `claudeship-cli`; quoted or not). Someone else's script that merely has
/// the marker in its name, or takes it as an argument, isn't ours.
fn runs_us_with(command: &str, marker: &str) -> bool {
    let Some(head) = command.trim_end().strip_suffix(marker) else {
        return false;
    };
    if !head.ends_with([' ', '\t']) {
        return false;
    }
    let program = head.trim_end();
    let program = program.strip_suffix('\'').unwrap_or(program);
    let name = program.rsplit('/').next().unwrap_or(program);
    name.to_ascii_lowercase().starts_with("claudeship")
}

fn is_old(command: &str) -> bool {
    runs_us_with(command, OLD_MARKER)
}

fn is_ours(command: &str) -> bool {
    runs_us_with(command, MARKER)
}

fn inner_hooks(matcher: &Value) -> Vec<Value> {
    matcher
        .get("hooks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The pure merge. Afterwards exactly one hook entry carries the marker,
/// running `command`: a current entry is left alone, one pointing elsewhere
/// (a moved binary) is repointed, and the Swift helper's entries are
/// removed (their matcher too, when nothing else is left in it). Every
/// other key and hook is untouched. Returns whether anything changed.
pub fn merged(root: &Map<String, Value>, command: &str) -> (Map<String, Value>, bool) {
    let mut root = root.clone();
    let mut hooks = root
        .get("hooks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let matchers = hooks
        .get("PermissionRequest")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut changed = false;
    let mut have_ours = false;
    let mut kept: Vec<Value> = Vec::new();
    for matcher in matchers {
        let Some(object) = matcher.as_object() else {
            kept.push(matcher);
            continue;
        };
        let entries = inner_hooks(&matcher);
        let mut out: Vec<Value> = Vec::new();
        let mut touched = false;
        for mut entry in entries {
            match command_of(&entry).map(str::to_string) {
                Some(c) if is_old(&c) => {
                    touched = true;
                }
                Some(c) if is_ours(&c) => {
                    if have_ours {
                        // A duplicate of ours: one is enough.
                        touched = true;
                        continue;
                    }
                    have_ours = true;
                    if c != command {
                        entry["command"] = command.into();
                        touched = true;
                    }
                    out.push(entry);
                }
                _ => out.push(entry),
            }
        }
        if !touched {
            kept.push(matcher);
            continue;
        }
        changed = true;
        if out.is_empty() && object.len() <= 1 {
            continue; // nothing but our old entry was in it
        }
        let mut object = object.clone();
        object.insert("hooks".into(), Value::Array(out));
        kept.push(Value::Object(object));
    }
    if !have_ours {
        kept.push(json!({"hooks": [{"type": "command", "command": command, "timeout": TIMEOUT}]}));
        changed = true;
    }
    if changed {
        hooks.insert("PermissionRequest".into(), Value::Array(kept));
        root.insert("hooks".into(), Value::Object(hooks));
    }
    (root, changed)
}

/// The pure removal: every entry carrying the marker (ours or the Swift
/// helper's) goes, then containers it emptied.
pub fn removed(root: &Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut root = root.clone();
    let Some(mut hooks) = root.get("hooks").and_then(Value::as_object).cloned() else {
        return (root, false);
    };
    let Some(matchers) = hooks.get("PermissionRequest").and_then(Value::as_array).cloned() else {
        return (root, false);
    };
    let marked = |entry: &Value| command_of(entry).is_some_and(|c| is_ours(c) || is_old(c));
    if !matchers.iter().any(|m| inner_hooks(m).iter().any(marked)) {
        return (root, false);
    }
    let kept: Vec<Value> = matchers
        .into_iter()
        .filter_map(|matcher| {
            let Some(object) = matcher.as_object() else {
                return Some(matcher);
            };
            let entries = inner_hooks(&matcher);
            if !entries.iter().any(marked) {
                return Some(matcher);
            }
            let inner: Vec<Value> = entries.into_iter().filter(|e| !marked(e)).collect();
            if inner.is_empty() && object.len() <= 1 {
                return None;
            }
            let mut object = object.clone();
            object.insert("hooks".into(), Value::Array(inner));
            Some(Value::Object(object))
        })
        .collect();
    if kept.is_empty() {
        hooks.remove("PermissionRequest");
    } else {
        hooks.insert("PermissionRequest".into(), Value::Array(kept));
    }
    if hooks.is_empty() {
        root.remove("hooks");
    } else {
        root.insert("hooks".into(), Value::Object(hooks));
    }
    (root, true)
}

/// Read the settings, transform, and write them back if anything changed
/// (pretty-printed, keys sorted: formatting is not kept). A file that isn't
/// a JSON object is left alone.
fn rewrite(
    path: &Path,
    transform: impl FnOnce(&Map<String, Value>) -> (Map<String, Value>, bool),
) -> Result<bool, String> {
    let root = match std::fs::read(path) {
        Ok(data) => match serde_json::from_slice::<Value>(&data) {
            Ok(Value::Object(root)) => root,
            _ => return Err(format!("{} is not a JSON object — won't touch it", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let (updated, changed) = transform(&root);
    if !changed {
        return Ok(false);
    }
    // A settings file that is a symlink (into a dotfiles repo, say) stays
    // one: the file it points at is what gets replaced.
    let resolved = std::fs::canonicalize(path).ok();
    let path = resolved.as_deref().unwrap_or(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(updated)).map_err(|e| e.to_string())?;
    text.push('\n');
    // Beside it and renamed over it: Claude Code never reads half a file.
    let temp = path.with_extension(format!("json.claudeship.{}", std::process::id()));
    // Owner-only until it has the original's mode: settings can hold secrets.
    let written = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .and_then(|mut f| f.write_all(text.as_bytes()))
    };
    written.map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("cannot write {}: {e}", temp.display())
    })?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&temp, meta.permissions());
    }
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("cannot replace {}: {e}", path.display())
    })?;
    Ok(true)
}

pub fn install() -> ! {
    let path = settings_path();
    let command = hook_command();
    match rewrite(&path, |root| merged(root, &command)) {
        Ok(true) => println!("PermissionRequest hook installed in {} ({command})", path.display()),
        Ok(false) => println!("Nothing to do — the hook is already in {}", path.display()),
        Err(e) => crate::cli::fail(&e),
    }
    std::process::exit(0);
}

pub fn uninstall() -> ! {
    let path = settings_path();
    match rewrite(&path, removed) {
        Ok(true) => println!("PermissionRequest hook removed from {}", path.display()),
        Ok(false) => println!("Nothing to do — no ClaudeShip hook in {}", path.display()),
        Err(e) => crate::cli::fail(&e),
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "/home/me/.local/bin/claudeship permission-hook";

    fn object(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn commands(root: &Map<String, Value>) -> Vec<String> {
        root["hooks"]["PermissionRequest"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(inner_hooks)
            .filter_map(|e| command_of(&e).map(str::to_string))
            .collect()
    }

    // MARK: hook installer merge (pure, no file IO)

    #[test]
    fn merge_and_remove() {
        let (installed, changed) = merged(&Map::new(), CMD);
        assert!(changed, "installer: fresh settings gains hook");
        assert_eq!(
            Value::Object(installed.clone()),
            json!({"hooks": {"PermissionRequest": [{"hooks": [{"type": "command", "command": CMD, "timeout": 86400}]}]}}),
            "installer: command written"
        );
        let (again, changed) = merged(&installed, CMD);
        assert!(!changed, "installer: idempotent");
        assert_eq!(again, installed);
        let (removed_root, changed) = removed(&installed);
        assert!(changed, "installer: removal reports change");
        assert!(!removed_root.contains_key("hooks"), "installer: emptied containers pruned");
        assert!(!removed(&removed_root).1, "nothing left to remove");

        let preserved = merged(&object(json!({"model": "opus", "hooks": {"Stop": [{"hooks": []}]}})), CMD).0;
        assert_eq!(preserved["model"], "opus", "installer: unrelated keys preserved");
        assert_eq!(preserved["hooks"]["Stop"], json!([{"hooks": []}]), "installer: unrelated hooks preserved");
        let back = removed(&preserved).0;
        assert_eq!(Value::Object(back), json!({"model": "opus", "hooks": {"Stop": [{"hooks": []}]}}));
    }

    #[test]
    fn other_permission_hooks_are_left_alone() {
        let theirs = json!({"type": "command", "command": "/usr/bin/audit-perms", "timeout": 5});
        let root = object(json!({"hooks": {"PermissionRequest": [
            {"matcher": "Bash", "hooks": [theirs.clone()]},
        ]}}));
        let (installed, changed) = merged(&root, CMD);
        assert!(changed);
        assert_eq!(installed["hooks"]["PermissionRequest"][0], json!({"matcher": "Bash", "hooks": [theirs.clone()]}));
        assert_eq!(commands(&installed), vec!["/usr/bin/audit-perms".to_string(), CMD.into()]);
        assert_eq!(removed(&installed).0, root, "removal restores the original");
    }

    // MARK: hook installer — repointing after a rename, and the Swift helper

    #[test]
    fn repointing() {
        let stale = object(json!({"hooks": {"PermissionRequest": [
            {"hooks": [{"type": "command", "command": "/old/place/claudeship permission-hook", "timeout": 1}]},
        ]}}));
        let (repointed, changed) = merged(&stale, CMD);
        assert!(changed, "hook: a stale command path is repointed");
        assert_eq!(commands(&repointed), vec![CMD.to_string()], "hook: repointed to this binary");
        assert_eq!(repointed["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"], 1, "only the command changes");
        assert!(!merged(&repointed, CMD).1, "hook: nothing to do once current");
    }

    #[test]
    fn the_swift_helper_is_replaced() {
        let swift = json!({"type": "command", "command": "/Applications/ClaudeShip.app/Contents/MacOS/ClaudeShip --permission-hook", "timeout": 86400});
        let other = json!({"type": "command", "command": "notify-me"});
        // Alone in its matcher: the matcher goes, ours is added.
        let root = object(json!({"theme": "dark", "hooks": {"PermissionRequest": [{"hooks": [swift.clone()]}]}}));
        let (merged_root, changed) = merged(&root, CMD);
        assert!(changed);
        assert_eq!(commands(&merged_root), vec![CMD.to_string()]);
        assert_eq!(merged_root["hooks"]["PermissionRequest"].as_array().unwrap().len(), 1);
        assert_eq!(merged_root["theme"], "dark");
        assert!(!merged(&merged_root, CMD).1, "one pass is enough");
        // Beside another hook, or with ours already present.
        let root = object(json!({"hooks": {"PermissionRequest": [
            {"hooks": [other.clone(), swift.clone()]},
            {"hooks": [{"type": "command", "command": CMD, "timeout": 86400}]},
        ]}}));
        let (merged_root, changed) = merged(&root, CMD);
        assert!(changed);
        assert_eq!(merged_root["hooks"]["PermissionRequest"][0], json!({"hooks": [other.clone()]}));
        assert_eq!(commands(&merged_root), vec!["notify-me".to_string(), CMD.into()]);
        // Removal takes the Swift one too.
        let root = object(json!({"hooks": {"PermissionRequest": [{"hooks": [swift, other.clone()]}]}}));
        assert_eq!(
            Value::Object(removed(&root).0),
            json!({"hooks": {"PermissionRequest": [{"hooks": [other]}]}})
        );
    }

    #[test]
    fn whose_hook_is_it() {
        assert!(is_ours(CMD));
        assert!(is_ours("'/a b/claudeship' permission-hook"));
        assert!(!is_ours("/x/my-permission-hook.sh"), "a name containing the marker is someone else's");
        assert!(!is_ours("/x/ClaudeShip --permission-hook"));
        assert!(is_old("/x/ClaudeShip --permission-hook"));
        assert!(is_ours("claudeship permission-hook"), "the fallback when the path is unknown");
        assert!(is_ours("'/Users/o'\\''b/target/debug/claudeship' permission-hook"));
        assert!(is_ours("/x/claudeship-cli permission-hook  "));
        assert!(!is_ours("/usr/local/bin/audit permission-hook"), "someone else's program, the marker its argument");
        assert!(!is_ours("/opt/claudeship/bin/notify permission-hook"), "the program's name decides, not its folder");
        assert!(!is_old("/usr/local/bin/guard --permission-hook"), "someone else's flag");
        assert!(!is_old("/x/ClaudeShip --permission-hook --verbose"));
        let foreign = object(json!({"hooks": {"PermissionRequest": [{"hooks": [
            {"type": "command", "command": "/usr/local/bin/audit permission-hook"},
            {"type": "command", "command": "/usr/local/bin/guard --permission-hook"},
        ]}]}}));
        assert!(!removed(&foreign).1, "removal leaves both alone");
        let (with_ours, _) = merged(&foreign, CMD);
        assert_eq!(
            commands(&with_ours),
            vec!["/usr/local/bin/audit permission-hook".to_string(), "/usr/local/bin/guard --permission-hook".into(), CMD.into()],
            "neither is repointed nor removed; ours is added"
        );
        let theirs = object(json!({"hooks": {"PermissionRequest": [{"hooks": [{"type": "command", "command": "/x/my-permission-hook.sh"}]}]}}));
        assert!(!removed(&theirs).1);
        assert_eq!(commands(&merged(&theirs, CMD).0), vec!["/x/my-permission-hook.sh".to_string(), CMD.into()]);
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/usr/local/bin/claudeship"), "/usr/local/bin/claudeship");
        assert_eq!(shell_quote("/Users/a b/x"), "'/Users/a b/x'");
        assert_eq!(shell_quote("/it's"), r"'/it'\''s'");
    }

    #[test]
    fn a_settings_file_that_is_not_an_object_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("cs-hook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "[1, 2]").unwrap();
        assert!(rewrite(&path, |r| merged(r, CMD)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1, 2]");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(rewrite(&path, |r| merged(r, CMD)), Ok(true), "a missing file is created");
        assert_eq!(rewrite(&path, |r| merged(r, CMD)), Ok(false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlinked_settings_file_stays_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cs-hook-ln-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dotfiles")).unwrap();
        let real = dir.join("dotfiles/settings.json");
        std::fs::write(&real, r#"{"model": "opus"}"#).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(rewrite(&link, |r| merged(r, CMD)), Ok(true));
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "still a link");
        let written: Value = serde_json::from_slice(&std::fs::read(&real).unwrap()).unwrap();
        assert_eq!(written["model"], "opus");
        assert_eq!(commands(written.as_object().unwrap()), vec![CMD.to_string()], "written through the link");
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600, "mode kept");
        assert_eq!(std::fs::read_dir(dir.join("dotfiles")).unwrap().count(), 1, "no temp file left");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
