import SwiftUI

/// The project directory: what's running first, then every project
/// folder on the Mac, in the hub's order.
/// `SIMCTL_CHILD_CH_OFF=glyph,launch,header,tally,search,recent` turns
/// pieces off at launch, for bisecting layout trouble in the simulator.
enum DebugOff {
    static let set = Set((ProcessInfo.processInfo.environment["CH_OFF"] ?? "").split(separator: ",").map(String.init))
    static func contains(_ name: String) -> Bool { set.contains(name) }
}

struct DirectoryView: View {
    @Environment(HubStore.self) private var store
    @Environment(\.navigate) private var navigate
    @State private var filter = ""
    @State private var settings = false
    @State private var expanded: Set<String> = []
    /// `-expand <name>` at launch (simulator scripting).
    nonisolated(unsafe) static var initialExpanded: String?

    var body: some View {
        @Bindable var store = store
        List {
            if let notice = store.notice {
                Section { NoticeBar(text: notice) { store.notice = nil } }
                    .listRowInsets(EdgeInsets(top: 4, leading: 16, bottom: 4, trailing: 16))
                    .listRowBackground(Color.clear)
            }
            if store.offline {
                Section {
                    OfflineNote(host: store.hostName)
                }
                .listRowBackground(Color.clear)
            } else if let error = store.error, store.state == nil {
                Section { Text(error).foregroundStyle(.secondary) }
            }
            if let state = store.state, !store.offline {
                if state.protocol != HubState.protocolVersion {
                    Section {
                        Text("The hub on the Mac is a different build than this app. Restart it when its sessions can end: claudeship hub stop, then claudeship hub start.")
                            .font(.footnote).foregroundStyle(Palette.orange)
                    }
                }
                let visible = state.projects.filter(matches)
                let running = visible.filter { !$0.sessions.isEmpty }
                let rest = visible.filter { $0.sessions.isEmpty }
                if running.isEmpty && filter.isEmpty {
                    Section("Running") {
                        Text("Nothing is running. Start a session below, or run claudeship in a terminal on the Mac.")
                            .foregroundStyle(.secondary)
                    }
                }
                ForEach(running) { project in
                    Section {
                        ForEach(project.sessions) { session in SessionRow(session: session, project: project) }
                        if !DebugOff.contains("launch") { launchRow(project) }
                        // A running project's earlier conversations sit
                        // behind a toggle, as on the web page's card.
                        if !project.recent.isEmpty {
                            EarlierRow(expanded: expanded.contains(project.path)) { toggle(project.path) }
                            if expanded.contains(project.path) { recentRows(project) }
                        }
                    } header: {
                        if DebugOff.contains("header") { Text(project.name) } else { ProjectHeader(project: project, running: true) }
                    }
                }
                // Sessions running outside the projects folder: shown while
                // they run, never tracked otherwise.
                if !state.elsewhere.isEmpty && filter.isEmpty {
                    Section("Running elsewhere") {
                        ForEach(state.elsewhere) { session in SessionRow(session: session, project: nil) }
                    }
                }
                if !rest.isEmpty {
                    Section {
                        ForEach(rest) { project in
                            ProjectRow(project: project, expanded: expanded.contains(project.path)) {
                                toggle(project.path)
                            }
                            if expanded.contains(project.path) {
                                if !DebugOff.contains("launch") { launchRow(project) }
                                recentRows(project)
                            }
                        }
                    } header: {
                        HStack {
                            Text("Projects")
                            Spacer()
                            Text(state.rootDisplay).font(.caption.monospaced()).textCase(nil)
                        }
                    }
                }
            } else if store.error == nil {
                Section { ProgressView().frame(maxWidth: .infinity) }
            }
        }
        .listStyle(.insetGrouped)
        .onAppear {
            if let name = Self.initialExpanded, let project = store.state?.projects.first(where: { $0.name == name }) {
                expanded.insert(project.path)
            }
        }
        .onChange(of: store.state?.projects.map(\.path)) { _, paths in
            if let name = Self.initialExpanded, let project = store.state?.projects.first(where: { $0.name == name }) {
                expanded.insert(project.path)
                Self.initialExpanded = nil
            }
        }
        .navigationTitle("ClaudeShip")
        .navigationBarTitleDisplayMode(.inline)
        .searchable(text: $filter, prompt: "Filter projects")
        .refreshable { await store.refresh() }
        .toolbar {
            // Plain, not in a glass capsule: it's a readout, not a control.
            if #available(iOS 26, *) {
                ToolbarItem(placement: .topBarLeading) { tally }.sharedBackgroundVisibility(.hidden)
            } else {
                ToolbarItem(placement: .topBarLeading) { tally }
            }
            ToolbarItem(placement: .topBarTrailing) {
                // The quick "+": a session in the Mac's home directory, in
                // auto mode — the New session button without the words or
                // the mode menu, for work that isn't about any one project.
                if let home = store.state?.home, !store.offline {
                    Button {
                        Task { if let id = await store.launch(path: home, mode: "auto") { navigate(id) } }
                    } label: {
                        Image(systemName: "plus")
                            .font(.subheadline.weight(.semibold))
                            .frame(width: 30, height: 30)
                            .background(Color.accentColor, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                            .foregroundStyle(.white)
                    }
                    .buttonStyle(.plain)
                    .disabled(store.launching)
                    .accessibilityLabel("New session in your home folder")
                }
            }
            ToolbarItem(placement: .topBarTrailing) {
                Button { settings = true } label: { Image(systemName: "gearshape") }
            }
        }
        .sheet(isPresented: $settings) { SettingsView() }
    }

    private var tally: some View {
        let all = store.offline ? [] : store.state?.allSessions ?? []
        let waiting = all.filter { $0.status == "waiting" }.count
        let busy = all.filter { $0.status == "busy" }.count
        return HStack(spacing: 8) {
            if waiting > 0 {
                Label("\(waiting)", systemImage: "circle.fill").foregroundStyle(Palette.orange)
            }
            if busy > 0 {
                Label("\(busy)", systemImage: "circle.lefthalf.filled").foregroundStyle(Palette.green)
            }
        }
        .font(.footnote.weight(.semibold))
        .labelStyle(.titleAndIcon)
    }

    private func matches(_ project: HubProject) -> Bool {
        let needle = filter.trimmingCharacters(in: .whitespaces).lowercased()
        if needle.isEmpty { return true }
        let haystack = ([project.name, project.branch ?? ""] + project.sessions.compactMap(\.title)
            + project.recent.map(\.title)).joined(separator: "\n").lowercased()
        return haystack.contains(needle)
    }

    private func toggle(_ path: String) {
        if expanded.contains(path) { expanded.remove(path) } else { expanded.insert(path) }
    }

    @ViewBuilder
    private func launchRow(_ project: HubProject) -> some View {
        LaunchRow(project: project)
    }

    @ViewBuilder
    private func recentRows(_ project: HubProject) -> some View {
        if project.recent.isEmpty {
            Text("No earlier conversations to resume.").font(.subheadline).foregroundStyle(.tertiary)
        } else {
            ForEach(project.recent) { conversation in
                ResumeRow(project: project, conversation: conversation)
            }
        }
    }
}

private struct ProjectHeader: View {
    let project: HubProject
    let running: Bool

    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            Text(project.name).font(.headline).textCase(nil).foregroundStyle(.primary)
            if project.sessions.count > 1 {
                Text("\(project.sessions.count) running").font(.caption2.weight(.semibold)).textCase(nil)
                    .padding(.horizontal, 6).padding(.vertical, 2)
                    .background(Color.secondary.opacity(0.15), in: Capsule())
            }
            Spacer()
            if let branch = project.branch {
                Label(branch, systemImage: "arrow.triangle.branch").font(.caption.monospaced()).textCase(nil)
                    .foregroundStyle(.secondary).lineLimit(1)
            }
        }
    }
}

/// A session: tap to open it when the hub owns it; otherwise just status.
struct SessionRow: View {
    @Environment(HubStore.self) private var store
    let session: HubSession
    let project: HubProject?
    @State private var confirmEnd = false

    var body: some View {
        // Only a hub-owned session can be ended from here.
        if session.attachable, let hubId = session.hubId {
            NavigationLink(value: hubId) { content }
                .contextMenu {
                    Button(role: .destructive) { confirmEnd = true } label: { Label("End session", systemImage: "xmark.circle") }
                }
                .confirmationDialog("End this session?", isPresented: $confirmEnd, titleVisibility: .visible) {
                    Button("End session", role: .destructive) {
                        Task { _ = await store.end(hubId: hubId) }
                    }
                } message: {
                    Text("Claude exits; the conversation can be resumed later.")
                }
        } else {
            content
        }
    }

    private var content: some View {
        HStack(alignment: .top, spacing: 10) {
            if DebugOff.contains("glyph") {
                Circle().fill(StatusText.color(session.status)).frame(width: 12, height: 12).padding(.top, 5)
            } else {
                StatusGlyph(status: session.status).padding(.top, 5)
            }
            VStack(alignment: .leading, spacing: 3) {
                Text(session.title ?? (session.status == "starting" ? "Starting…" : "New conversation"))
                    .font(.body.weight(session.title == nil ? .regular : .medium))
                    .foregroundStyle(session.title == nil ? .secondary : .primary)
                    .lineLimit(2)
                Text(meta).font(.footnote).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 6)
            if !session.attachable {
                Text(session.background ? "Background" : "Terminal only")
                    .font(.caption2).foregroundStyle(.tertiary)
                    .padding(.horizontal, 6).padding(.vertical, 2)
                    .overlay(RoundedRectangle(cornerRadius: 5).stroke(Color.secondary.opacity(0.3)))
            }
        }
        .padding(.vertical, 2)
    }

    private var meta: String {
        var parts = [StatusText.label(session.status)]
        if session.status == "waiting", let what = session.waitingFor { parts[0] += " — \(what)" }
        if let since = session.since { parts.append(Ago.age(ms: since, now: store.now)) }
        if project == nil { parts.append(session.cwd) } else if let sub = session.sub { parts.append(sub) }
        if let branch = session.branch, branch != project?.branch { parts.append(branch) }
        if session.viewers > 0 { parts.append("\(session.viewers) attached") }
        return parts.joined(separator: " · ")
    }
}

/// The Mac isn't answering. Plain words, no alarm: a sleeping or
/// switched-off Mac is the usual reason, and the polling keeps trying.
private struct OfflineNote: View {
    let host: String

    var body: some View {
        VStack(spacing: 8) {
            Image(systemName: "moon.zzz").font(.title2).foregroundStyle(.secondary)
            Text("Can't connect to \(host)").font(.headline)
            Text("The Mac may be asleep or off, or Tailscale is off on one side. This page will fill in again when it answers.")
                .font(.footnote).foregroundStyle(.secondary).multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 28)
    }
}

/// A project with nothing running: name, branch, when. Its conversations
/// show only when expanded, as the resume list.
private struct ProjectRow: View {
    @Environment(HubStore.self) private var store
    let project: HubProject
    let expanded: Bool
    let toggle: () -> Void

    var body: some View {
        Button(action: toggle) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                HStack(spacing: 8) {
                    Text(project.name).font(.body.weight(.semibold)).foregroundStyle(.primary)
                    if let branch = project.branch {
                        Text(branch).font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
                Spacer()
                if let at = project.lastActivity {
                    Text(Ago.ago(ms: at, now: store.now)).font(.caption).foregroundStyle(.tertiary)
                }
                Image(systemName: expanded ? "chevron.down" : "chevron.right").font(.caption.weight(.semibold)).foregroundStyle(.tertiary)
            }
        }
        .buttonStyle(.plain)
    }
}

/// "Earlier" — shows or hides a running project's resume list.
private struct EarlierRow: View {
    let expanded: Bool
    let toggle: () -> Void

    var body: some View {
        Button(action: toggle) {
            HStack {
                Text("Earlier conversations").font(.subheadline).foregroundStyle(.secondary)
                Spacer()
                Image(systemName: expanded ? "chevron.down" : "chevron.right").font(.caption.weight(.semibold)).foregroundStyle(.tertiary)
            }
        }
        .buttonStyle(.plain)
    }
}

/// "New session" with the mode menu beside it.
private struct LaunchRow: View {
    @Environment(HubStore.self) private var store
    @Environment(\.navigate) private var navigate
    let project: HubProject

    var body: some View {
        HStack(spacing: 8) {
            Button {
                Task { if let id = await store.launch(path: project.path) { navigate(id) } }
            } label: {
                Label("New session", systemImage: "plus")
                    .font(.subheadline.weight(.semibold))
                    .padding(.horizontal, 12).padding(.vertical, 7)
                    .background(Color.accentColor, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                    .foregroundStyle(.white)
            }
            .buttonStyle(.plain)
            .disabled(store.launching)
            Menu {
                Section("Start in") {
                    ForEach(PermissionMode.known.filter { store.state?.permissionModes.contains($0.mode) ?? true }, id: \.mode) { entry in
                        Button {
                            Task { if let id = await store.launch(path: project.path, mode: entry.mode) { navigate(id) } }
                        } label: {
                            if entry.mode == store.state?.defaultPermissionMode {
                                Label(entry.name, systemImage: "checkmark")
                            } else {
                                Text(entry.name)
                            }
                        }
                    }
                }
            } label: {
                Image(systemName: "chevron.down")
                    .font(.subheadline.weight(.semibold))
                    .padding(.horizontal, 10).padding(.vertical, 9)
                    .background(Color.accentColor.opacity(0.18), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            }
            Spacer()
            if let mode = store.state?.defaultPermissionMode {
                Text(PermissionMode.name(mode)).font(.caption).foregroundStyle(.tertiary)
            }
        }
        .padding(.vertical, 2)
    }
}

private struct ResumeRow: View {
    @Environment(HubStore.self) private var store
    @Environment(\.navigate) private var navigate
    let project: HubProject
    let conversation: HubConversation

    var body: some View {
        Button {
            Task { if let id = await store.launch(path: project.path, resume: conversation.sessionId) { navigate(id) } }
        } label: {
            HStack {
                Text(conversation.title).font(.subheadline).foregroundStyle(.primary).lineLimit(1)
                Spacer()
                Text(Ago.ago(ms: conversation.at, now: store.now)).font(.caption).foregroundStyle(.tertiary)
                Text("Resume").font(.caption.weight(.semibold)).foregroundStyle(Color.accentColor)
            }
        }
        .buttonStyle(.plain)
        .disabled(store.launching)
    }
}

/// How a row asks the directory's NavigationStack to open a session.
struct NavigateKey: EnvironmentKey {
    static let defaultValue: (String) -> Void = { _ in }
}

extension EnvironmentValues {
    var navigate: (String) -> Void {
        get { self[NavigateKey.self] }
        set { self[NavigateKey.self] = newValue }
    }
}
