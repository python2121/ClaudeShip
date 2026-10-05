import AppKit
import Foundation

/// Hand-rolled test harness — this machine has no XCTest/swift-testing (CLT
/// toolchain only), and we don't want the dependency anyway. Dispatched from
/// App.main via `ClaudeStatus --self-test`, before NSApplication exists, so
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
        "cwd":"/Users/andrewnowicki/Documents/code/claude-status",\
        "startedAt":1783455271121,"version":"2.1.202","kind":"interactive",\
        "name":"claude-status-b3","status":"busy",\
        "updatedAt":1783457374373,"statusUpdatedAt":1783457374373}
        """
        let entry = SessionScanner.parseRegistryEntry(Data(registryJSON.utf8))
        t.expectEqual(entry?.pid, 43606, "registry: pid")
        t.expectEqual(entry?.cwd, "/Users/andrewnowicki/Documents/code/claude-status", "registry: cwd")
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
            SessionScanner.projectDirName(forCwd: "/Users/andrewnowicki/Documents/code/claude-status"),
            "-Users-andrewnowicki-Documents-code-claude-status",
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

        // MARK: approval wire format

        let hookInput = #"{"session_id":"s-1","cwd":"/tmp/proj","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm -rf build\necho done","description":"clean"}}"#
        if let line = ApprovalWire.requestLine(fromHookInput: Data(hookInput.utf8)) {
            t.expectEqual(line.last, 0x0A, "wire: request line newline-terminated")
            let req = ApprovalWire.parseRequest(line.dropLast())
            t.expectEqual(req?.sessionId, "s-1", "wire: sessionId round-trips")
            t.expectEqual(req?.cwd, "/tmp/proj", "wire: cwd round-trips")
            t.expectEqual(req?.toolName, "Bash", "wire: toolName round-trips")
            t.expectEqual(req?.summary, "Bash: rm -rf build echo done", "wire: row summary collapses newlines")
            t.expectEqual(req?.detail, "Bash: rm -rf build\necho done", "wire: hover detail keeps newlines")
        } else {
            t.expectEqual(false, true, "wire: requestLine produced nil")
        }
        t.expectNil(ApprovalWire.requestLine(fromHookInput: Data("nope".utf8)), "wire: garbage stdin → nil")

        t.expectEqual(ApprovalWire.summary(tool: "Edit", input: ["file_path": "/a/b.swift"]),
                      "Edit: /a/b.swift", "wire: Edit summary uses file_path")
        t.expectEqual(ApprovalWire.summary(tool: "Mystery", input: nil), "Mystery", "wire: no input → bare tool name")
        t.expectEqual(ApprovalWire.summary(tool: "Bash", input: ["command": String(repeating: "x", count: 300)]).count <= 206,
                      true, "wire: summary clipped")
        t.expectEqual(ApprovalWire.summary(tool: "Bash", input: ["command": String(repeating: "y", count: 5000)], maxChars: 4000, collapseNewlines: false).count <= 4006,
                      true, "wire: detail clipped at its own cap")

        t.expectEqual(ApprovalWire.parseResponse(ApprovalWire.responseLine(allow: true).dropLast()), true, "wire: allow round-trips")
        t.expectEqual(ApprovalWire.parseResponse(ApprovalWire.responseLine(allow: false).dropLast()), false, "wire: deny round-trips")
        t.expectNil(ApprovalWire.parseResponse(Data("{}".utf8)), "wire: missing behavior → nil")

        t.expectEqual(
            ApprovalWire.decisionJSON(allow: true),
            #"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#,
            "wire: allow decision JSON matches CLI schema")
        t.expectEqual(
            ApprovalWire.decisionJSON(allow: false).contains(#""behavior":"deny""#), true,
            "wire: deny decision JSON has deny behavior")

        // MARK: auto-approve rules

        let now2 = Date(timeIntervalSince1970: 2_000_000)
        t.expectEqual(SessionStore.ruleAllows(nil, now: now2), false, "rules: no rule → no auto-approve")
        t.expectEqual(SessionStore.ruleAllows(.forSession, now: now2), true, "rules: forSession always allows")
        t.expectEqual(SessionStore.ruleAllows(.until(now2.addingTimeInterval(60)), now: now2), true, "rules: unexpired timer allows")
        t.expectEqual(SessionStore.ruleAllows(.until(now2.addingTimeInterval(-1)), now: now2), false, "rules: expired timer denies")

        // MARK: terminal-answer reconciliation

        let recv = Date(timeIntervalSince1970: 3_000_000)
        t.expectEqual(SessionStore.resolvedInTerminal(state: .waitingForInput, stateSince: recv.addingTimeInterval(1), receivedAt: recv),
                      false, "reconcile: still waiting → keep buttons")
        t.expectEqual(SessionStore.resolvedInTerminal(state: .busy, stateSince: recv.addingTimeInterval(-60), receivedAt: recv),
                      false, "reconcile: pre-prompt busy (stale stateSince) → keep")
        t.expectEqual(SessionStore.resolvedInTerminal(state: .busy, stateSince: recv.addingTimeInterval(3), receivedAt: recv),
                      true, "reconcile: fresh busy → answered in terminal, drop")
        t.expectEqual(SessionStore.resolvedInTerminal(state: .idle, stateSince: recv.addingTimeInterval(3), receivedAt: recv),
                      true, "reconcile: fresh idle (denied, turn over) → drop")
        t.expectEqual(SessionStore.resolvedInTerminal(state: .busy, stateSince: nil, receivedAt: recv),
                      false, "reconcile: no stateSince → keep (never guess)")
        t.expectEqual(SessionStore.shouldPruneUnmatched(receivedAt: recv, now: recv.addingTimeInterval(5)),
                      false, "reconcile: unmatched within grace → keep")
        t.expectEqual(SessionStore.shouldPruneUnmatched(receivedAt: recv, now: recv.addingTimeInterval(11)),
                      true, "reconcile: unmatched past grace → prune")

        // MARK: hook installer merge (pure, no file IO)

        let (installed, changed1) = HookInstaller.merged([:])
        t.expectEqual(changed1, true, "installer: fresh settings gains hook")
        let matchers = (installed["hooks"] as? [String: Any])?["PermissionRequest"] as? [[String: Any]]
        let cmd = (matchers?.first?["hooks"] as? [[String: Any]])?.first?["command"] as? String
        t.expectEqual(cmd, HookInstaller.hookCommand, "installer: command written")
        let (_, changed2) = HookInstaller.merged(installed)
        t.expectEqual(changed2, false, "installer: idempotent")
        let (removedRoot, changed3) = HookInstaller.removed(installed)
        t.expectEqual(changed3, true, "installer: removal reports change")
        t.expectNil(removedRoot["hooks"], "installer: emptied containers pruned")
        let preserved = HookInstaller.merged(["model": "opus", "hooks": ["Stop": [["hooks": []]]]]).0
        t.expectEqual(preserved["model"] as? String, "opus", "installer: unrelated keys preserved")
        t.expectEqual(((preserved["hooks"] as? [String: Any])?["Stop"] as? [[String: Any]])?.isEmpty, false,
                      "installer: unrelated hooks preserved")

        // MARK: approval server ↔ helper socket round-trip

        let sockPath = NSTemporaryDirectory() + "claudestatus-test-\(getpid()).sock"
        let server = ApprovalServer(path: sockPath)
        let gotRequest = DispatchSemaphore(value: 0)
        var requestedId: UUID?
        var requestedSummary: String?
        server.onRequest = { id, info in
            requestedId = id
            requestedSummary = info.summary
            gotRequest.signal()
        }
        if server.start() {
            var verdict: Bool? = nil
            let clientDone = DispatchSemaphore(value: 0)
            Thread.detachNewThread {
                if let fd = ApprovalSocket.connect(path: sockPath) {
                    let line = Data(#"{"toolName":"Bash","toolInput":{"command":"ls"},"sessionId":"s-9"}"# .utf8) + Data([0x0A])
                    _ = line.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
                    var buf = [UInt8](repeating: 0, count: 4096)
                    let n = read(fd, &buf, buf.count)
                    if n > 0 { verdict = ApprovalWire.parseResponse(Data(buf[0..<n]).prefix(while: { $0 != 0x0A })) }
                    close(fd)
                }
                clientDone.signal()
            }
            t.expectEqual(gotRequest.wait(timeout: .now() + 5), .success, "server: request arrives")
            t.expectEqual(requestedSummary, "Bash: ls", "server: summary parsed")
            if let id = requestedId { server.respond(id, allow: true) }
            t.expectEqual(clientDone.wait(timeout: .now() + 5), .success, "server: client completes")
            t.expectEqual(verdict, true, "server: client received allow")
            server.stop()
        } else {
            t.expectEqual(false, true, "server: failed to start on \(sockPath)")
        }

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
                      "cd '/Users/x/my proj' && claudeandrew hub attach 'ab12cd'", "hub attach: command typed into the new shell")

        t.expectEqual(fakeSession(1, cwd: "/", host: nil, background: true, jobId: "j").attachId, "j", "attachId: jobId wins")
        t.expectEqual(fakeSession(1, cwd: "/", host: nil, background: true, sessionId: "a34398c4-12a2-4b54-aaac-edc6a5e935a6").attachId, "a34398c4", "attachId: falls back to sessionId prefix")
        t.expectNil(fakeSession(1, cwd: "/", host: nil, background: true).attachId, "attachId: nothing to attach with")

        t.expectEqual(TerminalFocus.attachTerminalBundleId(running: ["com.apple.Terminal", "com.mitchellh.ghostty"]), "com.mitchellh.ghostty", "attach: Ghostty preferred when running")
        t.expectEqual(TerminalFocus.attachTerminalBundleId(running: ["com.googlecode.iterm2"]), "com.googlecode.iterm2", "attach: iTerm2 when running")
        t.expectEqual(TerminalFocus.attachTerminalBundleId(running: []), "com.apple.Terminal", "attach: Terminal.app when nothing runs")
        t.expectEqual(TerminalFocus.shellSingleQuoted("it's"), #"'it'\''s'"#, "shell quote: embedded apostrophe")
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

        // MARK: hub — wire framing

        let helloFrame = HubFrame.encodeJSON(.hello, ["op": "status"])
        let outputFrame = HubFrame.encode(.output, Data([0x1b, 0x5b, 0x48, 0x00, 0xff]))
        var frameDecoder = HubFrameDecoder()
        let stream = helloFrame + outputFrame
        let firstHalf = frameDecoder.feed(Data(stream.prefix(7)))
        t.expectEqual(firstHalf?.count, 0, "frames: a partial frame yields nothing yet")
        let rest = frameDecoder.feed(Data(stream.dropFirst(7)))
        t.expectEqual(rest?.map { $0.0 }, [.hello, .output], "frames: both frames once the bytes arrive")
        t.expectEqual(rest.map { HubFrame.json($0[0].1)["op"] as? String }, "status", "frames: JSON payload round-trips")
        t.expectEqual(rest?.last?.1, Data([0x1b, 0x5b, 0x48, 0x00, 0xff]), "frames: binary payload untouched")
        var junkDecoder = HubFrameDecoder()
        t.expectEqual(junkDecoder.feed(Data("GET / HTTP/1.1\r\n".utf8)) == nil, true, "frames: unknown type byte is a protocol error")
        var hugeDecoder = HubFrameDecoder()
        t.expectEqual(hugeDecoder.feed(Data([0x4f, 0x7f, 0xff, 0xff, 0xff])) == nil, true, "frames: oversized length is a protocol error")

        // MARK: hub — terminal stream

        func replayed(_ pieces: [TerminalStream.Piece]) -> String {
            pieces.map { piece -> String in
                switch piece {
                case .bytes(let data): return String(decoding: data, as: UTF8.self)
                case .clear: return "<CLEAR>"
                }
            }.joined()
        }
        let esc = "\u{1b}"
        var modeStream = TerminalStream()
        _ = modeStream.feed(Data("\(esc)[?1049h\(esc)[?25l\(esc)[?2004h\(esc)[?1004h\(esc)[>1u\(esc)[>4;2m\(esc)[?1004l".utf8))
        t.expectEqual(modeStream.modes.dec[1049], true, "modes: alt screen set")
        t.expectEqual(modeStream.modes.dec[25], false, "modes: cursor hidden")
        t.expectEqual(modeStream.modes.dec[1004], false, "modes: focus reporting set then reset")
        t.expectEqual(modeStream.modes.kitty, [1], "modes: kitty flags pushed")
        t.expectEqual(modeStream.modes.modifyOtherKeys, 2, "modes: modifyOtherKeys level")
        t.expectEqual(String(decoding: modeStream.modes.restoreSequence, as: UTF8.self),
                      "\(esc)[?1049h\(esc)[?25l\(esc)[?2004h\(esc)[>1u\(esc)[>4;2m", "modes: restore enters alt screen first")
        t.expectEqual(String(decoding: modeStream.modes.resetSequence, as: UTF8.self),
                      "\(esc)[<1u\(esc)[>4;0m\(esc)[?25h\(esc)[?2004l\(esc)[?1049l\(esc)[0m", "modes: reset leaves alt screen last")
        _ = modeStream.feed(Data("\(esc)[<u\(esc)[>4m\(esc)[?1049l\(esc)[?25h\(esc)[?2004l".utf8))
        t.expectEqual(modeStream.modes.resetSequence.isEmpty, true, "modes: nothing to undo after the program cleans up")
        t.expectEqual(TerminalModes().restoreSequence.isEmpty, true, "modes: default state needs no restore")

        // A sequence split across reads is still one sequence.
        var splitStream = TerminalStream()
        let splitA = splitStream.feed(Data("ab\(esc)[?20".utf8))
        let splitB = splitStream.feed(Data("04hcd".utf8))
        t.expectEqual(replayed(splitA) + replayed(splitB), "ab\(esc)[?2004hcd", "stream: split sequence reassembled in order")
        t.expectEqual(splitStream.modes.dec[2004], true, "stream: split sequence still tracked")

        // Queries, clipboard writes, notifications, and bells don't replay.
        var filterStream = TerminalStream()
        let noisy = "A\(esc)[c\(esc)[>0q\(esc)[6n\(esc)[?u\(esc)[?2026$p\(esc)[14tB"
            + "\(esc)]11;?\u{07}\(esc)]52;c;aGk=\u{07}\(esc)]9;done\(esc)\\\(esc)]777;notify;x;y\u{07}"
            + "\(esc)P+q544e\(esc)\\\(esc)_Gi=1,a=q;AAAA\(esc)\\\u{07}C"
        t.expectEqual(replayed(filterStream.feed(Data(noisy.utf8))), "ABC", "stream: queries and one-shot effects dropped from replay")
        var keepStream = TerminalStream()
        let kept = "\(esc)[1;31mred\(esc)[0m\(esc)]0;title\u{07}\(esc)]9;4;1;50\u{07}\(esc)[2 q\(esc)[8;30;100t\(esc)[1;1;2;2$x\(esc)(B"
        t.expectEqual(replayed(keepStream.feed(Data(kept.utf8))), kept, "stream: styling, titles, progress, and other state kept")
        t.expectEqual(TerminalStream.isQueryCSI(params: Array("!".utf8), final: 0x70), false, "stream: soft reset is not a query")
        t.expectEqual(TerminalStream.isQueryCSI(params: Array(">4".utf8), final: 0x6e), false, "stream: CSI > 4 n is a setting, not a status query")

        // What a mid-sequence attacher must be handed: the consumed half.
        var pendingStream = TerminalStream()
        _ = pendingStream.feed(Data("text\(esc)[38;2;12".utf8))
        t.expectEqual(String(decoding: pendingStream.pendingBytes, as: UTF8.self), "\(esc)[38;2;12", "stream: half a CSI is pending")
        _ = pendingStream.feed(Data(";34;56m".utf8))
        t.expectEqual(pendingStream.pendingBytes.isEmpty, true, "stream: nothing pending once it completes")
        _ = pendingStream.feed(Data("\(esc)]0;title\(esc)".utf8))
        t.expectEqual(String(decoding: pendingStream.pendingBytes, as: UTF8.self), "\(esc)]0;title\(esc)", "stream: a string caught at its terminator's ESC")
        var mokStream = TerminalStream()
        _ = mokStream.feed(Data("\(esc)[>4;2m\(esc)[>4n".utf8))
        t.expectEqual(mokStream.modes.modifyOtherKeys, 0, "modes: CSI > 4 n turns modifyOtherKeys off")
        _ = mokStream.feed(Data("\(esc)[>4;1m\(esc)[>m".utf8))
        t.expectEqual(mokStream.modes.modifyOtherKeys, 0, "modes: bare CSI > m resets it")

        let qr = HubCLI.qrModules("http://100.101.102.103:7433/auth?k=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        t.expectEqual(qr.map { $0.count >= 21 && $0.allSatisfy { $0.count == qr!.count } }, true, "qr: a square symbol")
        if let qr {
            // Finder patterns sit in three corners; the fourth is how a
            // reader tells the symbol's orientation, so it must be bottom-right.
            func finder(_ x: Int, _ y: Int) -> Bool {
                (0..<7).allSatisfy { qr[y][x + $0] && qr[y + 6][x + $0] && qr[y + $0][x] && qr[y + $0][x + 6] }
            }
            let n = qr.count
            t.expectEqual([finder(0, 0), finder(n - 7, 0), finder(0, n - 7), finder(n - 7, n - 7)],
                          [true, true, true, false], "qr: upright (finders top-left, top-right, bottom-left)")
        }

        // Clearing screen + scrollback restarts the replay.
        var clearStream = TerminalStream()
        _ = clearStream.feed(Data("\(esc)[?2004h".utf8))
        let cleared = clearStream.feed(Data("old\(esc)[2J\(esc)[3J\(esc)[Hnew".utf8))
        t.expectEqual(replayed(cleared), "<CLEAR>\(esc)[Hnew", "stream: clear pair drops what came before")
        if case .clear(let modesAtClear)? = cleared.first {
            t.expectEqual(modesAtClear.dec[2004], true, "stream: clear carries the modes in force")
        } else {
            t.expectEqual(true, false, "stream: clear piece present")
        }
        var noClearStream = TerminalStream()
        let notCleared = "\(esc)[2Jtext\(esc)[3J"
        t.expectEqual(replayed(noClearStream.feed(Data(notCleared.utf8))), notCleared, "stream: text between the two is not a clear pair")
        var altStream = TerminalStream()
        let altClear = "\(esc)[?1049h\(esc)[2J\(esc)[3J"
        t.expectEqual(replayed(altStream.feed(Data(altClear.utf8))), altClear, "stream: a clear inside the alt screen keeps the main-screen replay")

        // MARK: hub — whose keystroke was that

        func activity(_ text: String) -> Bool { TerminalInput.isUserActivity(Data(text.utf8)) }
        t.expectEqual(activity("a"), true, "input: a key")
        t.expectEqual(activity("\r"), true, "input: return")
        t.expectEqual(activity("\(esc)"), true, "input: the Esc key")
        t.expectEqual(activity("\(esc)[A"), true, "input: an arrow key")
        t.expectEqual(activity("\(esc)[200~pasted\(esc)[201~"), true, "input: a paste")
        t.expectEqual(activity("\(esc)b"), true, "input: Alt+key")
        t.expectEqual(activity("\(esc)[<0;10;5M"), true, "input: a mouse click")
        t.expectEqual(activity("\(esc)[<64;10;5M"), true, "input: the scroll wheel")
        t.expectEqual(activity("\(esc)[<32;10;5M"), true, "input: a drag")
        t.expectEqual(activity("\(esc)[<35;10;5M"), false, "input: the pointer merely moving")
        t.expectEqual(TerminalInput.isUserActivity(Data([0x1b, 0x5b, 0x4d, 32 + 35, 40, 40])), false, "input: legacy-encoded pointer motion")
        t.expectEqual(TerminalInput.isUserActivity(Data([0x1b, 0x5b, 0x4d, 32 + 0, 40, 40])), true, "input: legacy-encoded click")
        t.expectEqual(activity("\(esc)[I"), false, "input: focus gained")
        t.expectEqual(activity("\(esc)[O"), false, "input: focus lost")
        t.expectEqual(activity("\(esc)[?62;22c"), false, "input: device attributes answer")
        t.expectEqual(activity("\(esc)[24;80R"), false, "input: cursor position answer")
        t.expectEqual(activity("\(esc)[?1u"), false, "input: keyboard flags answer")
        t.expectEqual(activity("\(esc)[?2026;2$y"), false, "input: mode report")
        t.expectEqual(activity("\(esc)]11;rgb:1515/1414/1313\(esc)\\\(esc)[?997;1n"), false, "input: color answers")
        t.expectEqual(activity("\(esc)P>|xterm.js(5.5.0)\(esc)\\"), false, "input: version answer")
        t.expectEqual(activity("\(esc)[I\(esc)[?62c"), false, "input: several reports together")
        t.expectEqual(activity("\(esc)[Ix"), true, "input: a report followed by a key")
        t.expectEqual(activity(""), false, "input: nothing")

        t.expectEqual(Hub.exitCode(fromWaitStatus: 7 << 8), 7, "exit: status 7")
        t.expectEqual(Hub.exitCode(fromWaitStatus: 0), 0, "exit: success")
        t.expectEqual(Hub.exitCode(fromWaitStatus: SIGHUP), 129, "exit: killed by SIGHUP")
        t.expectEqual(Hub.exitCode(fromWaitStatus: SIGKILL | 0x80), 137, "exit: killed, core flag ignored")

        t.expectEqual(HubCLI.bypassesHub([]), false, "cli: bare command is a session")
        t.expectEqual(HubCLI.bypassesHub(["fix the bug", "--model", "opus"]), false, "cli: a prompt is a session")
        t.expectEqual(HubCLI.bypassesHub(["--resume", "abc"]), false, "cli: resume is a session")
        t.expectEqual(HubCLI.bypassesHub(["-p", "hi"]), true, "cli: print mode goes straight to claude")
        t.expectEqual(HubCLI.bypassesHub(["--version"]), true, "cli: version goes straight to claude")
        t.expectEqual(HubCLI.bypassesHub(["mcp", "list"]), true, "cli: a management subcommand goes straight to claude")
        t.expectEqual(HubCLI.bypassesHub(["--bg", "do it"]), true, "cli: claude's own background sessions aren't ours")
        t.expectEqual(HubCLI.bypassesHub(["update the docs"]), false, "cli: a prompt that starts like a subcommand")
        t.expectEqual(HubCLI.resumedSessionId(in: ["--resume", "93fb531a-9e91-4926-ab89-93ded70cba7e"]), "93fb531a-9e91-4926-ab89-93ded70cba7e", "cli: --resume id")
        t.expectEqual(HubCLI.resumedSessionId(in: ["-r", "93fb531a-9e91-4926-ab89-93ded70cba7e", "--model", "opus"]), "93fb531a-9e91-4926-ab89-93ded70cba7e", "cli: -r id")
        t.expectEqual(HubCLI.resumedSessionId(in: ["--resume=93fb531a-9e91-4926-ab89-93ded70cba7e"]), "93fb531a-9e91-4926-ab89-93ded70cba7e", "cli: --resume=id")
        t.expectNil(HubCLI.resumedSessionId(in: ["--resume"]), "cli: --resume with no id (claude's picker) is not a resume of one")
        t.expectNil(HubCLI.resumedSessionId(in: ["--resume", "latest"]), "cli: --resume of a non-uuid")
        t.expectNil(HubCLI.resumedSessionId(in: ["fix", "the", "bug"]), "cli: no resume")

        t.expectEqual(Hub.loginShellArgv(shell: "/bin/zsh", program: "claude", args: ["--resume", "x y"]),
                      ["/bin/zsh", "-l", "-i", "-c", "exec \"$0\" \"$@\"", "claude", "--resume", "x y"], "web launch: POSIX shell argv")
        t.expectEqual(Hub.loginShellArgv(shell: "/opt/homebrew/bin/fish", program: "claude", args: ["--permission-mode", "plan"]),
                      ["/opt/homebrew/bin/fish", "-l", "-i", "-c", "exec $argv", "claude", "--permission-mode", "plan"], "web launch: fish argv")

        // MARK: hub — replay buffer

        var replay = ReplayBuffer(capacity: 100_000)
        var setModes = TerminalModes()
        setModes.dec[2004] = true
        replay.append(Data(repeating: 0x61, count: 40_000), modesAfter: setModes)
        replay.append(Data(repeating: 0x62, count: 40_000), modesAfter: setModes)
        t.expectEqual(replay.snapshot().count, 80_000, "replay: under capacity keeps everything, no preamble")
        replay.append(Data(repeating: 0x63, count: 40_000), modesAfter: setModes)
        let trimmed = replay.snapshot()
        t.expectEqual(trimmed.count < 100_100, true, "replay: over capacity drops from the front")
        t.expectEqual(String(decoding: trimmed.prefix(8), as: UTF8.self), "\(esc)[?2004h", "replay: trimmed replay opens by restoring modes")
        t.expectEqual(trimmed.last, 0x63, "replay: newest output survives")
        replay.reset(base: TerminalModes())
        replay.append(Data("x".utf8), modesAfter: TerminalModes())
        t.expectEqual(String(decoding: replay.snapshot(), as: UTF8.self), "\(esc)[H\(esc)[2J\(esc)[3Jx", "replay: reset starts from a cleared screen")

        // MARK: hub — config

        let parsedConfig = HubConfig.parse(Data(#"{"port": 9000, "defaultPermissionMode": "plan", "root": "/tmp/projects"}"#.utf8))
        t.expectEqual(parsedConfig.port, 9000, "config: port")
        t.expectEqual(parsedConfig.defaultPermissionMode, "plan", "config: permission mode")
        t.expectEqual(parsedConfig.root, "/tmp/projects", "config: root")
        let sloppyConfig = HubConfig.parse(Data(#"{"port": 99999, "defaultPermissionMode": "yolo"}"#.utf8))
        t.expectEqual(sloppyConfig.port, HubConfig.fallback.port, "config: out-of-range port falls back")
        t.expectEqual(sloppyConfig.defaultPermissionMode, "auto", "config: unknown mode falls back to auto")
        t.expectEqual(HubConfig.parse(Data("not json".utf8)), HubConfig.fallback, "config: garbage falls back whole")
        t.expectEqual(parsedConfig.allowedHosts, [], "config: no extra host names by default")
        t.expectEqual(HubConfig.parse(Data(#"{"allowedHosts": ["Mac.Tail1.ts.net", ""]}"#.utf8)).allowedHosts, ["mac.tail1.ts.net"], "config: extra host names lowercased")

        // MARK: hub — directory model

        let day = Date(timeIntervalSince1970: 1_000_000)
        let ordered = HubState.order([
            ("idle-old", 0, day.addingTimeInterval(-9000)),
            ("one-b", 1, day),
            ("never", 0, nil),
            ("two", 2, day.addingTimeInterval(-500)),
            ("idle-new", 0, day.addingTimeInterval(-10)),
            ("one-a", 1, day.addingTimeInterval(-900)),
            ("also-never", 0, nil),
        ])
        t.expectEqual(ordered, [3, 5, 1, 4, 0, 6, 2], "order: running by count then name, idle by recency, unused last by name")

        let projectPaths = ["/code/app", "/code/app-two", "/code/lib"]
        t.expectEqual(HubState.projectIndex(forCwd: "/code/app", paths: projectPaths), 0, "project: exact directory")
        t.expectEqual(HubState.projectIndex(forCwd: "/code/app/.claude/worktrees/x", paths: projectPaths), 0, "project: a worktree inside it")
        t.expectEqual(HubState.projectIndex(forCwd: "/code/app-two/src", paths: projectPaths), 1, "project: a sibling sharing a name prefix")
        t.expectNil(HubState.projectIndex(forCwd: "/elsewhere", paths: projectPaths), "project: outside the root")
        t.expectEqual(HubState.branch(fromHEAD: "ref: refs/heads/feature/x\n"), "feature/x", "branch: symbolic ref")
        t.expectEqual(HubState.branch(fromHEAD: "9dfe4f7a1b2c3d4e5f60718293a4b5c6d7e8f901\n"), "9dfe4f7", "branch: detached head")
        t.expectNil(HubState.branch(fromHEAD: "gitdir: ../x"), "branch: not a HEAD file")
        let tmpRoot = NSTemporaryDirectory() + "claudestatus-selftest-\(getpid())"
        try? FileManager.default.createDirectory(atPath: tmpRoot + "/proj/inner", withIntermediateDirectories: true)
        try? FileManager.default.createDirectory(atPath: tmpRoot + "/.hidden", withIntermediateDirectories: true)
        let canonicalRoot = URL(fileURLWithPath: tmpRoot).standardizedFileURL.path
        t.expectEqual(HubState.launchTarget(tmpRoot + "/proj", root: tmpRoot), canonicalRoot + "/proj", "launch: a project directory")
        t.expectEqual(HubState.launchTarget(tmpRoot + "/proj/../proj/", root: tmpRoot), canonicalRoot + "/proj", "launch: path is normalized")
        t.expectNil(HubState.launchTarget(tmpRoot + "/proj/inner", root: tmpRoot), "launch: not a nested directory")
        t.expectNil(HubState.launchTarget(tmpRoot + "/proj/../..", root: tmpRoot), "launch: not above the root")
        t.expectNil(HubState.launchTarget(tmpRoot, root: tmpRoot), "launch: not the root itself")
        t.expectNil(HubState.launchTarget(tmpRoot + "/.hidden", root: tmpRoot), "launch: not a hidden directory")
        t.expectNil(HubState.launchTarget(tmpRoot + "/missing", root: tmpRoot), "launch: must exist")
        try? FileManager.default.removeItem(atPath: tmpRoot)
        t.expectEqual(HubState.isSessionId("93fb531a-9e91-4926-ab89-93ded70cba7e"), true, "resume: a uuid")
        t.expectEqual(HubState.isSessionId("x; rm -rf ~"), false, "resume: anything else refused")
        t.expectEqual(Hub.clampSize(rows: 40, cols: 120).map { [$0.rows, $0.cols] }, [40, 120], "size: sane size accepted")
        t.expectNil(Hub.clampSize(rows: 0, cols: 0), "size: empty size refused")
        t.expectNil(Hub.clampSize(rows: 70000, cols: 80), "size: absurd size refused")
        let webEnv = Hub.webEnvironment(shell: "/bin/zsh", base: ["HOME": "/Users/x", "PATH": "/bin", "CLAUDECODE": "1", "SECRET": "s"])
        t.expectEqual(webEnv["HOME"], "/Users/x", "env: home carried over")
        t.expectNil(webEnv["CLAUDECODE"], "env: the hub's own session markers are not inherited")
        t.expectNil(webEnv["SECRET"], "env: unlisted variables are not inherited")
        t.expectEqual(webEnv["TERM"], "xterm-256color", "env: terminal type set for the browser terminal")

        // MARK: hub — web: HTTP

        let getRequest = "GET /ws/term?id=ab12cd&rows=40&cols=120 HTTP/1.1\r\nHost: 100.101.102.103:7433\r\nUpgrade: websocket\r\n\r\n"
        if case .request(let parsed, let consumed) = HubHTTP.parse(Data((getRequest + "extra").utf8)) {
            t.expectEqual(consumed, getRequest.utf8.count, "http: reports where the request ends")
            t.expectEqual(parsed.method, "GET", "http: method")
            t.expectEqual(parsed.path, "/ws/term", "http: path without query")
            t.expectEqual(parsed.query, ["id": "ab12cd", "rows": "40", "cols": "120"], "http: query")
            t.expectEqual(parsed.headers["host"], "100.101.102.103:7433", "http: header names lowercased")
        } else {
            t.expectEqual(true, false, "http: GET parses")
        }
        let postHead = "POST /api/launch HTTP/1.1\r\nHost: localhost:7433\r\nContent-Length: 13\r\n\r\n"
        t.expectEqual(HubHTTP.parse(Data((postHead + "{\"path\"").utf8)), .incomplete, "http: waits for the whole body")
        if case .request(let parsed, let consumed) = HubHTTP.parse(Data((postHead + "{\"path\":\"/x\"}").utf8)) {
            t.expectEqual(consumed, postHead.utf8.count + 13, "http: consumed covers the body")
            t.expectEqual(String(decoding: parsed.body, as: UTF8.self), "{\"path\":\"/x\"}", "http: body")
        } else {
            t.expectEqual(true, false, "http: POST parses")
        }
        t.expectEqual(HubHTTP.parse(Data("GET /".utf8)), .incomplete, "http: partial head")
        t.expectEqual(HubHTTP.parse(Data("NONSENSE\r\n\r\n".utf8)), .invalid, "http: malformed request line")
        t.expectEqual(HubHTTP.parse(Data("POST / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n".utf8)), .invalid, "http: oversized body refused")

        // MARK: hub — web: who may connect

        let ts: [UInt8] = [100, 101, 102, 103]
        let tunnel: [[UInt8]] = [ts, HubWebSecurity.addressBytes("fd7a:115c:a1e0::1")!]
        func pair(_ local: String, _ remote: String) -> Bool {
            HubWebSecurity.isAllowedPair(local: HubWebSecurity.addressBytes(local)!,
                                         remote: HubWebSecurity.addressBytes(remote)!, tunnel: tunnel)
        }
        t.expectEqual(pair("127.0.0.1", "127.0.0.1"), true, "gate: loopback to loopback")
        t.expectEqual(pair("::1", "::1"), true, "gate: IPv6 loopback")
        t.expectEqual(pair("100.101.102.103", "100.64.0.9"), true, "gate: tailnet peer on our tunnel address")
        t.expectEqual(pair("::ffff:100.101.102.103", "::ffff:100.127.255.254"), true, "gate: IPv4-mapped tailnet pair")
        t.expectEqual(pair("fd7a:115c:a1e0::1", "fd7a:115c:a1e0::beef"), true, "gate: tailnet IPv6 pair")
        t.expectEqual(pair("100.70.1.1", "100.70.1.2"), false, "gate: CGNAT range on a non-tunnel interface refused")
        t.expectEqual(pair("192.168.1.20", "192.168.1.30"), false, "gate: LAN refused")
        t.expectEqual(pair("100.101.102.103", "192.168.1.30"), false, "gate: non-tailnet peer refused")
        t.expectEqual(pair("100.101.102.103", "100.128.0.1"), false, "gate: peer just past the tailnet range refused")
        t.expectEqual(pair("100.101.102.103", "100.63.255.255"), false, "gate: peer just before the tailnet range refused")
        t.expectEqual(pair("127.0.0.1", "100.64.0.9"), false, "gate: loopback only talks to loopback")
        t.expectEqual(pair("fe80::1", "fe80::2"), false, "gate: link-local refused")
        t.expectEqual(HubWebSecurity.isAllowedPair(local: ts, remote: [100, 64, 0, 9], tunnel: []), false, "gate: no tunnel, no tailnet access")

        t.expectEqual(HubWebSecurity.isAllowedHost("localhost:7433"), true, "host: localhost")
        t.expectEqual(HubWebSecurity.isAllowedHost("127.0.0.1:7433"), true, "host: loopback literal")
        t.expectEqual(HubWebSecurity.isAllowedHost("100.101.102.103:7433"), true, "host: tailnet literal")
        t.expectEqual(HubWebSecurity.isAllowedHost("[fd7a:115c:a1e0::1]:7433"), true, "host: tailnet IPv6 literal")
        t.expectEqual(HubWebSecurity.isAllowedHost("[::1]:7433"), true, "host: IPv6 loopback literal")
        t.expectEqual(HubWebSecurity.isAllowedHost("andrews-macbook-air:7433"), false, "host: a bare name is whatever DNS says it is")
        t.expectEqual(HubWebSecurity.isAllowedHost("mac.tail1234.ts.net"), false, "host: so is a tailnet name, over plain HTTP")
        t.expectEqual(HubWebSecurity.isAllowedHost("Mac.Tail1234.ts.net:443", extra: ["mac.tail1234.ts.net"]), true, "host: unless the config vouches for it")
        t.expectEqual(HubWebSecurity.isAllowedHost("evil.example.com:7433"), false, "host: rebinding domain refused")
        t.expectEqual(HubWebSecurity.isAllowedHost("192.168.1.20:7433"), false, "host: LAN literal refused")
        t.expectEqual(HubWebSecurity.isAllowedHost("localhost.evil.com"), false, "host: lookalike refused")
        t.expectEqual(HubWebSecurity.isAllowedHost(""), false, "host: empty refused")
        t.expectEqual(HubWebSecurity.isAllowedHost("localhost:1; script-src *"), false, "host: only digits may follow the colon")
        t.expectEqual(HubWebSecurity.isAllowedHost("[::1]x:7433"), false, "host: nothing between the bracket and the port")
        t.expectEqual(HubWebSecurity.isAllowedHost("[::1]"), true, "host: bracketed literal without a port")
        t.expectEqual(HubWebSecurity.isAllowedHost("localhost:"), true, "host: empty port")
        t.expectEqual(HubWebSecurity.isAllowedHost("127.0.0.1:123456"), false, "host: overlong port")

        t.expectEqual(HubWebSecurity.cookie(named: "claude_hub", in: "a=1; claude_hub=abc123; b=2"), "abc123", "cookie: found among others")
        t.expectNil(HubWebSecurity.cookie(named: "claude_hub", in: "xclaude_hub=abc; other=1"), "cookie: name must match whole")
        t.expectNil(HubWebSecurity.cookie(named: "claude_hub", in: nil), "cookie: no header")
        t.expectEqual(HubWebSecurity.constantTimeEquals("abcdef", "abcdef"), true, "token: equal")
        t.expectEqual(HubWebSecurity.constantTimeEquals("abcdef", "abcdeg"), false, "token: differs")
        t.expectEqual(HubWebSecurity.constantTimeEquals("abcdef", "abcde"), false, "token: prefix is not equal")
        t.expectEqual(HubWebSecurity.constantTimeEquals("", "abc"), false, "token: empty is not equal")

        t.expectEqual(HubWebSecurity.isSameOrigin(origin: nil, host: "localhost:7433"), true, "origin: absent (not a browser)")
        t.expectEqual(HubWebSecurity.isSameOrigin(origin: "http://localhost:7433", host: "localhost:7433"), true, "origin: our own page")
        t.expectEqual(HubWebSecurity.isSameOrigin(origin: "http://[fd7a:115c:a1e0::1]:7433", host: "[fd7a:115c:a1e0::1]:7433"), true, "origin: IPv6 page")
        t.expectEqual(HubWebSecurity.isSameOrigin(origin: "http://evil.example.com", host: "localhost:7433"), false, "origin: another site")
        t.expectEqual(HubWebSecurity.isSameOrigin(origin: "http://localhost:9999", host: "localhost:7433"), false, "origin: another port")
        t.expectEqual(HubWebSecurity.isSameOrigin(origin: "null", host: "localhost:7433"), false, "origin: opaque origin")

        // MARK: hub — web: WebSocket

        t.expectEqual(WebSocketCodec.acceptKey("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=", "ws: accept key (RFC 6455 example)")
        t.expectEqual(WebSocketCodec.encode(opcode: 2, payload: Data([1, 2, 3])), Data([0x82, 3, 1, 2, 3]), "ws: short server frame")
        t.expectEqual(WebSocketCodec.encode(opcode: 2, payload: Data(count: 300)).prefix(4), Data([0x82, 126, 0x01, 0x2c]), "ws: 16-bit length")
        t.expectEqual(WebSocketCodec.encode(opcode: 2, payload: Data(count: 70_000)).prefix(10),
                      Data([0x82, 127, 0, 0, 0, 0, 0, 0x01, 0x11, 0x70]), "ws: 64-bit length")
        func clientFrame(_ first: UInt8, _ payload: [UInt8], mask: [UInt8] = [0x37, 0xfa, 0x21, 0x3d]) -> Data {
            var frame = Data([first])
            if payload.count < 126 {
                frame.append(0x80 | UInt8(payload.count))
            } else {
                frame.append(contentsOf: [0x80 | 126, UInt8(payload.count >> 8), UInt8(payload.count & 0xff)])
            }
            frame.append(contentsOf: mask)
            frame.append(contentsOf: payload.enumerated().map { $1 ^ mask[$0 & 3] })
            return frame
        }
        var wsParser = WebSocketParser()
        // RFC 6455 §5.7: masked text frame "Hello".
        t.expectEqual(wsParser.feed(Data([0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58])),
                      [WebSocketParser.Message(opcode: 1, payload: Data("Hello".utf8))], "ws: masked text frame")
        let bigPayload = [UInt8](repeating: 0x41, count: 500)
        let bigFrame = clientFrame(0x82, bigPayload)
        t.expectEqual(wsParser.feed(Data(bigFrame.prefix(100))), [], "ws: partial frame waits")
        t.expectEqual(wsParser.feed(Data(bigFrame.dropFirst(100))),
                      [WebSocketParser.Message(opcode: 2, payload: Data(bigPayload))], "ws: 16-bit length frame across reads")
        let fragments = clientFrame(0x02, [1, 2]) + clientFrame(0x89, [9]) + clientFrame(0x80, [3])
        t.expectEqual(wsParser.feed(fragments), [
            WebSocketParser.Message(opcode: 9, payload: Data([9])),
            WebSocketParser.Message(opcode: 2, payload: Data([1, 2, 3])),
        ], "ws: fragments join, a ping may interleave")
        var unmaskedParser = WebSocketParser()
        t.expectEqual(unmaskedParser.feed(Data([0x81, 0x01, 0x41])) == nil, true, "ws: unmasked client frame is a protocol error")
        var strayParser = WebSocketParser()
        t.expectEqual(strayParser.feed(clientFrame(0x80, [1])) == nil, true, "ws: continuation with nothing to continue")

        t.finish()
    }
}
