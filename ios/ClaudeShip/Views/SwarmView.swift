import SwiftUI

/// "Add <hub> to the swarm with <member>": the phone as introducer. The
/// member hands over its swarm secret and peer list (`POST /api/swarm`),
/// the joining hub takes them (`POST /api/swarm/join`). Shown right after
/// pairing a second (or later) hub, and from Settings on any hub. The
/// secret passes through memory only: never shown, logged, or stored.
struct SwarmView: View {
    @Environment(HubRegistry.self) private var registry
    let joining: HubStore
    /// After pairing, "Not now" (the hub stays standalone); from Settings, "Cancel".
    var dismissLabel = "Not now"
    /// Simulator scripting (`-swarm-confirm`): press Add on appear.
    var autoConfirm = false
    let done: () -> Void
    @State private var memberKey: UUID?
    @State private var busy = false
    @State private var problem: String?

    private var candidates: [HubStore] { registry.swarmCandidates(for: joining) }
    private var member: HubStore? { candidates.first { $0.key == memberKey } ?? candidates.first }

    var body: some View {
        Form {
            Section {
                VStack(alignment: .leading, spacing: 8) {
                    Text(member.map { "Add \(joining.hostName) to the swarm with \($0.hostName)" } ?? "Add \(joining.hostName) to a swarm")
                        .font(.title3.weight(.semibold))
                    Text("Hubs in one swarm show each other's machines: from any of them, this phone sees every session on all of them and can open, start, and approve there. The hubs talk over Tailscale only.")
                        .foregroundStyle(.secondary)
                }
                .padding(.vertical, 4)
            }
            if candidates.count > 1 {
                Section("Join the swarm of") {
                    Picker("Swarm member", selection: Binding(get: { member?.key }, set: { memberKey = $0 })) {
                        ForEach(candidates) { store in
                            Text(store.hostName).tag(Optional(store.key))
                        }
                    }
                    .pickerStyle(.inline)
                    .labelsHidden()
                }
            }
            Section {
                Button {
                    Task { await add() }
                } label: {
                    if busy { ProgressView() } else { Text("Add to the swarm").fontWeight(.semibold) }
                }
                .disabled(busy || member == nil)
                Button(dismissLabel, action: done).disabled(busy)
            } footer: {
                if let problem {
                    Text(problem).foregroundStyle(Palette.orange)
                } else if member == nil {
                    Text("No other paired hub can form a swarm with \(joining.hostName): each must run a build that knows about swarms, and it may already be in one with them.")
                }
            }
        }
        .task {
            if autoConfirm, member != nil { await add() }
        }
    }

    private func add() async {
        guard let member, !busy else { return }
        busy = true
        defer { busy = false }
        do {
            try await registry.addToSwarm(joining, with: member)
            problem = nil
            done()
        } catch {
            problem = error.localizedDescription
        }
    }
}
