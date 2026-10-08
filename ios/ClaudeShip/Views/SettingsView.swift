import SwiftUI

/// One hub paired: its settings, as they always were, plus "Pair another
/// hub". Several: the list of hubs, each opening onto its own settings.
struct SettingsView: View {
    @Environment(HubRegistry.self) private var registry
    @Environment(\.dismiss) private var dismiss
    @State private var pairing = false
    @State private var confirmUnpair: HubStore?

    var body: some View {
        NavigationStack {
            Form {
                if registry.stores.count == 1, let store = registry.stores.first {
                    HubSettingsSections(store: store, pairAnother: { pairing = true })
                } else {
                    Section {
                        ForEach(registry.stores) { store in
                            NavigationLink {
                                Form { HubSettingsSections(store: store, pairAnother: nil) }
                                    .navigationTitle(store.hostName)
                                    .navigationBarTitleDisplayMode(.inline)
                            } label: {
                                HubLabel(store: store)
                            }
                            .swipeActions {
                                Button("Unpair", role: .destructive) { confirmUnpair = store }
                            }
                        }
                        Button { pairing = true } label: { Label("Pair another hub", systemImage: "plus") }
                    } header: {
                        Text("Hubs")
                    } footer: {
                        Text("Each hub keeps its own default mode. Swipe a hub to unpair it from this phone.")
                    }
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .sheet(isPresented: $pairing) { PairView(asSheet: true) }
            .confirmationDialog("Unpair \(confirmUnpair?.hostName ?? "this hub")?", isPresented: Binding(
                get: { confirmUnpair != nil }, set: { if !$0 { confirmUnpair = nil } }
            ), titleVisibility: .visible) {
                Button("Unpair", role: .destructive) {
                    if let store = confirmUnpair { registry.unpair(store.key) }
                    confirmUnpair = nil
                    if registry.isEmpty { dismiss() }
                }
            }
        }
    }
}

private struct HubLabel: View {
    let store: HubStore

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(store.hostName)
                if store.unpaired {
                    Text("Not accepted").font(.caption2).foregroundStyle(Palette.orange)
                } else if store.offline {
                    Text("Offline").font(.caption2).foregroundStyle(.secondary)
                }
            }
            Text(store.connection.displayAddress).font(.caption.monospaced()).foregroundStyle(.secondary)
        }
    }
}

/// One hub's settings: the default permission mode, what it is, unpair.
private struct HubSettingsSections: View {
    @Environment(HubRegistry.self) private var registry
    @Environment(\.dismiss) private var dismiss
    let store: HubStore
    let pairAnother: (() -> Void)?
    @State private var confirmUnpair = false
    @State private var swarming = false

    var body: some View {
        // No "start in" choice: the phone launches every session in auto,
        // and the mode is changed inside the session (the key bar's mode key).
        Section("Hub") {
            LabeledContent("Mac", value: store.state?.host ?? store.hostName)
            LabeledContent("Address", value: store.connection.displayAddress)
            LabeledContent("Projects", value: store.state?.rootDisplay ?? "—")
            if !registry.swarmCandidates(for: store).isEmpty {
                Button { swarming = true } label: { Label("Add to swarm with…", systemImage: "point.3.connected.trianglepath.dotted") }
            }
            if let pairAnother {
                Button { pairAnother() } label: { Label("Pair another hub", systemImage: "plus") }
            }
        }
        .sheet(isPresented: $swarming) {
            NavigationStack {
                SwarmView(joining: store, dismissLabel: "Cancel") { swarming = false }
                    .navigationTitle("Swarm")
                    .navigationBarTitleDisplayMode(.inline)
            }
        }
        // Remote command execution on that machine. Read-only here on
        // purpose: the hub takes the switch only from the machine itself
        // (a browser there at localhost, or claudeship hub jobs), never
        // from a phone, so nothing that merely holds the pairing can open it.
        if let jobs = store.state?.jobs {
            Section {
                LabeledContent("Jobs", value: jobs ? "On" : "Off")
            } header: {
                Text("Jobs on \(store.state?.host ?? store.hostName)")
            } footer: {
                Text((jobs
                      ? "Paired devices, swarm members, and Claude sessions can run commands and headless Claude on that machine, as you."
                      : "Nothing can run commands on that machine through the hub.")
                     + " Switched only at the machine itself: its web page opened there at localhost, or claudeship hub jobs on|off in a terminal there.")
            }
        }
        Section {
            Button("Unpair this phone", role: .destructive) { confirmUnpair = true }
        } footer: {
            Text("Forgets the pairing on this phone only. To unpair every browser and phone at once, run claudeship hub unlink on the Mac.")
        }
        .confirmationDialog("Unpair this phone from \(store.hostName)?", isPresented: $confirmUnpair, titleVisibility: .visible) {
            Button("Unpair", role: .destructive) {
                registry.unpair(store.key)
                dismiss()
            }
        }
    }
}
