import AppKit
import Foundation

/// Hand-rolled test harness — this machine has no XCTest/swift-testing (CLT
/// toolchain only), and we don't want the dependency anyway. Dispatched from
/// App.main via `ClaudeShip --self-test`, before NSApplication exists, so
/// it runs headless and exits with 0/1. Covers the pure logic: registry
/// parsing, status mapping, cwd→project-dir flattening, transcript tail
/// parsing, menubar label, and duration formatting.
enum SelfTest {
    struct Runner {
        var passed = 0
        var failures: [String] = []

        mutating func expectEqual<T: Equatable>(_ got: T, _ want: T, _ name: String) {
            if got == want {
                passed += 1
            } else {
                failures.append("\(name): got \(got), want \(want)")
            }
        }

        mutating func expectNil<T>(_ got: T?, _ name: String) {
            if got == nil {
                passed += 1
            } else {
                failures.append("\(name): got \(String(describing: got)), want nil")
            }
        }

        func finish() -> Never {
            for f in failures { print("FAIL  \(f)") }
            print("\(passed) passed, \(failures.count) failed")
            exit(failures.isEmpty ? 0 : 1)
        }
    }

    static func run() -> Never {
        var t = Runner()

        // MARK: registry parsing

        let registryJSON = """
        {"pid":43606,"sessionId":"93fb531a-9e91-4926-ab89-93ded70cba7e",\
        "cwd":"/Users/x/code/claude-status",\
        "startedAt":1783455271121,"version":"2.1.202","kind":"interactive",\
        "name":"claude-status-b3","status":"busy",\
        "updatedAt":1783457374373,"statusUpdatedAt":1783457374373}
        """
        let entry = SessionScanner.parseRegistryEntry(Data(registryJSON.utf8))
        t.expectEqual(entry?.pid, 43606, "registry: pid")
        t.expectEqual(entry?.cwd, "/Users/x/code/claude-status", "registry: cwd")
        t.expectEqual(entry?.sessionId, "93fb531a-9e91-4926-ab89-93ded70cba7e", "registry: sessionId")
        t.expectEqual(entry?.name, "claude-status-b3", "registry: name")
        t.expectEqual(entry?.status, "busy", "registry: status")
        t.expectEqual(entry?.startedAt, Date(timeIntervalSince1970: 1783455271.121), "registry: startedAt epoch-ms")
        t.expectEqual(entry?.statusUpdatedAt, Date(timeIntervalSince1970: 1783457374.373), "registry: statusUpdatedAt")
        t.expectNil(entry?.waitingFor, "registry: waitingFor absent")
        t.expectEqual(entry?.kind, "interactive", "registry: kind")
        t.expectNil(entry?.jobId, "registry: jobId absent on interactive")

        let bgEntry = SessionScanner.parseRegistryEntry(Data(#"{"pid":2,"kind":"bg","jobId":"a34398c4","sessionId":"a34398c4-12a2-4b54-aaac-edc6a5e935a6","status":"shell"}"#.utf8))
        t.expectEqual(bgEntry?.kind, "bg", "registry: kind bg")
        t.expectEqual(bgEntry?.jobId, "a34398c4", "registry: jobId")
        t.expectEqual(SessionScanner.isBackground(kind: "bg"), true, "kind bg → background")
        t.expectEqual(SessionScanner.isBackground(kind: "interactive"), false, "kind interactive → terminal")
        t.expectEqual(SessionScanner.isBackground(kind: nil), false, "missing kind → terminal (older CLI)")

        let withWaiting = SessionScanner.parseRegistryEntry(
            Data(#"{"pid":1,"status":"waiting","waitingFor":"permission"}"#.utf8))
        t.expectEqual(withWaiting?.waitingFor, "permission", "registry: waitingFor present")

        t.expectNil(SessionScanner.parseRegistryEntry(Data(#"{"nopid":true}"#.utf8)), "registry: missing pid → nil")
        t.expectNil(SessionScanner.parseRegistryEntry(Data("garbage".utf8)), "registry: garbage → nil")

        // MARK: status mapping — the CLI's enum is ["busy","shell","idle","waiting"]

        t.expectEqual(SessionScanner.state(fromStatus: "busy"), .busy, "status busy → busy")
        t.expectEqual(SessionScanner.state(fromStatus: "shell"), .shell, "status shell → shell")
        t.expectEqual(SessionScanner.state(fromStatus: "idle"), .idle, "status idle → idle")
        t.expectEqual(SessionScanner.state(fromStatus: "waiting"), .waitingForInput, "status waiting → waiting")
        t.expectEqual(SessionScanner.state(fromStatus: "someday-new"), .idle, "unknown status → idle, never a false alarm color")
        t.expectEqual(SessionScanner.state(fromStatus: nil), .idle, "missing status → idle")

        // MARK: projectDirName

        t.expectEqual(
            SessionScanner.projectDirName(forCwd: "/Users/x/code/claude-status"),
            "-Users-x-code-claude-status",
            "projectDirName flattens slashes"
        )
        t.expectEqual(
            SessionScanner.projectDirName(forCwd: "/Users/a_b/foo.bar"),
            "-Users-a-b-foo-bar",
            "projectDirName flattens underscores and dots"
        )

        // MARK: parseTail

        let twoLines = """
        {"type":"user","sessionId":"abc-123","gitBranch":"main","message":{"role":"user"}}
        {"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn"}}
        """
        let parsed = SessionScanner.parseTail(twoLines)
        t.expectEqual(parsed?.sessionId, "abc-123", "parseTail finds newest entry with sessionId")
        t.expectEqual(parsed?.gitBranch, "main", "parseTail reads gitBranch")

        let truncated = """
        ...half a json object"}
        {"type":"assistant","sessionId":"s1","gitBranch":"feature/x","message":{}}
        """
        t.expectEqual(SessionScanner.parseTail(truncated)?.gitBranch, "feature/x",
                      "parseTail skips truncated first line")

        // The newest entries often carry a sessionId but no gitBranch
        // (progress records, tool results) — the branch must come from the
        // newest entry that HAS one, not vanish.
        let branchlessNewest = """
        {"type":"user","sessionId":"abc-123","gitBranch":"feature/y","message":{}}
        {"type":"progress","sessionId":"abc-123"}
        {"type":"assistant","sessionId":"abc-123","message":{"stop_reason":"tool_use"}}
        """
        let tail2 = SessionScanner.parseTail(branchlessNewest)
        t.expectEqual(tail2?.sessionId, "abc-123", "parseTail: sessionId from newest entry")
        t.expectEqual(tail2?.gitBranch, "feature/y", "parseTail: branch survives branch-less newer entries")

        t.expectEqual(SessionScanner.parseTail(#"{"sessionId":"s","gitBranch":""}"#)?.gitBranch, nil,
                      "parseTail: empty-string branch ignored")

        t.expectNil(SessionScanner.parseTail(""), "parseTail empty → nil")
        t.expectNil(SessionScanner.parseTail("not json at all"), "parseTail garbage → nil")

        // MARK: pid liveness

        t.expectEqual(SessionScanner.pidAlive(getpid()), true, "own pid is alive")

        // MARK: menubar symbol

        t.expectEqual(SessionStore.trayState(busy: 0, waiting: 0), .idle, "tray: nothing running → idle")
        t.expectEqual(SessionStore.trayState(busy: 2, waiting: 0), .busy, "tray: busy sessions → busy")
        t.expectEqual(SessionStore.trayState(busy: 2, waiting: 1), .waiting, "tray: any waiting beats busy")

        t.expectEqual(SessionStore.trayColor(.idle), .labelColor, "tray color: idle → system label color")
        t.expectEqual(SessionStore.trayColor(.busy), .systemGreen, "tray color: busy → green")
        t.expectEqual(SessionStore.trayColor(.waiting), .systemOrange, "tray color: waiting → orange")

        // MARK: auto-approve rules

        let now2 = Date(timeIntervalSince1970: 2_000_000)
        t.expectEqual(SessionStore.ruleAllows(nil, now: now2), false, "rules: no rule → no auto-approve")
        t.expectEqual(SessionStore.ruleAllows(.forSession, now: now2), true, "rules: forSession always allows")
        t.expectEqual(SessionStore.ruleAllows(.until(now2.addingTimeInterval(60)), now: now2), true, "rules: unexpired timer allows")
        t.expectEqual(SessionStore.ruleAllows(.until(now2.addingTimeInterval(-1)), now: now2), false, "rules: expired timer denies")

        // MARK: terminal focus

        let titledTail = """
        {"type":"user","sessionId":"s1","gitBranch":"main","cwd":"/x"}
        {"type":"ai-title","aiTitle":"Old title","sessionId":"s1"}
        {"type":"ai-title","aiTitle":"Swift builds failing","sessionId":"s1"}
        {"type":"progress","sessionId":"s1"}
        """
        let titled = SessionScanner.parseTail(titledTail)
        t.expectEqual(titled?.aiTitle, "Swift builds failing", "tail: newest ai-title wins")
        t.expectEqual(titled?.gitBranch, "main", "tail: branch still found past ai-title entries")
        t.expectNil(SessionScanner.parseTail(#"{"type":"ai-title","aiTitle":"","sessionId":"s1"}"#)?.aiTitle, "tail: empty ai-title ignored")
        t.expectNil(SessionScanner.parseTail(#"{"type":"user","aiTitle":"not a title entry","sessionId":"s1"}"#)?.aiTitle, "tail: aiTitle only read off ai-title entries")

        t.expectEqual(TerminalFocus.adapter(forBundleId: "com.mitchellh.ghostty"), .ghostty, "focus: Ghostty adapter")
        t.expectEqual(TerminalFocus.adapter(forBundleId: "com.apple.Terminal"), .terminalApp, "focus: Terminal adapter")
        t.expectEqual(TerminalFocus.adapter(forBundleId: "com.googlecode.iterm2"), .iterm, "focus: iTerm adapter")
        t.expectEqual(TerminalFocus.adapter(forBundleId: "org.alacritty"), .generic, "focus: unknown app → generic")
        t.expectEqual(TerminalFocus.adapter(forBundleId: nil), .generic, "focus: nil bundle → generic")

        let surfaces = [
            TerminalFocus.GhosttyTerminal(id: "A", name: "◐ Fix the build", cwd: "/repo/one"),
            TerminalFocus.GhosttyTerminal(id: "B", name: "◑ Fix the build", cwd: "/repo/two"),
            TerminalFocus.GhosttyTerminal(id: "C", name: "✳ Claude Code", cwd: "/repo/two"),
            TerminalFocus.GhosttyTerminal(id: "D", name: "cargo run", cwd: "/repo/three/"),
        ]
        t.expectEqual(TerminalFocus.pickGhosttyTerminal(surfaces, cwd: "/repo/two", title: "Fix the build")?.id, "B", "pick: cwd + title beats title alone")
        t.expectEqual(TerminalFocus.pickGhosttyTerminal(surfaces, cwd: "/elsewhere", title: "Fix the build")?.id, "A", "pick: title alone when cwd differs")
        t.expectEqual(TerminalFocus.pickGhosttyTerminal(surfaces, cwd: "/repo/two", title: nil)?.id, "B", "pick: cwd alone, first in order")
        t.expectEqual(TerminalFocus.pickGhosttyTerminal(surfaces, cwd: "/repo/three", title: "")?.id, "D", "pick: trailing slash tolerated, empty title ignored")
        t.expectNil(TerminalFocus.pickGhosttyTerminal(surfaces, cwd: "/nope", title: "Nothing"), "pick: no match → nil (fall back to app)")

        func strList(_ xs: [String]) -> NSAppleEventDescriptor {
            let l = NSAppleEventDescriptor.list()
            for x in xs { l.insert(NSAppleEventDescriptor(string: x), at: 0) }
            return l
        }
        let reply = NSAppleEventDescriptor.list()
        reply.insert(strList(["id1", "id2"]), at: 0)
        reply.insert(strList(["◐ One", "Two"]), at: 0)
        reply.insert(strList(["/a", "/b"]), at: 0)
        t.expectEqual(TerminalFocus.parseGhosttyList(reply), [
            TerminalFocus.GhosttyTerminal(id: "id1", name: "◐ One", cwd: "/a"),
            TerminalFocus.GhosttyTerminal(id: "id2", name: "Two", cwd: "/b"),
        ], "ghostty: parallel lists zip into terminals")
        t.expectNil(TerminalFocus.parseGhosttyList(NSAppleEventDescriptor(string: "oops")), "ghostty: non-list reply → nil")
        t.expectEqual(TerminalFocus.parseGhosttyList(reply)?.count, 2, "ghostty: count")

        t.expectEqual(TerminalFocus.appleScriptLiteral(#"say "hi" \ bye"#), #"say \"hi\" \\ bye"#, "script: literal escaping")
        t.expectEqual(TerminalFocus.ghosttyFocusScript(terminalId: "X\"Y").contains(#"terminal id "X\"Y""#), true, "script: id embedded escaped")
        t.expectEqual(TerminalFocus.terminalAppScript(tty: "/dev/ttys004").contains(#"tty of t is "/dev/ttys004""#), true, "script: Terminal tty embedded")
        t.expectEqual(TerminalFocus.itermScript(tty: "/dev/ttys004").contains(#"tty of s is "/dev/ttys004""#), true, "script: iTerm tty embedded")

        t.expectEqual(TerminalFocus.adapter(forBundleId: "com.microsoft.VSCode"), .vscode, "focus: VS Code adapter")
        t.expectEqual(TerminalFocus.adapter(forBundleId: "com.todesktop.230313mzl4w4u92"), .vscode, "focus: Cursor adapter")
        t.expectEqual(TerminalFocus.cliName(forBundleId: "com.microsoft.VSCode"), "code", "vscode: cli name")
        t.expectEqual(TerminalFocus.cliName(forBundleId: "com.microsoft.VSCodeInsiders"), "code-insiders", "vscode: insiders cli name")
        t.expectEqual(TerminalFocus.cliName(forBundleId: "com.todesktop.230313mzl4w4u92"), "cursor", "vscode: cursor cli name")

        let reqData = TerminalFocus.vscodeRequest(nonce: "n-1", pids: [10, 20], cwd: "/w", sessionId: "s-1",
                                                  now: Date(timeIntervalSince1970: 1_700_000_000.5))
        let req = (try? JSONSerialization.jsonObject(with: reqData)) as? [String: Any]
        t.expectEqual(req?["nonce"] as? String, "n-1", "vscode request: nonce")
        t.expectEqual(req?["pids"] as? [Int], [10, 20], "vscode request: pids")
        t.expectEqual(req?["ts"] as? Int, 1_700_000_000_500, "vscode request: epoch ms")
        t.expectEqual(req?["cwd"] as? String, "/w", "vscode request: cwd")
        t.expectEqual(req?["sessionId"] as? String, "s-1", "vscode request: sessionId")

        let okReply = Data(#"{"nonce":"n-1","workspace":"/w/proj.code-workspace","terminal":"zsh","ts":1}"#.utf8)
        t.expectEqual(TerminalFocus.parseVSCodeReply(okReply, nonce: "n-1"),
                      TerminalFocus.VSCodeReply(nonce: "n-1", workspace: "/w/proj.code-workspace", terminal: "zsh"), "vscode reply: parsed")
        t.expectNil(TerminalFocus.parseVSCodeReply(okReply, nonce: "other"), "vscode reply: nonce mismatch → nil")
        t.expectEqual(TerminalFocus.parseVSCodeReply(Data(#"{"nonce":"n-1","workspace":null}"#.utf8), nonce: "n-1")?.workspace, nil, "vscode reply: untitled window → nil workspace")
        t.expectEqual(TerminalFocus.parseVSCodeReply(Data(#"{"nonce":"n-1","workspace":""}"#.utf8), nonce: "n-1")?.workspace, nil, "vscode reply: empty workspace → nil")
        t.expectNil(TerminalFocus.parseVSCodeReply(Data("garbage".utf8), nonce: "n-1"), "vscode reply: malformed → nil")

        let winJSON = Data(#"{"ts":1700000000000,"name":"claude-status","workspace":"/w/claude-status","terminals":[{"pid":501,"name":"zsh"},{"pid":502,"name":"✳ Claude Code"},{"name":"starting"}]}"#.utf8)
        let win = TerminalFocus.parseVSCodeWindowState(winJSON)
        t.expectEqual(win?.terminalPids, [501, 502], "window state: pids (pid-less terminal skipped)")
        t.expectEqual(win?.label, "claude-status", "window state: label from workspace name")
        t.expectEqual(win?.ts, Date(timeIntervalSince1970: 1_700_000_000), "window state: epoch ms")
        t.expectEqual(TerminalFocus.parseVSCodeWindowState(Data(#"{"ts":1,"workspace":"/w/proj","terminals":[]}"#.utf8))?.label, "proj", "window state: label falls back to folder name")
        t.expectEqual(TerminalFocus.parseVSCodeWindowState(Data(#"{"ts":1,"terminals":[]}"#.utf8))?.label, "untitled window", "window state: untitled")
        t.expectNil(TerminalFocus.parseVSCodeWindowState(Data(#"{"terminals":[]}"#.utf8)), "window state: no ts → nil")
        let winA = TerminalFocus.VSCodeWindowState(name: "A", workspace: nil, terminalPids: [11, 12], ts: Date())
        let winB = TerminalFocus.VSCodeWindowState(name: "B", workspace: nil, terminalPids: [21], ts: Date())
        t.expectEqual(TerminalFocus.vscodeWindow(owning: [900, 21, 1], in: [winA, winB])?.name, "B", "window lookup: any pid in the chain")
        t.expectNil(TerminalFocus.vscodeWindow(owning: [7], in: [winA, winB]), "window lookup: none → nil")
        let winDir = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("cs-windows-\(getpid())", isDirectory: true)
        try? FileManager.default.removeItem(at: winDir)
        try? FileManager.default.createDirectory(at: winDir, withIntermediateDirectories: true)
        let fresh = Int(Date().timeIntervalSince1970 * 1000)
        try? Data(#"{"ts":\#(fresh),"name":"fresh","terminals":[{"pid":1}]}"#.utf8).write(to: winDir.appendingPathComponent("a.json"))
        try? Data(#"{"ts":\#(fresh - 600_000),"name":"stale","terminals":[{"pid":2}]}"#.utf8).write(to: winDir.appendingPathComponent("b.json"))
        try? Data("junk".utf8).write(to: winDir.appendingPathComponent("c.json"))
        t.expectEqual(TerminalFocus.loadVSCodeWindows(dir: winDir).map(\.name), ["fresh"], "window load: stale and junk files dropped")
        t.expectEqual(TerminalFocus.loadVSCodeWindows(dir: winDir.appendingPathComponent("missing")).count, 0, "window load: missing dir → empty")
        try? FileManager.default.removeItem(at: winDir)
        t.expectEqual(TerminalFocus.displayName(forBundleId: "com.microsoft.VSCode", fallback: "Code"), "VS Code", "host name: VS Code")
        t.expectEqual(TerminalFocus.displayName(forBundleId: "com.mitchellh.ghostty", fallback: "Ghostty"), "Ghostty", "host name: fallback")

        func fakeSession(_ pid: pid_t, cwd: String, host: SessionHost?, background: Bool = false,
                         jobId: String? = nil, sessionId: String? = nil) -> ClaudeSession {
            ClaudeSession(pid: pid, cwd: cwd, name: nil, sessionId: sessionId, gitBranch: nil, title: nil, host: host,
                          isBackground: background, jobId: jobId,
                          state: .idle, waitingFor: nil, lastActivity: nil, stateSince: nil, startedAt: nil)
        }
        let vsA = SessionHost(bundleId: "com.microsoft.VSCode", appName: "VS Code", window: "alpha")
        let vsB = SessionHost(bundleId: "com.microsoft.VSCode", appName: "VS Code", window: "beta")
        let ghostty = SessionHost(bundleId: "com.mitchellh.ghostty", appName: "Ghostty", window: nil)
        t.expectEqual(vsA.label, "VS Code · alpha", "host: window label")
        t.expectEqual(ghostty.label, "Ghostty", "host: app-only label")
        t.expectEqual(vsA.groupKey == vsB.groupKey, false, "host: windows group separately")
        let grouped = SessionsView.grouped([
            fakeSession(1, cwd: "/a", host: vsB),
            fakeSession(2, cwd: "/b", host: ghostty),
            fakeSession(3, cwd: "/c", host: nil),
            fakeSession(4, cwd: "/d", host: vsA),
            fakeSession(5, cwd: "/a", host: vsB),
            fakeSession(6, cwd: "/f", host: SessionHost(bundleId: "com.microsoft.VSCode", appName: "VS Code", window: nil)),
        ])
        t.expectEqual(grouped.map(\.label), ["Ghostty", "VS Code", "Other"], "grouping: hosts by label, Other last")
        t.expectEqual(grouped[1].windows.map(\.label), ["alpha", "beta", nil], "grouping: windows by label, unreported last")
        t.expectEqual(grouped[1].windows[1].sessions.map(\.pid), [1, 5], "grouping: rows keep scan order inside a window")
        t.expectEqual(grouped[1].windows[1].sharedPath, "/a", "grouping: shared cwd → path on the window")
        t.expectEqual(grouped[1].windows[0].sharedPath, "/d", "grouping: single row → its cwd is the window's")
        t.expectNil(grouped[0].windows.first?.label, "grouping: Ghostty has no window level")
        t.expectNil(SessionsView.grouped([fakeSession(1, cwd: "/a", host: vsB), fakeSession(2, cwd: "/z", host: vsB)])[0].windows[0].sharedPath, "grouping: mixed cwds in a window → no shared path")
        t.expectEqual(SessionsView.grouped([]).count, 0, "grouping: empty")

        // MARK: background sessions

        let bgGrouped = SessionsView.grouped([
            fakeSession(1, cwd: "/c", host: nil),
            fakeSession(2, cwd: "/b", host: nil, background: true, jobId: "aaaa1111"),
            fakeSession(3, cwd: "/a", host: ghostty),
            fakeSession(4, cwd: "/d", host: nil, background: true, jobId: "bbbb2222"),
        ])
        t.expectEqual(bgGrouped.map(\.label), ["Ghostty", "Background", "Other"], "grouping: Background between hosts and Other")
        t.expectEqual(bgGrouped[1].key, SessionsView.backgroundKey, "grouping: background key")
        t.expectEqual(bgGrouped[1].windows.count, 1, "grouping: background has no window level")
        t.expectEqual(bgGrouped[1].windows[0].sessions.map(\.pid), [2, 4], "grouping: background rows keep scan order")
        t.expectNil(bgGrouped[1].windows[0].label, "grouping: background rows are each their own window (divider between)")
        t.expectEqual(SessionsView.grouped([fakeSession(9, cwd: "/z", host: nil, background: true)]).map(\.label), ["Background"], "grouping: lone background group")
        var hubSession = fakeSession(10, cwd: "/h", host: nil)
        hubSession.hubId = "ab12cd"
        let withHub = SessionsView.grouped([fakeSession(1, cwd: "/a", host: nil), hubSession,
                                            fakeSession(9, cwd: "/z", host: nil, background: true), fakeSession(2, cwd: "/b", host: ghostty)])
        t.expectEqual(withHub.map(\.label), ["Ghostty", "Virtual", "Background", "Other"], "grouping: Virtual between hosts and Background")
        t.expectEqual(withHub[1].key, SessionsView.virtualKey, "grouping: virtual key")
        t.expectEqual(TerminalFocus.hubAttachCommand(cwd: "/Users/x/my proj", hubId: "ab12cd"),
                      "cd '/Users/x/my proj' && claudeship hub attach 'ab12cd'", "hub attach: command typed into the new shell")

        t.expectEqual(fakeSession(1, cwd: "/", host: nil, background: true, jobId: "j").attachId, "j", "attachId: jobId wins")
        t.expectEqual(fakeSession(1, cwd: "/", host: nil, background: true, sessionId: "a34398c4-12a2-4b54-aaac-edc6a5e935a6").attachId, "a34398c4", "attachId: falls back to sessionId prefix")
        t.expectNil(fakeSession(1, cwd: "/", host: nil, background: true).attachId, "attachId: nothing to attach with")

        t.expectEqual(TerminalFocus.attachTerminalBundleId(running: ["com.apple.Terminal", "com.mitchellh.ghostty"]), "com.mitchellh.ghostty", "attach: Ghostty preferred when running")
        t.expectEqual(TerminalFocus.attachTerminalBundleId(running: ["com.googlecode.iterm2"]), "com.googlecode.iterm2", "attach: iTerm2 when running")
        t.expectEqual(TerminalFocus.attachTerminalBundleId(running: []), "com.apple.Terminal", "attach: Terminal.app when nothing runs")
        t.expectEqual(TerminalFocus.shellSingleQuoted("it's"), #"'it'\''s'"#, "shell quote: embedded apostrophe")
        t.expectEqual(TerminalFocus.newSessionCommand(cwd: "/Users/a b"),
                      "cd '/Users/a b' && claudeship --permission-mode auto", "new session: home, auto mode, through the hub")
        t.expectEqual(TerminalFocus.attachCommand(cwd: "/Users/a b/code", attachId: "a34398c4"),
                      "cd '/Users/a b/code' && claude attach 'a34398c4'", "attach: command line")
        t.expectEqual(TerminalFocus.ghosttyAttachScript(cwd: "/x", command: "cd '/x' && claude attach 'id'").contains(#"set initial input of cfg to "cd '/x' && claude attach 'id'" & linefeed"#), true, "attach: Ghostty script types the command")
        t.expectEqual(TerminalFocus.terminalAppAttachScript(command: #"say "hi""#).contains(#"do script "say \"hi\"""#), true, "attach: Terminal.app script escapes quotes")

        let chain = TerminalFocus.ancestorPids(of: getpid())
        t.expectEqual(chain.first, getpid(), "ancestors: starts with the pid itself")
        t.expectEqual(chain.contains(getppid()), true, "ancestors: includes the parent")
        t.expectEqual(chain.contains(1), false, "ancestors: stops below launchd")

        // Bridge round-trip against a fake extension: a thread watching the
        // request file that answers with the request's nonce.
        let bridgeDir = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("cs-bridge-\(getpid())", isDirectory: true)
        try? FileManager.default.removeItem(at: bridgeDir)
        t.expectNil(TerminalFocus.bridgeVSCode(pids: [1], cwd: "/", sessionId: nil, dir: bridgeDir, deadline: 0.2), "bridge: no extension → nil after deadline")
        Thread.detachNewThread {
            let request = bridgeDir.appendingPathComponent("request.json")
            for _ in 0..<100 {
                if let data = try? Data(contentsOf: request),
                   let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
                   let nonce = obj["nonce"] as? String, (obj["pids"] as? [Int]) == [42, 43] {
                    let reply = #"{"nonce":"\#(nonce)","workspace":"/fake/ws","terminal":"zsh"}"#
                    try? Data(reply.utf8).write(to: bridgeDir.appendingPathComponent("reply.json"))
                    return
                }
                usleep(10_000)
            }
        }
        let bridged = TerminalFocus.bridgeVSCode(pids: [42, 43], cwd: "/x", sessionId: "s", dir: bridgeDir, deadline: 2)
        t.expectEqual(bridged?.workspace, "/fake/ws", "bridge: reply from the owning window")
        t.expectEqual(bridged?.terminal, "zsh", "bridge: terminal name carried")
        try? FileManager.default.removeItem(at: bridgeDir)

        t.expectEqual(TerminalFocus.chooseHostIndex(policies: [.regular]), 0, "host: lone regular app")
        t.expectEqual(TerminalFocus.chooseHostIndex(policies: [.accessory, .regular]), 1, "host: skip helper, take the app above it")
        t.expectEqual(TerminalFocus.chooseHostIndex(policies: [.prohibited, .accessory, .regular, .regular]), 2, "host: nearest regular, not the outermost")
        t.expectEqual(TerminalFocus.chooseHostIndex(policies: [.accessory, .prohibited]), 0, "host: no regular app → nearest of any kind")
        t.expectNil(TerminalFocus.chooseHostIndex(policies: []), "host: no apps in the chain → nil")

        t.expectEqual(TerminalFocus.parentPid(of: getpid()), getppid(), "proc: parent pid via sysctl")
        t.expectNil(TerminalFocus.parentPid(of: 999_999), "proc: dead pid → nil")
        if let tty = TerminalFocus.ttyPath(of: getpid()) {
            t.expectEqual(tty.hasPrefix("/dev/tty"), true, "proc: tty path shape")
        } else {
            t.expectEqual(true, true, "proc: no controlling tty (headless run)")
        }

        // MARK: formatting

        let now = Date(timeIntervalSince1970: 1_000_000)
        t.expectEqual(StatusFormat.agoString(since: now.addingTimeInterval(-2), now: now), "just now", "ago: <5s")
        t.expectEqual(StatusFormat.agoString(since: now.addingTimeInterval(-30), now: now), "30s ago", "ago: seconds")
        t.expectEqual(StatusFormat.agoString(since: now.addingTimeInterval(-150), now: now), "2m ago", "ago: minutes")
        t.expectEqual(StatusFormat.compactAge(since: now.addingTimeInterval(-12), now: now), "12s", "age: seconds")
        t.expectEqual(StatusFormat.compactAge(since: now.addingTimeInterval(-150), now: now), "2m", "age: minutes")
        t.expectEqual(StatusFormat.compactAge(since: now.addingTimeInterval(-7500), now: now), "2h 5m", "age: hours")
        let start = Date(timeIntervalSince1970: 0)
        t.expectEqual(StatusFormat.compactDuration(from: start, to: start.addingTimeInterval(59)), "0m", "duration: sub-minute")
        t.expectEqual(StatusFormat.compactDuration(from: start, to: start.addingTimeInterval(9240)), "2h 34m", "duration: hours")

        // MARK: hub client — /api/state

        let stateJSON = #"""
        {"protocol":3,"approvalsSupported":true,"projects":[{"name":"app","sessions":[
          {"pid":101,"hubId":"ab12cd","status":"waiting","approvals":[
            {"id":"ap-1","tool":"Bash","summary":"Bash: ls","detail":"Bash: ls\n-la","receivedAt":1700000000000}],
           "autoApprove":{"until":1700000300000}},
          {"pid":102,"status":"idle","autoApprove":null}]}],
         "elsewhere":[{"pid":103,"hubId":"ef34gh","sessionId":"s-3","autoApprove":{"session":true}},{"cwd":"/no-pid"}]}
        """#
        if let hubState = HubClient.parseState(Data(stateJSON.utf8)) {
            t.expectEqual(hubState.sessions.map(\.pid), [101, 102, 103], "hub state: sessions from projects and elsewhere, pid-less skipped")
            t.expectEqual(hubState.sessions.map(\.hubId), ["ab12cd", nil, "ef34gh"], "hub state: hub ids")
            t.expectEqual(hubState.approvalsUsable, true, "hub state: protocol 3 with approvals is usable")
            t.expectEqual(hubState.sessions[0].approvals, [HubClient.Approval(
                id: "ap-1", tool: "Bash", summary: "Bash: ls", detail: "Bash: ls\n-la",
                receivedAt: Date(timeIntervalSince1970: 1_700_000_000))], "hub state: approval decoded")
            t.expectEqual(hubState.sessions[0].autoApprove, .until(Date(timeIntervalSince1970: 1_700_000_300)), "hub state: timed rule")
            t.expectNil(hubState.sessions[1].autoApprove, "hub state: null rule")
            t.expectEqual(hubState.sessions[2].autoApprove, .forSession, "hub state: session rule")
            t.expectEqual(hubState.sessions[2].sessionId, "s-3", "hub state: session id when present")
        } else {
            t.expectEqual(false, true, "hub state: parses")
        }
        t.expectEqual(HubClient.parseState(Data(#"{"protocol":2,"projects":[],"elsewhere":[]}"#.utf8))?.approvalsUsable, false,
                      "hub state: an older hub gets no approval buttons")
        t.expectEqual(HubClient.parseState(Data(#"{"protocol":4,"approvalsSupported":true}"#.utf8))?.approvalsUsable, false,
                      "hub state: a newer protocol gets none either")
        t.expectNil(HubClient.parseState(Data("<html>".utf8)), "hub state: not JSON → nil")
        t.expectEqual(HubClient.port(configData: Data(#"{"port":9000}"#.utf8)), 9000, "hub config: port")
        t.expectEqual(HubClient.port(configData: Data(#"{"port":99999}"#.utf8)), 7433, "hub config: out of range → default")
        t.expectEqual(HubClient.port(configData: nil), 7433, "hub config: missing → default")

        // MARK: flags that moved to the hub

        t.expectEqual(ClaudeShipMain.legacyFlagExitCode(["ClaudeShip", "--permission-hook"]), 0,
                      "legacy: a leftover hook entry exits 0 (exit 2 would deny every prompt)")
        t.expectEqual(ClaudeShipMain.legacyFlagExitCode(["ClaudeShip", "--install-hook"]), 2, "legacy: install-hook points elsewhere")
        t.expectEqual(ClaudeShipMain.legacyFlagExitCode(["ClaudeShip", "--uninstall-hook"]), 2, "legacy: uninstall-hook points elsewhere")
        t.expectNil(ClaudeShipMain.legacyFlagExitCode(["ClaudeShip", "--scan"]), "legacy: other flags untouched")

        t.finish()
    }
}
