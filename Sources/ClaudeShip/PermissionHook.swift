import Darwin
import Foundation

/// The `--permission-hook` mode: Claude Code's PermissionRequest hook spawns
/// this with the request JSON on stdin. We forward it to the running app
/// over the approval socket and block for a verdict.
///
/// The prime directive: emit a decision ONLY when the app explicitly said
/// allow or deny. Every failure mode — app not running, disconnect, crash,
/// malformed reply — exits 0 with no output, which Claude Code treats as
/// "no decision" and defers entirely to the terminal prompt.
///
/// There is deliberately NO deadline here: the CLI renders its terminal
/// prompt concurrently with this hook, so blocking doesn't hide anything —
/// it just keeps the app's buttons live as long as the prompt is unanswered.
/// The app hangs up on us (EOF) the moment the terminal answers (registry
/// reconcile) or the session dies; the hook `timeout` in settings.json is a
/// distant backstop.
enum PermissionHook {
    static func run(socketPath: String = ApprovalSocket.defaultPath) -> Never {
        let input = FileHandle.standardInput.readDataToEndOfFile()
        guard let request = ApprovalWire.requestLine(fromHookInput: input),
              let fd = ApprovalSocket.connect(path: socketPath)
        else { exit(0) }  // app not running (or garbage input) → terminal prompt
        defer_exit: do {
            let sent = request.withUnsafeBytes { raw in write(fd, raw.baseAddress, raw.count) }
            guard sent == request.count else { break defer_exit }

            guard let reply = readLine(fd),
                  let allow = ApprovalWire.parseResponse(reply)
            else { break defer_exit }  // app hung up without a verdict → terminal prompt

            print(ApprovalWire.decisionJSON(allow: allow))
        }
        close(fd)
        exit(0)
    }

    /// Blocking line read; nil on EOF (the app hung up without a verdict).
    private static func readLine(_ fd: Int32) -> Data? {
        var data = Data()
        var buf = [UInt8](repeating: 0, count: 4096)
        while data.count < ApprovalWire.maxLineBytes {
            let n = read(fd, &buf, buf.count)
            guard n > 0 else { return nil }           // peer closed
            data.append(contentsOf: buf[0..<n])
            if let nl = data.firstIndex(of: 0x0A) {
                return data.prefix(upTo: nl)
            }
        }
        return nil
    }
}

/// Installs/removes the PermissionRequest hook entry in ~/.claude/settings.json
/// (dispatched via `ClaudeShip --install-hook` / `--uninstall-hook`). The
/// merge is additive and surgical: it only touches the one hook entry whose
/// command contains our marker, and leaves everything else in the file as-is.
enum HookInstaller {
    /// Canonical installed-app path, not the invoking binary's — hooks must
    /// keep working after dev builds come and go.
    static let hookCommand = "/Applications/ClaudeShip.app/Contents/MacOS/ClaudeShip --permission-hook"
    static let commandMarker = "--permission-hook"
    /// Seconds Claude Code waits before killing the hook. The helper blocks
    /// as long as the prompt is unanswered (by design — the terminal prompt
    /// is live concurrently), so this is a far-off leak backstop, not UX.
    static let hookTimeout = 86_400

    static var settingsPath: String {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".claude/settings.json").path
    }

    static func install() throws -> Bool {
        try rewrite { merged($0) }
    }

    static func uninstall() throws -> Bool {
        try rewrite { removed($0) }
    }

    private static func rewrite(_ transform: ([String: Any]) -> ([String: Any], changed: Bool)) throws -> Bool {
        let url = URL(fileURLWithPath: settingsPath)
        var root: [String: Any] = [:]
        if let data = try? Data(contentsOf: url) {
            guard let existing = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
                throw NSError(domain: "ClaudeShip", code: 1, userInfo: [
                    NSLocalizedDescriptionKey: "\(settingsPath) is not a JSON object — won't touch it"])
            }
            root = existing
        }
        let (updated, changed) = transform(root)
        guard changed else { return false }
        let out = try JSONSerialization.data(withJSONObject: updated, options: [.prettyPrinted, .sortedKeys])
        try out.write(to: url)
        return true
    }

    /// Pure merge, split out for tests. Adds our hook entry unless a command
    /// carrying the marker is already registered — and if the registered
    /// one points somewhere else (the app's path changed), repoints it.
    static func merged(_ root: [String: Any]) -> ([String: Any], changed: Bool) {
        var root = root
        var hooks = root["hooks"] as? [String: Any] ?? [:]
        var matchers = hooks["PermissionRequest"] as? [[String: Any]] ?? []
        if containsOurHook(matchers) {
            var repointed = false
            for m in matchers.indices {
                var entries = matchers[m]["hooks"] as? [[String: Any]] ?? []
                for e in entries.indices {
                    guard let command = entries[e]["command"] as? String, command.contains(commandMarker),
                          command != hookCommand else { continue }
                    entries[e]["command"] = hookCommand
                    repointed = true
                }
                matchers[m]["hooks"] = entries
            }
            guard repointed else { return (root, false) }
            hooks["PermissionRequest"] = matchers
            root["hooks"] = hooks
            return (root, true)
        }
        matchers.append([
            "hooks": [["type": "command", "command": hookCommand, "timeout": hookTimeout]]
        ])
        hooks["PermissionRequest"] = matchers
        root["hooks"] = hooks
        return (root, true)
    }

    /// Pure removal, split out for tests. Drops hook commands carrying the
    /// marker, then prunes emptied containers.
    static func removed(_ root: [String: Any]) -> ([String: Any], changed: Bool) {
        var root = root
        guard var hooks = root["hooks"] as? [String: Any],
              let matchers = hooks["PermissionRequest"] as? [[String: Any]],
              containsOurHook(matchers)
        else { return (root, false) }
        let kept = matchers.compactMap { matcher -> [String: Any]? in
            var matcher = matcher
            let inner = (matcher["hooks"] as? [[String: Any]] ?? []).filter {
                !(($0["command"] as? String)?.contains(commandMarker) ?? false)
            }
            if inner.isEmpty && matcher.count <= 1 { return nil }
            matcher["hooks"] = inner
            return matcher
        }
        if kept.isEmpty {
            hooks["PermissionRequest"] = nil
        } else {
            hooks["PermissionRequest"] = kept
        }
        if hooks.isEmpty {
            root["hooks"] = nil
        } else {
            root["hooks"] = hooks
        }
        return (root, true)
    }

    private static func containsOurHook(_ matchers: [[String: Any]]) -> Bool {
        matchers.contains { matcher in
            (matcher["hooks"] as? [[String: Any]] ?? []).contains {
                (($0["command"] as? String)?.contains(commandMarker)) ?? false
            }
        }
    }
}
