import Foundation
import Observation

/// The directory, kept current by polling while the app is in front —
/// the same two-second cadence as the web page, slowed to six while a
/// session screen is up (its header is all that needs the directory then,
/// and each poll is a full scan on the Mac and a radio wake here).
@Observable
@MainActor
final class HubStore: Identifiable {
    /// The hub's id in `HubRegistry`.
    let key: UUID
    var id: UUID { key }
    let connection: HubConnection
    /// The hub's last known host name (the registry keeps it).
    private(set) var name: String
    @ObservationIgnored private let onHost: (String) -> Void
    private(set) var state: HubState?
    private(set) var error: String?
    /// The Mac has been unreachable for a few seconds: the directory hides
    /// its rows (they are stale, and there is nothing to start) behind a
    /// calm note until it answers again. The data is kept, so the rows are
    /// back the moment it does.
    private(set) var offline = false
    /// The host named in the offline note: the Mac's name when we have
    /// heard it, else the address we are dialling.
    var hostName: String { state?.host ?? name }
    @ObservationIgnored private var failingSince: Date?
    @ObservationIgnored private var offlineTask: Task<Void, Never>?
    static let offlineAfter: TimeInterval = 5
    private(set) var unpaired = false
    var notice: String?
    private(set) var launching = false
    /// A peer refused through this hub (409 a different build, 502 out of
    /// reach): an alert naming the machine, not a passing notice.
    var alert: ProxyAlert?
    /// The Mac's clock minus ours: ages are measured against times the Mac wrote.
    @ObservationIgnored private var clockOffset: TimeInterval = 0
    /// "now" on the Mac's clock, advanced once per poll. Stored, not
    /// computed, so only the views showing an age re-render per poll —
    /// `state` is reassigned only when the directory actually changed.
    private(set) var now = Date()
    /// Each swarm host's clock minus ours, keyed by `HubHost.target ?? ""`
    /// (a peer's clock can be off from this hub's as much as from ours).
    /// Learnt only while the host is reachable; an unreachable one keeps
    /// its last offset (its `now` is as old as its state).
    @ObservationIgnored private var hostOffsets: [String: TimeInterval] = [:]
    /// "now" on each host's clock, advanced once per poll like `now`.
    private(set) var hostNow: [String: Date] = [:]
    /// A session screen is on top: poll less.
    var viewingSession = false
    private var timer: Timer?
    private var ticks = 0
    private var inFlight = false
    private var noticeTask: Task<Void, Never>?
    /// Approvals with an answer in flight (their buttons are disabled).
    private(set) var answering: Set<String> = []

    init(key: UUID, name: String, connection: HubConnection, onHost: @escaping (String) -> Void) {
        self.key = key
        self.name = name
        self.connection = connection
        self.onHost = onHost
    }

    func setActive(_ active: Bool) {
        timer?.invalidate()
        timer = nil
        guard active else {
            // Not polling, so nothing can be learnt: don't let the clock
            // run out in the background and greet the user with "offline".
            offlineTask?.cancel()
            offlineTask = nil
            failingSince = nil
            return
        }
        Task { await refresh() }
        // Common mode, so a list being scrolled doesn't pause the polling.
        ticks = 0
        let timer = Timer(timeInterval: 2, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.ticks += 1
            if self.viewingSession, self.ticks % 3 != 0 { return }
            Task { await self.refresh() }
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    func refresh() async {
        guard connection.isPaired, !inFlight else { return }
        inFlight = true
        defer { inFlight = false }
        do {
            var fresh = try await connection.state()
            let here = Date()
            clockOffset = Double(fresh.now) / 1000 - here.timeIntervalSince1970
            now = here.addingTimeInterval(clockOffset)
            var clocks: [String: Date] = [:]
            for host in fresh.hosts ?? [] {
                let key = host.target ?? ""
                if host.isReachable, let theirs = host.now {
                    hostOffsets[key] = Double(theirs) / 1000 - here.timeIntervalSince1970
                }
                clocks[key] = here.addingTimeInterval(hostOffsets[key] ?? clockOffset)
            }
            hostNow = clocks
            // `now` differs every time (and a reachable host's `lastSeen`
            // with it); compare the rest.
            fresh.now = state?.now ?? fresh.now
            if let hosts = fresh.hosts {
                fresh.hosts = hosts.map { host in
                    guard let old = state?.hosts?.first(where: { $0.id == host.id }) else { return host }
                    var host = host
                    host.now = old.now
                    if host.isReachable && old.isReachable { host.lastSeen = old.lastSeen }
                    return host
                }
            }
            if fresh != state { state = fresh }
            if fresh.host != name {
                name = fresh.host
                onHost(fresh.host)
            }
            unpaired = false
            error = nil
            reachable()
        } catch HubError.unpaired {
            unpaired = true
            state = nil
            error = nil
            reachable()
        } catch {
            unpaired = false
            self.error = error.localizedDescription
            NSLog("ClaudeShip: refresh failed: %@", "\(error)")
            unreachable()
        }
    }

    private func reachable() {
        failingSince = nil
        offlineTask?.cancel()
        offlineTask = nil
        offline = false
    }

    /// The first failure starts a clock; a success before it runs out
    /// cancels it (a dropped poll on flaky Wi-Fi shouldn't blank the page).
    private func unreachable() {
        guard failingSince == nil else { return }
        failingSince = Date()
        offlineTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.offlineAfter))
            guard let self, !Task.isCancelled, self.failingSince != nil else { return }
            self.offline = true
        }
    }

    func show(_ text: String) {
        notice = text
        noticeTask?.cancel()
        noticeTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(8))
            if !Task.isCancelled { self?.notice = nil }
        }
    }

    struct ProxyAlert: Equatable {
        var title: String
        var message: String
    }

    /// "now" on a host's clock (`host`: a peer's id; nil, this hub's machine).
    func now(for host: String?) -> Date { hostNow[host ?? ""] ?? now }

    /// A host's name as a person reads it (`host`: a peer's id).
    func hostName(_ host: String?) -> String {
        guard let host else { return hostName }
        return state?.host(host)?.displayName ?? host
    }

    /// Says what went wrong: a peer refused through this hub as an alert
    /// naming it, anything else as the usual notice.
    private func fail(_ error: Error, _ what: String) {
        if case HubError.proxy(let refusal) = error {
            let name = hostName(refusal.host)
            alert = ProxyAlert(title: refusal.title(hostName: name), message: refusal.message(hostName: name))
        } else {
            show("\(what): \(error.localizedDescription)")
        }
    }

    /// Returns the new session's hub id, or nil (with a notice) on failure.
    /// `host`: the swarm peer to start it on (nil: this hub's machine).
    func launch(path: String, mode: String? = nil, resume: String? = nil, host: String? = nil) async -> String? {
        guard !launching else { return nil }
        launching = true
        defer { launching = false }
        do {
            let id = try await connection.launch(path: path, mode: mode, resume: resume, host: host)
            await refresh()
            return id
        } catch {
            fail(error, "Couldn't start a session")
            return nil
        }
    }

    func end(hubId: String, host: String?) async -> Bool {
        do {
            try await connection.end(hubId: hubId, host: host)
            await refresh()
            return true
        } catch {
            fail(error, "Couldn't end the session")
            return false
        }
    }

    func answer(_ approval: HubApproval, allow: Bool, host: String?) async {
        guard !answering.contains(approval.id) else { return }
        answering.insert(approval.id)
        defer { answering.remove(approval.id) }
        do {
            try await connection.approve(id: approval.id, allow: allow, host: host)
            await refresh()
        } catch {
            fail(error, "Couldn't answer")
        }
    }

    /// "Approve all …": set the standing rule; the hub answers whatever is
    /// already pending for the session with it. `rule`: "5m", "session", or
    /// "off".
    func autoApprove(_ session: HubSession, rule: String, host: String?) async {
        guard let sessionId = session.sessionId else { return }
        do {
            try await connection.autoApprove(sessionId: sessionId, rule: rule, host: host)
            await refresh()
        } catch {
            fail(error, "Couldn't change auto-approve")
        }
    }

    /// Paired again with a fresh token: forget the refusal and look.
    func repaired() {
        unpaired = false
        error = nil
        Task { await refresh() }
    }

    /// The session with this hub id on that host (nil: this hub's machine)
    /// and the project it sits in, if any.
    func session(hubId: String, host: String?) -> (session: HubSession, project: HubProject?)? {
        state?.host(host)?.session(hubId: hubId)
    }
}
