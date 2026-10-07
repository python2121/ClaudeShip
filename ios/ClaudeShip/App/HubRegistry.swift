import Foundation
import Observation

/// One paired hub as the phone remembers it. `name` is the hub's `host`
/// from its state, refreshed on every successful poll (the address until
/// the first one); the token is never written to UserDefaults — it lives
/// in the Keychain under `HubRegistry.tokenAccount(id)`.
struct Hub: Codable, Identifiable, Equatable {
    var id: UUID
    var name: String
    var baseURL: URL
    var token: String?

    private enum CodingKeys: String, CodingKey { case id, name, baseURL }
}

/// Where a session screen points: which paired hub carries it, which
/// machine of that hub's swarm it runs on (nil: the hub's own), and the
/// session's hub id there.
struct SessionRoute: Hashable {
    var hub: UUID
    var host: String? = nil
    var id: String
}

/// Every hub this phone is paired with, in pairing order, each with its
/// own `HubStore` (polling, offline clock, notices — all per hub).
@Observable
@MainActor
final class HubRegistry {
    private static let listKey = "hubs"
    /// The single-hub era's keys, migrated once on first launch.
    private static let legacyURLKey = "hubURL"
    private static let legacyTokenAccount = "token"
    private static let legacyHostKey = "lastHost"

    private(set) var hubs: [Hub] = []
    private(set) var stores: [HubStore] = []
    @ObservationIgnored private var active = false

    static func tokenAccount(_ id: UUID) -> String { "token.\(id.uuidString)" }

    init() {
        if let data = UserDefaults.standard.data(forKey: Self.listKey),
           let saved = try? JSONDecoder().decode([Hub].self, from: data) {
            hubs = saved.map { var hub = $0; hub.token = Keychain.string(for: Self.tokenAccount(hub.id)); return hub }
        }
        migrateLegacy()
        stores = hubs.map(makeStore)
    }

    var isEmpty: Bool { hubs.isEmpty }

    func store(for id: UUID) -> HubStore? { stores.first { $0.key == id } }

    /// Adds the hub, or — same base URL — gives the one already paired the
    /// new token and keeps its place, name, and sessions.
    @discardableResult
    func pair(base: URL, token: String) -> HubStore {
        if let i = hubs.firstIndex(where: { $0.baseURL == base }) {
            hubs[i].token = token
            Keychain.set(token, for: Self.tokenAccount(hubs[i].id))
            save()
            let store = stores[i]
            store.connection.token = token
            store.repaired()
            if active { store.setActive(true) }
            return store
        }
        let hub = Hub(id: UUID(), name: HubConnection.displayAddress(base), baseURL: base, token: token)
        Keychain.set(token, for: Self.tokenAccount(hub.id))
        hubs.append(hub)
        save()
        let store = makeStore(hub)
        stores.append(store)
        if active { store.setActive(true) }
        return store
    }

    /// Forgets the hub on this phone (its Keychain item too).
    func unpair(_ id: UUID) {
        guard let i = hubs.firstIndex(where: { $0.id == id }) else { return }
        stores[i].setActive(false)
        Keychain.delete(Self.tokenAccount(id))
        hubs.remove(at: i)
        stores.remove(at: i)
        save()
    }

    // MARK: Swarm

    /// Which paired hub shows each swarm host. With a swarm every paired
    /// member reports every machine; each appears once: under the hub
    /// where it is local, else the first (in pairing order) that reaches
    /// it, else the first that lists it. Only hubs answering right now
    /// count — an offline one has its own note, and its machine shows
    /// under whichever other hub still reaches it.
    var hostOwners: [String: UUID] {
        var owners: [String: (store: UUID, rank: Int)] = [:]
        for store in stores where !store.offline && !store.unpaired {
            for host in store.state?.hosts ?? [] where !host.id.isEmpty {
                let rank = host.isLocal ? 0 : host.isReachable ? 1 : 2
                if let current = owners[host.id], current.rank <= rank { continue }
                owners[host.id] = (store.key, rank)
            }
        }
        return owners.mapValues(\.store)
    }

    /// The hosts this hub's part of the directory shows, after the dedupe.
    /// An older hub (no `hosts`) is one host, itself, always shown.
    func hosts(of store: HubStore) -> [HubHost] {
        guard let state = store.state else { return [] }
        guard state.hosts?.isEmpty == false else { return [state.localHost] }
        let owners = hostOwners
        return state.allHosts.filter { owners[$0.id] == nil || owners[$0.id] == store.key }
    }

    /// The hub and host that have a session with this hub id (searching
    /// every host of every hub, the deduped placement first).
    func route(for hubId: String) -> SessionRoute? {
        for store in stores {
            for host in hosts(of: store) where host.session(hubId: hubId) != nil {
                return SessionRoute(hub: store.key, host: host.target, id: hubId)
            }
        }
        for store in stores {
            for host in store.state?.allHosts ?? [] where host.session(hubId: hubId) != nil {
                return SessionRoute(hub: store.key, host: host.target, id: hubId)
            }
        }
        return nil
    }

    /// Hubs `joining` can be added to the swarm with: paired, swarm-capable,
    /// and not already in one with it.
    func swarmCandidates(for joining: HubStore) -> [HubStore] {
        guard joining.state?.swarmCapable == true else { return [] }
        let own = joining.state?.localHostId
        return stores.filter { store in
            guard store.key != joining.key, !store.unpaired, let state = store.state, state.swarmCapable else { return false }
            guard let own else { return true }
            return !(state.hosts ?? []).contains { $0.id == own }
        }
    }

    /// Enrols `joining` in `member`'s swarm: the member's secret and peer
    /// list (`POST /api/swarm`) handed to the joining hub
    /// (`POST /api/swarm/join`). The secret lives only in this call.
    func addToSwarm(_ joining: HubStore, with member: HubStore) async throws {
        let invite: (secret: String, peers: Any)
        do {
            invite = try await member.connection.swarmInvite()
        } catch {
            throw HubError.refused("\(member.hostName): \(error.localizedDescription)")
        }
        do {
            try await joining.connection.swarmJoin(secret: invite.secret, peers: invite.peers)
        } catch {
            throw HubError.refused("\(joining.hostName): \(error.localizedDescription)")
        }
        async let a: Void = joining.refresh()
        async let b: Void = member.refresh()
        _ = await (a, b)
    }

    func setActive(_ active: Bool) {
        self.active = active
        for store in stores { store.setActive(active) }
    }

    /// A session screen is on top: every hub polls slower (the screen's
    /// header is all that needs a directory then).
    func setViewingSession(_ viewing: Bool) {
        for store in stores { store.viewingSession = viewing }
    }

    private func makeStore(_ hub: Hub) -> HubStore {
        HubStore(key: hub.id, name: hub.name, connection: HubConnection(baseURL: hub.baseURL, token: hub.token)) { [weak self] host in
            self?.rename(hub.id, to: host)
        }
    }

    private func rename(_ id: UUID, to name: String) {
        guard let i = hubs.firstIndex(where: { $0.id == id }), hubs[i].name != name else { return }
        hubs[i].name = name
        save()
    }

    private func save() {
        if let data = try? JSONEncoder().encode(hubs) { UserDefaults.standard.set(data, forKey: Self.listKey) }
    }

    /// A phone paired before the hubs list keeps its pairing.
    private func migrateLegacy() {
        let defaults = UserDefaults.standard
        guard let text = defaults.string(forKey: Self.legacyURLKey) else { return }
        // No token read back: either the old build was unpaired (nothing to
        // lose) or the keychain couldn't be read just now (locked before the
        // first unlock). Either way leave the old keys and try next launch.
        guard let token = Keychain.string(for: Self.legacyTokenAccount) else { return }
        if let base = URL(string: text), !hubs.contains(where: { $0.baseURL == base }) {
            let name = defaults.string(forKey: Self.legacyHostKey) ?? HubConnection.displayAddress(base)
            let hub = Hub(id: UUID(), name: name, baseURL: base, token: token)
            // Keep the old item unless the new one is safely written.
            guard Keychain.set(token, for: Self.tokenAccount(hub.id)) else { return }
            hubs.append(hub)
            save()
        }
        defaults.removeObject(forKey: Self.legacyURLKey)
        defaults.removeObject(forKey: Self.legacyHostKey)
        Keychain.delete(Self.legacyTokenAccount)
    }
}
