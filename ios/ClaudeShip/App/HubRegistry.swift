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

/// Where a session screen points: which hub, and the session's hub id there.
struct SessionRoute: Hashable {
    var hub: UUID
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
