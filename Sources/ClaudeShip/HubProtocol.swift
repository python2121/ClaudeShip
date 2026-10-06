import Darwin
import Foundation

/// Where the hub keeps its socket, lock, log, and config. `CLAUDESHIP_HOME`
/// relocates all of it, so a test hub never touches the real one.
enum HubPaths {
    static var home: URL {
        if let override = ProcessInfo.processInfo.environment["CLAUDESHIP_HOME"], !override.isEmpty {
            return URL(fileURLWithPath: override, isDirectory: true)
        }
        return FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/ClaudeShip/hub", isDirectory: true)
    }
    static var socket: String { home.appendingPathComponent("hub.sock").path }
    static var lock: String { home.appendingPathComponent("hub.lock").path }
    static var log: String { home.appendingPathComponent("hub.log").path }
    static var config: URL { home.appendingPathComponent("config.json") }

    /// How to run this binary as the `claudeship` command: the installed
    /// copy answers to that name; a dev build needs `--cli`.
    static let selfCommand: [String]? = {
        var size = UInt32(0)
        _NSGetExecutablePath(nil, &size)
        var buffer = [CChar](repeating: 0, count: Int(size))
        guard _NSGetExecutablePath(&buffer, &size) == 0 else { return nil }
        let executable = URL(fileURLWithPath: String(cString: buffer)).resolvingSymlinksInPath().path
        return HubCLI.isCLIName((executable as NSString).lastPathComponent) ? [executable] : [executable, "--cli"]
    }()

    static func ensureHome() {
        try? FileManager.default.createDirectory(
            at: home, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
    }
}

/// Hub settings, persisted as JSON. Parsing is tolerant: a missing or
/// malformed key falls back to its default rather than failing the load.
struct HubConfig: Equatable {
    /// The directory whose children are the web app's project list.
    var root: String
    var port: Int
    /// `--permission-mode` for sessions launched from the web app.
    var defaultPermissionMode: String
    /// Extra names the web server answers to, matched exactly. Empty by
    /// default: see `HubWebSecurity.isAllowedHost` for why names are risky.
    var allowedHosts: [String] = []

    /// What `claude --permission-mode` accepts (2.1.289).
    static let permissionModes = ["acceptEdits", "auto", "bypassPermissions", "manual", "plan", "dontAsk"]

    static var fallback: HubConfig {
        HubConfig(
            root: FileManager.default.homeDirectoryForCurrentUser
                .appendingPathComponent("Documents/code", isDirectory: true).path,
            port: 7433,
            defaultPermissionMode: "auto")
    }

    static func parse(_ data: Data) -> HubConfig {
        var config = fallback
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return config }
        if let root = obj["root"] as? String, !root.isEmpty {
            config.root = (root as NSString).expandingTildeInPath
        }
        if let port = obj["port"] as? Int, (1...65535).contains(port) { config.port = port }
        if let mode = obj["defaultPermissionMode"] as? String, permissionModes.contains(mode) {
            config.defaultPermissionMode = mode
        }
        if let hosts = obj["allowedHosts"] as? [String] {
            config.allowedHosts = hosts.map { $0.lowercased() }.filter { !$0.isEmpty }
        }
        return config
    }

    static func load() -> HubConfig {
        (try? Data(contentsOf: HubPaths.config)).map(parse) ?? fallback
    }

    func save() {
        let obj: [String: Any] = [
            "root": root, "port": port, "defaultPermissionMode": defaultPermissionMode, "allowedHosts": allowedHosts,
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: obj, options: [.prettyPrinted, .sortedKeys])
        else { return }
        try? data.write(to: HubPaths.config, options: .atomic)
    }
}

// MARK: - Local wire protocol

/// Framing between the hub and a terminal client on the Unix socket: one
/// type byte, a big-endian u32 length, then the payload. Control frames
/// carry JSON; input/output frames carry raw terminal bytes untouched.
enum HubFrame {
    enum Kind: UInt8 {
        case hello = 0x48      // client → hub: JSON {op, …}
        case input = 0x49      // client → hub: keystrokes
        case resize = 0x52     // client → hub: JSON {rows, cols}
        case attached = 0x41   // hub → client: JSON {id}
        case output = 0x4F     // hub → client: terminal output
        case exit = 0x58       // hub → client: JSON {code}
        case error = 0x45      // hub → client: JSON {message}
        case reply = 0x4C      // hub → client: JSON answer to a one-shot op
    }

    static let maxPayload = 16 << 20

    /// Bumped whenever the hub, the terminal client, or the web page would
    /// misunderstand an older peer. The hub outlives installs (it owns live
    /// sessions), so a new client or page can meet an old hub; each side
    /// states this number and says so plainly when they differ.
    static let version = 2

    static func encode(_ kind: Kind, _ payload: Data) -> Data {
        var frame = Data(capacity: payload.count + 5)
        frame.append(kind.rawValue)
        let n = UInt32(payload.count)
        frame.append(contentsOf: [UInt8(n >> 24), UInt8((n >> 16) & 0xff), UInt8((n >> 8) & 0xff), UInt8(n & 0xff)])
        frame.append(payload)
        return frame
    }

    static func encodeJSON(_ kind: Kind, _ object: [String: Any]) -> Data {
        encode(kind, (try? JSONSerialization.data(withJSONObject: object)) ?? Data("{}".utf8))
    }

    static func json(_ payload: Data) -> [String: Any] {
        ((try? JSONSerialization.jsonObject(with: payload)) as? [String: Any]) ?? [:]
    }
}

/// Reassembles frames from a byte stream that splits them arbitrarily.
struct HubFrameDecoder {
    private var buffer = Data()

    /// The complete frames now available, or nil when the stream is not
    /// speaking the protocol (unknown type, oversized length) and the
    /// connection should be dropped.
    mutating func feed(_ data: Data) -> [(HubFrame.Kind, Data)]? {
        buffer.append(data)
        var frames: [(HubFrame.Kind, Data)] = []
        while buffer.count >= 5 {
            let base = buffer.startIndex
            guard let kind = HubFrame.Kind(rawValue: buffer[base]) else { return nil }
            let length = Int(buffer[base + 1]) << 24 | Int(buffer[base + 2]) << 16
                | Int(buffer[base + 3]) << 8 | Int(buffer[base + 4])
            guard length <= HubFrame.maxPayload else { return nil }
            guard buffer.count >= 5 + length else { break }
            frames.append((kind, Data(buffer[(base + 5)..<(base + 5 + length)])))
            buffer = Data(buffer[(base + 5 + length)...])
        }
        return frames
    }
}

// MARK: - Terminal input

/// Tells what a person did at a terminal from what the terminal said on its
/// own. Terminals answer the program's queries, report focus changes, and
/// (with motion tracking on) report the pointer merely passing over — all
/// through the same input stream as keystrokes. Only the person's actions
/// should make a screen the one the session is sized for; otherwise two
/// attached screens, each answering the same query, would take the size
/// from each other forever.
enum TerminalInput {
    static func isUserActivity(_ data: Data) -> Bool {
        let bytes = [UInt8](data)
        var i = 0
        while i < bytes.count {
            guard bytes[i] == 0x1b else { return true }      // an ordinary key
            guard i + 1 < bytes.count else { return true }   // the Esc key
            switch bytes[i + 1] {
            case 0x5b:  // CSI
                var end = i + 2
                while end < bytes.count, !(0x40...0x7e).contains(bytes[end]) { end += 1 }
                guard end < bytes.count else { return true }
                if bytes[end] == 0x4d, end == i + 2 {
                    // Legacy (X10) mouse: three raw bytes follow the M.
                    guard end + 3 < bytes.count else { return true }
                    let button = Int(bytes[end + 1]) - 32
                    guard button & 32 != 0, button & 64 == 0, button & 3 == 3 else { return true }
                    i = end + 4
                    continue
                }
                if !isReport(params: Array(bytes[(i + 2)..<end]), final: bytes[end]) { return true }
                i = end + 1
            case 0x5d, 0x50, 0x5f, 0x5e, 0x58:  // OSC / DCS / APC / PM / SOS: always an answer
                var end = i + 2
                var terminated = false
                while end < bytes.count {
                    if bytes[end] == 0x07 { terminated = true; end += 1; break }
                    if bytes[end] == 0x1b, end + 1 < bytes.count, bytes[end + 1] == 0x5c {
                        terminated = true
                        end += 2
                        break
                    }
                    end += 1
                }
                guard terminated else { return true }
                i = end
            default:
                return true  // Alt+key and the like
            }
        }
        return false
    }

    private static func isReport(params: [UInt8], final: UInt8) -> Bool {
        let text = String(decoding: params, as: UTF8.self)
        switch final {
        case 0x49, 0x4f:  // focus in / out
            return params.isEmpty
        case 0x52:  // cursor position report
            return !params.isEmpty && params.allSatisfy { ($0 >= 0x30 && $0 <= 0x39) || $0 == 0x3b || $0 == 0x3f }
        case 0x63:  // device attributes
            return params.first == 0x3f || params.first == 0x3e
        case 0x6e, 0x74:  // status reports, window reports
            return true
        case 0x79:  // DECRPM
            return params.contains(0x24)
        case 0x75:  // kitty keyboard flags report
            return params.first == 0x3f
        case 0x4d, 0x6d:  // SGR mouse: only bare pointer motion is not an action
            guard params.first == 0x3c, let button = Int(text.dropFirst().split(separator: ";").first ?? "") else {
                return false
            }
            return button & 32 != 0 && button & 64 == 0 && button & 3 == 3
        default:
            return false
        }
    }
}

// MARK: - Terminal state tracking

/// The terminal modes a program has switched on, as far as they matter for
/// handing the screen to another terminal: a client that attaches late
/// needs them replayed, and one that detaches early needs them undone.
struct TerminalModes: Equatable {
    /// DEC private modes (`CSI ? n h/l`) from `tracked` that have been set
    /// or reset explicitly.
    var dec: [Int: Bool] = [:]
    /// Kitty keyboard protocol flag stack (`CSI > flags u` / `CSI < n u`).
    var kitty: [Int] = []
    /// xterm modifyOtherKeys level (`CSI > 4 ; n m`).
    var modifyOtherKeys = 0

    static let altScreen = [47, 1047, 1049]
    /// Cursor keys, cursor visibility, alt screen, mouse reporting, focus
    /// reporting, bracketed paste, color-scheme change reports.
    static let tracked: Set<Int> = [1, 25, 47, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 1047, 1049, 2004, 2031]

    var inAltScreen: Bool { Self.altScreen.contains { dec[$0] == true } }

    /// Takes a terminal in its default state to this one.
    var restoreSequence: Data {
        var s = ""
        for mode in Self.altScreen where dec[mode] == true { s += "\u{1b}[?\(mode)h" }
        for mode in dec.keys.sorted() where !Self.altScreen.contains(mode) {
            if mode == 25 {
                if dec[mode] == false { s += "\u{1b}[?25l" }
            } else if dec[mode] == true {
                s += "\u{1b}[?\(mode)h"
            }
        }
        for flags in kitty { s += "\u{1b}[>\(flags)u" }
        if modifyOtherKeys > 0 { s += "\u{1b}[>4;\(modifyOtherKeys)m" }
        return Data(s.utf8)
    }

    /// Takes a terminal in this state back to its defaults. Empty when
    /// nothing is set, so a clean exit writes nothing extra.
    var resetSequence: Data {
        var s = ""
        if !kitty.isEmpty { s += "\u{1b}[<\(kitty.count)u" }
        if modifyOtherKeys > 0 { s += "\u{1b}[>4;0m" }
        for mode in dec.keys.sorted() where !Self.altScreen.contains(mode) {
            if mode == 25 {
                if dec[mode] == false { s += "\u{1b}[?25h" }
            } else if dec[mode] == true {
                s += "\u{1b}[?\(mode)l"
            }
        }
        for mode in Self.altScreen where dec[mode] == true { s += "\u{1b}[?\(mode)l" }
        if !s.isEmpty { s += "\u{1b}[0m" }
        return Data(s.utf8)
    }
}

/// Walks a terminal output stream one escape sequence at a time. It keeps
/// `modes` current, and it returns the stream as it should be *replayed* to
/// a terminal that attaches later: identical, minus the sequences that must
/// not happen twice — queries (the new terminal would answer a question
/// asked long ago, and the answer would land in the program's input),
/// clipboard writes, notifications, and bells.
///
/// Live clients always get the raw bytes; only the replay copy is filtered.
struct TerminalStream {
    enum Piece: Equatable {
        case bytes(Data)
        /// The program wiped the screen and scrollback: nothing before this
        /// point is visible in any attached terminal, so the replay buffer
        /// can start over from these modes.
        case clear(TerminalModes)
    }

    private enum StringKind { case osc, dcs, apc, other }
    private enum State {
        case ground, esc, escIntermediate, csi
        case string(StringKind), stringEsc(StringKind)
        /// A string too long to hold: passed through unclassified.
        case pass(StringKind), passEsc(StringKind)
    }

    private var state: State = .ground
    /// Bytes of the sequence being read; empty in ground state.
    private var pending: [UInt8] = []
    private(set) var modes = TerminalModes()
    /// Half of an `ESC[2J` + `ESC[3J` pair, and how many other sequences
    /// have gone by since it.
    private var clearCandidate: (kind: Int, tokens: Int)?

    private static let maxCSI = 128
    private static let maxString = 65_536

    /// The bytes of a sequence still being read — consumed from the stream
    /// but not yet part of any returned piece. A client that attaches
    /// between two reads needs these after the replay: the live stream it
    /// joins resumes mid-sequence, and without the first half the rest
    /// would print as text.
    var pendingBytes: Data {
        switch state {
        case .stringEsc, .passEsc: return Data(pending + [0x1b])
        default: return Data(pending)
        }
    }

    mutating func feed(_ input: Data) -> [Piece] {
        var out: [UInt8] = []
        out.reserveCapacity(input.count)
        var pieces: [Piece] = []
        for byte in input { step(byte, &out, &pieces) }
        if !out.isEmpty { pieces.append(.bytes(Data(out))) }
        return pieces
    }

    private mutating func step(_ b: UInt8, _ out: inout [UInt8], _ pieces: inout [Piece]) {
        switch state {
        case .ground:
            if b == 0x1b {
                state = .esc
                pending = [b]
            } else {
                clearCandidate = nil
                if b != 0x07 { out.append(b) }  // a replayed bell would ring again
            }

        case .esc:
            switch b {
            case 0x5b: pending.append(b); state = .csi
            case 0x5d: pending.append(b); state = .string(.osc)
            case 0x50: pending.append(b); state = .string(.dcs)
            case 0x5f: pending.append(b); state = .string(.apc)
            case 0x58, 0x5e: pending.append(b); state = .string(.other)
            case 0x1b: out += pending; pending = [b]
            case 0x20...0x2f: pending.append(b); state = .escIntermediate
            default:
                if b == 0x63 { modes = TerminalModes() }  // RIS: full reset
                pending.append(b)
                emitPending(&out)
            }

        case .escIntermediate:
            if b == 0x1b {
                out += pending
                pending = [b]
                state = .esc
            } else {
                pending.append(b)
                if b >= 0x30 || pending.count > 16 { emitPending(&out) }
            }

        case .csi:
            if b == 0x1b {
                out += pending
                pending = [b]
                state = .esc
            } else if (0x40...0x7e).contains(b) {
                pending.append(b)
                finishCSI(&out, &pieces)
            } else {
                pending.append(b)
                if pending.count > Self.maxCSI { emitPending(&out) }
            }

        case .string(let kind):
            if b == 0x07 && kind == .osc {
                pending.append(b)
                finishString(kind, terminatorLength: 1, &out)
            } else if b == 0x1b {
                state = .stringEsc(kind)
            } else {
                pending.append(b)
                if pending.count > Self.maxString {
                    out += pending
                    pending = []
                    state = .pass(kind)
                }
            }

        case .stringEsc(let kind):
            if b == 0x5c {
                pending += [0x1b, 0x5c]
                finishString(kind, terminatorLength: 2, &out)
            } else {
                // ESC without `\` aborts the string and starts a new sequence.
                out += pending
                pending = [0x1b]
                state = .esc
                step(b, &out, &pieces)
            }

        case .pass(let kind):
            if b == 0x1b {
                state = .passEsc(kind)
            } else {
                out.append(b)
                if b == 0x07 && kind == .osc { state = .ground }
            }

        case .passEsc(let kind):
            if b == 0x5c {
                out += [0x1b, 0x5c]
                state = .ground
            } else {
                _ = kind
                pending = [0x1b]
                state = .esc
                step(b, &out, &pieces)
            }
        }
    }

    private mutating func emitPending(_ out: inout [UInt8]) {
        out += pending
        pending = []
        state = .ground
        bumpToken()
    }

    private mutating func bumpToken() {
        guard var candidate = clearCandidate else { return }
        candidate.tokens += 1
        clearCandidate = candidate.tokens > 2 ? nil : candidate
    }

    private mutating func finishCSI(_ out: inout [UInt8], _ pieces: inout [Piece]) {
        let final = pending[pending.count - 1]
        let params = Array(pending[2..<(pending.count - 1)])
        track(params: params, final: final)

        var clearKind: Int?
        if final == 0x4a {
            if params == [0x32] { clearKind = 2 } else if params == [0x33] { clearKind = 3 }
        }
        if let kind = clearKind, !modes.inAltScreen {
            if let candidate = clearCandidate, candidate.kind != kind {
                // Screen and scrollback both wiped: everything so far is gone
                // from every attached terminal, so drop it from the replay too.
                out.removeAll(keepingCapacity: true)
                pieces = [.clear(modes)]
                clearCandidate = nil
                pending = []
                state = .ground
                return
            }
            clearCandidate = (kind, 0)
        } else {
            bumpToken()
        }

        if !Self.isQueryCSI(params: params, final: final) { out += pending }
        pending = []
        state = .ground
    }

    private mutating func finishString(_ kind: StringKind, terminatorLength: Int, _ out: inout [UInt8]) {
        let payload = Array(pending[2..<(pending.count - terminatorLength)])
        if !Self.isReplayUnsafeString(kind: kind, payload: payload) { out += pending }
        pending = []
        state = .ground
        bumpToken()
    }

    private mutating func track(params: [UInt8], final: UInt8) {
        guard let lead = params.first else { return }
        let rest = String(decoding: params.dropFirst(), as: UTF8.self)
        switch (lead, final) {
        case (0x3f, 0x68), (0x3f, 0x6c):  // CSI ? … h / l
            for part in rest.split(separator: ";") {
                if let mode = Int(part), TerminalModes.tracked.contains(mode) {
                    modes.dec[mode] = final == 0x68
                }
            }
        case (0x3e, 0x75):  // CSI > flags u — push
            if modes.kitty.count < 16 { modes.kitty.append(Int(rest.split(separator: ";").first ?? "") ?? 0) }
        case (0x3c, 0x75):  // CSI < n u — pop
            modes.kitty.removeLast(min(modes.kitty.count, max(1, Int(rest) ?? 1)))
        case (0x3d, 0x75):  // CSI = flags ; mode u — change the current flags
            guard !modes.kitty.isEmpty else { return }
            let parts = rest.split(separator: ";", omittingEmptySubsequences: false)
            let flags = parts.first.flatMap { Int($0) } ?? 0
            let top = modes.kitty.count - 1
            switch parts.count > 1 ? (Int(parts[1]) ?? 1) : 1 {
            case 2: modes.kitty[top] |= flags
            case 3: modes.kitty[top] &= ~flags
            default: modes.kitty[top] = flags
            }
        case (0x3e, 0x6d):  // CSI > 4 ; n m — modifyOtherKeys; bare CSI > m resets
            let parts = rest.split(separator: ";", omittingEmptySubsequences: false)
            if rest.isEmpty || parts.first == "4" {
                modes.modifyOtherKeys = parts.count > 1 ? (Int(parts[1]) ?? 0) : 0
            }
        case (0x3e, 0x6e):  // CSI > 4 n — modifyOtherKeys off
            if rest.isEmpty || rest == "4" { modes.modifyOtherKeys = 0 }
        default:
            break
        }
    }

    /// CSI sequences that ask the terminal to report something back.
    static func isQueryCSI(params: [UInt8], final: UInt8) -> Bool {
        switch final {
        case 0x63:  // DA1/2/3
            return true
        case 0x6e:  // DSR — but CSI > n disables a key-modifier option
            return params.first != 0x3e
        case 0x75:  // kitty keyboard query: CSI ? u
            return params.first == 0x3f
        case 0x70:  // DECRQM: CSI [?] n $ p
            return params.contains(0x24)
        case 0x71:  // XTVERSION: CSI > q
            return params.first == 0x3e
        case 0x78:  // DECREQTPARM (but `$ x` is a rectangle fill)
            return !params.contains(0x24)
        case 0x74:  // window reports: CSI 11/13–21 t
            let first = String(decoding: params, as: UTF8.self).split(separator: ";").first.flatMap { Int($0) }
            return first.map { $0 == 11 || (13...21).contains($0) } ?? false
        default:
            return false
        }
    }

    /// OSC/DCS/APC strings that must not be replayed: queries, clipboard
    /// writes (OSC 52), and desktop notifications (OSC 9 / 99 / 777; OSC 9;4
    /// is a progress bar, which is state and stays).
    private static func isReplayUnsafeString(kind: StringKind, payload: [UInt8]) -> Bool {
        let text = String(decoding: payload, as: UTF8.self)
        switch kind {
        case .osc:
            let fields = text.split(separator: ";", omittingEmptySubsequences: false)
            guard let code = fields.first else { return false }
            if fields.dropFirst().contains("?") { return true }
            if code == "52" || code == "99" || code == "777" { return true }
            if code == "9" { return !(fields.count > 1 && fields[1] == "4") }
            return false
        case .dcs:
            return text.hasPrefix("+q") || text.hasPrefix("$q")
        case .apc:
            return text.hasPrefix("G") && text.contains("a=q")
        case .other:
            return false
        }
    }
}

/// The recent output of a session, kept so a terminal that attaches later
/// can be brought to the same screen. Bounded: when it overflows, whole
/// chunks drop off the front and `base` remembers the modes in force at the
/// new start, so the replay still begins from the right terminal state.
struct ReplayBuffer {
    private var chunks: [(data: Data, modes: TerminalModes)] = []
    private(set) var base = TerminalModes()
    private(set) var total = 0
    let capacity: Int
    private static let chunkTarget = 32_768

    init(capacity: Int = 4 << 20) { self.capacity = capacity }

    /// `modesAfter` is the terminal state once `data` has been written.
    mutating func append(_ data: Data, modesAfter: TerminalModes) {
        guard !data.isEmpty else { return }
        if let last = chunks.indices.last, chunks[last].data.count + data.count <= Self.chunkTarget {
            chunks[last].data.append(data)
            chunks[last].modes = modesAfter
        } else {
            chunks.append((data, modesAfter))
        }
        total += data.count
        while total > capacity, chunks.count > 1 {
            let dropped = chunks.removeFirst()
            total -= dropped.data.count
            base = dropped.modes
        }
    }

    /// Start over after the program cleared screen and scrollback.
    mutating func reset(base: TerminalModes) {
        let clear = Data("\u{1b}[H\u{1b}[2J\u{1b}[3J".utf8)
        chunks = [(clear, base)]
        total = clear.count
        self.base = base
    }

    /// Everything a freshly reset terminal needs to show the current screen.
    func snapshot() -> Data {
        var data = base.restoreSequence
        data.reserveCapacity(data.count + total)
        for chunk in chunks { data.append(chunk.data) }
        return data
    }
}
