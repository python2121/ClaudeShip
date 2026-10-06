import Darwin
import Foundation

/// What the state builder needs to know about a hub-owned session, copied
/// off the hub queue so the (blocking) build can run elsewhere.
struct HubSessionSnapshot {
    let id: String
    let pid: pid_t
    let cwd: String
    let startedAt: Date
    let viewers: Int
    let permissionMode: String?
}

/// Builds the web app's view of the machine: every project directory under
/// the configured root, the Claude sessions running in each, and each
/// project's recent conversations. Blocking (directory listings, transcript
/// tails) — call only on `HubState.queue`, which also guards the cache.
enum HubState {
    static let queue = DispatchQueue(label: "claudeship.hub.state")

    /// Transcript path → the title read from it at a given mtime, so an
    /// unchanged transcript is never tail-read twice.
    private static var titleCache: [String: (mtime: Date, title: String?)] = [:]

    // MARK: Pure pieces

    /// Display order for the directory: projects with running sessions
    /// first, most instances first; then everything else, most recently
    /// active first, then name. Running projects with the same count go by
    /// name alone: their activity time moves every time a session writes,
    /// and cards that swap places between polls are worse than any order.
    static func order(_ projects: [(name: String, running: Int, lastActivity: Date?)]) -> [Int] {
        projects.indices.sorted { a, b in
            let x = projects[a], y = projects[b]
            if (x.running > 0) != (y.running > 0) { return x.running > 0 }
            if x.running != y.running { return x.running > y.running }
            if x.running == 0, x.lastActivity != y.lastActivity {
                guard let xa = x.lastActivity else { return false }
                guard let ya = y.lastActivity else { return true }
                return xa > ya
            }
            return x.name.localizedCaseInsensitiveCompare(y.name) == .orderedAscending
        }
    }

    /// The project a working directory belongs to: the one it equals or
    /// sits inside (a worktree, a subfolder). Longest match wins.
    static func projectIndex(forCwd cwd: String, paths: [String]) -> Int? {
        var best: Int?
        for (index, path) in paths.enumerated() where cwd == path || cwd.hasPrefix(path + "/") {
            if best == nil || path.count > paths[best!].count { best = index }
        }
        return best
    }

    /// Branch name from the text of `.git/HEAD`; a short hash when detached.
    static func branch(fromHEAD text: String) -> String? {
        let head = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if head.hasPrefix("ref: refs/heads/") { return String(head.dropFirst("ref: refs/heads/".count)) }
        if head.count >= 7, head.allSatisfy({ $0.isHexDigit }) { return String(head.prefix(7)) }
        return nil
    }

    /// A launch target must be a direct child directory of the root — the
    /// same set the directory page lists. Returns the canonical path.
    static func launchTarget(_ requested: String, root: String) -> String? {
        let path = URL(fileURLWithPath: requested).standardizedFileURL.path
        let rootPath = URL(fileURLWithPath: root).standardizedFileURL.path
        guard (path as NSString).deletingLastPathComponent == rootPath,
              !(path as NSString).lastPathComponent.hasPrefix(".")
        else { return nil }
        var isDirectory: ObjCBool = false
        guard FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory), isDirectory.boolValue
        else { return nil }
        return path
    }

    static func isSessionId(_ text: String) -> Bool {
        UUID(uuidString: text) != nil
    }

    // MARK: Build

    static func build(hubSessions: [HubSessionSnapshot], config: HubConfig) -> [String: Any] {
        let fm = FileManager.default
        let root = URL(fileURLWithPath: config.root, isDirectory: true).standardizedFileURL
        let directories = ((try? fm.contentsOfDirectory(
            at: root, includingPropertiesForKeys: [.isDirectoryKey], options: [.skipsHiddenFiles])) ?? [])
            .filter { (try? $0.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true }
            .map { $0.standardizedFileURL.path }
            .sorted()

        // Live sessions: the registry is the truth about status; the hub
        // adds which of them it owns (and can therefore attach to). A hub
        // session's pid is its supervisor, so the Claude it runs is the
        // registry session with that pid nearest in its ancestry — nearest,
        // because a Claude started *inside* that session (by a tool call)
        // has the same supervisor further up.
        let scanned = SessionScanner.scan()
        let hubPids = Set(hubSessions.map(\.pid))
        var owner: [pid_t: HubSessionSnapshot] = [:]
        var unmatched = hubSessions
        var candidates: [(depth: Int, pid: pid_t, hubPid: pid_t)] = []
        for session in scanned {
            let chain = TerminalFocus.ancestorPids(of: session.pid)
            if let depth = chain.firstIndex(where: hubPids.contains) {
                candidates.append((depth, session.pid, chain[depth]))
            }
        }
        for candidate in candidates.sorted(by: { ($0.depth, $0.pid) < ($1.depth, $1.pid) }) {
            if let index = unmatched.firstIndex(where: { $0.pid == candidate.hubPid }) {
                owner[candidate.pid] = unmatched.remove(at: index)
            }
        }

        var perProject: [[[String: Any]]] = Array(repeating: [], count: directories.count)
        var activity: [Date?] = Array(repeating: nil, count: directories.count)
        var elsewhere: [[String: Any]] = []
        var liveIds = Set<String>()

        func place(_ entry: [String: Any], cwd: String, active: Date?) {
            guard let index = projectIndex(forCwd: cwd, paths: directories) else {
                elsewhere.append(entry)
                return
            }
            var entry = entry
            if cwd != directories[index] { entry["sub"] = String(cwd.dropFirst(directories[index].count + 1)) }
            perProject[index].append(entry)
            if let active, activity[index].map({ active > $0 }) ?? true { activity[index] = active }
        }

        for session in scanned {
            if let id = session.sessionId { liveIds.insert(id) }
            let hub = owner[session.pid]
            let status: String
            switch session.state {
            case .busy: status = "busy"
            case .shell: status = "shell"
            case .idle: status = "idle"
            case .waitingForInput: status = "waiting"
            }
            var entry: [String: Any] = [
                "key": hub.map { "h:\($0.id)" } ?? "p:\(session.pid)",
                "pid": Int(session.pid),
                "cwd": session.cwd,
                "status": status,
                "attachable": hub != nil,
                "background": session.isBackground,
                "viewers": hub?.viewers ?? 0,
            ]
            entry["hubId"] = hub?.id
            entry["mode"] = hub?.permissionMode
            entry["name"] = session.name
            entry["title"] = session.title
            entry["branch"] = session.gitBranch
            entry["waitingFor"] = session.waitingFor
            entry["since"] = session.stateSince.map(ms)
            entry["startedAt"] = (session.startedAt ?? hub?.startedAt).map(ms)
            place(entry, cwd: session.cwd,
                  active: [session.lastActivity, session.stateSince, session.startedAt].compactMap { $0 }.max())
        }
        // Hub sessions Claude hasn't registered yet: still starting up, or
        // parked on a first-run prompt. Attachable all the same.
        for hub in unmatched {
            var entry: [String: Any] = [
                "key": "h:\(hub.id)", "hubId": hub.id, "pid": Int(hub.pid), "cwd": hub.cwd,
                "status": "starting", "attachable": true, "background": false, "viewers": hub.viewers,
                "startedAt": ms(hub.startedAt), "since": ms(hub.startedAt),
            ]
            entry["mode"] = hub.permissionMode
            place(entry, cwd: hub.cwd, active: hub.startedAt)
        }

        var projects: [[String: Any]] = []
        var keys: [(name: String, running: Int, lastActivity: Date?)] = []
        var seenTranscripts = Set<String>()
        for (index, path) in directories.enumerated() {
            let transcripts = transcripts(forProject: path)
            for transcript in transcripts { seenTranscripts.insert(transcript.path) }
            let newest = transcripts.first?.mtime
            let last = [activity[index], newest].compactMap { $0 }.max()

            var recent: [[String: Any]] = []
            for transcript in transcripts.prefix(8) where recent.count < 3 {
                guard !liveIds.contains(transcript.sessionId),
                      let title = title(of: transcript.path, mtime: transcript.mtime)
                else { continue }
                recent.append(["sessionId": transcript.sessionId, "title": title, "at": ms(transcript.mtime)])
            }

            let sessions = perProject[index].sorted {
                (($0["startedAt"] as? Int) ?? 0, ($0["pid"] as? Int) ?? 0)
                    < (($1["startedAt"] as? Int) ?? 0, ($1["pid"] as? Int) ?? 0)
            }
            var project: [String: Any] = [
                "name": (path as NSString).lastPathComponent,
                "path": path,
                "sessions": sessions,
                "recent": recent,
            ]
            project["branch"] = (try? String(contentsOfFile: path + "/.git/HEAD", encoding: .utf8)).flatMap(branch(fromHEAD:))
            project["lastActivity"] = last.map(ms)
            projects.append(project)
            keys.append(((path as NSString).lastPathComponent, sessions.count, last))
        }
        titleCache = titleCache.filter { seenTranscripts.contains($0.key) }

        var host = [CChar](repeating: 0, count: 256)
        gethostname(&host, host.count)
        var hostname = String(cString: host)
        if hostname.hasSuffix(".local") { hostname.removeLast(".local".count) }

        return [
            "host": hostname,
            "protocol": HubFrame.version,
            "root": root.path,
            "rootDisplay": (root.path as NSString).abbreviatingWithTildeInPath,
            "defaultPermissionMode": config.defaultPermissionMode,
            "permissionModes": HubConfig.permissionModes,
            "now": ms(Date()),
            "projects": order(keys).map { projects[$0] },
            "elsewhere": elsewhere,
        ]
    }

    private static func ms(_ date: Date) -> Int {
        Int(date.timeIntervalSince1970 * 1000)
    }

    /// A project's transcripts, newest first.
    private static func transcripts(forProject path: String) -> [(path: String, sessionId: String, mtime: Date)] {
        let directory = SessionScanner.projectsRoot
            .appendingPathComponent(SessionScanner.projectDirName(forCwd: path), isDirectory: true)
        guard let files = try? FileManager.default.contentsOfDirectory(
            at: directory, includingPropertiesForKeys: [.contentModificationDateKey], options: [.skipsHiddenFiles])
        else { return [] }
        return files.compactMap { file -> (path: String, sessionId: String, mtime: Date)? in
            guard file.pathExtension == "jsonl",
                  let mtime = try? file.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate
            else { return nil }
            return (file.path, file.deletingPathExtension().lastPathComponent, mtime)
        }
        .sorted { $0.mtime > $1.mtime }
    }

    private static func title(of path: String, mtime: Date) -> String? {
        if let cached = titleCache[path], cached.mtime == mtime { return cached.title }
        let title = SessionScanner.readTail(of: URL(fileURLWithPath: path))?.aiTitle
        titleCache[path] = (mtime, title)
        return title
    }
}
