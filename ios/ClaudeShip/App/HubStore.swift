import Foundation
import Observation

/// The directory, kept current by polling while the app is in front —
/// the same two-second cadence as the web page, slowed to six while a
/// session screen is up (its header is all that needs the directory then,
/// and each poll is a full scan on the Mac and a radio wake here).
@Observable
@MainActor
final class HubStore {
    let connection: HubConnection
    private(set) var state: HubState?
    private(set) var error: String?
    /// The Mac has been unreachable for a few seconds: the directory hides
    /// its rows (they are stale, and there is nothing to start) behind a
    /// calm note until it answers again. The data is kept, so the rows are
    /// back the moment it does.
    private(set) var offline = false
    /// The host named in the offline note: the Mac's name when we have
    /// heard it, else the address we are dialling.
    var hostName: String {
        state?.host ?? UserDefaults.standard.string(forKey: Self.lastHostKey) ?? connection.displayAddress
    }
    private static let lastHostKey = "lastHost"
    @ObservationIgnored private var failingSince: Date?
    @ObservationIgnored private var offlineTask: Task<Void, Never>?
    static let offlineAfter: TimeInterval = 5
    private(set) var unpaired = false
    var notice: String?
    private(set) var launching = false
    /// The Mac's clock minus ours: ages are measured against times the Mac wrote.
    @ObservationIgnored private var clockOffset: TimeInterval = 0
    /// "now" on the Mac's clock, advanced once per poll. Stored, not
    /// computed, so only the views showing an age re-render per poll —
    /// `state` is reassigned only when the directory actually changed.
    private(set) var now = Date()
    /// A session screen is on top: poll less.
    var viewingSession = false
    private var timer: Timer?
    private var ticks = 0
    private var inFlight = false
    private var noticeTask: Task<Void, Never>?

    init(connection: HubConnection) {
        self.connection = connection
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
            clockOffset = Double(fresh.now) / 1000 - Date().timeIntervalSince1970
            now = Date().addingTimeInterval(clockOffset)
            // `now` differs every time; compare the rest.
            fresh.now = state?.now ?? fresh.now
            if fresh != state { state = fresh }
            UserDefaults.standard.set(fresh.host, forKey: Self.lastHostKey)
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

    /// Returns the new session's hub id, or nil (with a notice) on failure.
    func launch(path: String, mode: String? = nil, resume: String? = nil) async -> String? {
        guard !launching else { return nil }
        launching = true
        defer { launching = false }
        do {
            let id = try await connection.launch(path: path, mode: mode, resume: resume)
            await refresh()
            return id
        } catch {
            show("Couldn't start a session: \(error.localizedDescription)")
            return nil
        }
    }

    func end(hubId: String) async -> Bool {
        do {
            try await connection.end(hubId: hubId)
            await refresh()
            return true
        } catch {
            show("Couldn't end the session: \(error.localizedDescription)")
            return false
        }
    }

    func setDefaultMode(_ mode: String) async {
        do {
            try await connection.setDefaultMode(mode)
            await refresh()
        } catch {
            show("Couldn't save the setting: \(error.localizedDescription)")
        }
    }

    func unpair() {
        setActive(false)
        connection.unpair()
        state = nil
        unpaired = true
    }

    /// The session with this hub id and the project it sits in, if any.
    func session(hubId: String) -> (session: HubSession, project: HubProject?)? {
        guard let state else { return nil }
        for project in state.projects {
            if let session = project.sessions.first(where: { $0.hubId == hubId }) { return (session, project) }
        }
        if let session = state.elsewhere.first(where: { $0.hubId == hubId }) { return (session, nil) }
        return nil
    }
}
