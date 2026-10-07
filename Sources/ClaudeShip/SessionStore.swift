import AppKit
import Foundation

/// A permission request pending in the hub, awaiting a click in the overlay.
struct PendingApproval: Identifiable, Equatable {
    /// The hub's id for it — what `POST /api/approve` takes.
    let id: String
    let sessionId: String?
    let toolName: String
    /// Human-readable one-liner of what's being approved (e.g. the Bash
    /// command), pre-truncated by the hub for display.
    let summary: String
    /// The full approval text (newlines intact, generous cap) — what the
    /// tooltip shows on hover.
    let detail: String
    let receivedAt: Date
}

/// Single source of truth: polls SessionScanner (which also asks the hub
/// for its state), publishes the session list and the permission requests
/// pending in the hub. AppDelegate observes objectWillChange to repaint the
/// menubar count; SessionsView renders the rows.
@MainActor
final class SessionStore: ObservableObject {
    @Published private(set) var sessions: [ClaudeSession] = []
    /// Permission requests the hub holds, awaiting a click. Empty when no
    /// hub runs, or when it speaks another protocol than ours.
    @Published private(set) var pendingApprovals: [PendingApproval] = []
    /// Whether the overlay is on screen. The view's 1 s clock for the
    /// "held 2m" captions runs only then; the hosting view lives for the
    /// app's lifetime, so without this it would re-evaluate the whole
    /// overlay every second with nobody looking.
    @Published var panelVisible = false
    /// Menubar style: false (the default) = colored text on the bare menu
    /// bar, true = the "inverted" look — a solid color block with
    /// contrasting text. Persisted; AppDelegate repaints via
    /// objectWillChange on change.
    @Published var invertMenubarColors: Bool = UserDefaults.standard.object(forKey: SessionStore.invertMenubarColorsKey) as? Bool ?? false {
        didSet { UserDefaults.standard.set(invertMenubarColors, forKey: SessionStore.invertMenubarColorsKey) }
    }
    private static let invertMenubarColorsKey = "invertMenubarColors"

    typealias AutoApproveRule = HubClient.Rule
    /// Per-session auto-approve rules ("approve all …"), keyed by sessionId,
    /// as the hub reports them. The hub holds and applies them (in memory
    /// only); this copy drives the bolt on the row.
    @Published private(set) var autoApprove: [String: AutoApproveRule] = [:]

    /// Approvals answered here whose disappearance the hub hasn't reported
    /// yet (its state is cached for up to a second) — kept out of the list
    /// so a clicked row doesn't flash its buttons back for one poll.
    private var answered: Set<String> = []
    /// Rules set here, shown until the hub's state has had time to carry them.
    private var localRules: [String: (rule: AutoApproveRule, at: Date)] = [:]

    private var timer: Timer?
    /// Everything we poll is local (a readdir + a stat and a tail-read per
    /// session, one loopback request to the hub), so a tight interval is
    /// cheap and keeps the count honest.
    private let pollInterval: TimeInterval = 2

    init(startPolling: Bool = true) {
        guard startPolling else { return }
        Task { await refresh() }
        timer = Timer.scheduledTimer(withTimeInterval: pollInterval, repeats: true) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in await self.refresh() }
        }
    }

    func refresh() async {
        // The scan blocks on file IO and the hub request — keep it off the
        // main thread.
        var (scanned, hub) = await Task.detached(priority: .utility) { SessionScanner.scanWithHub() }.value
        // Branch and title are garnish scraped from the transcript tail and
        // not every scan can see them (the tail window may hold only entries
        // without). Sessions don't lose them — carry the last known values
        // forward instead of letting the label flicker out.
        for i in scanned.indices {
            let previous = sessions.first { $0.pid == scanned[i].pid }
            if scanned[i].gitBranch == nil { scanned[i].gitBranch = previous?.gitBranch }
            if scanned[i].title == nil { scanned[i].title = previous?.title }
        }
        // Publish only what the overlay can see change: a busy session's
        // transcript mtime moves every scan and would otherwise republish
        // (and re-diff the whole view) every 2 s for nothing.
        if !Self.sameForDisplay(scanned, sessions) { sessions = scanned }
        applyHubApprovals(hub, scanned: scanned)
    }

    static func sameForDisplay(_ a: [ClaudeSession], _ b: [ClaudeSession]) -> Bool {
        guard a.count == b.count else { return false }
        return zip(a, b).allSatisfy { x, y in
            var x = x, y = y
            x.lastActivity = nil
            y.lastActivity = nil
            return x == y
        }
    }

    // MARK: Approvals (held by the hub)

    /// Take the pending requests and rules from the hub's state. The hub
    /// does everything else: it notices an answer given in the terminal,
    /// prunes requests whose session is gone, and applies the rules.
    private func applyHubApprovals(_ hub: HubClient.State?, scanned: [ClaudeSession]) {
        var pending: [PendingApproval] = []
        var rules: [String: AutoApproveRule] = [:]
        if let hub, hub.approvalsUsable {
            for entry in hub.sessions {
                // The hub's entries are keyed by Claude pid; the registry
                // gives the sessionId when the hub's entry doesn't carry one.
                let sid = entry.sessionId ?? scanned.first { $0.pid == entry.pid }?.sessionId
                if let sid, let rule = entry.autoApprove { rules[sid] = rule }
                for a in entry.approvals {
                    pending.append(PendingApproval(id: a.id, sessionId: sid, toolName: a.tool,
                                                   summary: a.summary, detail: a.detail, receivedAt: a.receivedAt))
                }
            }
        }
        answered.formIntersection(pending.map(\.id))
        pending.removeAll { answered.contains($0.id) }
        pending.sort { $0.receivedAt < $1.receivedAt }
        let now = Date()
        localRules = localRules.filter { now.timeIntervalSince($0.value.at) < 4 }
        for (sid, local) in localRules where rules[sid] == nil { rules[sid] = local.rule }
        if pending != pendingApprovals { pendingApprovals = pending }
        if rules != autoApprove { autoApprove = rules }
    }

    func approve(_ approval: PendingApproval) { answer(approval, allow: true) }
    func deny(_ approval: PendingApproval) { answer(approval, allow: false) }

    private func answer(_ approval: PendingApproval, allow: Bool) {
        answered.insert(approval.id)
        pendingApprovals.removeAll { $0.id == approval.id }
        Task {
            let ok = await Task.detached { HubClient.approve(approval.id, allow: allow) }.value
            // Not delivered (hub gone mid-click): let the buttons come back
            // with the next poll if the hub still holds the request.
            if !ok { answered.remove(approval.id) }
            await refresh()
        }
    }

    /// "Approve all …": the hub sets the standing rule and answers whatever
    /// is already pending for that session.
    func approveAll(sessionId: String, rule: AutoApproveRule) {
        let wire: String
        switch rule {
        case .forSession: wire = "session"
        case .until: wire = "5m"
        }
        localRules[sessionId] = (rule, Date())
        autoApprove[sessionId] = rule
        let hidden = pendingApprovals.filter { $0.sessionId == sessionId }.map(\.id)
        answered.formUnion(hidden)
        pendingApprovals.removeAll { $0.sessionId == sessionId }
        Task {
            let ok = await Task.detached { HubClient.autoApprove(sessionId: sessionId, rule: wire) }.value
            if !ok {
                // Only this click's requests come back; others answered
                // individually stay hidden until the hub drops them.
                localRules[sessionId] = nil
                answered.subtract(hidden)
            }
            await refresh()
        }
    }

    nonisolated static func ruleAllows(_ rule: AutoApproveRule?, now: Date) -> Bool {
        switch rule {
        case .until(let expiry): return now < expiry
        case .forSession: return true
        case nil: return false
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
