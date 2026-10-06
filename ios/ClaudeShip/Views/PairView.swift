import SwiftUI

/// First run, and whenever the hub stops recognising this phone: get the
/// pairing link in, by camera or by hand.
struct PairView: View {
    @Environment(HubConnection.self) private var connection
    @Environment(HubStore.self) private var store
    @State private var link = ""
    @State private var scanning = false
    @State private var busy = false
    @State private var problem: String?

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Pair with the hub on your Mac")
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
                    } else if store.unpaired, connection.baseURL != nil {
                        Text("The hub at \(connection.displayAddress) no longer accepts this phone's pairing (the secret was rotated). Pair again.")
                    } else {
                        Text("The phone must be on the same Tailscale network as the Mac. Anyone with the link can use the hub, so treat it like a password.")
                    }
                }
            }
            .navigationTitle("Claude Ship")
            .sheet(isPresented: $scanning) {
                QRScannerSheet { code in
                    scanning = false
                    Task { await pair(code) }
                }
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
            try await connection.probe(base: base, token: token)
            connection.pair(base: base, token: token)
            problem = nil
            link = ""
            await store.refresh()
            store.setActive(true)
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
        case (.unpaired, .unpaired), (.unreachable, .unreachable): return true
        case (.refused(let x), .refused(let y)): return x == y
        default: return false
        }
    }
}
