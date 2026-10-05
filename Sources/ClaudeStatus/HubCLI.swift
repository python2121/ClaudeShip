import CoreImage
import Darwin
import Foundation

/// Written to by the signal handlers below; the attach loop polls the
/// other end. A handler may only do async-signal-safe work, and a one-byte
/// write to a pipe is the classic way to get the news out.
private var hubCLISignalPipe: Int32 = -1

/// The `claudeandrew` command: runs Claude through the hub, so the session
/// lives in the hub rather than in this terminal, and this terminal is one
/// (replaceable) view of it. `claudeandrew hub …` manages the hub itself.
///
/// Reached two ways: the binary invoked under the name `claudeandrew` (the
/// installed copy), or `ClaudeStatus --cli …` (a dev build).
enum HubCLI {
    static let commandName = "claudeandrew"

    static func run(_ args: [String]) -> Never {
        if args.first == "hub" {
            hubCommand(Array(args.dropFirst()))
        }
        if isatty(STDIN_FILENO) == 0 || isatty(STDOUT_FILENO) == 0 || bypassesHub(args) {
            execProgram(args)
        }
        guard let size = terminalSize() else { execProgram(args) }
        // `--resume` of a conversation that is already running in the hub
        // means "show me that one", not "start a copy of it".
        if let sessionId = resumedSessionId(in: args), let hubId = hubSession(forClaudeSessionId: sessionId) {
            let fd = connect(startingHubIfNeeded: false)
            attachLoop(fd: fd, hello: [
                "op": "attach", "protocol": HubFrame.version, "id": hubId, "rows": size.rows, "cols": size.cols,
            ], clearFirst: true)
        }
        let fd = connect(startingHubIfNeeded: true)
        let hello: [String: Any] = [
            "op": "launch", "protocol": HubFrame.version,
            "cwd": FileManager.default.currentDirectoryPath,
            "args": args,
            "env": ProcessInfo.processInfo.environment,
            "rows": size.rows, "cols": size.cols,
        ]
        attachLoop(fd: fd, hello: hello, clearFirst: false)
    }

    /// Invocations that aren't an interactive session to share — one-shot
    /// output, claude's own management subcommands, sessions claude runs
    /// somewhere else — go to claude untouched.
    static func bypassesHub(_ args: [String]) -> Bool {
        let flags: Set<String> = [
            "-p", "--print", "-v", "--version", "-h", "--help", "--bg", "--background", "--cloud", "--desktop",
        ]
        let subcommands: Set<String> = [
            "agents", "attach", "auth", "auto-mode", "doctor", "gateway", "import", "install", "logs", "mcp",
            "plugin", "plugins", "purge", "respawn", "rm", "setup-token", "stop", "kill", "ultrareview",
            "update", "upgrade",
        ]
        if let first = args.first, subcommands.contains(first) { return true }
        return args.contains { flags.contains($0) || $0.hasPrefix("--cloud=") }
    }

    /// The conversation `--resume <id>` / `-r <id>` / `--resume=<id>` asks for.
    static func resumedSessionId(in args: [String]) -> String? {
        for (index, arg) in args.enumerated() {
            if arg == "--resume" || arg == "-r" {
                guard index + 1 < args.count, HubState.isSessionId(args[index + 1]) else { return nil }
                return args[index + 1]
            }
            if arg.hasPrefix("--resume=") {
                let value = String(arg.dropFirst("--resume=".count))
                return HubState.isSessionId(value) ? value : nil
            }
        }
        return nil
    }

    /// The hub session running a given Claude conversation, if any: the
    /// registry names the Claude process, and a hub session's supervisor
    /// is one of its ancestors.
    static func hubSession(forClaudeSessionId sessionId: String) -> String? {
        let hub = liveSessions()
        guard !hub.isEmpty,
              let live = SessionScanner.scan().first(where: { $0.sessionId == sessionId })
        else { return nil }
        let chain = TerminalFocus.ancestorPids(of: live.pid)
        return hub.first { chain.contains($0.pid) }?.id
    }

    /// Ask the hub to end a session (what the web app's End button does).
    /// For the menubar app: never exits, never starts a hub. False when
    /// the hub isn't running or refused.
    static func endHubSession(_ hubId: String) -> Bool {
        guard let fd = tryConnect() else { return false }
        defer { close(fd) }
        var timeout = timeval(tv_sec: 2, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        guard HubIO.writeAll(fd, HubFrame.encodeJSON(.hello, ["op": "kill", "id": hubId])) else { return false }
        var decoder = HubFrameDecoder()
        var buffer = [UInt8](repeating: 0, count: 4096)
        while true {
            let n = read(fd, &buffer, buffer.count)
            if n < 0 && errno == EINTR { continue }
            guard n > 0, let frames = decoder.feed(Data(buffer[0..<n])) else { return false }
            for (kind, payload) in frames {
                if kind == .reply { return HubFrame.json(payload)["ok"] as? Bool == true }
                if kind == .error { return false }
            }
        }
    }

    /// The hub's live sessions (id and supervisor pid), for the menubar
    /// app's scanner. Empty when no hub is running or it doesn't answer
    /// within a second; never starts one.
    static func liveSessions() -> [(id: String, pid: pid_t)] {
        guard let fd = tryConnect() else { return [] }
        defer { close(fd) }
        var timeout = timeval(tv_sec: 1, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        guard HubIO.writeAll(fd, HubFrame.encodeJSON(.hello, ["op": "status"])) else { return [] }
        var decoder = HubFrameDecoder()
        var buffer = [UInt8](repeating: 0, count: 65_536)
        while true {
            let n = read(fd, &buffer, buffer.count)
            if n < 0 && errno == EINTR { continue }
            guard n > 0, let frames = decoder.feed(Data(buffer[0..<n])) else { return [] }
            for (kind, payload) in frames where kind == .reply {
                let sessions = HubFrame.json(payload)["sessions"] as? [[String: Any]] ?? []
                return sessions.compactMap { entry in
                    guard let id = entry["id"] as? String, let pid = entry["pid"] as? Int else { return nil }
                    return (id, pid_t(pid))
                }
            }
        }
    }

    // MARK: hub subcommands

    private static func hubCommand(_ args: [String]) -> Never {
        switch args.first ?? "status" {
        case "run":
            Hub.runForever()
        case "supervise":
            // Internal: what the hub runs at the head of each session's pty.
            HubSupervisor.run(Array(args.dropFirst()))
        case "start":
            close(connect(startingHubIfNeeded: true))
            printStatus()
        case "status", "ls":
            if args.contains("--json") { printStatusJSON() }
            printStatus()
        case "link":
            printLink()
        case "attach":
            guard args.count > 1, let size = terminalSize() else {
                fail(args.count > 1 ? "attach needs a terminal" : "usage: \(commandName) hub attach <id | claude session id>")
            }
            // A Claude conversation id (the UUID in `claude --resume`) is
            // accepted too, for other tools that know sessions that way.
            var id = args[1]
            if HubState.isSessionId(id) {
                guard let hubId = hubSession(forClaudeSessionId: id) else {
                    fail("no hub session is running conversation \(id)")
                }
                id = hubId
            }
            let fd = connect(startingHubIfNeeded: false)
            attachLoop(fd: fd, hello: [
                "op": "attach", "protocol": HubFrame.version, "id": id, "rows": size.rows, "cols": size.cols,
            ], clearFirst: true)
        case "kill":
            guard args.count > 1 else { fail("usage: \(commandName) hub kill <id>") }
            _ = request(["op": "kill", "id": args[1]])
            print("session \(args[1]) told to end")
            exit(0)
        case "stop":
            _ = request(["op": "stop", "force": args.contains("--force")])
            print("hub stopped")
            exit(0)
        case "unlink":
            guard request(["op": "unlink"])["ok"] as? Bool == true else { fail("could not write a new pairing secret") }
            print("Every paired browser is unpaired. Pair again with: \(commandName) hub link")
            exit(0)
        default:
            print("""
            usage: \(commandName) [claude arguments]   start Claude in this directory, through the hub
                   \(commandName) hub status [--json]   the hub, its web address, and its sessions
                   \(commandName) hub link              the link (and QR code) that pairs a browser with the hub
                   \(commandName) hub unlink            unpair every browser (a new secret; pair again with link)
                   \(commandName) hub start             start the hub if it isn't running
                   \(commandName) hub attach <id>       open a running session in this terminal (hub id or Claude session id)
                   \(commandName) hub kill <id>         end a session
                   \(commandName) hub stop [--force]    stop the hub (ends its sessions)
            """)
            exit(args.first == "help" || args.first == "--help" ? 0 : 2)
        }
    }

    /// `hub status --json`: for other tools. Each hub session is joined to
    /// the Claude registry entry it runs (when Claude has registered), so a
    /// caller holding a Claude session id or pid can find the hub id.
    private static func printStatusJSON() -> Never {
        let status = request(["op": "status"])
        let live = SessionScanner.scan()
        var sessions: [[String: Any]] = []
        for entry in status["sessions"] as? [[String: Any]] ?? [] {
            var session = entry
            if let pid = entry["pid"] as? Int,
               let claude = live.first(where: { TerminalFocus.ancestorPids(of: $0.pid).contains(pid_t(pid)) }) {
                session["claudePid"] = Int(claude.pid)
                session["sessionId"] = claude.sessionId
                session["status"] = {
                    switch claude.state {
                    case .busy: return "busy"
                    case .shell: return "shell"
                    case .idle: return "idle"
                    case .waitingForInput: return "waiting"
                    }
                }()
                session["title"] = claude.title
            }
            sessions.append(session)
        }
        let out: [String: Any] = [
            "pid": status["pid"] ?? 0, "protocol": status["protocol"] ?? 0, "port": status["port"] ?? 0,
            "webListening": status["webListening"] ?? false, "tailnetAddresses": tailnetAddresses(),
            "sessions": sessions,
        ]
        if let data = try? JSONSerialization.data(withJSONObject: out, options: [.prettyPrinted, .sortedKeys]) {
            print(String(decoding: data, as: UTF8.self))
        }
        exit(0)
    }

    private static func printStatus() -> Never {
        let status = request(["op": "status"])
        let pid = status["pid"] as? Int ?? 0
        let port = status["port"] as? Int ?? 0
        let started = Date(timeIntervalSince1970: status["startedAt"] as? Double ?? 0)
        print("hub: pid \(pid), up \(StatusFormat.compactAge(since: started))")
        if status["protocol"] as? Int != HubFrame.version {
            print("     NOTE: the hub is running a different build than this command. New sessions need it")
            print("     restarted, which ends its current ones: \(commandName) hub stop")
        }
        if status["webListening"] as? Bool == true {
            print("web: http://localhost:\(port)")
            for address in tailnetAddresses() { print("     http://\(address):\(port)") }
            print("     (a new browser must be paired first: \(commandName) hub link)")
        } else {
            print("web: not listening on port \(port) (see \(HubPaths.log))")
        }
        let sessions = status["sessions"] as? [[String: Any]] ?? []
        if sessions.isEmpty { print("no sessions") }
        for session in sessions {
            let id = session["id"] as? String ?? "?"
            let cwd = ((session["cwd"] as? String ?? "?") as NSString).abbreviatingWithTildeInPath
            let age = StatusFormat.compactAge(since: Date(timeIntervalSince1970: session["startedAt"] as? Double ?? 0))
            let clients = session["clients"] as? Int ?? 0
            print("  \(id)  \(cwd)  up \(age), \(clients) attached, \(session["cols"] ?? 0)x\(session["rows"] ?? 0)")
        }
        exit(0)
    }

    /// Print the pairing links: the web app's address with the hub's
    /// secret attached. Opening one once gives that browser a cookie; the
    /// QR code is the phone's way of opening it.
    private static func printLink() -> Never {
        close(connect(startingHubIfNeeded: true))
        // From the hub itself, not the file: what it is actually checking.
        let link = request(["op": "link"])
        let port = link["port"] as? Int ?? 0
        guard let token = link["token"] as? String, !token.isEmpty else {
            fail("the hub has no pairing secret (it could not write \(HubToken.path))")
        }
        print("Open once in each browser to pair it with this hub. Anyone with the link can use")
        print("the hub from this Mac or your tailnet, so treat it like a password.\n")
        print("  this Mac:  http://localhost:\(port)/auth?k=\(token)")
        let addresses = tailnetAddresses()
        for address in addresses { print("  tailnet:   http://\(address):\(port)/auth?k=\(token)") }
        if let address = addresses.first, let code = qrCode("http://\(address):\(port)/auth?k=\(token)") {
            print("\nScan with the phone (it must be on the tailnet):\n")
            print(code)
        } else if addresses.isEmpty {
            print("\nNo Tailscale address on this Mac right now — connect Tailscale and run this again")
            print("to get the link for a phone.")
        }
        exit(0)
    }

    /// A QR code drawn with half-block characters, black on white whatever
    /// the terminal's colors (scanners want dark modules on a light field).
    static func qrCode(_ text: String) -> String? {
        guard let modules = qrModules(text) else { return nil }
        let quiet = 2
        let side = modules.count + quiet * 2
        func dark(_ x: Int, _ y: Int) -> Bool {
            let mx = x - quiet, my = y - quiet
            return my >= 0 && my < modules.count && mx >= 0 && mx < modules.count && modules[my][mx]
        }
        var lines: [String] = []
        for y in stride(from: 0, to: side, by: 2) {
            var line = "  \u{1b}[30;107m"
            for x in 0..<side {
                switch (dark(x, y), dark(x, y + 1)) {
                case (true, true): line += "█"
                case (true, false): line += "▀"
                case (false, true): line += "▄"
                case (false, false): line += " "
                }
            }
            lines.append(line + "\u{1b}[0m")
        }
        return lines.joined(separator: "\n")
    }

    /// The QR symbol for `text` as rows of modules (true = dark), top row
    /// first, without a quiet zone.
    static func qrModules(_ text: String) -> [[Bool]]? {
        guard let filter = CIFilter(name: "CIQRCodeGenerator") else { return nil }
        filter.setValue(Data(text.utf8), forKey: "inputMessage")
        filter.setValue("M", forKey: "inputCorrectionLevel")
        guard let image = filter.outputImage else { return nil }
        let extent = image.extent.integral
        let width = Int(extent.width), height = Int(extent.height)
        guard width > 0, height > 0, width == height else { return nil }
        var bitmap = [UInt8](repeating: 0, count: width * height * 4)
        CIContext(options: [.useSoftwareRenderer: true]).render(
            image, toBitmap: &bitmap, rowBytes: width * 4, bounds: extent,
            format: .RGBA8, colorSpace: CGColorSpaceCreateDeviceRGB())
        var rows = (0..<height).map { y in (0..<width).map { x in bitmap[(y * width + x) * 4] < 128 } }
        // Trim the generator's own quiet zone.
        while let first = rows.first, !first.contains(true) { rows.removeFirst() }
        while let last = rows.last, !last.contains(true) { rows.removeLast() }
        guard let left = rows.compactMap({ $0.firstIndex(of: true) }).min(),
              let right = rows.compactMap({ $0.lastIndex(of: true) }).max()
        else { return nil }
        return rows.map { Array($0[left...right]) }
    }

    /// This machine's Tailscale IPv4 addresses (100.64.0.0/10), for
    /// printing the address a phone would use.
    static func tailnetAddresses() -> [String] {
        var list: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&list) == 0 else { return [] }
        defer { freeifaddrs(list) }
        var found: [String] = []
        var cursor = list
        while let entry = cursor {
            defer { cursor = entry.pointee.ifa_next }
            guard let address = entry.pointee.ifa_addr, address.pointee.sa_family == sa_family_t(AF_INET) else { continue }
            let bytes = address.withMemoryRebound(to: sockaddr_in.self, capacity: 1) {
                withUnsafeBytes(of: $0.pointee.sin_addr) { Array($0) }
            }
            if bytes[0] == 100, bytes[1] & 0xc0 == 64 {
                found.append(bytes.map(String.init).joined(separator: "."))
            }
        }
        return found
    }

    // MARK: Connection

    private static func fail(_ message: String) -> Never {
        FileHandle.standardError.write(Data("\(commandName): \(message)\n".utf8))
        exit(1)
    }

    private static func execProgram(_ args: [String]) -> Never {
        let argv = [Hub.program] + args
        var cArgs: [UnsafeMutablePointer<CChar>?] = argv.map { strdup($0) } + [nil]
        execvp(Hub.program, &cArgs)
        fail("cannot run \(Hub.program): \(String(cString: strerror(errno)))")
    }

    private static func terminalSize() -> (rows: Int, cols: Int)? {
        var size = winsize()
        for fd in [STDOUT_FILENO, STDIN_FILENO] where ioctl(fd, TIOCGWINSZ, &size) == 0 {
            if size.ws_row > 0 && size.ws_col > 0 { return (Int(size.ws_row), Int(size.ws_col)) }
        }
        return nil
    }

    private static func tryConnect() -> Int32? {
        let path = HubPaths.socket
        var addr = sockaddr_un()
        guard path.utf8.count < MemoryLayout.size(ofValue: addr.sun_path) else { return nil }
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return nil }
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: path.utf8) }
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard rc == 0 else {
            close(fd)
            return nil
        }
        var on: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        return fd
    }

    private static func connect(startingHubIfNeeded: Bool) -> Int32 {
        if let fd = tryConnect() { return fd }
        guard startingHubIfNeeded else { fail("the hub is not running (start it with: \(commandName) hub start)") }
        startHub()
        for _ in 0..<100 {
            usleep(50_000)
            if let fd = tryConnect() { return fd }
        }
        fail("the hub did not start — see \(HubPaths.log)")
    }

    /// Launch the hub as its own session, detached from this terminal, so
    /// closing the window it was first started from doesn't take it down.
    private static func startHub() {
        HubPaths.ensureHome()
        // The log is append-only; start a fresh one when it has grown.
        if let size = (try? FileManager.default.attributesOfItem(atPath: HubPaths.log))?[.size] as? Int, size > 2 << 20 {
            _ = rename(HubPaths.log, HubPaths.log + ".1")
        }
        guard let command = HubPaths.selfCommand else { fail("cannot locate own executable") }
        let executable = command[0]
        let argv = command + ["hub", "run"]

        var actions: posix_spawn_file_actions_t?
        posix_spawn_file_actions_init(&actions)
        defer { posix_spawn_file_actions_destroy(&actions) }
        posix_spawn_file_actions_addopen(&actions, 0, "/dev/null", O_RDONLY, 0)
        posix_spawn_file_actions_addopen(&actions, 1, HubPaths.log, O_WRONLY | O_CREAT | O_APPEND, 0o600)
        posix_spawn_file_actions_adddup2(&actions, 1, 2)
        var attr: posix_spawnattr_t?
        posix_spawnattr_init(&attr)
        defer { posix_spawnattr_destroy(&attr) }
        var allSignals = sigset_t()
        sigfillset(&allSignals)
        var noSignals = sigset_t()
        sigemptyset(&noSignals)
        posix_spawnattr_setsigdefault(&attr, &allSignals)
        posix_spawnattr_setsigmask(&attr, &noSignals)
        posix_spawnattr_setflags(&attr, Int16(
            POSIX_SPAWN_SETSID | POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK))

        var cArgs: [UnsafeMutablePointer<CChar>?] = argv.map { strdup($0) } + [nil]
        var cEnv: [UnsafeMutablePointer<CChar>?] =
            ProcessInfo.processInfo.environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
        defer {
            for pointer in cArgs { free(pointer) }
            for pointer in cEnv { free(pointer) }
        }
        var pid: pid_t = 0
        let rc = posix_spawn(&pid, executable, &actions, &attr, &cArgs, &cEnv)
        guard rc == 0 else { fail("cannot start the hub: \(String(cString: strerror(rc)))") }
    }

    /// One request, one reply.
    private static func request(_ hello: [String: Any]) -> [String: Any] {
        let fd = connect(startingHubIfNeeded: false)
        guard HubIO.writeAll(fd, HubFrame.encodeJSON(.hello, hello)) else { fail("lost the hub") }
        var decoder = HubFrameDecoder()
        var buffer = [UInt8](repeating: 0, count: 65_536)
        while true {
            let n = read(fd, &buffer, buffer.count)
            if n < 0 && errno == EINTR { continue }
            guard n > 0, let frames = decoder.feed(Data(buffer[0..<n])) else { fail("lost the hub") }
            for (kind, payload) in frames {
                if kind == .reply { return HubFrame.json(payload) }
                if kind == .error { fail(HubFrame.json(payload)["message"] as? String ?? "request failed") }
            }
        }
    }

    // MARK: Attached terminal

    /// Be the terminal for a session: keystrokes up, screen bytes down,
    /// untouched in both directions, until the session ends or this
    /// terminal goes away (which only detaches — the session keeps running
    /// in the hub).
    private static func attachLoop(fd: Int32, hello: [String: Any], clearFirst: Bool) -> Never {
        guard HubIO.writeAll(fd, HubFrame.encodeJSON(.hello, hello)) else { fail("lost the hub") }

        var pipeFDs: [Int32] = [0, 0]
        pipe(&pipeFDs)
        _ = fcntl(pipeFDs[1], F_SETFL, fcntl(pipeFDs[1], F_GETFL) | O_NONBLOCK)
        hubCLISignalPipe = pipeFDs[1]
        // Each handler reports its own signal number. SIGINT and SIGQUIT
        // never come from the keyboard in raw mode, but a `kill -INT` from
        // outside must still leave the terminal as it was found.
        for sig in [SIGWINCH, SIGTERM, SIGHUP, SIGINT, SIGQUIT] {
            signal(sig) { number in
                var byte = UInt8(truncatingIfNeeded: number)
                _ = write(hubCLISignalPipe, &byte, 1)
            }
        }
        signal(SIGPIPE, SIG_IGN)

        // Raw before the hub is even asked: from here on a keystroke is
        // input for the session (it waits in the tty until we forward it),
        // not a Ctrl+C that would kill this client and strand a session the
        // hub has already started.
        var original = termios()
        var raw = false
        if tcgetattr(STDIN_FILENO, &original) == 0 {
            var rawMode = original
            cfmakeraw(&rawMode)
            raw = tcsetattr(STDIN_FILENO, TCSANOW, &rawMode) == 0
        }
        var attached = false
        // Mirrors what the session has switched on in *this* terminal, so
        // it can be switched back off if we leave before the program does.
        var screen = TerminalStream()
        func restore() {
            guard raw else { return }
            let reset = screen.modes.resetSequence
            if !reset.isEmpty { _ = HubIO.writeAll(STDOUT_FILENO, reset) }
            tcsetattr(STDIN_FILENO, TCSANOW, &original)
            raw = false
        }
        func leave(_ code: Int32, _ note: String? = nil) -> Never {
            restore()
            if let note { FileHandle.standardError.write(Data("\r\n[\(commandName): \(note)]\r\n".utf8)) }
            exit(code)
        }

        var decoder = HubFrameDecoder()
        var buffer = [UInt8](repeating: 0, count: 65_536)
        var descriptors = [
            pollfd(fd: fd, events: Int16(POLLIN), revents: 0),
            pollfd(fd: STDIN_FILENO, events: Int16(POLLIN), revents: 0),
            pollfd(fd: pipeFDs[0], events: Int16(POLLIN), revents: 0),
        ]
        while true {
            // Keystrokes aren't read until the hub has answered; until then
            // they wait in the tty.
            descriptors[1].events = attached ? Int16(POLLIN) : 0
            if poll(&descriptors, 3, -1) < 0 {
                if errno == EINTR { continue }
                leave(1, "poll failed")
            }

            if descriptors[2].revents != 0 {
                let n = read(pipeFDs[0], &buffer, 64)
                if n > 0, let fatal = buffer[0..<n].first(where: { Int32($0) != SIGWINCH }) {
                    leave(128 + Int32(fatal))
                }
                if n > 0, let size = terminalSize() {
                    _ = HubIO.writeAll(fd, HubFrame.encodeJSON(.resize, ["rows": size.rows, "cols": size.cols]))
                }
            }

            if descriptors[0].revents != 0 {
                let n = read(fd, &buffer, buffer.count)
                if n < 0 && errno == EINTR { continue }
                guard n > 0, let frames = decoder.feed(Data(buffer[0..<n])) else {
                    leave(1, "disconnected from the hub. If it is still running, so is the session: \(commandName) hub status")
                }
                for (kind, payload) in frames {
                    switch kind {
                    case .attached:
                        guard HubFrame.json(payload)["protocol"] as? Int == HubFrame.version else {
                            leave(1, "the running hub is a different build than this command — the session it "
                                + "just started is still there (\(commandName) hub status)")
                        }
                        attached = true
                        if clearFirst { _ = HubIO.writeAll(STDOUT_FILENO, Data("\u{1b}[H\u{1b}[2J".utf8)) }
                    case .output:
                        _ = screen.feed(payload)
                        if !HubIO.writeAll(STDOUT_FILENO, payload) { leave(1) }
                    case .exit:
                        leave(Int32(HubFrame.json(payload)["code"] as? Int ?? 0))
                    case .error:
                        restore()
                        fail(HubFrame.json(payload)["message"] as? String ?? "the hub refused")
                    default:
                        break
                    }
                }
            }

            if descriptors[1].revents != 0 {
                let n = read(STDIN_FILENO, &buffer, buffer.count)
                if n < 0 && errno == EINTR { continue }
                // The terminal is gone. Leave quietly; the session stays.
                guard n > 0 else { leave(0) }
                if !HubIO.writeAll(fd, HubFrame.encode(.input, Data(buffer[0..<n]))) {
                    leave(1, "disconnected from the hub. If it is still running, so is the session: \(commandName) hub status")
                }
            }
        }
    }
}
