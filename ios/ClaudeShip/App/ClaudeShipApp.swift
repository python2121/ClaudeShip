import SwiftUI

@main
struct ClaudeShipApp: App {
    @State private var registry: HubRegistry
    @Environment(\.scenePhase) private var scenePhase

    init() {
        let registry = HubRegistry()
        // Launch arguments for a simulator driven from a script (there is no
        // way to tap "Open" on the custom-URL prompt from outside):
        //   -pair <pairing link>   pair (add the hub) before the first screen; repeatable
        //   -session <hub id>      open straight onto that session
        //   -expand <project name> start with that project's row expanded
        //   -swarm                 offer "add to swarm" for the last hub paired
        //   -swarm-confirm         …and press Add
        let arguments = CommandLine.arguments
        if let i = arguments.firstIndex(of: "-expand"), i + 1 < arguments.count {
            DirectoryView.initialExpanded = arguments[i + 1]
        }
        for (i, argument) in arguments.enumerated() where argument == "-pair" && i + 1 < arguments.count {
            if let (base, token) = HubConnection.parse(link: arguments[i + 1]) { registry.pair(base: base, token: token) }
        }
        if let i = arguments.firstIndex(of: "-session"), i + 1 < arguments.count {
            RootView.initialSession = arguments[i + 1]
        }
        if arguments.contains("-swarm") || arguments.contains("-swarm-confirm") {
            RootView.initialSwarm = arguments.contains("-swarm-confirm") ? .confirm : .offer
        }
        _registry = State(initialValue: registry)
    }

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(registry)
                .onChange(of: scenePhase, initial: true) { _, phase in
                    registry.setActive(phase == .active)
                }
        }
    }
}
