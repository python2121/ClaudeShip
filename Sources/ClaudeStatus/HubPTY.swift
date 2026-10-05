import Darwin
import Foundation

/// Starts a program on a pseudo-terminal the hub owns.
enum HubPTY {
    struct Spawned {
        let pid: pid_t
        /// The pty master, non-blocking. The caller owns and closes it.
        let master: Int32
    }

    struct SpawnError: Error, CustomStringConvertible {
        let description: String
    }

    /// Runs `argv` in `cwd` on a fresh pty of the given size, in a new
    /// session of its own.
    ///
    /// posix_spawn rather than forkpty: fork in a multithreaded Swift
    /// process is only safe up to the exec, and forkpty does real work in
    /// between. `POSIX_SPAWN_SETSID` makes the child a session leader, and
    /// the slave it then opens as fd 0 (without O_NOCTTY) becomes its
    /// controlling terminal. A `/bin/sh` trampoline sets the working
    /// directory and execs `HubSupervisor` (this binary), which stays as
    /// the session leader and runs the program as its foreground job — see
    /// there for why the program can't be the leader itself. The pid
    /// returned is the supervisor's; the program is its child.
    static func spawn(argv: [String], cwd: String, env: [String: String], rows: UInt16, cols: UInt16) throws -> Spawned {
        precondition(!argv.isEmpty)
        let master = posix_openpt(O_RDWR | O_NOCTTY)
        guard master >= 0 else { throw SpawnError(description: "posix_openpt: \(String(cString: strerror(errno)))") }
        guard grantpt(master) == 0, unlockpt(master) == 0, let name = ptsname(master) else {
            let reason = String(cString: strerror(errno))
            close(master)
            throw SpawnError(description: "pty setup: \(reason)")
        }
        let slavePath = String(cString: name)

        // Hold the slave open across the spawn: it carries the size and
        // termios we set here, and keeps the master from reading EOF before
        // the child has opened its own copy.
        let slave = open(slavePath, O_RDWR | O_NOCTTY)
        guard slave >= 0 else {
            let reason = String(cString: strerror(errno))
            close(master)
            throw SpawnError(description: "open \(slavePath): \(reason)")
        }
        defer { close(slave) }
        var size = winsize(ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0)
        _ = ioctl(slave, TIOCSWINSZ, &size)
        var term = termios()
        if tcgetattr(slave, &term) == 0 {
            term.c_iflag |= tcflag_t(IUTF8)
            _ = tcsetattr(slave, TCSANOW, &term)
        }

        var actions: posix_spawn_file_actions_t?
        posix_spawn_file_actions_init(&actions)
        defer { posix_spawn_file_actions_destroy(&actions) }
        posix_spawn_file_actions_addopen(&actions, 0, slavePath, O_RDWR, 0)
        posix_spawn_file_actions_adddup2(&actions, 0, 1)
        posix_spawn_file_actions_adddup2(&actions, 0, 2)

        var attr: posix_spawnattr_t?
        posix_spawnattr_init(&attr)
        defer { posix_spawnattr_destroy(&attr) }
        // The hub ignores SIGPIPE and blocks nothing; the child gets a clean
        // slate either way, and inherits no descriptor but its terminal.
        var allSignals = sigset_t()
        sigfillset(&allSignals)
        var noSignals = sigset_t()
        sigemptyset(&noSignals)
        posix_spawnattr_setsigdefault(&attr, &allSignals)
        posix_spawnattr_setsigmask(&attr, &noSignals)
        posix_spawnattr_setflags(&attr, Int16(
            POSIX_SPAWN_SETSID | POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK))

        guard let supervisor = HubPaths.selfCommand else {
            close(master)
            throw SpawnError(description: "cannot locate own executable")
        }
        let trampoline = ["/bin/sh", "-c", "cd \"$1\" && shift && exec \"$@\"", "sh", cwd]
            + supervisor + ["hub", "supervise"] + argv
        var environment = env
        environment["PWD"] = cwd
        var cArgs: [UnsafeMutablePointer<CChar>?] = trampoline.map { strdup($0) } + [nil]
        var cEnv: [UnsafeMutablePointer<CChar>?] = environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
        defer {
            for pointer in cArgs { free(pointer) }
            for pointer in cEnv { free(pointer) }
        }

        var pid: pid_t = 0
        let rc = posix_spawn(&pid, "/bin/sh", &actions, &attr, &cArgs, &cEnv)
        guard rc == 0 else {
            close(master)
            throw SpawnError(description: "posix_spawn: \(String(cString: strerror(rc)))")
        }
        _ = fcntl(master, F_SETFL, fcntl(master, F_GETFL) | O_NONBLOCK)
        _ = fcntl(master, F_SETFD, FD_CLOEXEC)
        return Spawned(pid: pid, master: master)
    }

    /// The process group currently in the foreground of the pty — the
    /// program, as opposed to the supervisor that leads the session.
    static func foregroundGroup(master: Int32) -> pid_t? {
        var group: pid_t = 0
        return ioctl(master, TIOCGPGRP, &group) == 0 && group > 1 ? group : nil
    }

    static func resize(master: Int32, rows: UInt16, cols: UInt16) {
        var size = winsize(ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0)
        _ = ioctl(master, TIOCSWINSZ, &size)
    }
}

/// The supervisor's job, for its signal handlers (which can capture nothing).
private var hubSupervisedJob: pid_t = 0

/// The leader of a session's pty: runs the program as a foreground job and
/// does the one piece of a shell's work the program can't do without —
/// resuming it when it suspends.
///
/// A session leader with no parent in its own session is an *orphaned
/// process group*, and the kernel discards SIGTSTP sent to one. Claude Code
/// suspends on Ctrl+Z by putting the terminal back to cooked mode, printing
/// "run `fg`", and stopping itself; as a bare leader it would do the first
/// two, fail silently at the third, and sit in that half-suspended state
/// for good, since the SIGCONT that would restore its screen never comes.
/// With this process as its parent the stop is real, and is answered at
/// once with SIGCONT: Ctrl+Z becomes a redraw instead of a dead session.
enum HubSupervisor {
    static func run(_ argv: [String]) -> Never {
        guard !argv.isEmpty else { exit(2) }
        // We hand the terminal to the job and take it back, from the
        // background; none of that may stop us.
        signal(SIGTTOU, SIG_IGN)
        signal(SIGTTIN, SIG_IGN)
        signal(SIGTSTP, SIG_IGN)
        // Held until the handlers below exist, so a hang-up that lands in
        // the microseconds around the spawn is forwarded rather than fatal.
        var held = sigset_t()
        sigemptyset(&held)
        for sig in [SIGHUP, SIGTERM, SIGINT, SIGQUIT] { sigaddset(&held, sig) }
        sigprocmask(SIG_BLOCK, &held, nil)

        var attr: posix_spawnattr_t?
        posix_spawnattr_init(&attr)
        var allSignals = sigset_t()
        sigfillset(&allSignals)
        var noSignals = sigset_t()
        sigemptyset(&noSignals)
        posix_spawnattr_setsigdefault(&attr, &allSignals)
        posix_spawnattr_setsigmask(&attr, &noSignals)
        posix_spawnattr_setpgroup(&attr, 0)
        posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_SETPGROUP | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK))

        var cArgs: [UnsafeMutablePointer<CChar>?] = argv.map { strdup($0) } + [nil]
        var cEnv: [UnsafeMutablePointer<CChar>?] =
            ProcessInfo.processInfo.environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
        var pid: pid_t = 0
        let rc = posix_spawnp(&pid, argv[0], nil, &attr, &cArgs, &cEnv)
        guard rc == 0 else {
            FileHandle.standardError.write(Data(
                "\(HubCLI.commandName): cannot run \(argv[0]): \(String(cString: strerror(rc)))\r\n".utf8))
            exit(127)
        }
        _ = tcsetpgrp(STDIN_FILENO, pid)

        // Asked to end — by the hub, or by the kernel when the hub is gone
        // and the pty with it (which hangs up only the session leader) —
        // pass it on and keep waiting. Dying here instead would revoke the
        // terminal under the program mid-cleanup, report our death as its
        // exit status, and leave a program that shrugs off SIGHUP running
        // where the hub can no longer find it.
        hubSupervisedJob = pid
        for sig in [SIGHUP, SIGINT, SIGQUIT] {
            signal(sig) { number in kill(-hubSupervisedJob, number) }
        }
        // SIGTERM is the hub's escalation: the program ignored the hang-up.
        signal(SIGTERM) { _ in kill(-hubSupervisedJob, SIGKILL) }
        sigprocmask(SIG_UNBLOCK, &held, nil)

        while true {
            var status: Int32 = 0
            let waited = waitpid(pid, &status, WUNTRACED)
            if waited < 0 {
                if errno == EINTR { continue }
                exit(1)
            }
            if status & 0x7f == 0x7f {
                // Stopped: put it back in the foreground and carry on.
                _ = tcsetpgrp(STDIN_FILENO, pid)
                kill(-pid, SIGCONT)
                continue
            }
            exit(Hub.exitCode(fromWaitStatus: status))
        }
    }
}
