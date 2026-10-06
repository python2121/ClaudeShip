import AppKit

@main
struct ClaudeShipMain {
    // Held in a static so NSApplication's weak `delegate` reference doesn't
    // free it.
    private static let appDelegate = AppDelegate()

    static func main() {
        // The session hub and its terminal client share this binary. The
        // installed copy is invoked as `claudeship`, where every argument
        // belongs to claude — so this dispatch comes before any flag
        // checks. A dev build reaches the same code with `--cli`.
        let arguments = CommandLine.arguments
        if let invokedAs = arguments.first, (invokedAs as NSString).lastPathComponent == HubCLI.commandName {
            HubCLI.run(Array(arguments.dropFirst()))
        }
        if arguments.count > 1, arguments[1] == "--cli" {
            HubCLI.run(Array(arguments.dropFirst(2)))
        }

        // Hook helper mode: Claude Code's PermissionRequest hook pipes the
        // request in on stdin; we relay it to the running app and print a
        // verdict (or nothing — silence hands the prompt back to the
        // terminal). Dispatched first: it must never touch the GUI, the
        // single-instance lock, or block on anything but its own deadline.
        if CommandLine.arguments.contains("--permission-hook") {
            PermissionHook.run()
        }

        // Register / remove the PermissionRequest hook in ~/.claude/settings.json.
        if CommandLine.arguments.contains("--install-hook") {
            runHookInstaller(install: true)
        }
        if CommandLine.arguments.contains("--uninstall-hook") {
            runHookInstaller(install: false)
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

    private static func runHookInstaller(install: Bool) -> Never {
        do {
            let changed = install ? try HookInstaller.install() : try HookInstaller.uninstall()
            let verb = install ? "installed in" : "removed from"
            print(changed
                ? "PermissionRequest hook \(verb) \(HookInstaller.settingsPath)"
                : "Nothing to do — hook \(install ? "already present" : "not present") in \(HookInstaller.settingsPath)")
            exit(0)
        } catch {
            FileHandle.standardError.write(Data("hook \(install ? "install" : "uninstall") failed: \(error.localizedDescription)\n".utf8))
            exit(1)
        }
    }
}
