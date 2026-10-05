import CryptoKit
import Darwin
import Foundation
import Network
import Security

// MARK: - HTTP

struct HubHTTPRequest: Equatable {
    var method: String
    var path: String
    var query: [String: String]
    /// Header names lowercased.
    var headers: [String: String]
    var body: Data
}

enum HubHTTP {
    enum ParseResult: Equatable {
        case incomplete
        case invalid
        /// The request, and how many bytes of the buffer it used.
        case request(HubHTTPRequest, consumed: Int)
    }

    static let maxHead = 65_536
    static let maxBody = 1 << 20

    /// Parse one request from the start of `buffer`.
    static func parse(_ buffer: Data) -> ParseResult {
        guard let headEnd = buffer.range(of: Data("\r\n\r\n".utf8)) else {
            return buffer.count > maxHead ? .invalid : .incomplete
        }
        guard headEnd.lowerBound - buffer.startIndex <= maxHead,
              let head = String(data: buffer[buffer.startIndex..<headEnd.lowerBound], encoding: .utf8)
        else { return .invalid }
        var lines = head.components(separatedBy: "\r\n")
        let requestLine = lines.removeFirst().split(separator: " ")
        guard requestLine.count == 3, requestLine[2].hasPrefix("HTTP/1.") else { return .invalid }

        let target = String(requestLine[1])
        let parts = target.split(separator: "?", maxSplits: 1, omittingEmptySubsequences: false)
        guard let path = String(parts[0]).removingPercentEncoding, path.hasPrefix("/") else { return .invalid }
        var query: [String: String] = [:]
        if parts.count > 1 {
            for pair in parts[1].split(separator: "&") {
                let kv = pair.split(separator: "=", maxSplits: 1, omittingEmptySubsequences: false)
                guard let key = String(kv[0]).removingPercentEncoding else { continue }
                query[key] = kv.count > 1 ? (String(kv[1]).removingPercentEncoding ?? "") : ""
            }
        }

        var headers: [String: String] = [:]
        for line in lines {
            guard let colon = line.firstIndex(of: ":") else { return .invalid }
            headers[line[..<colon].lowercased()] = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
        }

        var body = Data()
        if let lengthText = headers["content-length"] {
            guard let length = Int(lengthText), length >= 0, length <= maxBody else { return .invalid }
            guard buffer.endIndex - headEnd.upperBound >= length else { return .incomplete }
            body = Data(buffer[headEnd.upperBound..<(headEnd.upperBound + length)])
        }
        return .request(
            HubHTTPRequest(method: String(requestLine[0]), path: path, query: query, headers: headers, body: body),
            consumed: headEnd.upperBound - buffer.startIndex + body.count)
    }

    static func response(status: Int, reason: String, contentType: String, body: Data, extra: [String] = []) -> Data {
        var head = "HTTP/1.1 \(status) \(reason)\r\n"
        head += "Content-Type: \(contentType)\r\n"
        head += "Content-Length: \(body.count)\r\n"
        head += "Connection: close\r\n"
        head += "Cache-Control: no-store\r\n"
        head += "X-Content-Type-Options: nosniff\r\n"
        head += "Referrer-Policy: no-referrer\r\n"
        for line in extra { head += line + "\r\n" }
        head += "\r\n"
        var data = Data(head.utf8)
        data.append(body)
        return data
    }
}

// MARK: - Who may connect

/// The web app is a remote terminal, so three independent things must all
/// hold before a request does anything: the connection came over loopback
/// or the Tailscale interface, the browser addressed us by a name that DNS
/// cannot be made to lie about, and the request carries this hub's pairing
/// secret.
enum HubWebSecurity {
    /// IPv4-mapped IPv6 (::ffff:a.b.c.d) as the four IPv4 bytes; anything
    /// else unchanged.
    static func canonical(_ bytes: [UInt8]) -> [UInt8] {
        if bytes.count == 16, bytes[0..<10].allSatisfy({ $0 == 0 }), bytes[10] == 0xff, bytes[11] == 0xff {
            return Array(bytes[12..<16])
        }
        return bytes
    }

    static func isLoopback(_ raw: [UInt8]) -> Bool {
        let bytes = canonical(raw)
        if bytes.count == 4 { return bytes[0] == 127 }
        return bytes.count == 16 && bytes[0..<15].allSatisfy({ $0 == 0 }) && bytes[15] == 1
    }

    /// Tailscale's address ranges: 100.64.0.0/10 and fd7a:115c:a1e0::/48.
    static func isTailnet(_ raw: [UInt8]) -> Bool {
        let bytes = canonical(raw)
        if bytes.count == 4 { return bytes[0] == 100 && bytes[1] & 0xc0 == 64 }
        return bytes.count == 16 && Array(bytes[0..<6]) == [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0]
    }

    /// Whether a connection between these two addresses may be served.
    /// Loopback talks only to loopback. Otherwise the address we were
    /// reached on must be one a tunnel interface actually holds — the
    /// 100.64/10 range alone proves nothing, since carriers and some Wi-Fi
    /// networks hand out the same range on the LAN — and the peer must be
    /// in the tailnet ranges too. `tunnel` is `tunnelAddresses()`.
    static func isAllowedPair(local: [UInt8], remote: [UInt8], tunnel: [[UInt8]]) -> Bool {
        if isLoopback(local) { return isLoopback(remote) }
        let local = canonical(local)
        return isTailnet(local) && tunnel.contains(local) && isTailnet(remote)
    }

    /// The addresses assigned to this machine's tunnel (utun) interfaces,
    /// which is where Tailscale lives.
    static func tunnelAddresses() -> [[UInt8]] {
        var list: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&list) == 0 else { return [] }
        defer { freeifaddrs(list) }
        var found: [[UInt8]] = []
        var cursor = list
        while let entry = cursor {
            defer { cursor = entry.pointee.ifa_next }
            guard String(cString: entry.pointee.ifa_name).hasPrefix("utun"),
                  let address = entry.pointee.ifa_addr
            else { continue }
            if address.pointee.sa_family == sa_family_t(AF_INET) {
                found.append(address.withMemoryRebound(to: sockaddr_in.self, capacity: 1) {
                    withUnsafeBytes(of: $0.pointee.sin_addr) { Array($0) }
                })
            } else if address.pointee.sa_family == sa_family_t(AF_INET6) {
                found.append(address.withMemoryRebound(to: sockaddr_in6.self, capacity: 1) {
                    withUnsafeBytes(of: $0.pointee.sin6_addr) { Array($0) }
                })
            }
        }
        return found
    }

    static func addressBytes(_ literal: String) -> [UInt8]? {
        var v4 = in_addr()
        if inet_pton(AF_INET, literal, &v4) == 1 {
            return withUnsafeBytes(of: &v4) { Array($0) }
        }
        var v6 = in6_addr()
        if inet_pton(AF_INET6, literal, &v6) == 1 {
            return withUnsafeBytes(of: &v6) { Array($0) }
        }
        return nil
    }

    /// The name the browser used for us, from the Host header. Only names
    /// that never go through DNS are accepted — `localhost` and loopback or
    /// tailnet address literals — because over plain HTTP any other name
    /// can be answered by whoever runs the network's DNS, first with their
    /// own server and then with 127.0.0.1 (DNS rebinding), and the page
    /// they served would then be "same-origin" with us, cookie and all.
    /// `extra` is the config's exact-match escape hatch (`allowedHosts`),
    /// for a name the user vouches for, e.g. behind `tailscale serve`.
    static func isAllowedHost(_ header: String, extra: [String] = []) -> Bool {
        var host = header.lowercased()
        var port: Substring = ""
        if host.hasPrefix("[") {
            guard let end = host.firstIndex(of: "]") else { return false }
            let rest = host[host.index(after: end)...]
            guard rest.isEmpty || rest.hasPrefix(":") else { return false }
            port = rest.dropFirst()
            host = String(host[host.index(after: host.startIndex)..<end])
        } else if let colon = host.lastIndex(of: ":") {
            port = host[host.index(after: colon)...]
            host = String(host[..<colon])
        }
        // Nothing but a port number may follow the name: the header is
        // echoed into a response header later.
        guard !host.isEmpty, port.count <= 5, port.allSatisfy({ $0.isASCII && $0.isNumber }) else { return false }
        if host == "localhost" { return true }
        if let bytes = addressBytes(host) { return isLoopback(bytes) || isTailnet(bytes) }
        return extra.contains(host)
    }

    /// A browser states the page a request came from in Origin. One that
    /// isn't us is another site reaching in — WebSockets in particular are
    /// not covered by the same-origin policy, so this check is the guard.
    /// No Origin at all means not a browser (curl, a script); those still
    /// need the pairing secret.
    static func isSameOrigin(origin: String?, host: String?) -> Bool {
        guard let origin else { return true }
        guard let host, let url = URL(string: origin), var name = url.host else { return false }
        if name.contains(":") { name = "[\(name)]" }
        let authority = url.port.map { "\(name):\($0)" } ?? name
        return authority.lowercased() == host.lowercased()
    }

    static func cookie(named name: String, in header: String?) -> String? {
        for pair in (header ?? "").split(separator: ";") {
            let kv = pair.trimmingCharacters(in: .whitespaces).split(separator: "=", maxSplits: 1)
            if kv.count == 2, kv[0] == name { return String(kv[1]) }
        }
        return nil
    }

    /// Comparison that takes the same time wherever the strings differ.
    static func constantTimeEquals(_ a: String, _ b: String) -> Bool {
        let x = Array(a.utf8), y = Array(b.utf8)
        var difference = UInt8(x.count == y.count ? 0 : 1)
        for i in 0..<max(x.count, y.count) {
            difference |= (i < x.count ? x[i] : 0) ^ (i < y.count ? y[i] : 0)
        }
        return difference == 0
    }
}

/// The pairing secret: a random token only this user can read. A browser
/// presents it once (the link from `claudeandrew hub link`) and gets it
/// back as a cookie. Network position alone is not a login — other users
/// of this Mac, a sandboxed app, or anything forwarding a port can all
/// reach 127.0.0.1.
enum HubToken {
    static let cookieName = "claude_hub"
    static var path: String { HubPaths.home.appendingPathComponent("token").path }

    /// The existing token, or a newly minted one. nil only if the hub's
    /// directory can't be written, in which case nothing can authenticate.
    static func loadOrCreate() -> String? {
        if let existing = try? String(contentsOfFile: path, encoding: .utf8) {
            let token = existing.trimmingCharacters(in: .whitespacesAndNewlines)
            if token.count >= 32 { return token }
        }
        return create()
    }

    /// A fresh token, replacing any on disk.
    static func create() -> String? {
        var bytes = [UInt8](repeating: 0, count: 32)
        guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else { return nil }
        let token = bytes.map { String(format: "%02x", $0) }.joined()
        HubPaths.ensureHome()
        // Written whole or not at all: a reader never sees half a secret.
        let staging = path + ".new"
        unlink(staging)
        let fd = open(staging, O_WRONLY | O_CREAT | O_EXCL, 0o600)
        guard fd >= 0 else { return nil }
        let written = HubIO.writeAll(fd, Data(token.utf8))
        close(fd)
        guard written, rename(staging, path) == 0 else {
            unlink(staging)
            return nil
        }
        return token
    }

    static func read() -> String? {
        (try? String(contentsOfFile: path, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

// MARK: - WebSocket

enum WebSocketCodec {
    static func acceptKey(_ key: String) -> String {
        let digest = Insecure.SHA1.hash(data: Data((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").utf8))
        return Data(digest).base64EncodedString()
    }

    /// A server-to-client frame (never masked).
    static func encode(opcode: UInt8, payload: Data) -> Data {
        var frame = Data(capacity: payload.count + 10)
        frame.append(0x80 | opcode)
        if payload.count < 126 {
            frame.append(UInt8(payload.count))
        } else if payload.count <= 0xffff {
            frame.append(126)
            frame.append(contentsOf: [UInt8(payload.count >> 8), UInt8(payload.count & 0xff)])
        } else {
            frame.append(127)
            let n = UInt64(payload.count)
            for shift in stride(from: 56, through: 0, by: -8) { frame.append(UInt8((n >> UInt64(shift)) & 0xff)) }
        }
        frame.append(payload)
        return frame
    }

    static func text(_ object: [String: Any]) -> Data {
        encode(opcode: 1, payload: (try? JSONSerialization.data(withJSONObject: object)) ?? Data("{}".utf8))
    }
}

/// Reassembles client-to-server WebSocket messages from a byte stream.
struct WebSocketParser {
    struct Message: Equatable {
        /// 1 text, 2 binary, 8 close, 9 ping, 10 pong.
        var opcode: UInt8
        var payload: Data
    }

    static let maxMessage = 8 << 20
    private var buffer = Data()
    private var fragmentOpcode: UInt8?
    private var fragment = Data()

    /// Complete messages now available; nil on a protocol violation.
    mutating func feed(_ data: Data) -> [Message]? {
        buffer.append(data)
        var messages: [Message] = []
        var offset = buffer.startIndex
        // Only when something was consumed: a large frame arriving in small
        // reads must not be recopied whole on every one of them.
        defer { if offset != buffer.startIndex { buffer = Data(buffer[offset...]) } }
        while buffer.endIndex - offset >= 2 {
            let b0 = buffer[offset], b1 = buffer[offset + 1]
            let fin = b0 & 0x80 != 0
            let opcode = b0 & 0x0f
            // Extensions are never negotiated, and clients must mask.
            guard b0 & 0x70 == 0, b1 & 0x80 != 0 else { return nil }
            var length = Int(b1 & 0x7f)
            var header = 2
            if length == 126 {
                guard buffer.endIndex - offset >= 4 else { break }
                length = Int(buffer[offset + 2]) << 8 | Int(buffer[offset + 3])
                header = 4
            } else if length == 127 {
                guard buffer.endIndex - offset >= 10 else { break }
                var wide: UInt64 = 0
                for i in 2..<10 { wide = wide << 8 | UInt64(buffer[offset + i]) }
                guard wide <= UInt64(Self.maxMessage) else { return nil }
                length = Int(wide)
                header = 10
            }
            guard length <= Self.maxMessage else { return nil }
            guard buffer.endIndex - offset >= header + 4 + length else { break }
            let maskStart = offset + header
            let mask = [buffer[maskStart], buffer[maskStart + 1], buffer[maskStart + 2], buffer[maskStart + 3]]
            var payload = Data(buffer[(maskStart + 4)..<(maskStart + 4 + length)])
            payload.withUnsafeMutableBytes { raw in
                for i in 0..<raw.count { raw[i] ^= mask[i & 3] }
            }
            offset = maskStart + 4 + length

            if opcode >= 8 {
                guard fin, length < 126 else { return nil }
                messages.append(Message(opcode: opcode, payload: payload))
            } else if opcode == 0 {
                guard let started = fragmentOpcode, fragment.count + payload.count <= Self.maxMessage else { return nil }
                fragment.append(payload)
                if fin {
                    messages.append(Message(opcode: started, payload: fragment))
                    fragmentOpcode = nil
                    fragment = Data()
                }
            } else if opcode == 1 || opcode == 2 {
                guard fragmentOpcode == nil else { return nil }
                if fin {
                    messages.append(Message(opcode: opcode, payload: payload))
                } else {
                    fragmentOpcode = opcode
                    fragment = payload
                }
            } else {
                return nil
            }
        }
        return messages
    }
}

// MARK: - Server

/// The hub's web face: the project directory page, its JSON API, and one
/// WebSocket per browser terminal. Everything runs on the hub queue.
final class HubWebServer {
    private unowned let hub: Hub
    private var listener: NWListener?
    private var connections: [ObjectIdentifier: HubWebConnection] = [:]
    private(set) var listening = false

    /// The pairing secret every API and terminal request must carry.
    private(set) var token = HubToken.loadOrCreate()
    private var loggedListenFailure = false

    /// Replace the pairing secret, un-pairing every browser, and hang up
    /// the terminals they have open.
    func rotateToken() -> Bool {
        token = HubToken.create()
        for connection in Array(connections.values) { connection.drop() }
        return token != nil
    }
    private var sweepTimer: DispatchSourceTimer?
    static let maxConnections = 128

    private var cachedState: (at: Date, data: Data)?
    private var stateWaiters: [(Data) -> Void] = []
    private var buildingState = false

    init(hub: Hub) {
        self.hub = hub
    }

    var queue: DispatchQueue { hub.queue }

    func start() {
        let tcp = NWProtocolTCP.Options()
        tcp.noDelay = true
        // A phone that walks out of range never says goodbye; keepalive is
        // what eventually notices and frees its attachment.
        tcp.enableKeepalive = true
        tcp.keepaliveIdle = 30
        tcp.keepaliveInterval = 10
        tcp.keepaliveCount = 4
        let parameters = NWParameters(tls: nil, tcp: tcp)
        parameters.allowLocalEndpointReuse = true
        guard let port = NWEndpoint.Port(rawValue: UInt16(hub.config.port)),
              let listener = try? NWListener(using: parameters, on: port)
        else {
            if !loggedListenFailure { HubLog.log("web: cannot listen on port \(hub.config.port); retrying quietly") }
            loggedListenFailure = true
            retryLater()
            return
        }
        listener.newConnectionHandler = { [weak self] connection in
            guard let self else { return }
            // Each connection is a descriptor the hub also needs for ptys.
            guard self.connections.count < Self.maxConnections else {
                connection.cancel()
                return
            }
            let web = HubWebConnection(connection: connection, server: self, hub: self.hub)
            self.connections[ObjectIdentifier(web)] = web
            web.start()
        }
        listener.stateUpdateHandler = { [weak self, weak listener] state in
            guard let self, let listener, self.listener === listener else { return }
            switch state {
            case .ready:
                self.listening = true
                self.loggedListenFailure = false
                HubLog.log("web: listening on port \(self.hub.config.port)")
            case .failed(let error):
                if !self.loggedListenFailure { HubLog.log("web: listener failed: \(error); retrying quietly") }
                self.loggedListenFailure = true
                self.listening = false
                listener.cancel()
                self.listener = nil
                self.retryLater()
            default:
                break
            }
        }
        self.listener = listener
        listener.start(queue: queue)

        if sweepTimer == nil {
            // Drops connections that stopped making progress: a request
            // that never completes, a peer that stopped reading.
            let timer = DispatchSource.makeTimerSource(queue: queue)
            timer.schedule(deadline: .now() + 5, repeating: 5)
            timer.setEventHandler { [weak self] in
                guard let self else { return }
                let now = Date()
                for connection in Array(self.connections.values) { connection.sweep(now: now) }
            }
            timer.resume()
            sweepTimer = timer
        }
    }

    private func retryLater() {
        queue.asyncAfter(deadline: .now() + 10) { [weak self] in
            guard let self, self.listener == nil else { return }
            self.start()
        }
    }

    func remove(_ connection: HubWebConnection) {
        connections[ObjectIdentifier(connection)] = nil
    }

    /// The directory state as JSON. Built off the hub queue (it reads the
    /// disk), at most once a second however many browsers are polling.
    func state(_ completion: @escaping (Data) -> Void) {
        if let cachedState, Date().timeIntervalSince(cachedState.at) < 1 {
            completion(cachedState.data)
            return
        }
        stateWaiters.append(completion)
        guard !buildingState else { return }
        buildingState = true
        let snapshots = hub.sessions.map {
            HubSessionSnapshot(id: $0.id, pid: $0.pid, cwd: $0.cwd, startedAt: $0.startedAt,
                               viewers: $0.attachments.count, permissionMode: $0.permissionMode)
        }
        let config = hub.config
        HubState.queue.async { [weak self] in
            let object = HubState.build(hubSessions: snapshots, config: config)
            let data = (try? JSONSerialization.data(withJSONObject: object)) ?? Data("{}".utf8)
            self?.queue.async {
                guard let self else { return }
                self.cachedState = (Date(), data)
                self.buildingState = false
                let waiters = self.stateWaiters
                self.stateWaiters = []
                for waiter in waiters { waiter(data) }
            }
        }
    }

    func invalidateState() {
        cachedState = nil
    }

    /// The directory holding index.html and friends: inside the app bundle
    /// when installed, the repo's `web/` when run from a build directory.
    static let assetRoot: URL? = {
        let fm = FileManager.default
        if let override = ProcessInfo.processInfo.environment["CLAUDEANDREW_WEB"], !override.isEmpty {
            return URL(fileURLWithPath: override, isDirectory: true)
        }
        var size = UInt32(0)
        _NSGetExecutablePath(nil, &size)
        var buffer = [CChar](repeating: 0, count: Int(size))
        guard _NSGetExecutablePath(&buffer, &size) == 0 else { return nil }
        var directory = URL(fileURLWithPath: String(cString: buffer)).resolvingSymlinksInPath().deletingLastPathComponent()
        let bundled = directory.deletingLastPathComponent().appendingPathComponent("Resources/web", isDirectory: true)
        if fm.fileExists(atPath: bundled.appendingPathComponent("index.html").path) { return bundled }
        for _ in 0..<6 {
            let candidate = directory.appendingPathComponent("web", isDirectory: true)
            if fm.fileExists(atPath: candidate.appendingPathComponent("index.html").path) { return candidate }
            directory = directory.deletingLastPathComponent()
        }
        return nil
    }()
}

/// One accepted TCP connection: a single HTTP request, or a WebSocket that
/// stays open as a terminal attachment.
final class HubWebConnection: HubAttachment {
    private let connection: NWConnection
    private unowned let server: HubWebServer
    private unowned let hub: Hub
    private var buffer = Data()
    private var parser: WebSocketParser?
    private weak var session: HubSession?
    private var queuedBytes = 0
    private var closed = false
    private var responded = false
    private let openedAt = Date()
    /// When queued output last drained a little.
    private var lastProgress = Date()
    var rows: UInt16 = 24
    var cols: UInt16 = 80
    private static let maxQueuedBytes = 32 << 20

    init(connection: NWConnection, server: HubWebServer, hub: Hub) {
        self.connection = connection
        self.server = server
        self.hub = hub
    }

    func start() {
        connection.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                guard self.peersAllowed() else {
                    self.finish()
                    return
                }
                self.receive()
            case .failed, .cancelled:
                self.finish()
            default:
                break
            }
        }
        connection.start(queue: server.queue)
    }

    /// Called periodically: give up on a plain request that hasn't finished
    /// in fifteen seconds (they take milliseconds; anything slower is
    /// holding one of a limited number of slots), and on a terminal whose
    /// peer has read nothing for a minute while output piled up.
    func sweep(now: Date) {
        if parser == nil {
            // A request that hasn't even been answered yet; one that is
            // mid-response (xterm.js over a thin link) gets the stall rule.
            if !responded, now.timeIntervalSince(openedAt) > 15 { finish() }
        } else if queuedBytes > 0, now.timeIntervalSince(lastProgress) > 60 {
            finish()
        }
    }

    /// See `HubWebSecurity.isAllowedPair`: loopback to loopback, or tailnet
    /// peer to an address our tunnel interface holds.
    private func peersAllowed() -> Bool {
        func bytes(_ endpoint: NWEndpoint?) -> [UInt8]? {
            guard case .hostPort(let host, _)? = endpoint else { return nil }
            switch host {
            case .ipv4(let address): return [UInt8](address.rawValue)
            case .ipv6(let address): return [UInt8](address.rawValue)
            default: return nil
            }
        }
        guard let remote = bytes(connection.currentPath?.remoteEndpoint ?? connection.endpoint),
              let local = bytes(connection.currentPath?.localEndpoint)
        else { return false }
        return HubWebSecurity.isAllowedPair(local: local, remote: remote, tunnel: HubWebSecurity.tunnelAddresses())
    }

    private func receive() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { [weak self] data, _, complete, error in
            guard let self, !self.closed else { return }
            if let data, !data.isEmpty { self.received(data) }
            if complete || error != nil {
                self.finish()
            } else if !self.closed {
                self.receive()
            }
        }
    }

    private func received(_ data: Data) {
        if parser != nil {
            webSocketBytes(data)
            return
        }
        guard !responded else { return }
        buffer.append(data)
        switch HubHTTP.parse(buffer) {
        case .incomplete:
            break
        case .invalid:
            respond(400, "Bad Request", text: "bad request")
        case .request(let request, let consumed):
            // Anything past the request is the start of the WebSocket
            // stream, if this turns into one.
            let leftover = Data(buffer.dropFirst(consumed))
            buffer = Data()
            route(request)
            if parser != nil, !leftover.isEmpty, !closed { webSocketBytes(leftover) }
        }
    }

    // MARK: HTTP

    private func route(_ request: HubHTTPRequest) {
        guard let host = request.headers["host"],
              HubWebSecurity.isAllowedHost(host, extra: hub.config.allowedHosts)
        else {
            respond(403, "Forbidden", text: "This hub answers only to localhost or its Tailscale IP address.")
            return
        }
        let sameOrigin = HubWebSecurity.isSameOrigin(origin: request.headers["origin"], host: host)
        let paired = server.token.map { token in
            HubWebSecurity.cookie(named: HubToken.cookieName, in: request.headers["cookie"])
                .map { HubWebSecurity.constantTimeEquals($0, token) } ?? false
        } ?? false

        // Pairing: the link from `claudeandrew hub link` carries the secret
        // once; from then on the browser holds it as a cookie.
        if request.method == "GET", request.path == "/auth" {
            guard let token = server.token, let offered = request.query["k"],
                  HubWebSecurity.constantTimeEquals(offered, token)
            else {
                respond(403, "Forbidden", text: "That pairing link is not valid for this hub.")
                return
            }
            respond(303, "See Other", contentType: "text/plain; charset=utf-8", body: Data(), extra: [
                "Location: /",
                "Set-Cookie: \(HubToken.cookieName)=\(token); Path=/; Max-Age=31536000; HttpOnly; SameSite=Strict",
            ])
            return
        }

        if request.headers["upgrade"]?.lowercased() == "websocket" {
            guard request.method == "GET", request.path == "/ws/term", sameOrigin, paired,
                  let key = request.headers["sec-websocket-key"]
            else {
                respond(403, "Forbidden", text: "websocket refused")
                return
            }
            upgrade(request, key: key)
            return
        }

        if request.path.hasPrefix("/api/"), !paired {
            respond(401, "Unauthorized", json: ["error": "not paired"])
            return
        }

        switch (request.method, request.path) {
        case ("GET", "/api/state"):
            server.state { [weak self] data in
                self?.respond(200, "OK", contentType: "application/json", body: data)
            }
        case ("POST", "/api/launch"), ("POST", "/api/kill"), ("POST", "/api/settings"):
            // JSON-only and same-origin: a form on another site can send
            // neither, so it can't start or end sessions through the user's
            // browser.
            guard sameOrigin, request.headers["content-type"]?.lowercased().hasPrefix("application/json") == true,
                  let body = try? JSONSerialization.jsonObject(with: request.body) as? [String: Any]
            else {
                respond(403, "Forbidden", json: ["error": "refused"])
                return
            }
            api(request.path, body)
        case ("GET", _):
            serveAsset(request.path, host: host)
        default:
            respond(405, "Method Not Allowed", text: "method not allowed")
        }
    }

    private func api(_ path: String, _ body: [String: Any]) {
        switch path {
        case "/api/launch":
            guard let target = HubState.launchTarget(body["path"] as? String ?? "", root: hub.config.root) else {
                respond(400, "Bad Request", json: ["error": "not a project directory"])
                return
            }
            let mode = body["permissionMode"] as? String ?? hub.config.defaultPermissionMode
            guard HubConfig.permissionModes.contains(mode) else {
                respond(400, "Bad Request", json: ["error": "unknown permission mode"])
                return
            }
            let resume = body["resume"] as? String
            if let resume, !HubState.isSessionId(resume) {
                respond(400, "Bad Request", json: ["error": "bad conversation id"])
                return
            }
            do {
                let session = try hub.launchFromWeb(cwd: target, permissionMode: mode, resume: resume)
                server.invalidateState()
                respond(200, "OK", json: ["id": session.id])
            } catch {
                respond(500, "Internal Server Error", json: ["error": "\(error)"])
            }
        case "/api/kill":
            guard let session = hub.session(id: body["id"] as? String ?? "") else {
                respond(404, "Not Found", json: ["error": "no such session"])
                return
            }
            hub.terminate(session)
            server.invalidateState()
            respond(200, "OK", json: ["ok": true])
        default:
            guard let mode = body["defaultPermissionMode"] as? String, HubConfig.permissionModes.contains(mode) else {
                respond(400, "Bad Request", json: ["error": "unknown permission mode"])
                return
            }
            hub.config.defaultPermissionMode = mode
            hub.config.save()
            server.invalidateState()
            respond(200, "OK", json: ["ok": true])
        }
    }

    private static let contentTypes = [
        "html": "text/html; charset=utf-8", "js": "text/javascript; charset=utf-8",
        "css": "text/css; charset=utf-8", "svg": "image/svg+xml", "png": "image/png",
        "json": "application/json", "webmanifest": "application/manifest+json",
    ]

    private func serveAsset(_ path: String, host: String) {
        guard let root = HubWebServer.assetRoot else {
            respond(500, "Internal Server Error", text: "web assets not found")
            return
        }
        let relative = path == "/" ? "index.html" : String(path.dropFirst())
        let components = relative.split(separator: "/", omittingEmptySubsequences: false)
        guard !components.contains(where: { $0.isEmpty || $0.hasPrefix(".") }),
              let type = Self.contentTypes[(relative as NSString).pathExtension],
              let data = try? Data(contentsOf: root.appendingPathComponent(relative))
        else {
            respond(404, "Not Found", text: "not found")
            return
        }
        // Everything the page loads is ours; xterm.js injects <style>
        // elements, hence the inline-style allowance. The WebSocket origin
        // is spelled out because Safari doesn't count ws: under 'self';
        // `host` has already passed isAllowedHost, so it is safe to echo.
        let csp = "Content-Security-Policy: default-src 'self'; style-src 'self' 'unsafe-inline'; "
            + "img-src 'self' data:; connect-src 'self' ws://\(host) wss://\(host); "
            + "frame-ancestors 'none'; base-uri 'none'"
        respond(200, "OK", contentType: type, body: data, extra: [csp])
    }

    private func respond(_ status: Int, _ reason: String, text: String) {
        respond(status, reason, contentType: "text/plain; charset=utf-8", body: Data(text.utf8))
    }

    private func respond(_ status: Int, _ reason: String, json: [String: Any]) {
        respond(status, reason, contentType: "application/json",
                body: (try? JSONSerialization.data(withJSONObject: json)) ?? Data("{}".utf8))
    }

    private func respond(_ status: Int, _ reason: String, contentType: String, body: Data, extra: [String] = []) {
        guard !closed, !responded else { return }
        responded = true
        let data = HubHTTP.response(status: status, reason: reason, contentType: contentType, body: body, extra: extra)
        connection.send(content: data, completion: .contentProcessed { [weak self] _ in self?.finish() })
    }

    // MARK: WebSocket

    private func upgrade(_ request: HubHTTPRequest, key: String) {
        responded = true
        parser = WebSocketParser()
        let head = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            + "Sec-WebSocket-Accept: \(WebSocketCodec.acceptKey(key))\r\n\r\n"
        send(Data(head.utf8))

        guard let session = hub.sessionOrRecentlyEnded(id: request.query["id"] ?? ""),
              let size = Hub.clampSize(rows: Int(request.query["rows"] ?? "") ?? 0,
                                       cols: Int(request.query["cols"] ?? "") ?? 0)
        else {
            send(WebSocketCodec.text(["type": "gone"]))
            closeWebSocket()
            return
        }
        rows = size.rows
        cols = size.cols
        self.session = session
        // `claim=0`: a screen that was only mirroring reconnects as a
        // spectator rather than resizing the session under its user.
        hub.attach(self, to: session, claim: request.query["claim"] != "0")
    }

    /// Hang up now (the pairing secret changed under this connection).
    func drop() {
        finish()
    }

    private func webSocketBytes(_ data: Data) {
        guard let messages = parser?.feed(data) else {
            finish()
            return
        }
        for message in messages {
            guard !closed else { return }
            switch message.opcode {
            case 2:
                if let session { hub.input(message.payload, from: self, to: session) }
            case 1:
                let object = HubFrame.json(message.payload)
                if object["type"] as? String == "resize", let session,
                   let size = Hub.clampSize(rows: object["rows"] as? Int ?? 0, cols: object["cols"] as? Int ?? 0) {
                    hub.resize(self, in: session, rows: size.rows, cols: size.cols)
                } else if object["type"] as? String == "fit", let session,
                          let size = Hub.clampSize(rows: object["rows"] as? Int ?? 0, cols: object["cols"] as? Int ?? 0) {
                    hub.noteFit(self, in: session, rows: size.rows, cols: size.cols)
                } else if object["type"] as? String == "ping" {
                    // The page's liveness probe: a half-open socket never answers.
                    send(WebSocketCodec.text(["type": "pong"]))
                }
            case 9:
                send(WebSocketCodec.encode(opcode: 10, payload: message.payload))
            case 8:
                closeWebSocket()
            default:
                break
            }
        }
    }

    func sendOutput(_ data: Data) {
        // One WebSocket message per chunk keeps frames small enough that a
        // slow link interleaves them with control messages.
        var offset = data.startIndex
        while offset < data.endIndex {
            let end = min(offset + 262_144, data.endIndex)
            send(WebSocketCodec.encode(opcode: 2, payload: Data(data[offset..<end])))
            offset = end
        }
    }

    func sendSize(rows: UInt16, cols: UInt16, owner: Bool) {
        send(WebSocketCodec.text(["type": "size", "rows": Int(rows), "cols": Int(cols), "owner": owner]))
    }

    func sendExit(code: Int32) {
        session = nil
        send(WebSocketCodec.text(["type": "exit", "code": Int(code)]))
        closeWebSocket()
    }

    private func closeWebSocket() {
        guard !closed else { return }
        let frame = WebSocketCodec.encode(opcode: 8, payload: Data([0x03, 0xe8]))
        connection.send(content: frame, completion: .contentProcessed { [weak self] _ in self?.finish() })
    }

    private func send(_ data: Data) {
        guard !closed else { return }
        if queuedBytes == 0 { lastProgress = Date() }
        queuedBytes += data.count
        guard queuedBytes <= Self.maxQueuedBytes else {
            // Hopelessly behind; it can reconnect and take the replay.
            finish()
            return
        }
        connection.send(content: data, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            self.queuedBytes -= data.count
            self.lastProgress = Date()
            if error != nil { self.finish() }
        })
    }

    private func finish() {
        guard !closed else { return }
        closed = true
        if let session { hub.detach(self, from: session) }
        session = nil
        connection.stateUpdateHandler = nil
        connection.cancel()
        server.remove(self)
    }
}
