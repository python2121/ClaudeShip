import SwiftUI

struct RootView: View {
    @Environment(HubConnection.self) private var connection
    @Environment(HubStore.self) private var store
    /// Hub ids of the sessions open above the directory (one at a time).
    @State private var path: [String] = []
    /// From the `-session` launch argument.
    nonisolated(unsafe) static var initialSession: String?

    var body: some View {
        Group {
            if connection.isPaired && !store.unpaired {
                NavigationStack(path: $path) {
                    DirectoryView()
                        .environment(\.navigate) { hubId in path = [hubId] }
                        .navigationDestination(for: String.self) { hubId in
                            SessionScreen(hubId: hubId)
                        }
                }
            } else {
                PairView()
            }
        }
        .onChange(of: store.unpaired) { _, unpaired in
            if unpaired { path = [] }
        }
        .task {
            if let id = RootView.initialSession {
                RootView.initialSession = nil
                await store.refresh()
                path = [id]
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
                    connection.pair(base: base, token: token)
                    Task { await store.refresh(); store.setActive(true) }
                }
            case "session":
                if let id = query["id"] { path = [id] }
            default:
                break
            }
        }
    }
}
