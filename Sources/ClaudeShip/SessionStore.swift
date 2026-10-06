import AppKit
import Foundation

/// Single source of truth: polls SessionScanner, publishes the session list,
/// and owns the approval server (pending permission requests + verdicts).
/// AppDelegate observes objectWillChange to repaint the menubar count;
/// SessionsView renders the rows.
@MainActor
final class SessionStore: ObservableObject {
    @Published private(set) var sessions: [ClaudeSession] = []
    /// Permission requests forwarded by the hook helper, awaiting a click.
    @Published private(set) var pendingApprovals: [PendingApproval] = []
    /// Menubar style: false (the default) = colored text on the bare menu
    /// bar, true = the "inverted" look — a solid color block with
    /// contrasting text. Persisted; AppDelegate repaints via
    /// objectWillChange on change.
    @Published var invertMenubarColors: Bool = UserDefaults.standard.object(forKey: SessionStore.invertMenubarColorsKey) as? Bool ?? false {
        didSet { UserDefaults.standard.set(invertMenubarColors, forKey: SessionStore.invertMenubarColorsKey) }
    }
    private static let invertMenubarColorsKey = "invertMenubarColors"

    enum AutoApproveRule: Equatable {
        case until(Date)
        case forSession
    }
    /// Per-session auto-approve rules ("approve all …"), keyed by sessionId.
    /// Checked on arrival; matching requests are allowed without ever
    /// showing in the UI. Not persisted — an approval standing order
    /// shouldn't outlive the app that granted it.
    private(set) var autoApprove: [String: AutoApproveRule] = [:]

    private var server: ApprovalServer?
    private var timer: Timer?
    /// Everything we poll is local (a readdir + a stat and a tail-read per
    /// session), so a tight interval is cheap and keeps the count honest.
    private let pollInterval: TimeInterval = 2

    init(startPolling: Bool = true) {
        guard startPolling else { return }
        startApprovalServer()
        Task { await refresh() }
        timer = Timer.scheduledTimer(withTimeInterval: pollInterval, repeats: true) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in await self.refresh() }
        }
    }

    func refresh() async {
        // The scan blocks on file IO — keep it off the main thread.
        var scanned = await Task.detached(priority: .utility) { SessionScanner.scan() }.value
        // Branch and title are garnish scraped from the transcript tail and
        // not every scan can see them (the tail window may hold only entries
        // without). Sessions don't lose them — carry the last known values
        // forward instead of letting the label flicker out.
        for i in scanned.indices {
            let previous = sessions.first { $0.pid == scanned[i].pid }
            if scanned[i].gitBranch == nil { scanned[i].gitBranch = previous?.gitBranch }
            if scanned[i].title == nil { scanned[i].title = previous?.title }
        }
        if scanned != sessions { sessions = scanned }
        reconcilePendingWithRegistry()
        pruneRules()
    }

    /// The terminal prompt renders concurrently with our buttons (the CLI
    /// shows it while the PermissionRequest hook is still running), so the
    /// user can answer in either place. When they answer in the terminal,
    /// the session's registry status leaves `waiting` with a fresh
    /// statusUpdatedAt — drop our stale buttons and close the helper
    /// connection without a verdict, so we never talk over their answer.
    private func reconcilePendingWithRegistry() {
        let now = Date()
        for approval in pendingApprovals {
            let session = approval.sessionId.flatMap { sid in
                sessions.first { $0.sessionId == sid }
            }
            let drop: Bool
            if let session {
                drop = Self.resolvedInTerminal(state: session.state,
                                               stateSince: session.stateSince,
                                               receivedAt: approval.receivedAt)
            } else {
                // No matching live session (it exited, or the ids never
                // lined up). With no helper deadline anymore, this is the
                // only thing standing between an unanswerable request and
                // an immortal one — prune after a short grace period that
                // covers registry lag at session start.
                drop = Self.shouldPruneUnmatched(receivedAt: approval.receivedAt, now: now)
            }
            guard drop else { continue }
            server?.cancel(approval.id)
            pendingApprovals.removeAll { $0.id == approval.id }
        }
    }

    nonisolated static func shouldPruneUnmatched(receivedAt: Date, now: Date) -> Bool {
        now.timeIntervalSince(receivedAt) > 10
    }

    /// True when a status change *newer than the approval's arrival* moved
    /// the session out of `waiting`. At arrival the session still reads
    /// `busy` from *before* the prompt (stale stateSince, no false drop);
    /// the prompt itself reads `waiting` (kept); only an answer produces a
    /// non-waiting status that postdates the request.
    nonisolated static func resolvedInTerminal(state: ClaudeSession.State, stateSince: Date?, receivedAt: Date) -> Bool {
        state != .waitingForInput && (stateSince ?? .distantPast) > receivedAt
    }

    // MARK: Approvals

    private func startApprovalServer() {
        let server = ApprovalServer(path: ApprovalSocket.defaultPath)
        server.onRequest = { [weak self] id, info in
            let approval = PendingApproval(
                id: id, sessionId: info.sessionId, cwd: info.cwd,
                toolName: info.toolName, summary: info.summary,
                detail: info.detail, receivedAt: Date())
            Task { @MainActor in self?.received(approval) }
        }
        server.onClosed = { [weak self] id in
            // Helper hit its deadline (or died): the prompt is back in the
            // terminal — drop the buttons immediately.
            Task { @MainActor in self?.pendingApprovals.removeAll { $0.id == id } }
        }
        if server.start() { self.server = server }
    }

    private func received(_ approval: PendingApproval) {
        if let sid = approval.sessionId,
           Self.ruleAllows(autoApprove[sid], now: Date()) {
            server?.respond(approval.id, allow: true)
            return
        }
        pendingApprovals.append(approval)
    }

    func approve(_ approval: PendingApproval) {
        server?.respond(approval.id, allow: true)
        pendingApprovals.removeAll { $0.id == approval.id }
    }

    func deny(_ approval: PendingApproval) {
        server?.respond(approval.id, allow: false)
        pendingApprovals.removeAll { $0.id == approval.id }
    }

    /// "Approve all …": set the standing rule, then let it swallow whatever
    /// is already pending for that session.
    func approveAll(sessionId: String, rule: AutoApproveRule) {
        autoApprove[sessionId] = rule
        for approval in pendingApprovals where approval.sessionId == sessionId {
            approve(approval)
        }
    }

    nonisolated static func ruleAllows(_ rule: AutoApproveRule?, now: Date) -> Bool {
        switch rule {
        case .until(let expiry): return now < expiry
        case .forSession: return true
        case nil: return false
        }
    }

    /// Expired timers and rules for sessions that no longer exist fall away.
    private func pruneRules() {
        let liveIds = Set(sessions.compactMap { $0.sessionId })
        let now = Date()
        autoApprove = autoApprove.filter { sid, rule in
            guard liveIds.contains(sid) else { return false }
            if case .until(let expiry) = rule { return now < expiry }
            return true
        }
    }

    func firstPending(for session: ClaudeSession) -> PendingApproval? {
        guard let sid = session.sessionId else { return nil }
        return pendingApprovals.first { $0.sessionId == sid }
    }

    func pendingCount(for session: ClaudeSession) -> Int {
        guard let sid = session.sessionId else { return 0 }
        return pendingApprovals.filter { $0.sessionId == sid }.count
    }

    func hasAutoApprove(_ session: ClaudeSession) -> Bool {
        guard let sid = session.sessionId else { return false }
        return Self.ruleAllows(autoApprove[sid], now: Date())
    }

    /// A session with a pending approval is waiting on the user, whatever
    /// the registry says (the hook fires before the CLI flips its status).
    func effectiveState(_ session: ClaudeSession) -> ClaudeSession.State {
        firstPending(for: session) != nil ? .waitingForInput : session.state
    }

    // MARK: Counts

    var waitingCount: Int { sessions.filter { effectiveState($0) == .waitingForInput }.count }
    var busyCount: Int { sessions.filter { effectiveState($0) == .busy }.count }
    var shellCount: Int { sessions.filter { effectiveState($0) == .shell }.count }
    var idleCount: Int { sessions.filter { effectiveState($0) == .idle }.count }

    // MARK: Menubar symbol

    /// What the tray glyph communicates: ● orange when anything needs the
    /// user (approval pending / waiting on input), an animated ◐◓◑◒ spinner
    /// in green while sessions are working, ○ in the system label color —
    /// black/white with the theme, like the neighboring menu bar items —
    /// when everything is idle (or nothing is running).
    enum TrayState: Equatable {
        case idle
        case busy
        case waiting
    }

    var trayState: TrayState { Self.trayState(busy: busyCount, waiting: waitingCount) }

    nonisolated static func trayState(busy: Int, waiting: Int) -> TrayState {
        if waiting > 0 { return .waiting }
        if busy > 0 { return .busy }
        return .idle
    }

    nonisolated static func trayColor(_ state: TrayState) -> NSColor {
        switch state {
        case .waiting: return .systemOrange
        case .busy: return .systemGreen
        case .idle: return .labelColor
        }
    }
}
