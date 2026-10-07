import SwiftUI

/// First run, whenever the only hub stops recognising this phone, and (as
/// a sheet) pairing another hub: get the pairing link in, by camera or by
/// hand. A link for a hub already paired replaces that hub's token.
struct PairView: View {
    @Environment(HubRegistry.self) private var registry
    @Environment(\.dismiss) private var dismiss
    /// Presented over the directory or settings, not as the whole app.
    var asSheet = false
    /// The hub whose pairing was refused, when that's why we're here.
    var refused: HubStore?
    @State private var link = ""
    @State private var scanning = false
    @State private var busy = false
    @State private var problem: String?
    /// A new hub just paired that could join another's swarm: the sheet's
    /// second step.
    @State private var joining: HubStore?

    var body: some View {
        NavigationStack {
            if let joining {
                SwarmView(joining: joining) { dismiss() }
                    .navigationTitle("Swarm")
                    .navigationBarTitleDisplayMode(.inline)
            } else {
                form
            }
        }
    }

    private var form: some View {
        Form {
            Section {
                VStack(alignment: .leading, spacing: 8) {
                    Text(asSheet && refused == nil ? "Pair another hub" : "Pair with the hub on your Mac")
                        .font(.title3.weight(.semibold))
                    Text("In a terminal on the Mac, run **claudeship hub link**. It prints a link and shows a QR code. Scan the code, or paste the link below.")
                        .foregroundStyle(.secondary)
                }
                .padding(.vertical, 4)
            }
            Section {
                Button {
                    scanning = true
                } label: {
                    Label("Scan the QR code", systemImage: "qrcode.viewfinder")
                }
                .disabled(busy)
            }
            Section {
                TextField("http://100.…:7433/auth?k=…", text: $link)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .keyboardType(.URL)
                    .submitLabel(.go)
                    .onSubmit { Task { await pair(link) } }
                Button {
                    Task { await pair(link) }
                } label: {
                    if busy { ProgressView() } else { Text("Pair") }
                }
                .disabled(busy || link.trimmingCharacters(in: .whitespaces).isEmpty)
            } header: {
                Text("Or paste the link")
            } footer: {
                if let problem {
                    Text(problem).foregroundStyle(Palette.orange)
                } else if let stale = refused ?? (asSheet ? nil : registry.stores.first(where: \.unpaired)) {
                    Text("The hub at \(stale.hostName) (\(stale.connection.displayAddress)) no longer accepts this phone's pairing (the secret was rotated). Pair again.")
                } else {
                    Text("The phone must be on the same Tailscale network as the Mac. Anyone with the link can use the hub, so treat it like a password.")
                }
            }
        }
        .navigationTitle(asSheet ? "Pair" : "ClaudeShip")
        .navigationBarTitleDisplayMode(asSheet ? .inline : .automatic)
        .toolbar {
            if asSheet {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            }
        }
        .sheet(isPresented: $scanning) {
            QRScannerSheet { code in
                scanning = false
                Task { await pair(code) }
            }
        }
    }

    private func pair(_ text: String) async {
        guard let (base, token) = HubConnection.parse(link: text) else {
            problem = "That doesn't look like a pairing link."
            return
        }
        busy = true
        defer { busy = false }
        // Checked before it is kept: pairing first would switch to the
        // directory at once and lose this screen's chance to say what went wrong.
        do {
            try await HubConnection(baseURL: base, token: token).probe(base: base, token: token)
            let isNew = !registry.hubs.contains { $0.baseURL == base }
            let store = registry.pair(base: base, token: token)
            problem = nil
            link = ""
            await store.refresh()
            store.setActive(true)
            guard asSheet else { return }
            // A second (or later) hub: offer to put it in a swarm with one
            // already paired. A hub paired again keeps whatever swarm it has.
            if isNew, !registry.swarmCandidates(for: store).isEmpty {
                joining = store
            } else {
                dismiss()
            }
        } catch {
            problem = (error as? HubError) == .unpaired
                ? "The hub didn't accept that link. Run claudeship hub link again for a current one."
                : error.localizedDescription
        }
    }
}

extension HubError: Equatable {
    static func == (a: HubError, b: HubError) -> Bool {
        switch (a, b) {
        case (.unpaired, .unpaired): return true
        case (.unreachable(let x), .unreachable(let y)): return x == y
        case (.refused(let x), .refused(let y)): return x == y
        case (.proxy(let x), .proxy(let y)): return x == y
        default: return false
        }
    }
}
