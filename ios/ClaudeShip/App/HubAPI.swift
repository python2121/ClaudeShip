import Foundation

// MARK: - What /api/state describes

struct HubState: Decodable, Equatable {
    var host: String
    var root: String
    var rootDisplay: String
    /// The Mac's home directory: where the top bar's quick "+" starts a
    /// session (absent from an older hub).
    var home: String?
    var defaultPermissionMode: String
    var permissionModes: [String]
    var now: Int
    var `protocol`: Int?
    var projects: [HubProject]
    var elsewhere: [HubSession]
    /// The hub relays permission prompts (protocol 3; absent before).
    var approvalsSupported: Bool?
    /// Every machine in this hub's swarm, local first (phase 10; absent
    /// from an older hub, which is then exactly one host: the fields above).
    var hosts: [HubHost]?

    /// Must match PROTOCOL in the hub (hub/src/frame.rs) and in web/app.js.
    static let protocolVersion = 3

    var allSessions: [HubSession] { projects.flatMap(\.sessions) + elsewhere }

    /// The hub's own machine as a host, from the top-level fields — what an
    /// older hub (no `hosts`) is in its entirety.
    var localHost: HubHost {
        HubHost(id: hosts?.first(where: { $0.local == true })?.id ?? "", name: host, local: true, reachable: true,
                lastSeen: nil, protocol: self.protocol, now: now, root: root, rootDisplay: rootDisplay, home: home,
                defaultPermissionMode: defaultPermissionMode, projects: projects, elsewhere: elsewhere,
                approvalsSupported: approvalsSupported)
    }

    /// The hosts to show: the swarm's, or the hub alone.
    var allHosts: [HubHost] {
        if let hosts, !hosts.isEmpty { return hosts }
        return [localHost]
    }

    /// The host with this id (nil: the hub's own).
    func host(_ id: String?) -> HubHost? {
        guard let id, !id.isEmpty else { return allHosts.first { $0.local == true } ?? localHost }
        return allHosts.first { $0.id == id }
    }

    /// A swarm hub: can enrol and be enrolled.
    var swarmCapable: Bool { hosts != nil }

    /// This hub's own host id in the swarm, when it reports one.
    var localHostId: String? { hosts?.first(where: { $0.local == true })?.id }
}

/// One machine of the swarm as the hub reports it (phase 10). Every field
/// but the id is optional: a hub may report a peer it has never heard from.
struct HubHost: Decodable, Identifiable, Equatable {
    var id: String
    var name: String?
    /// The hub answering is this machine.
    var local: Bool?
    var reachable: Bool?
    /// Epoch ms (the answering hub's clock) of the last answer from it.
    var lastSeen: Int?
    var `protocol`: Int?
    /// "now" on this host's clock when its state was taken (epoch ms).
    var now: Int?
    var root: String?
    var rootDisplay: String?
    var home: String?
    var defaultPermissionMode: String?
    var projects: [HubProject]?
    var elsewhere: [HubSession]?
    var approvalsSupported: Bool?

    var isLocal: Bool { local == true }
    var isReachable: Bool { local == true || reachable != false }
    var displayName: String { name.flatMap { $0.isEmpty ? nil : $0 } ?? String(id.prefix(8)) }
    var projectList: [HubProject] { projects ?? [] }
    var elsewhereList: [HubSession] { elsewhere ?? [] }
    var allSessions: [HubSession] { projectList.flatMap(\.sessions) + elsewhereList }
    /// What an action names as `host`: nothing for the answering hub's own
    /// machine (an older hub never sees the field), the id for a peer.
    var target: String? { isLocal || id.isEmpty ? nil : id }

    /// The session with this hub id and the project it sits in, if any.
    func session(hubId: String) -> (session: HubSession, project: HubProject?)? {
        for project in projectList {
            if let session = project.sessions.first(where: { $0.hubId == hubId }) { return (session, project) }
        }
        if let session = elsewhereList.first(where: { $0.hubId == hubId }) { return (session, nil) }
        return nil
    }
}

struct HubProject: Decodable, Identifiable, Equatable {
    var name: String
    var path: String
    var branch: String?
    var lastActivity: Int?
    var sessions: [HubSession]
    var recent: [HubConversation]
    var id: String { path }
}

struct HubSession: Decodable, Identifiable, Equatable {
    var key: String
    var hubId: String?
    var pid: Int
    var cwd: String
    var sub: String?
    var name: String?
    var title: String?
    var branch: String?
    var status: String
    var waitingFor: String?
    var since: Int?
    var startedAt: Int?
    var attachable: Bool
    var viewers: Int
    var mode: String?
    var background: Bool
    /// Claude's session UUID — what auto-approve rules are keyed by.
    var sessionId: String?
    /// Permission prompts waiting on an answer (protocol 3; absent before).
    var approvals: [HubApproval]?
    var autoApprove: HubAutoApprove?
    var id: String { key }

    var pendingApprovals: [HubApproval] { approvals ?? [] }
}

/// A permission prompt the hub is holding for a session. The terminal
/// prompt is live at the same time; whichever answers first wins.
struct HubApproval: Decodable, Identifiable, Equatable {
    var id: String
    var tool: String?
    var summary: String?
    var detail: String?
    /// Epoch ms on the hub's clock.
    var receivedAt: Int?

    /// "Bash: npm test" — the caption under the session's title. The hub's
    /// summary already starts with the tool's name; the bare tool is the
    /// fallback.
    var caption: String {
        [summary, tool].compactMap { $0?.isEmpty == false ? $0 : nil }.first ?? ""
    }
}

/// A standing "approve all" rule: until a time (epoch ms) or for the session.
struct HubAutoApprove: Decodable, Equatable {
    var until: Int?
    var session: Bool?

    func isActive(now: Date) -> Bool {
        if session == true { return true }
        guard let until else { return false }
        return Double(until) / 1000 > now.timeIntervalSince1970
    }
}

struct HubConversation: Decodable, Identifiable, Equatable {
    var sessionId: String
    var title: String
    var at: Int
    var id: String { sessionId }
}

/// Claude's permission modes, in the order the menus show them.
enum PermissionMode {
    static let known: [(mode: String, name: String, about: String)] = [
        ("auto", "Auto", "Works without asking, behind Claude's own safety checks."),
        ("acceptEdits", "Accept edits", "Edits files without asking. Still asks before running commands."),
        ("plan", "Plan", "Reads and plans; changes nothing until you approve."),
        ("manual", "Manual", "Asks before every edit and command."),
        ("bypassPermissions", "Bypass permissions", "Never asks. Everything is allowed."),
    ]
    static func name(_ mode: String) -> String { known.first { $0.mode == mode }?.name ?? mode }
}

// MARK: - Calls

enum HubError: Error, LocalizedError {
    case unpaired
    /// The transport failed; the detail says how, because "can't reach"
    /// alone once hid a permission problem on the phone for a day.
    case unreachable(String?)
    case refused(String)
    /// The hub wouldn't relay to a peer: 409 (a different build) or 502
    /// (can't reach it). `host` is the peer's id; the store names it.
    case proxy(ProxyRefusal)

    var errorDescription: String? {
        switch self {
        case .unpaired: return "This phone isn't paired with the hub."
        case .unreachable(let detail):
            return "Can't reach the hub on the Mac" + (detail.map { ": \($0)" } ?? ".")
        case .refused(let why): return why
        case .proxy(let refusal): return refusal.message(hostName: refusal.host ?? "the other machine")
        }
    }

    /// What URLSession's error means for a person holding the phone.
    static func unreachable(from error: Error) -> HubError {
        let nsError = error as NSError
        let posix = (nsError.userInfo[NSUnderlyingErrorKey] as? NSError).flatMap { $0.domain == NSPOSIXErrorDomain ? $0.code : nil }
        switch (nsError.domain, nsError.code, posix) {
        case (NSURLErrorDomain, NSURLErrorTimedOut, _):
            return .unreachable("no answer in time. Is Tailscale connected on both ends?")
        case (NSURLErrorDomain, _, Int(EHOSTUNREACH)), (NSURLErrorDomain, _, Int(ENETUNREACH)),
             (NSURLErrorDomain, _, Int(EPERM)), (NSURLErrorDomain, NSURLErrorNotConnectedToInternet, _):
            return .unreachable("the phone can't route to it. Check that Tailscale is on and that ClaudeShip is allowed Local Network access in Settings.")
        case (NSURLErrorDomain, NSURLErrorCannotConnectToHost, _):
            return .unreachable("the Mac refused the connection. Is the hub running (claudeship hub status)?")
        case (NSURLErrorDomain, NSURLErrorNetworkConnectionLost, _):
            return .unreachable("the Mac closed the connection without answering.")
        default:
            return .unreachable(nsError.localizedDescription)
        }
    }
}

/// A home hub's refusal to relay to a peer, as `/api/*` and `/ws/term` give it.
struct ProxyRefusal: Equatable {
    var mismatch: Bool
    var host: String?
    var theirs: Int?
    var ours: Int?

    /// From a 409 or 502 body (`{error, host, theirs?, ours?}`); nil for
    /// anything else.
    static func parse(_ data: Data, status: Int) -> ProxyRefusal? {
        guard status == 409 || status == 502 else { return nil }
        let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        let error = object["error"] as? String
        let mismatch = status == 409 && (error == nil || error == "protocol mismatch")
        guard mismatch || (status == 502 && (error == nil || error == "unreachable")) else { return nil }
        return ProxyRefusal(mismatch: mismatch, host: object["host"] as? String,
                            theirs: object["theirs"] as? Int, ours: object["ours"] as? Int)
    }

    func title(hostName: String) -> String {
        mismatch ? "\(hostName) runs a different build" : "Can't reach \(hostName)"
    }

    func message(hostName: String) -> String {
        if mismatch {
            let versions = theirs.map { t in ours.map { " (protocol \(t), this hub \($0))" } ?? "" } ?? ""
            return "\(hostName) runs a different build of the hub\(versions). Restart the hub on \(hostName) when its sessions can end: claudeship hub stop, then claudeship hub start."
        }
        return "Can't reach \(hostName) from this hub right now. It may be asleep or off; try again when it answers."
    }
}

extension HubConnection {
    private func send(_ request: URLRequest?) async throws -> (Data, Int) {
        guard let request else { throw HubError.unpaired }
        do {
            let (data, response) = try await session.data(for: request)
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            if status == 401 { throw HubError.unpaired }
            return (data, status)
        } catch let error as HubError {
            throw error
        } catch {
            throw HubError.unreachable(from: error)
        }
    }

    private func reason(_ data: Data, _ status: Int) -> String {
        let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        return object?["error"] as? String ?? "The hub refused (\(status))."
    }

    /// The error for a non-success answer: a proxy refusal when it is one.
    private func refusal(_ data: Data, _ status: Int) -> HubError {
        if let proxy = ProxyRefusal.parse(data, status: status) { return .proxy(proxy) }
        return .refused(reason(data, status))
    }

    /// Adds `host` to an action's body when it targets a peer.
    private func body(_ fields: [String: Any], host: String?) -> [String: Any] {
        var fields = fields
        if let host { fields["host"] = host }
        return fields
    }

    func state() async throws -> HubState {
        let (data, status) = try await send(request("/api/state"))
        guard status == 200 else { throw HubError.refused(reason(data, status)) }
        do {
            return try JSONDecoder().decode(HubState.self, from: data)
        } catch {
            throw HubError.refused("The hub answered in a form this app doesn't understand; it may be an older build.")
        }
    }

    /// Returns the new session's hub id.
    /// `host`: the swarm peer to run it on (nil: this hub's machine).
    func launch(path: String, mode: String?, resume: String?, host: String?) async throws -> String {
        var fields: [String: Any] = ["path": path]
        if let mode { fields["permissionMode"] = mode }
        if let resume { fields["resume"] = resume }
        let (data, status) = try await send(request("/api/launch", method: "POST", json: body(fields, host: host)))
        guard status == 200,
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let id = object["id"] as? String
        else { throw refusal(data, status) }
        return id
    }

    func end(hubId: String, host: String?) async throws {
        let (data, status) = try await send(request("/api/kill", method: "POST", json: body(["id": hubId], host: host)))
        guard status == 200 else { throw refusal(data, status) }
    }

    /// Answers a pending approval. Gone already (answered in the terminal,
    /// or by another screen) is not an error.
    func approve(id: String, allow: Bool, host: String?) async throws {
        let (data, status) = try await send(request("/api/approve", method: "POST", json: body(["id": id, "allow": allow], host: host)))
        guard status == 200 || status == 404 else { throw refusal(data, status) }
    }

    /// `rule`: "5m", "session", or "off".
    func autoApprove(sessionId: String, rule: String, host: String?) async throws {
        let (data, status) = try await send(request("/api/auto-approve", method: "POST",
                                                    json: body(["sessionId": sessionId, "rule": rule], host: host)))
        guard status == 200 else { throw refusal(data, status) }
    }

    func setDefaultMode(_ mode: String, host: String?) async throws {
        let (data, status) = try await send(request("/api/settings", method: "POST",
                                                    json: body(["defaultPermissionMode": mode], host: host)))
        guard status == 200 else { throw refusal(data, status) }
    }

    /// Swarm enrolment, step one, on a member: the swarm's secret and its
    /// peer records. The secret only ever passes through memory — never
    /// logged, never shown, never stored on the phone.
    func swarmInvite() async throws -> (secret: String, peers: Any) {
        let (data, status) = try await send(request("/api/swarm", method: "POST", json: [:]))
        guard status == 200,
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let secret = object["secret"] as? String, !secret.isEmpty
        else {
            throw status == 200 || status == 404
                ? HubError.refused("This hub can't share its swarm; it may be an older build.")
                : refusal(data, status)
        }
        // The peer records go to the joining hub as they came.
        return (secret, object["peers"] ?? [Any]())
    }

    /// Swarm enrolment, step two, on the joining hub.
    func swarmJoin(secret: String, peers: Any) async throws {
        let (data, status) = try await send(request("/api/swarm/join", method: "POST", json: ["secret": secret, "peers": peers]))
        guard status == 200 else {
            throw status == 404
                ? HubError.refused("This hub can't join a swarm; it may be an older build.")
                : refusal(data, status)
        }
    }

    /// Checks a pairing before it is kept: the state call either works or
    /// says why not.
    func probe(base: URL, token: String) async throws {
        guard let url = URL(string: "/api/state", relativeTo: base) else { throw HubError.unreachable(nil) }
        var request = URLRequest(url: url)
        request.cachePolicy = .reloadIgnoringLocalCacheData
        request.setValue("\(HubConnection.cookieName)=\(token)", forHTTPHeaderField: "Cookie")
        let (data, status) = try await send(request)
        guard status == 200 else { throw HubError.refused(reason(data, status)) }
    }
}
