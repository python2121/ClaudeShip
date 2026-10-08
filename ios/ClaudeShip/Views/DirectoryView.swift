import SwiftUI

/// The project directory: what's running first, then every project
/// folder on the Mac, in the hub's order.
/// `SIMCTL_CHILD_CH_OFF=glyph,launch,header,tally,search,recent,running` turns
/// pieces off at launch, for bisecting layout trouble in the simulator.
enum DebugOff {
    static let set = Set((ProcessInfo.processInfo.environment["CH_OFF"] ?? "").split(separator: ",").map(String.init))
    static func contains(_ name: String) -> Bool { set.contains(name) }
}

struct DirectoryView: View {
    @Environment(HubRegistry.self) private var registry
    @Environment(\.navigate) private var navigate
    @State private var filter = ""
    @State private var settings = false
    /// Expanded projects, as `HostSections.key` (hub id + host + path: two
    /// machines can both have ~/code/x).
    @State private var expanded: Set<String> = []
    /// The quick "+" is asking which machine.
    @State private var choosingHost = false
    /// A hub that refused this phone, being paired again.
    @State private var repairing: HubStore?
    /// Which computer's idle projects the Projects block lists
    /// (`ProjectsTab.id`); remembered across launches, as the web page
    /// remembers its tab. Empty or stale: the first tab.
    @AppStorage("projectsTab") private var projectsTab = ""
    /// `-expand <name>` at launch (simulator scripting).
    nonisolated(unsafe) static var initialExpanded: String?
    /// `-settings` at launch (simulator scripting): open the sheet.
    nonisolated(unsafe) static var initialSettings = false

    var body: some View {
        List {
            // What's running, per hub and then per machine; with only one
            // hub, no title row — the directory looks exactly as it did
            // before there could be two.
            if !DebugOff.contains("running") {
                ForEach(registry.stores) { store in
                    HubSections(filter: filter, expanded: $expanded, titled: registry.stores.count > 1) { repairing = store }
                        .environment(store)
                }
            }
            // Then the idle projects of one machine at a time, as on the web
            // page: a tab per computer rather than a whole list per member.
            projects
        }
        .listStyle(.insetGrouped)
        .onChange(of: registry.stores.map { $0.state?.allHosts.map(\.projectList.count) ?? [] }, initial: true) { _, _ in
            guard let name = Self.initialExpanded else { return }
            for store in registry.stores {
                for host in registry.hosts(of: store) {
                    if let project = host.projectList.first(where: { $0.name == name }) {
                        expanded.insert(HostSections.key(store, host.target, project.path))
                        projectsTab = ProjectsTab(store: store, host: host).id
                        Self.initialExpanded = nil
                        return
                    }
                }
            }
        }
        .navigationTitle("ClaudeShip")
        .navigationBarTitleDisplayMode(.inline)
        .searchable(text: $filter, prompt: "Filter projects")
        .refreshable {
            await withTaskGroup(of: Void.self) { group in
                for store in registry.stores { group.addTask { await store.refresh() } }
            }
        }
        .toolbar {
            // Plain, not in a glass capsule: it's a readout, not a control.
            if #available(iOS 26, *) {
                ToolbarItem(placement: .topBarLeading) { tally }.sharedBackgroundVisibility(.hidden)
            } else {
                ToolbarItem(placement: .topBarLeading) { tally }
            }
            // The "+" and the gear sit on the bar itself, no shared capsule.
            if #available(iOS 26, *) {
                ToolbarItem(placement: .topBarTrailing) { quickButton }.sharedBackgroundVisibility(.hidden)
                ToolbarItem(placement: .topBarTrailing) { settingsButton }.sharedBackgroundVisibility(.hidden)
            } else {
                ToolbarItem(placement: .topBarTrailing) { quickButton }
                ToolbarItem(placement: .topBarTrailing) { settingsButton }
            }
        }
        .confirmationDialog("New session in the home folder on", isPresented: $choosingHost, titleVisibility: .visible) {
            ForEach(quickTargets) { target in
                Button(target.host.target == nil ? target.store.hostName : target.host.displayName) { quick(target) }
            }
        }
        .onAppear {
            if Self.initialSettings { Self.initialSettings = false; settings = true }
        }
        .sheet(isPresented: $settings) { SettingsView() }
        .sheet(item: $repairing) { store in PairView(asSheet: true, refused: store) }
    }

    /// The quick "+": a session in the Mac's home directory, in auto mode —
    /// the New session button without the words or the mode menu, for work
    /// that isn't about any one project. With two machines or more in view
    /// it asks which.
    @ViewBuilder private var quickButton: some View {
        if !quickTargets.isEmpty {
            Button {
                if quickTargets.count > 1 { choosingHost = true } else if let target = quickTargets.first { quick(target) }
            } label: {
                Image(systemName: "plus")
                    .font(.subheadline.weight(.semibold))
                    .frame(width: 30, height: 30)
                    .background(Color.accentColor, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                    .foregroundStyle(.white)
            }
            .buttonStyle(.plain)
            .disabled(quickTargets.contains { $0.store.launching })
            .accessibilityLabel("New session in your home folder")
        }
    }

    private var settingsButton: some View {
        Button { settings = true } label: { Image(systemName: "gearshape") }
    }

    /// A machine the quick "+" can start a session on, through a hub.
    private struct QuickTarget: Identifiable {
        let store: HubStore
        let host: HubHost
        var id: String { "\(store.key.uuidString)/\(host.id)" }
    }

    /// Machines the quick "+" can start a session on right now: every
    /// reachable host in view (each once, after the swarm dedupe).
    private var quickTargets: [QuickTarget] {
        registry.stores.filter { !$0.offline && !$0.unpaired }.flatMap { store in
            registry.hosts(of: store).filter { $0.isReachable && $0.home != nil }.map { QuickTarget(store: store, host: $0) }
        }
    }

    private func quick(_ target: QuickTarget) {
        guard let home = target.host.home else { return }
        let store = target.store, host = target.host.target
        Task { if let id = await store.launch(path: home, mode: "auto", host: host) { navigate(store.route(id, host: host)) } }
    }

    /// A computer whose idle projects the Projects block can list: the hub
    /// that shows it (after the swarm dedupe) and the host.
    private struct ProjectsTab: Identifiable {
        let store: HubStore
        let host: HubHost
        /// `HostSections.key` without the path.
        var id: String { "\(store.key.uuidString)/\(host.id)" }
    }

    /// Every machine in view, in store order and then the hub's host
    /// order (its own machine first): one tab each. Only hubs answering
    /// count — an offline hub has its note, and its machine shows under
    /// another member that still reaches it.
    private var projectsTabs: [ProjectsTab] {
        registry.stores.filter { !$0.offline && !$0.unpaired && $0.state != nil }.flatMap { store in
            registry.hosts(of: store).map { ProjectsTab(store: store, host: $0) }
        }
    }

    /// The remembered tab, or the first when it has gone.
    private var chosenTab: ProjectsTab? {
        let tabs = projectsTabs
        return tabs.first { $0.id == projectsTab } ?? tabs.first
    }

    @ViewBuilder private var projects: some View {
        let tabs = projectsTabs
        if let chosen = chosenTab {
            if tabs.count > 1 {
                Section {
                    tabPicker(tabs, chosen: chosen)
                }
                .listRowBackground(Color.clear)
                .listRowInsets(EdgeInsets(top: 8, leading: 4, bottom: 0, trailing: 4))
            }
            HostSections(host: chosen.host, homeProtocol: chosen.store.state?.protocol, filter: filter,
                         expanded: $expanded, headed: tabs.count > 1, part: .projects)
                .environment(chosen.store)
                .environment(\.hostScope, HostScope(target: chosen.host.target, defaultMode: chosen.host.defaultPermissionMode))
        }
    }

    /// Segmented for a few computers, a menu beyond that (segments get
    /// cramped). Each label is the computer's name and how many idle
    /// projects it has (after the filter); an unreachable one is dimmed.
    @ViewBuilder private func tabPicker(_ tabs: [ProjectsTab], chosen: ProjectsTab) -> some View {
        let selection = Binding<String>(get: { chosen.id }, set: { projectsTab = $0 })
        let picker = Picker("Projects on", selection: selection) {
            ForEach(tabs) { tab in
                Text("\(tab.host.displayName) · \(HostSections.idleCount(of: tab.host, filter: filter))")
                    .foregroundStyle(tab.host.isReachable ? .primary : .secondary)
                    .tag(tab.id)
            }
        }
        if tabs.count <= 3 {
            picker.pickerStyle(.segmented).labelsHidden()
        } else {
            picker.pickerStyle(.menu)
        }
    }

    private var tally: some View {
        let all = registry.stores.flatMap { store in
            store.offline || store.unpaired ? [] : registry.hosts(of: store).filter(\.isReachable).flatMap(\.allSessions)
        }
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
}

/// One hub's part of the directory: a run of sections per machine it
/// shows (one, unheaded, for a hub alone; one per swarm host it owns after
/// the dedupe, headed, otherwise). Offline, refused, and notices are the
/// hub's; reachability and version trouble are each host's.
private struct HubSections: View {
    @Environment(HubRegistry.self) private var registry
    @Environment(HubStore.self) private var store
    let filter: String
    @Binding var expanded: Set<String>
    /// Several hubs: head the run with the hub's name.
    let titled: Bool
    /// Pair this (refused) hub again.
    let repair: () -> Void

    var body: some View {
        if titled {
            Section {
                HubTitle(store: store)
            }
            .listRowBackground(Color.clear)
            .listRowInsets(EdgeInsets(top: 6, leading: 4, bottom: 0, trailing: 4))
        }
        if let notice = store.notice {
            Section { NoticeBar(text: notice) { store.notice = nil } }
                .listRowInsets(EdgeInsets(top: 4, leading: 16, bottom: 4, trailing: 16))
                .listRowBackground(Color.clear)
        }
        if store.unpaired {
            // Only with several hubs: a single refused hub gets the pairing screen.
            Section {
                VStack(alignment: .leading, spacing: 8) {
                    Text("\(store.hostName) no longer accepts this phone's pairing (the secret was rotated).")
                        .font(.subheadline).foregroundStyle(.secondary)
                    Button("Pair again", action: repair).font(.subheadline.weight(.semibold))
                        .buttonStyle(.borderless)
                }
                .padding(.vertical, 4)
            }
        } else if store.offline {
            Section {
                OfflineNote(host: store.hostName)
            }
            .listRowBackground(Color.clear)
        } else if let error = store.error, store.state == nil {
            Section { Text(error).foregroundStyle(.secondary) }
        }
        if let state = store.state, !store.offline, !store.unpaired {
            let hosts = registry.hosts(of: store)
            // Headed when it's more than this hub's own machine.
            let headed = hosts.count > 1 || hosts.contains { $0.target != nil }
            ForEach(hosts) { host in
                HostSections(host: host, homeProtocol: state.protocol, filter: filter, expanded: $expanded, headed: headed, part: .running)
                    .environment(\.hostScope, HostScope(target: host.target, defaultMode: host.defaultPermissionMode))
            }
        } else if store.error == nil && !store.unpaired {
            Section { ProgressView().frame(maxWidth: .infinity) }
        }
    }
}

/// One machine's part of the directory, in two parts the directory places
/// separately: what's running on it (`.running`: its name when several
/// machines are in view, reachability, version notices, the running
/// project cards, "Running elsewhere"), and its idle project folders
/// (`.projects`), in the hub's order.
private struct HostSections: View {
    enum Part { case running, projects }

    @Environment(HubStore.self) private var store
    let host: HubHost
    /// The answering hub's protocol: it relays only to a peer on the same.
    let homeProtocol: Int?
    let filter: String
    @Binding var expanded: Set<String>
    /// Several machines in view: head the run with this one's name.
    let headed: Bool
    let part: Part

    static func key(_ store: HubStore, _ host: String?, _ path: String) -> String {
        "\(store.key.uuidString)/\(host ?? "")/\(path)"
    }

    /// How many of the machine's idle projects the filter leaves: the
    /// Projects tab's count.
    static func idleCount(of host: HubHost, filter: String) -> Int {
        host.projectList.filter { $0.sessions.isEmpty && matches($0, filter: filter) }.count
    }

    var body: some View {
        switch part {
        case .running: running
        case .projects: projects
        }
    }

    @ViewBuilder private var running: some View {
        if headed {
            Section {
                HostTitle(host: host)
            }
            .listRowBackground(Color.clear)
            .listRowInsets(EdgeInsets(top: 2, leading: 4, bottom: 0, trailing: 4))
        }
        if !host.isReachable {
            Section {
                Label {
                    Text(host.lastSeen.map { "Unreachable since \(Ago.since(ms: $0, now: store.now))" } ?? "Unreachable")
                        .font(.footnote)
                } icon: {
                    Image(systemName: "moon.zzz")
                }
                .foregroundStyle(.secondary)
            }
        }
        if host.protocol != HubState.protocolVersion {
            Section {
                Text("The hub on \(headed ? host.displayName : "the Mac") is a different build than this app. Restart it when its sessions can end: claudeship hub stop, then claudeship hub start.")
                    .font(.footnote).foregroundStyle(Palette.orange)
            }
        } else if host.target != nil, let homeProtocol, homeProtocol != host.protocol {
            Section {
                Text("\(host.displayName) runs a different build than \(store.hostName), which can't relay to it until both run the same one.")
                    .font(.footnote).foregroundStyle(Palette.orange)
            }
        }
        Group {
            let running = host.projectList.filter { !$0.sessions.isEmpty && matches($0) }
            // Only when nothing at all runs on the machine — a session
            // outside the projects folder counts, as on the web page.
            if running.isEmpty && host.elsewhereList.isEmpty && filter.isEmpty {
                Section("Running") {
                    Text(host.isReachable
                         ? "Nothing is running. Start a session below, or run claudeship in a terminal on \(headed ? host.displayName : "the Mac")."
                         : "Nothing was running when it was last seen.")
                        .foregroundStyle(.secondary)
                }
            }
            ForEach(running) { project in
                Section {
                    ForEach(project.sessions) { session in SessionRow(session: session, project: project) }
                    // A running project's earlier conversations sit
                    // behind a toggle, as on the web page's card.
                    if !project.recent.isEmpty {
                        EarlierRow(expanded: isExpanded(project)) { toggle(project) }
                        if isExpanded(project) { recentRows(project) }
                    }
                } header: {
                    if DebugOff.contains("header") { Text(project.name) } else { ProjectHeader(project: project, running: true) }
                }
            }
            // Sessions running outside the projects folder: shown while
            // they run, never tracked otherwise.
            if !host.elsewhereList.isEmpty && filter.isEmpty {
                Section("Running elsewhere") {
                    ForEach(host.elsewhereList) { session in SessionRow(session: session, project: nil) }
                }
            }
        }
        // An unreachable machine's last state, for reference: nothing on it
        // can be started, opened, or answered until it is back.
        .disabled(!host.isReachable)
    }

    @ViewBuilder private var projects: some View {
        let rest = host.projectList.filter { $0.sessions.isEmpty && matches($0) }
        if !rest.isEmpty {
            Section {
                ForEach(rest) { project in
                    ProjectRow(project: project, expanded: isExpanded(project)) { toggle(project) }
                    if isExpanded(project) {
                        if !DebugOff.contains("launch") { launchRow(project) }
                        recentRows(project)
                    }
                }
            } header: {
                HStack {
                    Text("Projects")
                    Spacer()
                    Text(host.rootDisplay ?? "").font(.caption.monospaced()).textCase(nil)
                }
            }
            .disabled(!host.isReachable)
        }
    }

    private func matches(_ project: HubProject) -> Bool { Self.matches(project, filter: filter) }

    static func matches(_ project: HubProject, filter: String) -> Bool {
        let needle = filter.trimmingCharacters(in: .whitespaces).lowercased()
        if needle.isEmpty { return true }
        let haystack = ([project.name, project.branch ?? ""] + project.sessions.compactMap(\.title)
            + project.recent.map(\.title)).joined(separator: "\n").lowercased()
        return haystack.contains(needle)
    }

    private func isExpanded(_ project: HubProject) -> Bool { expanded.contains(Self.key(store, host.target, project.path)) }

    private func toggle(_ project: HubProject) {
        let key = Self.key(store, host.target, project.path)
        if expanded.contains(key) { expanded.remove(key) } else { expanded.insert(key) }
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

/// A swarm machine's name over its run of sections; the one the hub runs
/// on is marked.
private struct HostTitle: View {
    let host: HubHost

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Image(systemName: host.isReachable ? "server.rack" : "moon.zzz").font(.footnote).foregroundStyle(.secondary)
            Text(host.displayName).font(.headline).lineLimit(1)
            if host.isLocal {
                Text("hub").font(.caption2.weight(.semibold))
                    .padding(.horizontal, 6).padding(.vertical, 1)
                    .background(Color.accentColor.opacity(0.18), in: Capsule())
                    .accessibilityLabel("the hub's own machine")
            }
            Spacer()
            if !host.isReachable {
                Text("Unreachable").font(.caption).foregroundStyle(.secondary)
            }
        }
        .accessibilityAddTraits(.isHeader)
    }
}

/// Which machine a row belongs to: what its actions name as `host`, and
/// that machine's default permission mode.
struct HostScope {
    /// A swarm peer's id; nil for the answering hub's own machine.
    var target: String?
    var defaultMode: String?
}

private struct HostScopeKey: EnvironmentKey {
    static let defaultValue = HostScope()
}

extension EnvironmentValues {
    var hostScope: HostScope {
        get { self[HostScopeKey.self] }
        set { self[HostScopeKey.self] = newValue }
    }
}

/// The hub's name over its run of sections, when there are several hubs.
private struct HubTitle: View {
    let store: HubStore

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: "desktopcomputer").font(.subheadline).foregroundStyle(.secondary)
            Text(store.hostName).font(.title3.weight(.bold)).lineLimit(1)
            Spacer()
            if store.offline {
                Text("Offline").font(.caption).foregroundStyle(.secondary)
            }
        }
        .accessibilityAddTraits(.isHeader)
    }
}

/// A running project's name over its card. The "+" at its end starts
/// another session in the project (always in auto mode), in place of the
/// launch row an idle project shows when expanded.
private struct ProjectHeader: View {
    @Environment(HubStore.self) private var store
    @Environment(\.navigate) private var navigate
    @Environment(\.hostScope) private var scope
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
            if running && !DebugOff.contains("launch") {
                Button {
                    Task { if let id = await store.launch(path: project.path, mode: "auto", host: scope.target) { navigate(store.route(id, host: scope.target)) } }
                } label: {
                    Image(systemName: "plus")
                        .font(.caption.weight(.bold))
                        .frame(width: 22, height: 22)
                        .background(Color.accentColor, in: RoundedRectangle(cornerRadius: 6, style: .continuous))
                        .foregroundStyle(.white)
                }
                .buttonStyle(.plain)
                .disabled(store.launching)
                .accessibilityLabel("New session in \(project.name)")
            }
        }
    }
}

/// A session: tap to open it when the hub owns it; otherwise just status.
/// With a permission prompt pending, Approve / Deny / ⋯ take the status
/// line's place and the prompt's summary is the caption (tap: the detail);
/// the title and a trailing Open button still attach to the session.
struct SessionRow: View {
    @Environment(HubStore.self) private var store
    @Environment(\.navigate) private var navigate
    @Environment(\.hostScope) private var scope
    let session: HubSession
    let project: HubProject?
    @State private var confirmEnd = false
    @State private var detail: HubApproval?

    var body: some View {
        // Only a hub-owned session can be ended from here.
        if session.attachable, let hubId = session.hubId {
            row(hubId: hubId)
                .contextMenu {
                    Button(role: .destructive) { confirmEnd = true } label: { Label("End session", systemImage: "xmark.circle") }
                }
                .confirmationDialog("End this session?", isPresented: $confirmEnd, titleVisibility: .visible) {
                    Button("End session", role: .destructive) {
                        Task { _ = await store.end(hubId: hubId, host: scope.target) }
                    }
                } message: {
                    Text("Claude exits; the conversation can be resumed later.")
                }
        } else {
            row(hubId: nil)
        }
    }

    @ViewBuilder
    private func row(hubId: String?) -> some View {
        if let approval = session.pendingApprovals.first {
            // Not a NavigationLink: its own buttons must take their taps.
            approvalContent(approval, hubId: hubId)
                .sheet(item: $detail) { approval in ApprovalDetail(approval: approval, session: session).environment(store).environment(\.hostScope, scope) }
        } else if let hubId {
            NavigationLink(value: store.route(hubId, host: scope.target)) { content }
        } else {
            content
        }
    }

    private var autoApproving: Bool { session.autoApprove?.isActive(now: store.now(for: scope.target)) == true }

    private var glyph: some View {
        Group {
            if DebugOff.contains("glyph") {
                Circle().fill(StatusText.color(session.status)).frame(width: 12, height: 12)
            } else {
                StatusGlyph(status: session.status)
            }
        }
        .padding(.top, 5)
    }

    private var title: some View {
        HStack(alignment: .firstTextBaseline, spacing: 5) {
            Text(session.title ?? (session.status == "starting" ? "Starting…" : "New conversation"))
                .font(.body.weight(session.title == nil ? .regular : .medium))
                .foregroundStyle(session.title == nil ? .secondary : .primary)
                .lineLimit(2)
            if autoApproving {
                // A standing "approve all" rule, as on the Mac overlay.
                Image(systemName: "bolt.fill").font(.caption).foregroundStyle(.yellow)
                    .accessibilityLabel("Approving automatically")
            }
        }
    }

    private var content: some View {
        HStack(alignment: .top, spacing: 10) {
            glyph
            VStack(alignment: .leading, spacing: 3) {
                title
                Text(meta).font(.footnote).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 6)
        }
        .padding(.vertical, 2)
    }

    private func approvalContent(_ approval: HubApproval, hubId: String?) -> some View {
        let busy = store.answering.contains(approval.id)
        let more = session.pendingApprovals.count - 1
        return HStack(alignment: .top, spacing: 10) {
            glyph
            VStack(alignment: .leading, spacing: 6) {
                if let hubId {
                    // The name area opens the session, as a normal row does.
                    Button { navigate(store.route(hubId, host: scope.target)) } label: {
                        title.frame(maxWidth: .infinity, alignment: .leading).contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityHint("Opens the session's terminal")
                } else {
                    title
                }
                HStack(spacing: 8) {
                    Button("Approve") { Task { await store.answer(approval, allow: true, host: scope.target) } }
                        .buttonStyle(.borderedProminent).tint(Palette.green).disabled(busy)
                    Button("Deny") { Task { await store.answer(approval, allow: false, host: scope.target) } }
                        .buttonStyle(.bordered).disabled(busy)
                    if session.sessionId != nil {
                        Menu {
                            Button("Approve all for 5 minutes") { Task { await store.autoApprove(session, rule: "5m", host: scope.target) } }
                            Button("Approve all for this session") { Task { await store.autoApprove(session, rule: "session", host: scope.target) } }
                            if autoApproving {
                                Button("Stop approving", role: .destructive) { Task { await store.autoApprove(session, rule: "off", host: scope.target) } }
                            }
                        } label: {
                            Image(systemName: "ellipsis")
                                .frame(width: 30, height: 26)
                                .background(Color.secondary.opacity(0.15), in: Capsule())
                        }
                        .accessibilityLabel("More approval options")
                        .disabled(busy)
                    }
                    if more > 0 {
                        Text("+\(more) more").font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                    if let hubId {
                        // The attach a normal row gets from its whole-row link,
                        // here as a visible control beside the answers.
                        Spacer(minLength: 0)
                        Button { navigate(store.route(hubId, host: scope.target)) } label: {
                            HStack(spacing: 3) {
                                Text("Open")
                                Image(systemName: "chevron.right").font(.caption2.weight(.bold))
                            }
                        }
                        .buttonStyle(.bordered)
                        .accessibilityLabel("Open session")
                        .accessibilityHint("Attaches to the session's terminal")
                    }
                }
                .controlSize(.small)
                .font(.subheadline.weight(.semibold))
                Button { detail = approval } label: {
                    Text(approval.caption.isEmpty ? "Permission requested" : approval.caption)
                        .font(.footnote.monospaced()).foregroundStyle(.secondary)
                        .lineLimit(2).multilineTextAlignment(.leading)
                }
                .buttonStyle(.plain)
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 2)
    }

    private var meta: String {
        var parts = [StatusText.label(session.status)]
        if session.status == "waiting", let what = session.waitingFor { parts[0] += " — \(what)" }
        if let since = session.since { parts.append(Ago.age(ms: since, now: store.now(for: scope.target))) }
        // Not the hub's to attach: where it lives instead, in the caption.
        if !session.attachable { parts.append(session.background ? "Background" : "Terminal only") }
        if project == nil { parts.append(session.cwd) } else if let sub = session.sub { parts.append(sub) }
        if let branch = session.branch, branch != project?.branch { parts.append(branch) }
        if session.viewers > 0 { parts.append("\(session.viewers) attached") }
        return parts.joined(separator: " · ")
    }
}

/// A permission prompt in full: what the tool wants to do, and the answer.
private struct ApprovalDetail: View {
    @Environment(HubStore.self) private var store
    @Environment(\.hostScope) private var scope
    @Environment(\.dismiss) private var dismiss
    let approval: HubApproval
    let session: HubSession

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    if let summary = approval.summary, !summary.isEmpty {
                        Text(summary).font(.body.weight(.medium))
                    }
                    Text(approval.detail ?? approval.summary ?? "")
                        .font(.footnote.monospaced())
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(12)
                        .background(Color.secondary.opacity(0.1), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                    Text(session.title ?? session.cwd).font(.footnote).foregroundStyle(.secondary)
                }
                .padding()
            }
            .safeAreaInset(edge: .bottom) {
                HStack(spacing: 12) {
                    Button { answer(false) } label: { Text("Deny").frame(maxWidth: .infinity) }
                        .buttonStyle(.bordered)
                    Button { answer(true) } label: { Text("Approve").frame(maxWidth: .infinity) }
                        .buttonStyle(.borderedProminent).tint(Palette.green)
                }
                .controlSize(.large)
                .font(.body.weight(.semibold))
                .padding()
                .background(.bar)
                .disabled(store.answering.contains(approval.id))
            }
            .navigationTitle(approval.tool ?? "Permission")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
        }
        .presentationDetents([.medium, .large])
    }

    private func answer(_ allow: Bool) {
        dismiss()
        Task { await store.answer(approval, allow: allow, host: scope.target) }
    }
}

extension HubStore {
    /// Where a session of this hub (on that swarm host; nil: its own) opens.
    func route(_ hubId: String, host: String? = nil) -> SessionRoute { SessionRoute(hub: key, host: host, id: hubId) }
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
    @Environment(\.hostScope) private var scope
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
                    Text(Ago.ago(ms: at, now: store.now(for: scope.target))).font(.caption).foregroundStyle(.tertiary)
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

/// "New session" in an idle project's expanded row. Always auto mode: the
/// phone starts every session in auto (no mode menu), like its quick "+".
private struct LaunchRow: View {
    @Environment(HubStore.self) private var store
    @Environment(\.navigate) private var navigate
    @Environment(\.hostScope) private var scope
    let project: HubProject

    var body: some View {
        HStack(spacing: 8) {
            Button {
                Task { if let id = await store.launch(path: project.path, mode: "auto", host: scope.target) { navigate(store.route(id, host: scope.target)) } }
            } label: {
                Label("New session", systemImage: "plus")
                    .font(.subheadline.weight(.semibold))
                    .padding(.horizontal, 12).padding(.vertical, 7)
                    .background(Color.accentColor, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                    .foregroundStyle(.white)
            }
            .buttonStyle(.plain)
            .disabled(store.launching)
            Spacer()
        }
        .padding(.vertical, 2)
    }
}

private struct ResumeRow: View {
    @Environment(HubStore.self) private var store
    @Environment(\.navigate) private var navigate
    @Environment(\.hostScope) private var scope
    let project: HubProject
    let conversation: HubConversation

    var body: some View {
        Button {
            Task { if let id = await store.launch(path: project.path, mode: "auto", resume: conversation.sessionId, host: scope.target) { navigate(store.route(id, host: scope.target)) } }
        } label: {
            HStack {
                Text(conversation.title).font(.subheadline).foregroundStyle(.primary).lineLimit(1)
                Spacer()
                Text(Ago.ago(ms: conversation.at, now: store.now(for: scope.target))).font(.caption).foregroundStyle(.tertiary)
                Text("Resume").font(.caption.weight(.semibold)).foregroundStyle(Color.accentColor)
            }
        }
        .buttonStyle(.plain)
        .disabled(store.launching)
    }
}

/// How a row asks the directory's NavigationStack to open a session.
struct NavigateKey: EnvironmentKey {
    static let defaultValue: (SessionRoute) -> Void = { _ in }
}

extension EnvironmentValues {
    var navigate: (SessionRoute) -> Void {
        get { self[NavigateKey.self] }
        set { self[NavigateKey.self] = newValue }
    }
}
