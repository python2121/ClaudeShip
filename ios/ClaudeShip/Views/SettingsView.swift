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

    var body: some View {
        Section {
            ForEach(PermissionMode.known.filter { store.state?.permissionModes.contains($0.mode) ?? true }, id: \.mode) { entry in
                Button {
                    Task { await store.setDefaultMode(entry.mode) }
                } label: {
                    HStack(alignment: .top, spacing: 12) {
                        Image(systemName: entry.mode == store.state?.defaultPermissionMode ? "largecircle.fill.circle" : "circle")
                            .foregroundStyle(entry.mode == store.state?.defaultPermissionMode ? Color.accentColor : .secondary)
                            .padding(.top, 2)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(entry.name).foregroundStyle(.primary)
                            Text(entry.about).font(.footnote).foregroundStyle(.secondary)
                        }
                    }
                }
            }
        } header: {
            Text("New sessions start in")
        } footer: {
            Text("Used by New session and Resume. The arrow beside New session picks a different mode for one launch.")
        }
        Section("Hub") {
            LabeledContent("Mac", value: store.state?.host ?? store.hostName)
            LabeledContent("Address", value: store.connection.displayAddress)
            LabeledContent("Projects", value: store.state?.rootDisplay ?? "—")
            if let pairAnother {
                Button { pairAnother() } label: { Label("Pair another hub", systemImage: "plus") }
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
