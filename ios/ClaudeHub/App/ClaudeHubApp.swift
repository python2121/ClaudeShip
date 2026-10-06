import SwiftUI

@main
struct ClaudeHubApp: App {
    @State private var connection = HubConnection()
    @State private var store: HubStore
    @Environment(\.scenePhase) private var scenePhase

    init() {
        let connection = HubConnection()
        // Launch arguments for a simulator driven from a script (there is no
        // way to tap "Open" on the custom-URL prompt from outside):
        //   -pair <pairing link>   pair before the first screen appears
        //   -session <hub id>      open straight onto that session
        let arguments = CommandLine.arguments
        if let i = arguments.firstIndex(of: "-pair"), i + 1 < arguments.count,
           let (base, token) = HubConnection.parse(link: arguments[i + 1]) {
            connection.pair(base: base, token: token)
        }
        if let i = arguments.firstIndex(of: "-session"), i + 1 < arguments.count {
            RootView.initialSession = arguments[i + 1]
        }
        _connection = State(initialValue: connection)
        _store = State(initialValue: HubStore(connection: connection))
    }

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(connection)
                .environment(store)
                .onChange(of: scenePhase, initial: true) { _, phase in
                    store.setActive(phase == .active)
                }
        }
    }
}
