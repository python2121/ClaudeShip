import Foundation

/// The menubar app's view of the session hub (the Rust `claudeship` binary,
/// `hub/`): an HTTP client of its loopback API, authenticated with the
/// pairing secret the hub keeps in its home directory. It reads three things
/// from `GET /api/state` — which registry sessions run on a hub pty (and
/// under which hub id), the permission requests pending per session, and
/// each session's standing auto-approve rule — and posts End, Approve/Deny,
/// and "Approve all". Every call is synchronous and blocking; call it off
/// the main thread. No hub answering is an ordinary state, not an error.
enum HubClient {
    /// The API version this app understands. A hub stating another one still
    /// gets its sessions listed and ended, but no approval buttons.
    static let protocolVersion = 3

    struct Approval: Equatable {
        var id: String
        var tool: String
        var summary: String
        var detail: String
        var receivedAt: Date
    }

    enum Rule: Equatable {
        case until(Date)
        case forSession
    }

    struct Session: Equatable {
        /// The Claude process's pid (the registry's), what the scanner joins on.
        var pid: pid_t
        var hubId: String?
        var sessionId: String?
        var approvals: [Approval] = []
        var autoApprove: Rule?
    }

    struct State: Equatable {
        var protocolVersion: Int
        var approvalsSupported: Bool
        var sessions: [Session]
        /// Approval data is trusted only from a hub speaking our protocol.
        var approvalsUsable: Bool { protocolVersion == HubClient.protocolVersion && approvalsSupported }
    }

    // MARK: Where the hub is

    static var home: URL {
        if let override = ProcessInfo.processInfo.environment["CLAUDESHIP_HOME"], !override.isEmpty {
            return URL(fileURLWithPath: override, isDirectory: true)
        }
        return FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/ClaudeShip/hub", isDirectory: true)
    }

    static func port(configData: Data?) -> Int {
        guard let data = configData,
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let port = obj["port"] as? Int, (1...65535).contains(port)
        else { return 7433 }
        return port
    }

    private static func token() -> String? {
        guard let data = try? Data(contentsOf: home.appendingPathComponent("token")),
              let text = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
              !text.isEmpty, text.allSatisfy({ $0.isLetter || $0.isNumber })
        else { return nil }
        return text
    }

    // MARK: Calls

    /// The hub's sessions, or nil when no hub answers (not running, not
    /// paired yet, or not speaking JSON).
    static func state() -> State? {
        guard let (status, body) = request("GET", "/api/state", body: nil, timeout: 1), status == 200 else { return nil }
        return parseState(body)
    }

    static func kill(_ hubId: String) -> Bool { post("/api/kill", ["id": hubId]) }
    /// 404 counts as delivered: the request is gone, answered in the
    /// terminal or by another screen first.
    static func approve(_ id: String, allow: Bool) -> Bool {
        post("/api/approve", ["id": id, "allow": allow], alsoOK: [404])
    }
    /// `rule`: "5m", "session", or "off".
    static func autoApprove(sessionId: String, rule: String) -> Bool {
        post("/api/auto-approve", ["sessionId": sessionId, "rule": rule])
    }

    private static func post(_ path: String, _ object: [String: Any], alsoOK: Set<Int> = []) -> Bool {
        guard let body = try? JSONSerialization.data(withJSONObject: object),
              let (status, _) = request("POST", path, body: body, timeout: 2) else { return false }
        return (200..<300).contains(status) || alsoOK.contains(status)
    }

    /// One request on a fresh ephemeral session: no cookie jar, no cache, no
    /// proxy. The token goes in the Cookie header and nowhere else (never
    /// logged). No Origin header: the hub demands a matching one only from
    /// browsers.
    private static func request(_ method: String, _ path: String, body: Data?, timeout: TimeInterval) -> (Int, Data)? {
        guard let token = token() else { return nil }
        let port = Self.port(configData: try? Data(contentsOf: home.appendingPathComponent("config.json")))
        guard let url = URL(string: "http://127.0.0.1:\(port)\(path)") else { return nil }
        var req = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: timeout)
        req.httpMethod = method
        req.httpShouldHandleCookies = false
        req.setValue("claude_ship=\(token)", forHTTPHeaderField: "Cookie")
        if let body {
            req.httpBody = body
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        let configuration = URLSessionConfiguration.ephemeral
        configuration.httpShouldSetCookies = false
        configuration.httpCookieStorage = nil
        configuration.urlCache = nil
        configuration.connectionProxyDictionary = [:]
        configuration.timeoutIntervalForRequest = timeout
        configuration.timeoutIntervalForResource = timeout
        let session = URLSession(configuration: configuration)
        defer { session.finishTasksAndInvalidate() }

        let done = DispatchSemaphore(value: 0)
        var result: (Int, Data)?
        session.dataTask(with: req) { data, response, _ in
            if let http = response as? HTTPURLResponse { result = (http.statusCode, data ?? Data()) }
            done.signal()
        }.resume()
        guard done.wait(timeout: .now() + timeout + 0.5) == .success else { return nil }
        return result
    }

    // MARK: Parsing (pure; self-tested)

    /// `/api/state` → the sessions under every project and `elsewhere`.
    /// Entries without a pid are skipped; unknown fields are ignored.
    static func parseState(_ data: Data) -> State? {
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        var entries: [[String: Any]] = []
        for project in obj["projects"] as? [[String: Any]] ?? [] {
            entries += project["sessions"] as? [[String: Any]] ?? []
        }
        entries += obj["elsewhere"] as? [[String: Any]] ?? []
        let sessions: [Session] = entries.compactMap { entry in
            guard let pid = entry["pid"] as? Int else { return nil }
            let approvals: [Approval] = (entry["approvals"] as? [[String: Any]] ?? []).compactMap { a in
                guard let id = a["id"] as? String, !id.isEmpty else { return nil }
                let tool = a["tool"] as? String ?? "unknown"
                let summary = a["summary"] as? String ?? tool
                return Approval(
                    id: id, tool: tool,
                    summary: summary.isEmpty ? tool : summary,
                    detail: a["detail"] as? String ?? summary,
                    receivedAt: (a["receivedAt"] as? Double).map { Date(timeIntervalSince1970: $0 / 1000) } ?? Date())
            }
            return Session(pid: pid_t(pid),
                           hubId: entry["hubId"] as? String,
                           sessionId: entry["sessionId"] as? String,
                           approvals: approvals,
                           autoApprove: parseRule(entry["autoApprove"]))
        }
        return State(protocolVersion: obj["protocol"] as? Int ?? 0,
                     approvalsSupported: obj["approvalsSupported"] as? Bool ?? false,
                     sessions: sessions)
    }

    static func parseRule(_ value: Any?) -> Rule? {
        guard let dict = value as? [String: Any] else { return nil }
        if let until = dict["until"] as? Double { return .until(Date(timeIntervalSince1970: until / 1000)) }
        if dict["session"] as? Bool == true { return .forSession }
        return nil
    }
}
