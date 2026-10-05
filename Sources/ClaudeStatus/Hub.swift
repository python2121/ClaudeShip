import Darwin
import Foundation

enum HubLog {
    static func log(_ message: String) {
        let stamp = ISO8601DateFormatter().string(from: Date())
        FileHandle.standardError.write(Data("\(stamp) \(message)\n".utf8))
    }
}

/// One screen looking at a session: a terminal client on the Unix socket,
/// or a browser on a WebSocket. Called only on the hub queue.
protocol HubAttachment: AnyObject {
    /// The size this client's own window fits.
    var rows: UInt16 { get set }
    var cols: UInt16 { get set }
    func sendOutput(_ data: Data)
    /// The session's size, and whether it is this client the session is
    /// sized for. Sent when either changes, and in answer to a resize.
    func sendSize(rows: UInt16, cols: UInt16, owner: Bool)
    func sendExit(code: Int32)
}

/// A program running on a hub-owned pty. Touched only on the hub queue.
final class HubSession {
    let id: String
    /// The session leader — `HubSupervisor`; the program is its child.
    let pid: pid_t
    let master: Int32
    let cwd: String
    let startedAt = Date()
    /// "terminal" (started by the `claudeandrew` command) or "web".
    let origin: String
    let permissionMode: String?
    var rows: UInt16
    var cols: UInt16
    var stream = TerminalStream()
    var replay = ReplayBuffer()
    var attachments: [HubAttachment] = []
    /// The client whose window the pty is currently sized for: the one
    /// that attached, typed, or resized most recently.
    weak var sizeOwner: HubAttachment?
    var readSource: DispatchSourceRead?
    var processSource: DispatchSourceProcess?
    var pendingInput = Data()
    var inputRetryScheduled = false
    /// Grows while the program isn't reading, so a wedged one doesn't keep
    /// the hub waking up 300 times a second for nothing.
    var inputRetryMilliseconds = 3
    /// No more reads, writes, or ioctls on `master` (it is closed, or about
    /// to be by the read source's cancel handler).
    var masterClosed = false
    var finished = false
    var exitCode: Int32 = 0

    init(id: String, pid: pid_t, master: Int32, cwd: String, origin: String, permissionMode: String?,
         rows: UInt16, cols: UInt16) {
        self.id = id
        self.pid = pid
        self.master = master
        self.cwd = cwd
        self.origin = origin
        self.permissionMode = permissionMode
        self.rows = rows
        self.cols = cols
    }
}

/// The session hub: a long-lived process that owns every pty, so a session
/// outlives whichever terminal or browser happens to be looking at it.
/// Terminal clients reach it over a Unix socket, browsers through
/// `HubWebServer`. All state lives on `queue`.
final class Hub {
    let queue = DispatchQueue(label: "claudestatus.hub")
    private(set) var sessions: [HubSession] = []
    /// Sessions that ended in the last half minute, kept so a screen that
    /// arrives just too late still gets what the program said and how it
    /// exited — a launch that fails at once would otherwise just vanish.
    private var ended: [HubSession] = []
    var config: HubConfig
    let startedAt = Date()
    private var acceptSource: DispatchSourceRead?
    private var locals: [ObjectIdentifier: HubLocalClient] = [:]
    private var web: HubWebServer?

    /// The program a session runs. `CLAUDEANDREW_CMD` swaps in a stand-in
    /// so tests can exercise the pty path without starting Claude.
    static var program: String {
        ProcessInfo.processInfo.environment["CLAUDEANDREW_CMD"].flatMap { $0.isEmpty ? nil : $0 } ?? "claude"
    }

    init(config: HubConfig) {
        self.config = config
    }

    // MARK: Lifecycle

    /// Become the hub: take the single-instance lock, open the socket and
    /// the web server, and never return.
    static func runForever() -> Never {
        signal(SIGPIPE, SIG_IGN)
        HubPaths.ensureHome()
        let lockFD = open(HubPaths.lock, O_CREAT | O_RDWR | O_CLOEXEC, 0o600)
        guard lockFD >= 0, flock(lockFD, LOCK_EX | LOCK_NB) == 0 else {
            HubLog.log("another hub already holds \(HubPaths.lock); exiting")
            exit(0)
        }
        // Don't pin whatever directory the first client happened to be in.
        _ = chdir("/")
        // Every session and every viewer is a descriptor or three; the
        // default soft limit of 256 is not many.
        var limit = rlimit()
        if getrlimit(RLIMIT_NOFILE, &limit) == 0 {
            limit.rlim_cur = min(4096, limit.rlim_max)
            _ = setrlimit(RLIMIT_NOFILE, &limit)
        }

        let hub = Hub(config: .load())
        hub.queue.sync { hub.start() }

        // Dispatch delivers these only once the default action is disabled.
        var signalSources: [DispatchSourceSignal] = []
        for sig in [SIGTERM, SIGINT] {
            signal(sig, SIG_IGN)
            let source = DispatchSource.makeSignalSource(signal: sig, queue: hub.queue)
            source.setEventHandler { hub.shutdown() }
            source.resume()
            signalSources.append(source)
        }
        withExtendedLifetime(signalSources) { dispatchMain() }
    }

    private func start() {
        let path = HubPaths.socket
        var addr = sockaddr_un()
        guard path.utf8.count < MemoryLayout.size(ofValue: addr.sun_path) else {
            HubLog.log("socket path too long: \(path)")
            exit(1)
        }
        unlink(path)
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            HubLog.log("socket: \(String(cString: strerror(errno)))")
            exit(1)
        }
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { buffer in
            buffer.copyBytes(from: path.utf8)
        }
        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard bound == 0, chmod(path, 0o600) == 0, listen(fd, 16) == 0 else {
            HubLog.log("bind \(path): \(String(cString: strerror(errno)))")
            exit(1)
        }

        let source = DispatchSource.makeReadSource(fileDescriptor: fd, queue: queue)
        source.setEventHandler { [weak self] in
            let client = accept(fd, nil, nil)
            guard client >= 0, let self else { return }
            _ = fcntl(client, F_SETFD, FD_CLOEXEC)
            var on: Int32 = 1
            setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
            let local = HubLocalClient(fd: client, hub: self)
            self.locals[ObjectIdentifier(local)] = local
            local.start()
        }
        source.resume()
        acceptSource = source

        let server = HubWebServer(hub: self)
        server.start()
        web = server
        HubLog.log("hub up: pid \(getpid()), socket \(path), web port \(config.port), root \(config.root)")
    }

    /// Hang up every session and exit.
    func shutdown() -> Never {
        HubLog.log("hub shutting down with \(sessions.count) session(s)")
        for session in sessions { kill(-session.pid, SIGHUP) }
        unlink(HubPaths.socket)
        exit(0)
    }

    func localClosed(_ client: HubLocalClient) {
        locals[ObjectIdentifier(client)] = nil
    }

    // MARK: Sessions

    func session(id: String) -> HubSession? {
        sessions.first { $0.id == id }
    }

    /// A live session, or one that ended moments ago (attach handles both).
    func sessionOrRecentlyEnded(id: String) -> HubSession? {
        session(id: id) ?? ended.first { $0.id == id }
    }

    /// The shell's convention: the exit status, or 128 + the fatal signal.
    static func exitCode(fromWaitStatus status: Int32) -> Int32 {
        let signal = status & 0x7f
        return signal == 0 ? (status >> 8) & 0xff : 128 + signal
    }

    static func clampSize(rows: Int, cols: Int) -> (rows: UInt16, cols: UInt16)? {
        guard rows >= 2, cols >= 10, rows <= 1000, cols <= 2000 else { return nil }
        return (UInt16(rows), UInt16(cols))
    }

    private func newID() -> String {
        while true {
            let id = String(format: "%06x", UInt32.random(in: 0..<(1 << 24)))
            if session(id: id) == nil { return id }
        }
    }

    func launch(argv: [String], cwd: String, env: [String: String], rows: UInt16, cols: UInt16,
                origin: String, permissionMode: String?) throws -> HubSession {
        var isDirectory: ObjCBool = false
        guard FileManager.default.fileExists(atPath: cwd, isDirectory: &isDirectory), isDirectory.boolValue else {
            throw HubPTY.SpawnError(description: "not a directory: \(cwd)")
        }
        let spawned = try HubPTY.spawn(argv: argv, cwd: cwd, env: env, rows: rows, cols: cols)
        let session = HubSession(
            id: newID(), pid: spawned.pid, master: spawned.master, cwd: cwd, origin: origin,
            permissionMode: permissionMode, rows: rows, cols: cols)
        sessions.append(session)

        let master = spawned.master
        let read = DispatchSource.makeReadSource(fileDescriptor: master, queue: queue)
        read.setEventHandler { [weak self, weak session] in
            guard let self, let session else { return }
            self.drain(session)
        }
        // The only place the master is closed: after the source is fully
        // cancelled, so the descriptor can't be reused under a live source.
        read.setCancelHandler { close(master) }
        read.resume()
        session.readSource = read

        let process = DispatchSource.makeProcessSource(identifier: spawned.pid, eventMask: .exit, queue: queue)
        process.setEventHandler { [weak self, weak session] in
            guard let self, let session else { return }
            self.reap(session)
        }
        process.resume()
        session.processSource = process
        // Covers a child that died before the source was watching.
        reap(session)

        HubLog.log("session \(session.id): pid \(session.pid) in \(cwd) (\(origin))")
        return session
    }

    /// Read what the pty has. Bounded per call so one chatty session can't
    /// starve the queue; the read source fires again if more is waiting.
    private func drain(_ session: HubSession, untilEmpty: Bool = false) {
        guard !session.masterClosed else { return }
        var buffer = [UInt8](repeating: 0, count: 65_536)
        var rounds = 0
        while rounds < (untilEmpty ? 256 : 8) {
            rounds += 1
            let n = buffer.withUnsafeMutableBytes { read(session.master, $0.baseAddress, $0.count) }
            if n > 0 {
                output(session, Data(buffer[0..<n]))
            } else if n < 0 && errno == EINTR {
                continue
            } else if n < 0 && errno == EAGAIN {
                return
            } else {
                // EOF or EIO: every slave descriptor is closed.
                closeMaster(session)
                reap(session)
                return
            }
        }
    }

    private func output(_ session: HubSession, _ chunk: Data) {
        for piece in session.stream.feed(chunk) {
            switch piece {
            case .bytes(let data): session.replay.append(data, modesAfter: session.stream.modes)
            case .clear(let modes): session.replay.reset(base: modes)
            }
        }
        for attachment in session.attachments { attachment.sendOutput(chunk) }
    }

    private func closeMaster(_ session: HubSession) {
        guard !session.masterClosed else { return }
        session.masterClosed = true
        session.pendingInput.removeAll()
        session.readSource?.cancel()
        session.readSource = nil
    }

    /// If the session's process has exited, collect it and end the session.
    private func reap(_ session: HubSession) {
        guard !session.finished else { return }
        var status: Int32 = 0
        guard waitpid(session.pid, &status, WNOHANG) == session.pid else { return }
        session.finished = true
        // Whatever the program wrote on its way out (its terminal cleanup,
        // usually) still has to reach the clients before the exit notice.
        drain(session, untilEmpty: true)
        closeMaster(session)
        session.processSource?.cancel()
        session.processSource = nil

        let code = Hub.exitCode(fromWaitStatus: status)
        session.exitCode = code
        HubLog.log("session \(session.id): pid \(session.pid) exited with \(code)")
        sessions.removeAll { $0 === session }
        ended.append(session)
        queue.asyncAfter(deadline: .now() + 30) { [weak self, weak session] in
            self?.ended.removeAll { $0 === session }
        }
        let attachments = session.attachments
        session.attachments = []
        for attachment in attachments { attachment.sendExit(code: code) }
    }

    /// End a session the way closing its terminal window would: hang up
    /// the supervisor, which passes it to the program (once — some
    /// programs take a second SIGHUP as "skip the cleanup") and stays until
    /// the program has exited, so `finished` means it is really gone. If
    /// it still isn't after five seconds: SIGTERM, which the supervisor
    /// answers by SIGKILLing its job, then SIGKILL for whatever is left.
    func terminate(_ session: HubSession) {
        guard !session.finished else { return }
        kill(-session.pid, SIGHUP)
        queue.asyncAfter(deadline: .now() + 5) { [weak self, weak session] in
            guard let self, let session, !session.finished else { return }
            kill(-session.pid, SIGTERM)
            self.queue.asyncAfter(deadline: .now() + 1) { [weak session] in
                guard let session, !session.finished else { return }
                // Both groups are in the pty's own session, and the supervisor
                // is unreaped until `finished`, so neither id can have been reused.
                if !session.masterClosed, let job = HubPTY.foregroundGroup(master: session.master), job != session.pid {
                    kill(-job, SIGKILL)
                }
                kill(-session.pid, SIGKILL)
            }
        }
    }

    // MARK: Attachments

    /// `claim: false` joins as a spectator: the session keeps the size it
    /// has (unless nobody owns it), and this client is told what that is.
    func attach(_ attachment: HubAttachment, to session: HubSession, claim: Bool = true) {
        guard !session.finished else {
            // Died before anyone was looking: show what it said, then go.
            attachment.sendOutput(session.replay.snapshot())
            attachment.sendExit(code: session.exitCode)
            return
        }
        session.attachments.append(attachment)
        // Size first, so the program is already redrawing for this window
        // by the time the replay lands.
        if !((claim || session.sizeOwner == nil) && claimSize(attachment, session)) {
            attachment.sendSize(rows: session.rows, cols: session.cols, owner: session.sizeOwner === attachment)
        }
        // Plus the front of any sequence a read boundary has split, so the
        // live stream this client now joins continues it.
        attachment.sendOutput(session.replay.snapshot() + session.stream.pendingBytes)
    }

    func detach(_ attachment: HubAttachment, from session: HubSession) {
        guard let index = session.attachments.firstIndex(where: { $0 === attachment }) else { return }
        session.attachments.remove(at: index)
        if session.sizeOwner === attachment {
            session.sizeOwner = nil
            if let next = session.attachments.last { claimSize(next, session) }
        }
    }

    func input(_ data: Data, from attachment: HubAttachment, to session: HubSession) {
        guard !session.masterClosed else { return }
        // Typing here makes this the screen the session is sized for — but
        // only typing, not the terminal's own chatter.
        if session.sizeOwner !== attachment, TerminalInput.isUserActivity(data) {
            claimSize(attachment, session)
        }
        guard session.pendingInput.count + data.count <= 8 << 20 else { return }
        session.pendingInput.append(data)
        flushInput(session)
    }

    func resize(_ attachment: HubAttachment, in session: HubSession, rows: UInt16, cols: UInt16) {
        attachment.rows = rows
        attachment.cols = cols
        // Always answered, so the asker knows where it stands even when
        // the session was already that size.
        if !claimSize(attachment, session) {
            attachment.sendSize(rows: session.rows, cols: session.cols, owner: true)
        }
    }

    /// A client's window changed while it is only watching: remember the
    /// size for when it next takes over, and resize now only if it already
    /// is the owner.
    func noteFit(_ attachment: HubAttachment, in session: HubSession, rows: UInt16, cols: UInt16) {
        attachment.rows = rows
        attachment.cols = cols
        if session.sizeOwner === attachment { claimSize(attachment, session) }
    }

    /// Make `attachment` the client the pty is sized for. Returns whether
    /// anything changed — the size, or who owns it — in which case every
    /// client (this one included) has been told.
    @discardableResult
    private func claimSize(_ attachment: HubAttachment, _ session: HubSession) -> Bool {
        let newOwner = session.sizeOwner !== attachment
        session.sizeOwner = attachment
        var resized = false
        if !session.masterClosed, (attachment.rows, attachment.cols) != (session.rows, session.cols) {
            session.rows = attachment.rows
            session.cols = attachment.cols
            HubPTY.resize(master: session.master, rows: session.rows, cols: session.cols)
            resized = true
        }
        guard resized || newOwner else { return false }
        for other in session.attachments {
            other.sendSize(rows: session.rows, cols: session.cols, owner: other === attachment)
        }
        return true
    }

    /// Write queued keystrokes to the pty. Its input queue is small (about
    /// 1 KB on macOS), so a paste takes many rounds: write what fits, and
    /// come back in a few milliseconds for the rest.
    ///
    /// A timer, not a write-readiness source: kqueue's EVFILT_WRITE on a
    /// pty master does not reliably fire when the slave drains its input
    /// (measured: an 8 KB paste stalled after the first kilobyte most of
    /// the time). Polling a queue that is only ever non-empty mid-paste
    /// costs nothing the rest of the time.
    private func flushInput(_ session: HubSession) {
        while !session.pendingInput.isEmpty, !session.masterClosed {
            let n = session.pendingInput.withUnsafeBytes { write(session.master, $0.baseAddress, $0.count) }
            if n > 0 {
                session.pendingInput.removeFirst(n)
                session.inputRetryMilliseconds = 3
            } else if n < 0 && errno == EINTR {
                continue
            } else if n < 0 && errno == EAGAIN {
                guard !session.inputRetryScheduled else { return }
                session.inputRetryScheduled = true
                let delay = session.inputRetryMilliseconds
                session.inputRetryMilliseconds = min(100, delay * 2)
                queue.asyncAfter(deadline: .now() + .milliseconds(delay)) { [weak self, weak session] in
                    guard let self, let session else { return }
                    session.inputRetryScheduled = false
                    self.flushInput(session)
                }
                return
            } else {
                session.pendingInput.removeAll()
            }
        }
    }

    // MARK: Launch recipes

    /// What `claudeandrew [args]` runs: the program, straight from the
    /// client's own environment and directory, as if typed there.
    func launchFromTerminal(cwd: String, args: [String], env: [String: String],
                            rows: UInt16, cols: UInt16) throws -> HubSession {
        try launch(argv: [Hub.program] + args, cwd: cwd, env: env, rows: rows, cols: cols,
                   origin: "terminal", permissionMode: nil)
    }

    /// What the web app runs. There is no terminal to inherit from, so the
    /// program starts under the user's login shell (`-l -i`, which is where
    /// PATH and version managers get set up) with a small clean
    /// environment rather than whatever the hub happened to be started in.
    func launchFromWeb(cwd: String, permissionMode: String, resume: String?) throws -> HubSession {
        var args = ["--permission-mode", permissionMode]
        if let resume { args += ["--resume", resume] }
        let shell = Hub.loginShell()
        return try launch(argv: Hub.loginShellArgv(shell: shell, program: Hub.program, args: args), cwd: cwd,
                          env: Hub.webEnvironment(shell: shell), rows: 36, cols: 120,
                          origin: "web", permissionMode: permissionMode)
    }

    /// `program args…` run by a login, interactive shell. The words travel
    /// as arguments, never inside the script text: after `-c <script>`, the
    /// next word is $0 in POSIX shells and the first of $argv in fish — the
    /// program name either way.
    static func loginShellArgv(shell: String, program: String, args: [String]) -> [String] {
        let exec = shell.hasSuffix("/fish") ? "exec $argv" : "exec \"$0\" \"$@\""
        return [shell, "-l", "-i", "-c", exec, program] + args
    }

    static func loginShell() -> String {
        let known = ["zsh", "bash", "fish", "sh", "dash", "ksh"]
        if let entry = getpwuid(getuid()), let shell = entry.pointee.pw_shell {
            let path = String(cString: shell)
            if known.contains((path as NSString).lastPathComponent), access(path, X_OK) == 0 { return path }
        }
        return "/bin/zsh"
    }

    static func webEnvironment(shell: String, base: [String: String] = ProcessInfo.processInfo.environment) -> [String: String] {
        var env: [String: String] = [:]
        for key in ["HOME", "USER", "LOGNAME", "PATH", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "SSH_AUTH_SOCK",
                    "__CF_USER_TEXT_ENCODING"] {
            if let value = base[key] { env[key] = value }
        }
        env["SHELL"] = shell
        env["TERM"] = "xterm-256color"
        env["COLORTERM"] = "truecolor"
        if env["LANG"] == nil { env["LANG"] = "en_US.UTF-8" }
        if env["PATH"] == nil { env["PATH"] = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin" }
        return env
    }

    // MARK: Local clients

    func describe() -> [String: Any] {
        [
            "pid": Int(getpid()),
            "protocol": HubFrame.version,
            "startedAt": startedAt.timeIntervalSince1970,
            "port": config.port,
            "webListening": web?.listening ?? false,
            "root": config.root,
            "sessions": sessions.map { session -> [String: Any] in
                [
                    "id": session.id, "pid": Int(session.pid), "cwd": session.cwd,
                    "startedAt": session.startedAt.timeIntervalSince1970,
                    "clients": session.attachments.count, "origin": session.origin,
                    "rows": Int(session.rows), "cols": Int(session.cols),
                ]
            },
        ]
    }

    /// The first frame from a terminal client says what it wants.
    func handleHello(_ request: [String: Any], from client: HubLocalClient) {
        let op = request["op"] as? String ?? ""
        let size = Hub.clampSize(rows: request["rows"] as? Int ?? 0, cols: request["cols"] as? Int ?? 0)
        // Managing the hub works across builds; sharing a terminal doesn't.
        if (op == "launch" || op == "attach"), request["protocol"] as? Int != HubFrame.version {
            let theirs = request["protocol"] as? Int ?? 0
            client.fail(theirs < HubFrame.version
                ? "this \(HubCLI.commandName) is an older build than the running hub. Run the installed one "
                    + "(a new terminal, or reinstall)."
                : "the running hub is an older build than this command. Restart it when its sessions can end: "
                    + "\(HubCLI.commandName) hub stop, then try again.")
            return
        }
        switch op {
        case "launch":
            guard let cwd = request["cwd"] as? String, let size else {
                client.fail("launch needs a directory and a terminal size")
                return
            }
            do {
                let session = try launchFromTerminal(
                    cwd: cwd, args: request["args"] as? [String] ?? [],
                    env: request["env"] as? [String: String] ?? [:], rows: size.rows, cols: size.cols)
                client.begin(session, rows: size.rows, cols: size.cols)
            } catch {
                client.fail("\(error)")
            }
        case "attach":
            guard let size else {
                client.fail("attach needs a terminal size")
                return
            }
            guard let session = sessionOrRecentlyEnded(id: request["id"] as? String ?? "") else {
                client.fail("no such session: \(request["id"] as? String ?? "")")
                return
            }
            client.begin(session, rows: size.rows, cols: size.cols)
        case "status":
            client.reply(describe())
        case "link":
            // Only this user can reach the socket; the secret goes no further.
            client.reply(["port": config.port, "token": web?.token ?? ""])
        case "unlink":
            // A new pairing secret: every browser paired so far is out.
            client.reply(["ok": web?.rotateToken() ?? false])
        case "kill":
            guard let session = session(id: request["id"] as? String ?? "") else {
                client.fail("no such session: \(request["id"] as? String ?? "")")
                return
            }
            terminate(session)
            client.reply(["ok": true])
        case "stop":
            if !sessions.isEmpty && request["force"] as? Bool != true {
                client.fail("\(sessions.count) session(s) still running — they end with the hub. Use --force to stop anyway.")
                return
            }
            client.reply(["ok": true]) { [weak self] in self?.shutdown() }
        default:
            client.fail("unknown request: \(op)")
        }
    }
}

/// A `claudeandrew` process connected over the Unix socket. One connection
/// is either one attached terminal or one request/reply.
final class HubLocalClient: HubAttachment {
    private let fd: Int32
    private unowned let hub: Hub
    var rows: UInt16 = 24
    var cols: UInt16 = 80
    private var session: HubSession?
    private var decoder = HubFrameDecoder()
    private var readSource: DispatchSourceRead?
    /// Writes block, so they run here — a stalled terminal (a suspended
    /// client, a full socket) must not stall the hub.
    private let writer = DispatchQueue(label: "claudestatus.hub.local-writer")
    private let lock = NSLock()
    private var queuedBytes = 0
    private var closed = false
    private var greeted = false
    private static let maxQueuedBytes = 32 << 20

    init(fd: Int32, hub: Hub) {
        self.fd = fd
        self.hub = hub
    }

    func start() {
        let fd = self.fd
        let writer = self.writer
        let source = DispatchSource.makeReadSource(fileDescriptor: fd, queue: hub.queue)
        source.setEventHandler { [weak self] in self?.readable() }
        source.setCancelHandler {
            // Unblocks a writer stuck mid-write; the close itself queues
            // behind whatever writes are left, so the fd can't be reused
            // under them.
            shutdown(fd, SHUT_RDWR)
            writer.async { Darwin.close(fd) }
        }
        source.resume()
        readSource = source
    }

    private func readable() {
        var buffer = [UInt8](repeating: 0, count: 65_536)
        let n = buffer.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
        if n < 0 && (errno == EINTR || errno == EAGAIN) { return }
        guard n > 0, let frames = decoder.feed(Data(buffer[0..<n])) else {
            close()
            return
        }
        for (kind, payload) in frames {
            guard !closed else { return }
            switch kind {
            case .hello:
                guard !greeted else { continue }
                greeted = true
                hub.handleHello(HubFrame.json(payload), from: self)
            case .input:
                if let session { hub.input(payload, from: self, to: session) }
            case .resize:
                let request = HubFrame.json(payload)
                if let session,
                   let size = Hub.clampSize(rows: request["rows"] as? Int ?? 0, cols: request["cols"] as? Int ?? 0) {
                    hub.resize(self, in: session, rows: size.rows, cols: size.cols)
                }
            default:
                break
            }
        }
    }

    /// Attach this client to `session` and tell it so.
    func begin(_ session: HubSession, rows: UInt16, cols: UInt16) {
        self.rows = rows
        self.cols = cols
        self.session = session
        send(HubFrame.encodeJSON(.attached, [
            "id": session.id, "pid": Int(session.pid), "protocol": HubFrame.version,
        ]))
        hub.attach(self, to: session)
    }

    func fail(_ message: String) {
        send(HubFrame.encodeJSON(.error, ["message": message]))
        closeAfterFlush()
    }

    func reply(_ object: [String: Any], then: (() -> Void)? = nil) {
        send(HubFrame.encodeJSON(.reply, object))
        closeAfterFlush(then: then)
    }

    func sendOutput(_ data: Data) {
        send(HubFrame.encode(.output, data))
    }

    func sendSize(rows: UInt16, cols: UInt16, owner: Bool) {
        // A terminal client draws whatever arrives; it has no use for this.
    }

    func sendExit(code: Int32) {
        session = nil
        send(HubFrame.encodeJSON(.exit, ["code": Int(code)]))
        closeAfterFlush()
    }

    private func send(_ frame: Data) {
        guard !closed else { return }
        lock.lock()
        queuedBytes += frame.count
        let overflow = queuedBytes > Self.maxQueuedBytes
        lock.unlock()
        if overflow {
            // Not reading for this long means it's gone or stuck; a client
            // that comes back can re-attach and get the replay.
            close()
            return
        }
        let fd = self.fd
        writer.async { [weak self] in
            let ok = HubIO.writeAll(fd, frame)
            guard let self else { return }
            self.lock.lock()
            self.queuedBytes -= frame.count
            self.lock.unlock()
            if !ok { self.hub.queue.async { self.close() } }
        }
    }

    private func closeAfterFlush(then: (() -> Void)? = nil) {
        // Strong on purpose: the peer may hang up the moment it has the
        // reply, and `then` (the hub's shutdown, for `stop`) must still run.
        writer.async { [self] in
            hub.queue.async {
                self.close()
                then?()
            }
        }
    }

    func close() {
        guard !closed else { return }
        closed = true
        if let session {
            hub.detach(self, from: session)
            self.session = nil
        }
        hub.localClosed(self)
        readSource?.cancel()
        readSource = nil
    }
}

enum HubIO {
    /// Blocking write of the whole buffer; false if the peer is gone.
    static func writeAll(_ fd: Int32, _ data: Data) -> Bool {
        data.withUnsafeBytes { raw -> Bool in
            var offset = 0
            while offset < raw.count {
                let n = write(fd, raw.baseAddress! + offset, raw.count - offset)
                if n > 0 {
                    offset += n
                } else if n < 0 && errno == EINTR {
                    continue
                } else if n < 0 && errno == EAGAIN {
                    var descriptor = pollfd(fd: fd, events: Int16(POLLOUT), revents: 0)
                    _ = poll(&descriptor, 1, 1000)
                } else {
                    return false
                }
            }
            return true
        }
    }
}
