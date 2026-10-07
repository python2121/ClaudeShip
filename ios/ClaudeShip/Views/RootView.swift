import SwiftUI

struct RootView: View {
    @Environment(HubRegistry.self) private var registry
    /// The session open above the directory (one at a time).
    @State private var path: [SessionRoute] = []
    /// From the `-session` launch argument.
    nonisolated(unsafe) static var initialSession: String?

    /// Nothing paired, or the only hub stopped accepting this phone (its
    /// secret was rotated): the pairing screen, as with a single hub always.
    /// With several hubs a refused one says so in its own section instead.
    private var needsPairing: Bool {
        registry.isEmpty || (registry.stores.count == 1 && registry.stores[0].unpaired)
    }

    var body: some View {
        Group {
            if needsPairing {
                PairView()
            } else {
                NavigationStack(path: $path) {
                    DirectoryView()
                        .environment(\.navigate) { route in path = [route] }
                        .navigationDestination(for: SessionRoute.self) { route in
                            if let store = registry.store(for: route.hub) {
                                SessionScreen(hubId: route.id).environment(store)
                            } else {
                                ContentUnavailableView("Hub not paired", systemImage: "link.badge.plus",
                                                       description: Text("This phone is no longer paired with that hub."))
                            }
                        }
                }
            }
        }
        // A hub that refuses us (or is unpaired) takes its open session with it.
        .onChange(of: registry.stores.filter { !$0.unpaired }.map(\.key)) { _, live in
            path.removeAll { !live.contains($0.hub) }
        }
        .task {
            if let id = RootView.initialSession {
                RootView.initialSession = nil
                if let route = await locate(id) { path = [route] }
            }
        }
        // claudeship://pair?link=<pairing link>  — pairing from anywhere a
        // link can be tapped; claudeship://session?id=<hub id> opens one.
        .onOpenURL { url in
            guard url.scheme == "claudeship", let components = URLComponents(url: url, resolvingAgainstBaseURL: false) else { return }
            let query = Dictionary((components.queryItems ?? []).map { ($0.name, $0.value ?? "") }, uniquingKeysWith: { first, _ in first })
            switch url.host {
            case "pair":
                if let link = query["link"], let (base, token) = HubConnection.parse(link: link) {
                    let store = registry.pair(base: base, token: token)
                    Task { await store.refresh(); store.setActive(true) }
                }
            case "session":
                if let id = query["id"] { Task { if let route = await locate(id) { path = [route] } } }
            default:
                break
            }
        }
    }

    /// The hub that has a session with this hub id, else the first hub.
    private func locate(_ hubId: String) async -> SessionRoute? {
        for store in registry.stores where store.state == nil {
            await store.refresh()
            // A poll already in flight (the one starting the app) makes
            // refresh return at once; wait for its answer, briefly.
            var waited = 0
            while store.state == nil, store.error == nil, !store.unpaired, waited < 90 {
                try? await Task.sleep(for: .milliseconds(100))
                waited += 1
            }
        }
        let store = registry.stores.first { $0.session(hubId: hubId) != nil } ?? registry.stores.first
        return store.map { SessionRoute(hub: $0.key, id: hubId) }
    }
}
