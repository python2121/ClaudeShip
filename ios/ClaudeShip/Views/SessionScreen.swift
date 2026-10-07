import SwiftUI

/// A session, full screen: the terminal, with who it is and how it's
/// doing in the bar above.
struct SessionScreen: View {
    @Environment(HubRegistry.self) private var registry
    /// The hub this session lives on (RootView puts it in the environment).
    @Environment(HubStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase
    let hubId: String
    @State private var terminal: TerminalSession?
    @State private var confirmEnd = false
    @State private var ending = false

    private var info: (session: HubSession, project: HubProject?)? { store.session(hubId: hubId) }

    var body: some View {
        ZStack(alignment: .top) {
            Palette.terminalBackground.ignoresSafeArea()
            if let terminal {
                TerminalContainer(session: terminal)
                    .padding(.leading, 6)
                if terminal.reconnecting {
                    pill("Reconnecting…", action: nil)
                } else if let note = terminal.note {
                    pill(note, action: ("Retry", { terminal.retryNow() }))
                } else if let foreign = terminal.foreignSize {
                    pill("Mirroring another screen (\(foreign.cols)×\(foreign.rows))", action: ("Fit here", { terminal.claim() }))
                }
            }
        }
        .onAppear { registry.setViewingSession(true) }
        .onDisappear { registry.setViewingSession(false) }
        .task {
            if terminal == nil {
                let session = TerminalSession(hubId: hubId, connection: store.connection)
                // The store outlives every session screen; no cycle to break.
                session.hubReachable = { store.error == nil }
                terminal = session
            }
        }
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbarBackground(Color(red: 0.106, green: 0.102, blue: 0.094), for: .navigationBar)
        .toolbarColorScheme(.dark, for: .navigationBar)
        .toolbar {
            ToolbarItem(placement: .principal) { heading }
            ToolbarItem(placement: .topBarTrailing) {
                Button(role: .destructive) { confirmEnd = true } label: { Text("End") }
                    .disabled(ending || terminal?.ended != nil)
            }
        }
        .navigationBarTitleDisplayMode(.inline)
        .confirmationDialog("End this session?", isPresented: $confirmEnd, titleVisibility: .visible) {
            Button("End session", role: .destructive) {
                ending = true
                Task { if !(await store.end(hubId: hubId)) { ending = false } }
            }
        } message: {
            Text("Claude exits. The conversation can be resumed later.")
        }
        .onChange(of: terminal?.ended?.heading) { _, _ in
            // The session is over, however it ended: back to the directory
            // with a word about what happened.
            guard let ended = terminal?.ended else { return }
            store.show("\(ended.heading). \(ended.detail)")
            dismiss()
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { terminal?.revive() }
        }
        .onDisappear { terminal?.close() }
    }

    private var heading: some View {
        HStack(spacing: 8) {
            StatusGlyph(status: info?.session.status ?? "starting", size: 11)
            VStack(spacing: 1) {
                Text(info?.project?.name ?? info?.session.cwd ?? "Session")
                    .font(.subheadline.weight(.semibold)).lineLimit(1)
                if let session = info?.session {
                    Text([StatusText.label(session.status), session.since.map { Ago.age(ms: $0, now: store.now) },
                          registry.stores.count > 1 ? store.hostName : nil]
                        .compactMap { $0 }.joined(separator: " · "))
                        .font(.caption2).foregroundStyle(StatusText.color(session.status)).lineLimit(1)
                }
            }
        }
    }

    private func pill(_ text: String, action: (String, () -> Void)?) -> some View {
        HStack(spacing: 10) {
            Text(text).font(.footnote)
            if let action {
                Button(action.0, action: action.1)
                    .font(.footnote.weight(.semibold))
                    .buttonStyle(.borderedProminent)
                    .buttonBorderShape(.capsule)
                    .controlSize(.small)
            }
        }
        .padding(.leading, 14).padding(.trailing, action == nil ? 14 : 6).padding(.vertical, 6)
        .background(Color(white: 0.17), in: Capsule())
        .foregroundStyle(.white)
        .padding(.top, 10)
        .transition(.opacity)
    }
}
