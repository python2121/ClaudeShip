//! Which `claudeship` invocations are sessions to share through the hub.

use crate::web::state::is_session_id;

/// Invocations that aren't an interactive session to share — one-shot
/// output, claude's own management subcommands, sessions claude runs
/// somewhere else — go to claude untouched.
pub fn bypasses_hub<S: AsRef<str>>(args: &[S]) -> bool {
    const FLAGS: [&str; 10] = [
        "-p",
        "--print",
        "-v",
        "--version",
        "-h",
        "--help",
        "--bg",
        "--background",
        "--cloud",
        "--desktop",
    ];
    const SUBCOMMANDS: [&str; 21] = [
        "agents",
        "attach",
        "auth",
        "auto-mode",
        "doctor",
        "gateway",
        "import",
        "install",
        "logs",
        "mcp",
        "plugin",
        "plugins",
        "purge",
        "respawn",
        "rm",
        "setup-token",
        "stop",
        "kill",
        "ultrareview",
        "update",
        "upgrade",
    ];
    if let Some(first) = args.first()
        && SUBCOMMANDS.contains(&first.as_ref())
    {
        return true;
    }
    args.iter().any(|a| {
        let a = a.as_ref();
        FLAGS.contains(&a) || a.starts_with("--cloud=")
    })
}

/// The conversation `--resume <id>` / `-r <id>` / `--resume=<id>` asks for.
/// The first resume flag decides: a picker (`--resume` with no id) or a
/// non-uuid is not a resume of one conversation.
pub fn resumed_session_id<S: AsRef<str>>(args: &[S]) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        let arg = arg.as_ref();
        if arg == "--resume" || arg == "-r" {
            let next = args.get(index + 1)?.as_ref();
            return is_session_id(next).then(|| next.to_string());
        }
        if let Some(value) = arg.strip_prefix("--resume=") {
            return is_session_id(value).then(|| value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "93fb531a-9e91-4926-ab89-93ded70cba7e";

    #[test]
    fn what_goes_through_the_hub() {
        assert!(!bypasses_hub::<&str>(&[]), "cli: bare command is a session");
        assert!(
            !bypasses_hub(&["fix the bug", "--model", "opus"]),
            "cli: a prompt is a session"
        );
        assert!(
            !bypasses_hub(&["--resume", "abc"]),
            "cli: resume is a session"
        );
        assert!(
            bypasses_hub(&["-p", "hi"]),
            "cli: print mode goes straight to claude"
        );
        assert!(
            bypasses_hub(&["--version"]),
            "cli: version goes straight to claude"
        );
        assert!(
            bypasses_hub(&["mcp", "list"]),
            "cli: a management subcommand goes straight to claude"
        );
        assert!(
            bypasses_hub(&["--bg", "do it"]),
            "cli: claude's own background sessions aren't ours"
        );
        assert!(
            !bypasses_hub(&["update the docs"]),
            "cli: a prompt that starts like a subcommand"
        );
        assert!(bypasses_hub(&["--cloud=x"]), "cli: --cloud=<env>");
        assert!(
            !bypasses_hub(&["fix", "mcp"]),
            "cli: a subcommand name later in the args is a prompt word"
        );
    }

    #[test]
    fn resumed_conversation() {
        assert_eq!(
            resumed_session_id(&["--resume", ID]).as_deref(),
            Some(ID),
            "cli: --resume id"
        );
        assert_eq!(
            resumed_session_id(&["-r", ID, "--model", "opus"]).as_deref(),
            Some(ID),
            "cli: -r id"
        );
        assert_eq!(
            resumed_session_id(&[format!("--resume={ID}")]).as_deref(),
            Some(ID),
            "cli: --resume=id"
        );
        assert_eq!(
            resumed_session_id(&["--resume"]),
            None,
            "cli: --resume with no id (claude's picker) is not a resume of one"
        );
        assert_eq!(
            resumed_session_id(&["--resume", "latest"]),
            None,
            "cli: --resume of a non-uuid"
        );
        assert_eq!(
            resumed_session_id(&["fix", "the", "bug"]),
            None,
            "cli: no resume"
        );
        assert_eq!(
            resumed_session_id(&["--resume", "latest", "-r", ID]),
            None,
            "the first resume flag decides"
        );
        assert_eq!(
            resumed_session_id(&["--model", "opus", "--resume=", "x"]),
            None,
            "an empty --resume= is not a resume"
        );
    }
}
