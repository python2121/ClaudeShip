import AppKit
import Darwin
import Foundation

/// Brings the terminal hosting a Claude session to the front when its row
/// is clicked. Two tiers:
///
/// 1. **Emulator adapters** — Ghostty (1.3+), Terminal.app, iTerm2 — select
///    the exact tab/split through their AppleScript dictionaries. Ghostty
///    exposes no pid/tty, so its terminals are matched on working directory
///    plus the tab title Claude Code sets (the transcript's `ai-title`
///    entry). Terminal.app and iTerm2 expose a `tty`, matched against the
///    session pid's controlling terminal.
/// 2. **VS Code bridge** — VS Code has no scripting for terminals, so a
///    vendored extension (`vscode-extension/`) runs in every window and
///    watches a request file; the window owning the terminal whose shell pid
///    is in the session's ancestor chain selects it and replies with its
///    workspace path, which we hand to the editor's own CLI to raise that
///    window. Also covers Cursor / Insiders / VSCodium.
/// 3. **Generic fallback** — the nearest ancestor of the session pid that is
///    a bundled app is activated (Kitty, WezTerm, anything). Right app,
///    whatever tab it was on.
///
/// A **background session** (registry `kind: "bg"`) runs under the Claude
/// Code daemon on its own pty and has no terminal to raise. Its click opens a
/// new window in the user's terminal — Ghostty, iTerm2 or Terminal.app,
/// whichever is running, else Terminal.app — in the session's cwd, running
/// `claude attach <jobId>`. The `claude` on the shell's PATH is used, since
/// the command is typed into a normal login shell rather than exec'd.
///
/// Apple events need `NSAppleEventsUsageDescription` in Info.plist and a
/// one-time Automation grant per target app (TCC). A denied, failing, or
/// non-matching adapter falls through to the generic tier, never to nothing.
///
/// Everything here blocks (sysctl, Apple events) — call off the main thread.
enum TerminalFocus {

    enum Adapter: Equatable {
        case ghostty
        case terminalApp
        case iterm
        case vscode
        case generic
    }

    static func adapter(forBundleId id: String?) -> Adapter {
        switch id {
        case "com.mitchellh.ghostty": return .ghostty
        case "com.apple.Terminal": return .terminalApp
        case "com.googlecode.iterm2": return .iterm
        case "com.microsoft.VSCode", "com.microsoft.VSCodeInsiders",
             "com.todesktop.230313mzl4w4u92" /* Cursor */, "com.vscodium":
            return .vscode
        default: return .generic
        }
    }

    enum Outcome: Equatable {
        case focusedTerminal(Adapter)
        case activatedApp(String)
        case noHostApp
        /// Opened a terminal window (bundle id) running `claude attach`.
        case attachedBackground(String)
        case noAttachId
        case attachFailed(String)
    }

    // MARK: Entry point

    static func focus(_ session: ClaudeSession) -> Outcome {
        if let hubId = session.hubId {
            return openTerminal(cwd: session.cwd, command: hubAttachCommand(cwd: session.cwd, hubId: hubId))
        }
        if session.isBackground { return attachBackground(session) }
        guard let app = hostApp(of: session.pid) else { return .noHostApp }
        let adapter = adapter(forBundleId: app.bundleIdentifier)
        if focusExactTerminal(session, adapter: adapter, app: app) {
            return .focusedTerminal(adapter)
        }
        activate(app)
        return .activatedApp(app.bundleIdentifier ?? "?")
    }

    private static func focusExactTerminal(_ session: ClaudeSession, adapter: Adapter, app: NSRunningApplication) -> Bool {
        switch adapter {
        case .ghostty:
            guard let terminals = ghosttyTerminals(),
                  let pick = pickGhosttyTerminal(terminals, cwd: session.cwd, title: session.title)
            else { return false }
            return runScript(ghosttyFocusScript(terminalId: pick.id)) == "ok"
        case .terminalApp:
            guard let tty = ttyPath(of: session.pid) else { return false }
            return runScript(terminalAppScript(tty: tty)) == "ok"
        case .iterm:
            guard let tty = ttyPath(of: session.pid) else { return false }
            return runScript(itermScript(tty: tty)) == "ok"
        case .vscode:
            return focusVSCodeTerminal(session, app: app)
        case .generic:
            return false
        }
    }

    /// `activate()` is the cooperative macOS 14 API and can be refused when
    /// we aren't the active app (the overlay is a non-activating panel, so
    /// we usually aren't). Opening the running app's bundle URL is the
    /// `open -a` path and is honored regardless.
    private static func activate(_ app: NSRunningApplication) {
        if app.activate() { return }
        guard let url = app.bundleURL else { return }
        let config = NSWorkspace.OpenConfiguration()
        config.activates = true
        NSWorkspace.shared.openApplication(at: url, configuration: config)
    }

    // MARK: Background sessions (open a terminal running `claude attach`)

    static let ghosttyBundleId = "com.mitchellh.ghostty"
    static let itermBundleId = "com.googlecode.iterm2"
    static let terminalAppBundleId = "com.apple.Terminal"

    /// Prefer a terminal the user already has open; Terminal.app is always
    /// installed, so it's the last resort even when it isn't running.
    static func attachTerminalBundleId(running: Set<String>) -> String {
        for id in [ghosttyBundleId, itermBundleId, terminalAppBundleId] where running.contains(id) {
            return id
        }
        return terminalAppBundleId
    }

    /// The line typed into the new shell, single-quoted for it.
    static func attachCommand(cwd: String, attachId: String) -> String {
        "cd \(shellSingleQuoted(cwd)) && claude attach \(shellSingleQuoted(attachId))"
    }

    /// Same, for a session the hub owns.
    static func hubAttachCommand(cwd: String, hubId: String) -> String {
        "cd \(shellSingleQuoted(cwd)) && \(HubCLI.commandName) hub attach \(shellSingleQuoted(hubId))"
    }

    static func shellSingleQuoted(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    private static func attachBackground(_ session: ClaudeSession) -> Outcome {
        guard let attachId = session.attachId, !attachId.isEmpty else { return .noAttachId }
        return openTerminal(cwd: session.cwd, command: attachCommand(cwd: session.cwd, attachId: attachId))
    }

    /// A new window in the user's terminal, in `cwd`, with `command` typed
    /// into its shell.
    private static func openTerminal(cwd: String, command: String) -> Outcome {
        let running = Set(NSWorkspace.shared.runningApplications.compactMap(\.bundleIdentifier))
        let bundleId = attachTerminalBundleId(running: running)
        let script: String
        switch bundleId {
        case ghosttyBundleId: script = ghosttyAttachScript(cwd: cwd, command: command)
        case itermBundleId: script = itermAttachScript(command: command)
        default: script = terminalAppAttachScript(command: command)
        }
        if runScript(script) == "ok" { return .attachedBackground(bundleId) }
        // Ghostty/iTerm declined (Automation not granted, say): Terminal.app
        // is the universal fallback, and its grant is a separate one.
        if bundleId != terminalAppBundleId, runScript(terminalAppAttachScript(command: command)) == "ok" {
            return .attachedBackground(terminalAppBundleId)
        }
        return .attachFailed(bundleId)
    }

    /// Ghostty 1.3+: a surface configuration carries the working directory
    /// and text to type once the shell is up. The command isn't set as the
    /// surface's `command` so the user's shell (and PATH) stays in charge and
    /// Ctrl+Z out of `claude attach` lands in a normal prompt.
    static func ghosttyAttachScript(cwd: String, command: String) -> String {
        """
        tell application id "\(ghosttyBundleId)"
          set cfg to new surface configuration
          set initial working directory of cfg to "\(appleScriptLiteral(cwd))"
          set initial input of cfg to "\(appleScriptLiteral(command))" & linefeed
          set w to new window with configuration cfg
          activate
          return "ok"
        end tell
        """
    }

    static func itermAttachScript(command: String) -> String {
        """
        tell application id "\(itermBundleId)"
          set w to (create window with default profile)
          tell current session of w to write text "\(appleScriptLiteral(command))"
          activate
          return "ok"
        end tell
        """
    }

    static func terminalAppAttachScript(command: String) -> String {
        """
        tell application id "\(terminalAppBundleId)"
          do script "\(appleScriptLiteral(command))"
          activate
          return "ok"
        end tell
        """
    }

    // MARK: Ghostty

    struct GhosttyTerminal: Equatable {
        var id: String
        var name: String
        var cwd: String
    }

    /// One round-trip for the whole surface list; the pick happens here so
    /// it's testable and the matching rules live in one place.
    static let ghosttyListScript = """
    tell application id "com.mitchellh.ghostty"
      return {id of every terminal, name of every terminal, working directory of every terminal}
    end tell
    """

    static func ghosttyTerminals() -> [GhosttyTerminal]? {
        runScriptDescriptor(ghosttyListScript).flatMap(parseGhosttyList)
    }

    /// The script returns three parallel lists (ids, names, cwds). A missing
    /// title or cwd comes back as `missing value`, read here as "".
    static func parseGhosttyList(_ desc: NSAppleEventDescriptor) -> [GhosttyTerminal]? {
        guard desc.numberOfItems == 3,
              let ids = desc.atIndex(1), let names = desc.atIndex(2), let cwds = desc.atIndex(3),
              ids.numberOfItems == names.numberOfItems, ids.numberOfItems == cwds.numberOfItems
        else { return nil }
        var out: [GhosttyTerminal] = []
        var i = 1
        while i <= ids.numberOfItems {
            if let id = ids.atIndex(i)?.stringValue, !id.isEmpty {
                out.append(GhosttyTerminal(
                    id: id,
                    name: names.atIndex(i)?.stringValue ?? "",
                    cwd: cwds.atIndex(i)?.stringValue ?? ""))
            }
            i += 1
        }
        return out
    }

    /// Match order: cwd + title (exact), then title alone (the shell moved,
    /// or Ghostty reports a differently-resolved path), then cwd alone (no
    /// title yet — first turn — or a title collision; first in Ghostty's
    /// order wins). nil when nothing matches so the caller falls back to
    /// activating the app rather than guessing a surface.
    static func pickGhosttyTerminal(_ terminals: [GhosttyTerminal], cwd: String, title: String?) -> GhosttyTerminal? {
        let wantTitle = title.flatMap { $0.isEmpty ? nil : $0 }
        func titleHit(_ t: GhosttyTerminal) -> Bool { wantTitle.map { t.name.hasSuffix($0) } ?? false }
        func cwdHit(_ t: GhosttyTerminal) -> Bool { samePath(t.cwd, cwd) }
        return terminals.first { titleHit($0) && cwdHit($0) }
            ?? terminals.first { titleHit($0) }
            ?? terminals.first { cwdHit($0) }
    }

    static func samePath(_ a: String, _ b: String) -> Bool {
        trimSlashes(a) == trimSlashes(b)
    }

    private static func trimSlashes(_ path: String) -> String {
        var s = Substring(path)
        while s.count > 1 && s.hasSuffix("/") { s = s.dropLast() }
        return String(s)
    }

    static func ghosttyFocusScript(terminalId: String) -> String {
        """
        tell application id "com.mitchellh.ghostty"
          focus terminal id "\(appleScriptLiteral(terminalId))"
          activate
          return "ok"
        end tell
        """
    }

    // MARK: Terminal.app / iTerm2 (tty match)

    static func terminalAppScript(tty: String) -> String {
        """
        tell application id "com.apple.Terminal"
          repeat with w in windows
            repeat with t in tabs of w
              if tty of t is "\(appleScriptLiteral(tty))" then
                set selected tab of w to t
                set index of w to 1
                activate
                return "ok"
              end if
            end repeat
          end repeat
          return "none"
        end tell
        """
    }

    static func itermScript(tty: String) -> String {
        """
        tell application id "com.googlecode.iterm2"
          repeat with w in windows
            repeat with t in tabs of w
              repeat with s in sessions of t
                if tty of s is "\(appleScriptLiteral(tty))" then
                  select s
                  select t
                  select w
                  activate
                  return "ok"
                end if
              end repeat
            end repeat
          end repeat
          return "none"
        end tell
        """
    }

    /// Escape for interpolation inside an AppleScript double-quoted string.
    static func appleScriptLiteral(_ s: String) -> String {
        s.replacingOccurrences(of: "\\", with: "\\\\")
         .replacingOccurrences(of: "\"", with: "\\\"")
    }

    // MARK: VS Code (bridge extension)

    /// Shared with the extension (see `vscode-extension/extension.js`):
    /// `request.json` from us, `reply.json` from the owning window.
    static var vscodeBridgeDir: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/ClaudeStatus/vscode-focus", isDirectory: true)
    }

    struct VSCodeReply: Equatable {
        var nonce: String
        /// `.code-workspace` file or first folder of the owning window; nil
        /// for an untitled window (nothing for the CLI to raise by).
        var workspace: String?
        var terminal: String?
    }

    static func vscodeRequest(nonce: String, pids: [pid_t], cwd: String, sessionId: String?, now: Date) -> Data {
        var obj: [String: Any] = [
            "ts": Int(now.timeIntervalSince1970 * 1000),
            "nonce": nonce,
            "pids": pids.map { Int($0) },
            "cwd": cwd,
        ]
        if let sessionId { obj["sessionId"] = sessionId }
        return (try? JSONSerialization.data(withJSONObject: obj)) ?? Data()
    }

    static func parseVSCodeReply(_ data: Data, nonce: String) -> VSCodeReply? {
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              obj["nonce"] as? String == nonce else { return nil }
        let workspace = (obj["workspace"] as? String).flatMap { $0.isEmpty ? nil : $0 }
        return VSCodeReply(nonce: nonce, workspace: workspace, terminal: obj["terminal"] as? String)
    }

    /// The pid plus every ancestor below launchd. The extension matches
    /// `Terminal.processId` (the shell) against this, so it doesn't matter
    /// how many layers sit between claude and the shell.
    static func ancestorPids(of pid: pid_t, limit: Int = 16) -> [pid_t] {
        var chain = [pid]
        var current = pid
        for _ in 0..<limit {
            guard let parent = parentPid(of: current), parent > 1 else { break }
            chain.append(parent)
            current = parent
        }
        return chain
    }

    /// Drop the request atomically (temp + rename, so the watcher never sees
    /// a partial file) and poll for the owning window's reply. nil on
    /// timeout: extension not installed, remote window, or no window owns
    /// the terminal — the caller falls back to activating the app.
    static func bridgeVSCode(pids: [pid_t], cwd: String, sessionId: String?,
                             dir: URL = vscodeBridgeDir, deadline: TimeInterval = 1.5) -> VSCodeReply? {
        let fm = FileManager.default
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        let request = dir.appendingPathComponent("request.json")
        let reply = dir.appendingPathComponent("reply.json")
        try? fm.removeItem(at: reply)
        let nonce = UUID().uuidString
        let tmp = dir.appendingPathComponent("request.json.\(nonce).tmp")
        let body = vscodeRequest(nonce: nonce, pids: pids, cwd: cwd, sessionId: sessionId, now: Date())
        guard (try? body.write(to: tmp)) != nil, rename(tmp.path, request.path) == 0 else { return nil }
        let end = Date().addingTimeInterval(deadline)
        while Date() < end {
            if let data = try? Data(contentsOf: reply), let got = parseVSCodeReply(data, nonce: nonce) {
                return got
            }
            usleep(50_000)
        }
        return nil
    }

    // MARK: VS Code window state (row grouping)

    /// What each VS Code window publishes to `windows/<id>.json` (see the
    /// extension): which shell pids its terminals run, and what to call it.
    struct VSCodeWindowState: Equatable {
        var name: String?
        var workspace: String?
        var terminalPids: [pid_t]
        var ts: Date

        /// Workspace name, else the folder's last component, else untitled.
        var label: String {
            if let name, !name.isEmpty { return name }
            if let workspace, !workspace.isEmpty { return (workspace as NSString).lastPathComponent }
            return "untitled window"
        }
    }

    static var vscodeWindowsDir: URL { vscodeBridgeDir.appendingPathComponent("windows", isDirectory: true) }

    static func parseVSCodeWindowState(_ data: Data) -> VSCodeWindowState? {
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let ts = obj["ts"] as? Double else { return nil }
        let pids = (obj["terminals"] as? [[String: Any]])?.compactMap { ($0["pid"] as? Int).map { pid_t($0) } } ?? []
        return VSCodeWindowState(
            name: obj["name"] as? String,
            workspace: obj["workspace"] as? String,
            terminalPids: pids,
            ts: Date(timeIntervalSince1970: ts / 1000))
    }

    /// Every window's state, minus files older than `maxAge` (the extension
    /// heartbeats every 30 s; a crashed window's file just ages out).
    static func loadVSCodeWindows(dir: URL = vscodeWindowsDir, now: Date = Date(), maxAge: TimeInterval = 90) -> [VSCodeWindowState] {
        guard let files = try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil) else { return [] }
        return files
            .filter { $0.pathExtension == "json" }
            .compactMap { (try? Data(contentsOf: $0)).flatMap(parseVSCodeWindowState) }
            .filter { now.timeIntervalSince($0.ts) <= maxAge }
    }

    static func vscodeWindow(owning pids: [pid_t], in windows: [VSCodeWindowState]) -> VSCodeWindowState? {
        let wanted = Set(pids)
        return windows.first { !wanted.isDisjoint(with: $0.terminalPids) }
    }

    // MARK: Host (for grouping rows)

    /// Nicer than `localizedName` where the app's own name is a bare "Code".
    static func displayName(forBundleId id: String?, fallback: String) -> String {
        switch id {
        case "com.microsoft.VSCode": return "VS Code"
        case "com.microsoft.VSCodeInsiders": return "VS Code Insiders"
        case "com.todesktop.230313mzl4w4u92": return "Cursor"
        case "com.vscodium": return "VSCodium"
        case "com.apple.Terminal": return "Terminal"
        default: return fallback
        }
    }

    /// The session's host app and, for VS Code, its window. `windows` is
    /// loaded once per scan and passed in so a poll reads the state files once.
    static func host(of pid: pid_t, windows: [VSCodeWindowState]) -> SessionHost? {
        guard let app = hostApp(of: pid), let bundleId = app.bundleIdentifier else { return nil }
        let name = displayName(forBundleId: bundleId, fallback: app.localizedName ?? bundleId)
        var window: String?
        if adapter(forBundleId: bundleId) == .vscode {
            window = vscodeWindow(owning: ancestorPids(of: pid), in: windows)?.label
        }
        return SessionHost(bundleId: bundleId, appName: name, window: window)
    }

    /// Each editor ships its CLI inside the bundle; `<cli> <folder>` focuses
    /// the window that already has that folder open instead of opening one.
    static func cliName(forBundleId id: String?) -> String {
        switch id {
        case "com.microsoft.VSCodeInsiders": return "code-insiders"
        case "com.todesktop.230313mzl4w4u92": return "cursor"
        case "com.vscodium": return "codium"
        default: return "code"
        }
    }

    static func cliURL(for app: NSRunningApplication) -> URL? {
        guard let bundle = app.bundleURL else { return nil }
        let url = bundle.appendingPathComponent("Contents/Resources/app/bin/\(cliName(forBundleId: app.bundleIdentifier))")
        return FileManager.default.isExecutableFile(atPath: url.path) ? url : nil
    }

    private static func focusVSCodeTerminal(_ session: ClaudeSession, app: NSRunningApplication) -> Bool {
        guard let reply = bridgeVSCode(pids: ancestorPids(of: session.pid), cwd: session.cwd, sessionId: session.sessionId)
        else { return false }
        // The tab is selected by now; raise its window. No workspace (untitled
        // window) or no CLI → plain activation, which is right for one window.
        if let workspace = reply.workspace, let cli = cliURL(for: app), runCLI(cli, [workspace]) {
            return true
        }
        activate(app)
        return true
    }

    @discardableResult
    private static func runCLI(_ url: URL, _ arguments: [String]) -> Bool {
        let process = Process()
        process.executableURL = url
        process.arguments = arguments
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        do { try process.run() } catch { return false }
        process.waitUntilExit()
        return process.terminationStatus == 0
    }

    // MARK: AppleScript execution

    static func runScript(_ source: String) -> String? {
        runScriptDescriptor(source)?.stringValue
    }

    static func runScriptDescriptor(_ source: String) -> NSAppleEventDescriptor? {
        guard let script = NSAppleScript(source: source) else { return nil }
        var error: NSDictionary?
        let result = script.executeAndReturnError(&error)
        if let error {
            NSLog("ClaudeStatus: AppleScript failed: %@", error)
            return nil
        }
        return result
    }

    // MARK: Process tree (sysctl)

    private static func processInfo(_ pid: pid_t) -> kinfo_proc? {
        var info = kinfo_proc()
        var size = MemoryLayout<kinfo_proc>.stride
        var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid]
        guard sysctl(&mib, UInt32(mib.count), &info, &size, nil, 0) == 0, size > 0 else { return nil }
        return info
    }

    static func parentPid(of pid: pid_t) -> pid_t? {
        processInfo(pid)?.kp_eproc.e_ppid
    }

    /// "/dev/ttys004" for the pid's controlling terminal, nil when it has none.
    static func ttyPath(of pid: pid_t) -> String? {
        guard let info = processInfo(pid) else { return nil }
        let dev = info.kp_eproc.e_tdev
        guard dev != -1, let name = devname(dev, S_IFCHR) else { return nil }
        return "/dev/" + String(cString: name)
    }

    /// Walk up from the session pid (claude → zsh → login → Ghostty, say)
    /// collecting every ancestor LaunchServices knows as an app, then pick
    /// the one worth activating. Shell and login processes have no
    /// NSRunningApplication. Electron editors do have registered helpers in
    /// the chain — VS Code's pty host is "Code Helper (Plugin)", an
    /// LSUIElement bundle that shares the id `com.microsoft.VSCode.helper`
    /// — and activating one of those does nothing visible, so the pick
    /// prefers the first ancestor with a regular (Dock-visible) activation
    /// policy and only settles for an accessory when nothing regular is above
    /// it.
    static func hostApp(of pid: pid_t) -> NSRunningApplication? {
        var apps: [NSRunningApplication] = []
        var current = pid
        for _ in 0..<16 {
            guard let parent = parentPid(of: current), parent > 1 else { break }
            if parent != getpid(),
               let app = NSRunningApplication(processIdentifier: parent),
               app.bundleIdentifier != nil {
                apps.append(app)
            }
            current = parent
        }
        return chooseHostIndex(policies: apps.map(\.activationPolicy)).map { apps[$0] }
    }

    /// Nearest regular app wins; otherwise the nearest app of any policy.
    static func chooseHostIndex(policies: [NSApplication.ActivationPolicy]) -> Int? {
        policies.firstIndex(of: .regular) ?? (policies.isEmpty ? nil : 0)
    }
}
