import Foundation

// MARK: - What /api/state describes

struct HubState: Decodable {
    var host: String
    var root: String
    var rootDisplay: String
    var defaultPermissionMode: String
    var permissionModes: [String]
    var now: Int
    var `protocol`: Int?
    var projects: [HubProject]
    var elsewhere: [HubSession]

    /// Must match HubFrame.version in the hub (and PROTOCOL in web/app.js).
    static let protocolVersion = 2

    var allSessions: [HubSession] { projects.flatMap(\.sessions) + elsewhere }
}

struct HubProject: Decodable, Identifiable {
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
    var id: String { key }
}

struct HubConversation: Decodable, Identifiable {
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
    case unreachable
    case refused(String)

    var errorDescription: String? {
        switch self {
        case .unpaired: return "This phone isn't paired with the hub."
        case .unreachable: return "Can't reach the hub on the Mac."
        case .refused(let why): return why
        }
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
            throw HubError.unreachable
        }
    }

    private func reason(_ data: Data, _ status: Int) -> String {
        let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        return object?["error"] as? String ?? "The hub refused (\(status))."
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
    func launch(path: String, mode: String?, resume: String?) async throws -> String {
        var body: [String: Any] = ["path": path]
        if let mode { body["permissionMode"] = mode }
        if let resume { body["resume"] = resume }
        let (data, status) = try await send(request("/api/launch", method: "POST", json: body))
        guard status == 200,
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let id = object["id"] as? String
        else { throw HubError.refused(reason(data, status)) }
        return id
    }

    func end(hubId: String) async throws {
        let (data, status) = try await send(request("/api/kill", method: "POST", json: ["id": hubId]))
        guard status == 200 else { throw HubError.refused(reason(data, status)) }
    }

    func setDefaultMode(_ mode: String) async throws {
        let (data, status) = try await send(request("/api/settings", method: "POST", json: ["defaultPermissionMode": mode]))
        guard status == 200 else { throw HubError.refused(reason(data, status)) }
    }

    /// Checks a pairing before it is kept: the state call either works or
    /// says why not.
    func probe(base: URL, token: String) async throws {
        guard let url = URL(string: "/api/state", relativeTo: base) else { throw HubError.unreachable }
        var request = URLRequest(url: url)
        request.cachePolicy = .reloadIgnoringLocalCacheData
        request.setValue("\(HubConnection.cookieName)=\(token)", forHTTPHeaderField: "Cookie")
        let (data, status) = try await send(request)
        guard status == 200 else { throw HubError.refused(reason(data, status)) }
    }
}
