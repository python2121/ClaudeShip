import Darwin
import Foundation

/// A permission request forwarded by the `--permission-hook` helper, waiting
/// for the user's verdict in the overlay.
struct PendingApproval: Identifiable, Equatable {
    let id: UUID          // connection id — the handle used to respond
    let sessionId: String?
    let cwd: String?
    let toolName: String
    /// Human-readable one-liner of what's being approved (e.g. the Bash
    /// command), pre-truncated for display.
    let summary: String
    /// The full approval text (newlines intact, generous cap) — what the
    /// tooltip shows on hover.
    let detail: String
    let receivedAt: Date
}

/// What the app learns about one permission request from its request line.
struct ApprovalRequestInfo: Equatable {
    let sessionId: String?
    let cwd: String?
    let toolName: String
    let summary: String
    let detail: String
}

/// Wire format between the hook helper and the app, plus the JSON the hook
/// must print back to Claude Code. One JSON object per line in both
/// directions on the socket.
enum ApprovalWire {
    static let maxLineBytes = 1_048_576

    /// Helper side: hook stdin JSON → the request line sent to the app.
    /// Field names follow Claude Code's hook input (snake_case in, camelCase
    /// on our wire).
    static func requestLine(fromHookInput data: Data) -> Data? {
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        var out: [String: Any] = [:]
        out["toolName"] = obj["tool_name"] as? String ?? "unknown"
        if let sid = obj["session_id"] as? String { out["sessionId"] = sid }
        if let cwd = obj["cwd"] as? String { out["cwd"] = cwd }
        if let input = obj["tool_input"], JSONSerialization.isValidJSONObject(["x": input]) {
            out["toolInput"] = input
        }
        guard var line = try? JSONSerialization.data(withJSONObject: out) else { return nil }
        line.append(0x0A)
        return line
    }

    /// App side: request line → displayable request. `summary` is the
    /// clipped single line for the row; `detail` keeps newlines and a much
    /// more generous cap for the hover tooltip.
    static func parseRequest(_ line: Data) -> ApprovalRequestInfo? {
        guard let obj = try? JSONSerialization.jsonObject(with: line) as? [String: Any] else { return nil }
        let tool = obj["toolName"] as? String ?? "unknown"
        return ApprovalRequestInfo(
            sessionId: obj["sessionId"] as? String,
            cwd: obj["cwd"] as? String,
            toolName: tool,
            summary: summary(tool: tool, input: obj["toolInput"]),
            detail: summary(tool: tool, input: obj["toolInput"], maxChars: 4000, collapseNewlines: false)
        )
    }

    /// "Bash: swift build" / "Edit: /path/to/file" / "WebFetch: {…}" — the
    /// most meaningful single field per tool, falling back to compact JSON.
    static func summary(tool: String, input: Any?, maxChars: Int = 200, collapseNewlines: Bool = true) -> String {
        var detail = ""
        if let dict = input as? [String: Any] {
            // The fields users actually recognize, in priority order.
            for key in ["command", "file_path", "url", "pattern", "prompt", "description"] {
                if let v = dict[key] as? String, !v.isEmpty { detail = v; break }
            }
            if detail.isEmpty, !dict.isEmpty,
               let data = try? JSONSerialization.data(withJSONObject: dict),
               let text = String(data: data, encoding: .utf8) {
                detail = text
            }
        }
        if collapseNewlines {
            detail = detail.split(whereSeparator: \.isNewline).joined(separator: " ")
        }
        let clipped = detail.count > maxChars ? String(detail.prefix(maxChars - 1)) + "…" : detail
        return clipped.isEmpty ? tool : "\(tool): \(clipped)"
    }

    /// App → helper verdict line.
    static func responseLine(allow: Bool) -> Data {
        Data("{\"behavior\":\"\(allow ? "allow" : "deny")\"}\n".utf8)
    }

    /// Helper side: verdict line → allow? (nil = unparseable, treat as no
    /// decision).
    static func parseResponse(_ line: Data) -> Bool? {
        guard let obj = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
              let behavior = obj["behavior"] as? String else { return nil }
        switch behavior {
        case "allow": return true
        case "deny": return false
        default: return nil
        }
    }

    /// The JSON the hook prints to stdout for Claude Code. Schema verified
    /// against the CLI's own output validator (hookSpecificOutput →
    /// hookEventName "PermissionRequest" → decision.behavior allow|deny).
    static func decisionJSON(allow: Bool) -> String {
        if allow {
            return #"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
        }
        return #"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied via ClaudeShip"}}}"#
    }
}

/// Blocking Unix-domain-socket server for approval requests. One connection
/// per pending approval: the helper connects, sends one request line, and
/// blocks; we reply with a verdict line when the user clicks (or the helper
/// gives up and disconnects — its silence means "fall through to the
/// terminal prompt"). Callbacks fire on internal threads; hop to the main
/// actor yourself.
final class ApprovalServer {
    let path: String
    var onRequest: ((_ id: UUID, _ info: ApprovalRequestInfo) -> Void)?
    var onClosed: ((_ id: UUID) -> Void)?

    private var listenFD: Int32 = -1
    private let lock = NSLock()
    private var conns: [UUID: Int32] = [:]
    private var responded: Set<UUID> = []

    init(path: String) { self.path = path }

    @discardableResult
    func start() -> Bool {
        try? FileManager.default.createDirectory(
            at: URL(fileURLWithPath: path).deletingLastPathComponent(),
            withIntermediateDirectories: true)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return false }
        unlink(path)  // stale socket from a previous run
        guard bindUnix(fd, path: path), listen(fd, 8) == 0 else {
            close(fd)
            return false
        }
        listenFD = fd
        Thread.detachNewThread { [weak self] in self?.acceptLoop(fd) }
        return true
    }

    func stop() {
        let fd = listenFD
        listenFD = -1
        if fd >= 0 {
            shutdown(fd, SHUT_RDWR)
            close(fd)
        }
        unlink(path)
    }

    /// Answer a pending approval. The connection's reader thread does the
    /// actual close (avoids racing an fd-reuse); we just write and shut down.
    func respond(_ id: UUID, allow: Bool) {
        lock.lock()
        let fd = conns[id]
        if fd != nil { responded.insert(id) }
        lock.unlock()
        guard let fd else { return }
        ApprovalWire.responseLine(allow: allow).withUnsafeBytes { raw in
            _ = write(fd, raw.baseAddress, raw.count)
        }
        shutdown(fd, SHUT_RDWR)
    }

    /// Drop a pending approval WITHOUT a verdict — used when the prompt was
    /// answered in the terminal, so any verdict from us would be talking
    /// over a decision already made. The helper sees EOF and exits silently.
    /// Marks the connection responded so onClosed doesn't re-fire (the
    /// caller is the one cleaning up).
    func cancel(_ id: UUID) {
        lock.lock()
        let fd = conns[id]
        if fd != nil { responded.insert(id) }
        lock.unlock()
        guard let fd else { return }
        shutdown(fd, SHUT_RDWR)
    }

    // MARK: Internals

    private func acceptLoop(_ fd: Int32) {
        while true {
            let conn = accept(fd, nil, nil)
            guard conn >= 0 else {
                if listenFD < 0 { return }  // stopped
                continue
            }
            let id = UUID()
            lock.lock(); conns[id] = conn; lock.unlock()
            Thread.detachNewThread { [weak self] in self?.serve(id: id, fd: conn) }
        }
    }

    private func serve(id: UUID, fd: Int32) {
        defer {
            lock.lock()
            conns[id] = nil
            let wasResponded = responded.remove(id) != nil
            lock.unlock()
            close(fd)
            if !wasResponded { onClosed?(id) }
        }

        guard let line = readLine(fd),
              let req = ApprovalWire.parseRequest(line) else { return }
        onRequest?(id, req)

        // Block until the peer closes (helper timeout/death) or respond()
        // shuts the socket down. Either way the deferred cleanup runs, and
        // onClosed fires only if no verdict was sent.
        var scratch = [UInt8](repeating: 0, count: 256)
        while read(fd, &scratch, scratch.count) > 0 {}
    }

    private func readLine(_ fd: Int32) -> Data? {
        var data = Data()
        var buf = [UInt8](repeating: 0, count: 4096)
        while data.count < ApprovalWire.maxLineBytes {
            let n = read(fd, &buf, buf.count)
            guard n > 0 else { return nil }
            data.append(contentsOf: buf[0..<n])
            if let nl = data.firstIndex(of: 0x0A) {
                return data.prefix(upTo: nl)
            }
        }
        return nil
    }

    fileprivate func bindUnix(_ fd: Int32, path: String) -> Bool {
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let maxLen = MemoryLayout.size(ofValue: addr.sun_path) - 1
        let bytes = Array(path.utf8)
        guard bytes.count <= maxLen else { return false }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: bytes)
        }
        let len = socklen_t(MemoryLayout<sockaddr_un>.size)
        return withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, len) }
        } == 0
    }
}

enum ApprovalSocket {
    /// Well-known rendezvous path. sun_path caps at 104 bytes on macOS, so
    /// this lives in App Support, not some deep sandboxed temp dir.
    static var defaultPath: String {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/ClaudeShip/approvals.sock")
            .path
    }

    /// Client connect for the hook helper. Returns the fd or nil.
    static func connect(path: String) -> Int32? {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return nil }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let maxLen = MemoryLayout.size(ofValue: addr.sun_path) - 1
        let bytes = Array(path.utf8)
        guard bytes.count <= maxLen else { close(fd); return nil }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: bytes)
        }
        let len = socklen_t(MemoryLayout<sockaddr_un>.size)
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, len) }
        }
        guard rc == 0 else { close(fd); return nil }
        return fd
    }
}
