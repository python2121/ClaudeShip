import AppKit

@main
struct ClaudeShipMain {
    // Held in a static so NSApplication's weak `delegate` reference doesn't
    // free it.
    private static let appDelegate = AppDelegate()

    static func main() {
        // The hook helper and the hub moved to the Rust `claudeship`
        // binary (hub/). Point anyone still calling the old flags there.
        if let code = legacyFlagExitCode(CommandLine.arguments) {
            FileHandle.standardError.write(Data("ClaudeShip: the PermissionRequest hook now belongs to the hub; run `claudeship hub install-hook`.\n".utf8))
            exit(code)
        }

        // Headless self-test mode: run the hand-rolled assertion suite and
        // exit, before NSApplication (or the single-instance lock) exists.
        if CommandLine.arguments.contains("--self-test") {
            SelfTest.run()
        }

        // Headless scan mode: print one line per detected session and exit.
        // Handy for debugging the detection heuristics without the GUI.
        if CommandLine.arguments.contains("--scan") {
            let sessions = SessionScanner.scan()
            for s in sessions {
                let state: String
                switch s.state {
                case .busy: state = "busy"
                case .shell: state = "shell"
                case .idle: state = "idle"
                case .waitingForInput: state = "waiting" + (s.waitingFor.map { " (\($0))" } ?? "")
                }
                let since = s.stateSince.map { StatusFormat.compactAge(since: $0) } ?? "?"
                let host = TerminalFocus.hostApp(of: s.pid)?.bundleIdentifier ?? "-"
                let bg = s.isBackground ? " bg=\(s.attachId ?? "?")" : (s.hubId.map { " hub=\($0)" } ?? "")
                print("pid=\(s.pid) [\(state) for \(since)]\(bg) \(s.name ?? s.projectName) (\(s.gitBranch ?? "-")) \(s.cwd)")
                print("    host=\(host) group=\(s.host.map { "\"\($0.label)\"" } ?? "-") title=\(s.title.map { "\"\($0)\"" } ?? "-")")
            }
            print("\(sessions.count) session(s), \(sessions.filter { $0.state == .waitingForInput }.count) waiting")
            exit(0)
        }

        // `--host <pid>`: show which app the ancestor walk picks for any pid,
        // and every app it saw on the way — for debugging new emulators.
        if let i = CommandLine.arguments.firstIndex(of: "--host") {
            guard i + 1 < CommandLine.arguments.count, let pid = Int32(CommandLine.arguments[i + 1]) else {
                FileHandle.standardError.write(Data("usage: ClaudeShip --host <pid>\n".utf8))
                exit(2)
            }
            var current = pid
            while let parent = TerminalFocus.parentPid(of: current), parent > 1 {
                if let app = NSRunningApplication(processIdentifier: parent), let id = app.bundleIdentifier {
                    print("  ancestor \(parent): \(id) policy=\(app.activationPolicy.rawValue) (\(app.localizedName ?? "?"))")
                }
                current = parent
            }
            let host = TerminalFocus.hostApp(of: pid)
            print("host=\(host?.bundleIdentifier ?? "-") pid=\(host?.processIdentifier ?? 0)")
            exit(0)
        }

        // Headless focus: `--focus <pid>` raises that session's terminal and
        // prints the outcome — exercises the adapter path without the GUI.
        if let i = CommandLine.arguments.firstIndex(of: "--focus") {
            guard i + 1 < CommandLine.arguments.count, let pid = Int32(CommandLine.arguments[i + 1]) else {
                FileHandle.standardError.write(Data("usage: ClaudeShip --focus <pid>\n".utf8))
                exit(2)
            }
            // Any pid works: a non-session pid gets a bare record (no cwd/title),
            // enough to exercise the tty- and pid-based adapters.
            let session = SessionScanner.scan().first(where: { $0.pid == pid }) ?? ClaudeSession(
                pid: pid, cwd: "?", name: nil, sessionId: nil, gitBranch: nil, title: nil, host: nil,
                isBackground: false, jobId: nil,
                state: .idle, waitingFor: nil, lastActivity: nil, stateSince: nil, startedAt: nil)
            print("host=\(TerminalFocus.hostApp(of: pid)?.bundleIdentifier ?? "-") title=\(session.title ?? "-")\(session.isBackground ? " bg=\(session.attachId ?? "?")" : "")")
            print("outcome=\(TerminalFocus.focus(session))")
            exit(0)
        }

        // Only one menubar GUI at a time. If another instance already holds
        // the lock (e.g. the LaunchAgent copy is up and something launched a
        // second one), bow out cleanly instead of stacking a duplicate
        // status item.
        guard SingleInstance.acquire() else {
            FileHandle.standardError.write(
                Data("ClaudeShip: another instance is already running; exiting.\n".utf8))
            exit(0)
        }

        let app = NSApplication.shared
        app.delegate = appDelegate
        app.setActivationPolicy(.accessory)
        app.run()
    }

    /// The exit code for a flag that moved to the hub, nil for anything else.
    /// `--permission-hook` exits 0: an old settings.json entry may still run
    /// it (until `claudeship hub install-hook` replaces it, or for good with
    /// SKIP_HOOK=1), and Claude Code reads a PermissionRequest hook's exit 2
    /// as a deny — silence with 0 is "no decision", the terminal prompt
    /// stays the answer. The installer flags are typed by people: exit 2.
    static func legacyFlagExitCode(_ arguments: [String]) -> Int32? {
        if arguments.contains("--permission-hook") { return 0 }
        if arguments.contains("--install-hook") || arguments.contains("--uninstall-hook") { return 2 }
        return nil
    }
}
