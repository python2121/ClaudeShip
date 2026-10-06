import Darwin
import Foundation

/// Where a session's terminal lives — the host app and, for VS Code, the
/// window (from the bridge extension's per-window state). Drives the
/// overlay's row grouping.
struct SessionHost: Equatable {
    var bundleId: String
    var appName: String
    /// VS Code window label (workspace name); nil for other hosts, or for a
    /// VS Code window whose extension hasn't published state.
    var window: String?

    var groupKey: String { window.map { "\(bundleId)#\($0)" } ?? bundleId }
    var label: String { window.map { "\(appName) · \($0)" } ?? appName }
}

/// One live Claude Code session, from the live-session registry.
struct ClaudeSession: Identifiable, Equatable {
    /// Claude Code's own session status enum (found in the CLI's registry
    /// writer: ["busy","shell","idle","waiting"]).
    /// - busy: Claude is working
    /// - shell: the user is running a `!` shell command inside the session
    /// - idle: sitting at the prompt, nothing requested of the user
    /// - waitingForInput: blocked on the user (permission prompt etc.)
    enum State: Equatable {
        case busy
        case shell
        case idle
        case waitingForInput
    }

    let pid: pid_t
    let cwd: String
    /// Claude Code's derived session name, e.g. "claude-status-b3".
    let name: String?
    let sessionId: String?
    /// var, not let: the store carries the last known branch forward when a
    /// scan can't see one in the transcript tail.
    var gitBranch: String?
    /// The tab title Claude Code sets for the session (the transcript's
    /// newest `ai-title` entry). Used to pick the right Ghostty surface on
    /// click; carried forward like the branch.
    var title: String?
    /// Host app / VS Code window, for grouping. nil when the walk finds no app.
    var host: SessionHost?
    /// Registry `kind` is "bg": a detached session run by the Claude Code
    /// daemon on its own pty (`claude --bg`, a fork with `--reply-on-resume`,
    /// a claimed spare). It lives in no terminal — clicking it opens one and
    /// runs `claude attach`.
    let isBackground: Bool
    /// The daemon's short id for a background session (`jobId`), what
    /// `claude attach <id>` takes.
    let jobId: String?
    let state: State
    /// What the session is blocked on, when the CLI says (`waitingFor`).
    let waitingFor: String?
    /// Transcript mtime — the last moment the session wrote anything.
    let lastActivity: Date?
    /// When the current status began (`statusUpdatedAt`).
    let stateSince: Date?
    /// Session launch time (`startedAt`).
    let startedAt: Date?
    /// Set when the session runs on a pty the session hub owns (started by
    /// `claudeandrew` or the web app): its hub id, what
    /// `claudeandrew hub attach <id>` takes. Such a session has no terminal
    /// window of its own to raise — any number of screens may be looking
    /// at it — so clicking it opens a fresh terminal attached to it.
    var hubId: String? = nil

    var id: pid_t { pid }
    var projectName: String { (cwd as NSString).lastPathComponent }
    /// What to hand `claude attach`. The registry's jobId has always been the
    /// first eight characters of the sessionId; fall back to that when the
    /// field is missing so an older CLI still attaches.
    var attachId: String? { jobId ?? sessionId.map { String($0.prefix(8)) } }
}

/// Finds running Claude Code sessions. The source of truth is the live
/// registry Claude Code itself maintains: one JSON status file per running
/// process at `~/.claude/sessions/<pid>.json`, carrying an authoritative
/// `status` — no transcript heuristics needed. (A first cut inferred state
/// from transcript tails; it couldn't tell "idle at the prompt" from
/// "waiting on a permission prompt". The registry can.)
///
/// Transcripts under `~/.claude/projects/<dashed-cwd>/<session-id>.jsonl`
/// are still read, but only for garnish: git branch and last-write time.
///
/// All functions are synchronous and blocking — call off the main thread.
enum SessionScanner {

    static var claudeRoot: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".claude", isDirectory: true)
    }
    static var sessionsRoot: URL { claudeRoot.appendingPathComponent("sessions", isDirectory: true) }
    static var projectsRoot: URL { claudeRoot.appendingPathComponent("projects", isDirectory: true) }

    // MARK: Scan

    static func scan() -> [ClaudeSession] {
        guard let entries = try? FileManager.default.contentsOfDirectory(
            at: sessionsRoot, includingPropertiesForKeys: nil, options: [.skipsHiddenFiles]
        ) else { return [] }

        // VS Code window state and the hub's session list are read once per
        // scan, not once per session.
        let windows = TerminalFocus.loadVSCodeWindows()
        let hubSessions = HubCLI.liveSessions()
        var sessions: [ClaudeSession] = []
        for file in entries where file.pathExtension == "json" {
            // Registry files are named <pid>.json; anything else isn't ours.
            guard Int32(file.deletingPathExtension().lastPathComponent) != nil,
                  let data = try? Data(contentsOf: file),
                  let entry = parseRegistryEntry(data)
            else { continue }
            // A file whose pid is dead is a leftover from a crash — skip it.
            guard pidAlive(entry.pid) else { continue }
            var session = session(for: entry, windows: windows)
            if !hubSessions.isEmpty {
                // The hub's pid for a session is its supervisor, an ancestor of
                // the Claude process.
                let chain = TerminalFocus.ancestorPids(of: entry.pid)
                session.hubId = hubSessions.first { chain.contains($0.pid) }?.id
            }
            sessions.append(session)
        }
        // Stable order so popover rows don't jump between polls.
        return sessions.sorted { ($0.cwd, $0.pid) < ($1.cwd, $1.pid) }
    }

    private static func session(for entry: RegistryEntry, windows: [TerminalFocus.VSCodeWindowState]) -> ClaudeSession {
        var branch: String?
        var title: String?
        var mtime: Date?
        if let cwd = entry.cwd, let sid = entry.sessionId {
            let transcript = projectsRoot
                .appendingPathComponent(projectDirName(forCwd: cwd), isDirectory: true)
                .appendingPathComponent("\(sid).jsonl")
            mtime = (try? FileManager.default.attributesOfItem(atPath: transcript.path)[.modificationDate]) as? Date
            if let mtime, let tail = cachedTail(of: transcript, mtime: mtime) {
                branch = tail.gitBranch
                title = tail.aiTitle
            }
        }
        let background = isBackground(kind: entry.kind)
        return ClaudeSession(
            pid: entry.pid,
            cwd: entry.cwd ?? "?",
            name: entry.name,
            sessionId: entry.sessionId,
            gitBranch: branch,
            title: title,
            // A background session hangs off the daemon (reparented to launchd),
            // so the ancestor walk finds nothing — skip it rather than risk a
            // spurious host if the daemon were ever launched from an app.
            host: background ? nil : TerminalFocus.host(of: entry.pid, windows: windows),
            isBackground: background,
            jobId: entry.jobId,
            state: state(fromStatus: entry.status),
            waitingFor: entry.waitingFor,
            lastActivity: mtime,
            stateSince: entry.statusUpdatedAt,
            startedAt: entry.startedAt
        )
    }

    // MARK: Registry parsing

    struct RegistryEntry: Equatable {
        var pid: pid_t
        var cwd: String?
        var sessionId: String?
        var name: String?
        var status: String?
        var waitingFor: String?
        var startedAt: Date?
        var statusUpdatedAt: Date?
        /// "interactive" for a terminal session, "bg" for a daemon-run one.
        var kind: String?
        var jobId: String?
    }

    static func parseRegistryEntry(_ data: Data) -> RegistryEntry? {
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let pid = obj["pid"] as? Int
        else { return nil }
        func epochMS(_ key: String) -> Date? {
            (obj[key] as? Double).map { Date(timeIntervalSince1970: $0 / 1000) }
        }
        return RegistryEntry(
            pid: pid_t(pid),
            cwd: obj["cwd"] as? String,
            sessionId: obj["sessionId"] as? String,
            name: obj["name"] as? String,
            status: obj["status"] as? String,
            waitingFor: obj["waitingFor"] as? String,
            startedAt: epochMS("startedAt"),
            statusUpdatedAt: epochMS("statusUpdatedAt"),
            kind: obj["kind"] as? String,
            jobId: obj["jobId"] as? String
        )
    }

    /// Only an explicit "bg" counts; a missing kind (older CLI) is a
    /// terminal session, so the row keeps its click-to-focus behavior.
    static func isBackground(kind: String?) -> Bool { kind == "bg" }

    /// Map the registry's status string to our state. Unknown/missing values
    /// read as idle — never a false alarm color.
    static func state(fromStatus status: String?) -> ClaudeSession.State {
        switch status {
        case "busy": return .busy
        case "shell": return .shell
        case "waiting": return .waitingForInput
        default: return .idle
        }
    }

    /// `kill(pid, 0)` sends no signal, just checks deliverability. EPERM
    /// still means "alive" (someone else's process — shouldn't happen for
    /// registry entries, but don't false-negative on it).
    static func pidAlive(_ pid: pid_t) -> Bool {
        kill(pid, 0) == 0 || errno == EPERM
    }

    // MARK: Transcript lookup (branch garnish only)

    /// Transcript path → the tail parsed at a given mtime. A busy session
    /// rewrites its transcript constantly, but an idle one doesn't, and
    /// re-reading 64 KB per session every two seconds was most of the
    /// app's resting CPU. Scans run off-main and may overlap, hence the lock.
    private static var tailCache: [String: (mtime: Date, tail: TranscriptTail?)] = [:]
    private static let tailCacheLock = NSLock()

    static func cachedTail(of url: URL, mtime: Date) -> TranscriptTail? {
        tailCacheLock.lock()
        let hit = tailCache[url.path]
        tailCacheLock.unlock()
        if let hit, hit.mtime == mtime { return hit.tail }
        let tail = readTail(of: url)
        tailCacheLock.lock()
        tailCache[url.path] = (mtime, tail)
        if tailCache.count > 256 { tailCache.removeAll() }  // sessions come and go; keep it bounded
        tailCacheLock.unlock()
        return tail
    }

    /// Claude Code names each project dir by flattening the cwd: every char
    /// that isn't alphanumeric or `-` becomes `-` (slashes included, so the
    /// name leads with one).
    static func projectDirName(forCwd cwd: String) -> String {
        String(cwd.map { $0.isLetter || $0.isNumber || $0 == "-" ? $0 : "-" })
    }

    struct TranscriptTail: Equatable {
        var sessionId: String? = nil
        var gitBranch: String? = nil
        /// Newest `{"type":"ai-title","aiTitle":…}` entry. Claude Code
        /// rewrites it every turn, so it's reliably inside the tail window.
        var aiTitle: String? = nil
    }

    /// Read the last chunk of the transcript and pull sessionId/gitBranch
    /// from the newest entry that carries them (one JSON object per line).
    static func readTail(of url: URL, maxBytes: Int = 65_536) -> TranscriptTail? {
        guard let handle = try? FileHandle(forReadingFrom: url) else { return nil }
        defer { try? handle.close() }
        let size = (try? handle.seekToEnd()) ?? 0
        let offset = size > UInt64(maxBytes) ? size - UInt64(maxBytes) : 0
        try? handle.seek(toOffset: offset)
        guard let data = try? handle.readToEnd(),
              let text = String(data: data, encoding: .utf8) else { return nil }
        return parseTail(text)
    }

    /// Split out from readTail so tests can feed synthetic transcripts.
    /// sessionId and gitBranch are hunted for independently, each from the
    /// newest entry that carries it — plenty of entries have a sessionId but
    /// no gitBranch (progress records, tool results), and taking the branch
    /// only off the newest sessionId entry made the label flicker in and
    /// out between polls.
    static func parseTail(_ text: String) -> TranscriptTail? {
        var tail = TranscriptTail()
        for line in text.split(separator: "\n").reversed() {
            guard let obj = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any]
            else { continue }  // first line may be truncated by the byte window
            if tail.sessionId == nil, let sid = obj["sessionId"] as? String {
                tail.sessionId = sid
            }
            if tail.gitBranch == nil, let branch = obj["gitBranch"] as? String, !branch.isEmpty {
                tail.gitBranch = branch
            }
            if tail.aiTitle == nil, obj["type"] as? String == "ai-title",
               let title = obj["aiTitle"] as? String, !title.isEmpty {
                tail.aiTitle = title
            }
            if tail.sessionId != nil && tail.gitBranch != nil && tail.aiTitle != nil { break }
        }
        return tail.sessionId == nil && tail.gitBranch == nil && tail.aiTitle == nil ? nil : tail
    }
}
