import SwiftUI

struct SettingsView: View {
    @Environment(HubStore.self) private var store
    @Environment(HubConnection.self) private var connection
    @Environment(\.dismiss) private var dismiss
    @State private var confirmUnpair = false

    var body: some View {
        NavigationStack {
            Form {
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
                    LabeledContent("Mac", value: store.state?.host ?? "—")
                    LabeledContent("Address", value: connection.displayAddress)
                    LabeledContent("Projects", value: store.state?.rootDisplay ?? "—")
                }
                Section {
                    Button("Unpair this phone", role: .destructive) { confirmUnpair = true }
                } footer: {
                    Text("Forgets the pairing on this phone only. To unpair every browser and phone at once, run claudeship hub unlink on the Mac.")
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .confirmationDialog("Unpair this phone?", isPresented: $confirmUnpair, titleVisibility: .visible) {
                Button("Unpair", role: .destructive) {
                    store.unpair()
                    dismiss()
                }
            }
        }
    }
}
