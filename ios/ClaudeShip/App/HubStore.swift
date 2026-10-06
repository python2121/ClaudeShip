import Foundation
import Observation

/// The directory, kept current by polling while the app is in front —
/// the same two-second cadence as the web page.
@Observable
@MainActor
final class HubStore {
    let connection: HubConnection
    private(set) var state: HubState?
    private(set) var error: String?
    private(set) var unpaired = false
    var notice: String?
    private(set) var launching = false
    /// The Mac's clock minus ours: ages are measured against times the Mac wrote.
    private(set) var clockOffset: TimeInterval = 0
    private var timer: Timer?
    private var inFlight = false
    private var noticeTask: Task<Void, Never>?

    init(connection: HubConnection) {
        self.connection = connection
    }

    func setActive(_ active: Bool) {
        timer?.invalidate()
        timer = nil
        guard active else { return }
        Task { await refresh() }
        // Common mode, so a list being scrolled doesn't pause the polling.
        let timer = Timer(timeInterval: 2, repeats: true) { [weak self] _ in
            Task { await self?.refresh() }
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    func refresh() async {
        guard connection.isPaired, !inFlight else { return }
        inFlight = true
        defer { inFlight = false }
        do {
            let fresh = try await connection.state()
            clockOffset = Double(fresh.now) / 1000 - Date().timeIntervalSince1970
            state = fresh
            unpaired = false
            error = nil
        } catch HubError.unpaired {
            unpaired = true
            state = nil
            error = nil
        } catch {
            unpaired = false
            self.error = error.localizedDescription
            NSLog("ClaudeShip: refresh failed: %@", "\(error)")
        }
    }

    /// "now" on the Mac's clock.
    var now: Date { Date().addingTimeInterval(clockOffset) }

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
