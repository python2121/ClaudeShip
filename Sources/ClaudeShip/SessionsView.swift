import Combine
import SwiftUI

struct SessionsView: View {
    @ObservedObject var store: SessionStore
    /// Row click: bring the session's terminal to the front. Injected by
    /// AppDelegate (which also closes the panel); no-op in previews/tests.
    var onFocusSession: (ClaudeSession) -> Void = { _ in }
    var onEndSession: (ClaudeSession) -> Void = { _ in }
    /// The footer's "+": a new session in the home directory.
    var onNewSession: () -> Void = {}
    @ViewState private var now: Date = Date()
    @ViewState private var hoveredPid: pid_t? = nil

    private let tick = Timer.publish(every: 1, on: .main, in: .common).autoconnect()

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            header

            Divider()

            if store.sessions.isEmpty {
                emptyState
            } else {
                sessionRows
            }

            Divider()

            footer
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(width: 392, alignment: .leading)
        .onReceive(tick) { if store.panelVisible { now = $0 } }
        .onChange(of: store.panelVisible) { visible in if visible { now = Date() } }
    }

    // MARK: Sections

    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            Text("Claude Sessions")
                .font(.headline)
            Spacer()
            Text(summary)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private var summary: String {
        if store.sessions.isEmpty { return "none running" }
        var parts: [String] = []
        if store.busyCount > 0 { parts.append("\(store.busyCount) working") }
        if store.shellCount > 0 { parts.append("\(store.shellCount) in shell") }
        if store.waitingCount > 0 { parts.append("\(store.waitingCount) waiting") }
        if store.idleCount > 0 { parts.append("\(store.idleCount) idle") }
        return parts.joined(separator: " · ")
    }

    private var emptyState: some View {
        Text("No Claude sessions running.")
            .font(.callout)
            .foregroundStyle(.secondary)
            .padding(.vertical, 4)
    }

    /// Two levels: host app (Ghostty, VS Code, …) then, for VS Code, the
    /// window; sessions sit inside. The app header only appears once there's
    /// more than one app — a single-host setup stays flat. A window header
    /// carries the path when every session in it shares one, and sessions in
    /// the same window aren't separated by lines; separate windows are.
    private var sessionRows: some View {
        let hosts = Self.grouped(store.sessions)
        // A background group is always headed, even alone: the header is what
        // tells the user there's no terminal to look for.
        let headed = hosts.count > 1 || hosts.contains { $0.key == Self.backgroundKey || $0.key == Self.virtualKey }
        return VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(hosts.enumerated()), id: \.element.key) { hostIndex, host in
                if hostIndex > 0 { Divider() }
                VStack(alignment: .leading, spacing: 6) {
                    if headed { hostHeader(host) }
                    ForEach(Array(host.windows.enumerated()), id: \.element.key) { windowIndex, window in
                        if windowIndex > 0 { Divider() }
                        VStack(alignment: .leading, spacing: 6) {
                            if window.label != nil { windowHeader(window) }
                            ForEach(Array(window.sessions.enumerated()), id: \.element.id) { index, session in
                                // No window label means each session is its
                                // own window (Ghostty tabs, an unreported VS
                                // Code window): keep the line between them.
                                if index > 0 && window.label == nil { Divider() }
                                // Inside a window group the window header already
                                // names the project, so the row leads with the
                                // conversation summary instead.
                                clickableRow(for: session,
                                             showPath: !(window.label != nil && window.sharedPath != nil),
                                             leadWithTitle: window.label != nil,
                                             compact: window.label != nil && window.sessions.count > 1)
                            }
                        }
                    }
                }
            }
        }
    }

    private func hostHeader(_ host: HostGroup) -> some View {
        Text(host.label.uppercased())
            .font(.caption2.weight(.semibold))
            .foregroundStyle(.tertiary)
            .tracking(0.6)
            .lineLimit(1)
            .truncationMode(.middle)
    }

    /// The window's name in the row-title weight, its path beneath — the same
    /// shape as a row's name + path, so the eye reads it as "the project".
    private func windowHeader(_ window: WindowGroup) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(window.label ?? "")
                .font(.system(size: 14, weight: .semibold))
                .lineLimit(1)
                .truncationMode(.middle)
            if let path = window.sharedPath {
                Text(abbreviatedPath(path))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }
    }

    struct WindowGroup: Equatable {
        var key: String
        /// VS Code window label; nil for hosts without windows we can see.
        var label: String?
        var sessions: [ClaudeSession]

        /// The one working directory every session in the window runs in, or
        /// nil when they differ (then each row shows its own).
        var sharedPath: String? {
            let cwds = Set(sessions.map(\.cwd))
            return cwds.count == 1 ? cwds.first : nil
        }
    }

    struct HostGroup: Equatable {
        var key: String
        var label: String
        var windows: [WindowGroup]
    }

    static let virtualKey = "~virtual"
    static let backgroundKey = "~background"
    static let otherKey = "~other"

    /// Stable grouping: hosts by label, then "Virtual" (sessions on the hub's
    /// ptys — attachable from anywhere, owned by no window), then
    /// "Background" (daemon-run sessions with no terminal), then "Other"
    /// (hosts we couldn't resolve); windows by label (unlabeled last);
    /// sessions inside keep the scanner's (cwd, pid) order.
    static func grouped(_ sessions: [ClaudeSession]) -> [HostGroup] {
        var hosts: [HostGroup] = []
        for session in sessions {
            let hostKey = session.hubId != nil ? virtualKey
                : session.isBackground ? backgroundKey : (session.host?.bundleId ?? otherKey)
            let hostLabel = session.hubId != nil ? "Virtual"
                : session.isBackground ? "Background" : (session.host?.appName ?? "Other")
            let windowLabel = session.hubId != nil ? nil : session.host?.window
            let windowKey = windowLabel ?? "~none"
            let h: Int
            if let i = hosts.firstIndex(where: { $0.key == hostKey }) {
                h = i
            } else {
                hosts.append(HostGroup(key: hostKey, label: hostLabel, windows: []))
                h = hosts.count - 1
            }
            if let w = hosts[h].windows.firstIndex(where: { $0.key == windowKey }) {
                hosts[h].windows[w].sessions.append(session)
            } else {
                hosts[h].windows.append(WindowGroup(key: windowKey, label: windowLabel, sessions: [session]))
            }
        }
        for i in hosts.indices {
            hosts[i].windows.sort { a, b in
                if (a.label == nil) != (b.label == nil) { return b.label == nil }
                return (a.label ?? "") < (b.label ?? "")
            }
        }
        func tier(_ key: String) -> Int {
            switch key {
            case virtualKey: return 1
            case backgroundKey: return 2
            case otherKey: return 3
            default: return 0
            }
        }
        return hosts.sorted { a, b in
            if tier(a.key) != tier(b.key) { return tier(a.key) < tier(b.key) }
            return (a.label, a.key) < (b.label, b.key)
        }
    }

    private func clickableRow(for session: ClaudeSession, showPath: Bool = true, leadWithTitle: Bool = false, compact: Bool = false) -> some View {
        row(for: session, showPath: showPath, leadWithTitle: leadWithTitle, compact: compact)
            .padding(.horizontal, 6)
            .padding(.vertical, 5)
            .background(
                RoundedRectangle(cornerRadius: 6, style: .continuous)
                    .fill(Color.primary.opacity(hoveredPid == session.pid ? 0.06 : 0))
            )
            .padding(.horizontal, -6)
            .padding(.vertical, -5)
            .contentShape(Rectangle())
            .onHover { inside in
                if inside {
                    hoveredPid = session.pid
                    NSCursor.pointingHand.set()
                } else {
                    if hoveredPid == session.pid { hoveredPid = nil }
                    NSCursor.arrow.set()
                }
            }
            .onTapGesture { onFocusSession(session) }
            // Right-click: end the session, the way the web app's End button
            // does — a hang-up to the Claude process, escalating if ignored.
            .contextMenu {
                Button("End session") { onEndSession(session) }
            }
            .help(session.hubId != nil
                  ? "Open a terminal window attached to this session"
                  : session.isBackground
                  ? "Open a terminal attached to this background session"
                  : "Bring this session's terminal to the front")
    }

    /// `leadWithTitle`: the bold line is the conversation summary ("TBD"
    /// until Claude Code has written one) and the italic title line is
    /// dropped — used under a window header that already names the project.
    /// `compact`: several sessions share that window, so the bold line steps
    /// down a size to read as items under the header rather than peers of it.
    private func row(for session: ClaudeSession, showPath: Bool = true, leadWithTitle: Bool = false, compact: Bool = false) -> some View {
        let effectiveState = store.effectiveState(session)
        let title = session.title.flatMap { $0.isEmpty ? nil : $0 }
        return HStack(alignment: .top, spacing: 8) {
            Circle()
                .fill(stateColor(effectiveState))
                .frame(width: 8, height: 8)
                .padding(.top, 5)

            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(leadWithTitle ? (title ?? "TBD") : session.projectName)
                        .font(.system(size: compact ? 12.5 : 14, weight: .semibold))
                        .lineLimit(1)
                        .truncationMode(.tail)
                    if store.hasAutoApprove(session) {
                        Image(systemName: "bolt.fill")
                            .font(.caption2)
                            .foregroundStyle(.yellow)
                            .help("Auto-approving permission requests for this session")
                    }
                }
                if showPath {
                    Text(abbreviatedPath(session.cwd))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                // The conversation title Claude Code generates (the transcript's
                // `ai-title`) — the cheapest way to tell two sessions in one
                // project apart.
                if !leadWithTitle, let title {
                    Text(title)
                        .font(.caption)
                        .italic()
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                if let branch = session.gitBranch, !branch.isEmpty {
                    Label(branch, systemImage: "arrow.triangle.branch")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            Spacer()

            // A pending permission request takes over the status area: the
            // verdict buttons render exactly where the state text would be,
            // with the command under them where the timing caption goes.
            if let approval = store.firstPending(for: session) {
                approvalControls(for: approval, in: session)
            } else {
                VStack(alignment: .trailing, spacing: 2) {
                    Text(stateText(session))
                        .font(.callout)
                        .foregroundStyle(stateColor(session.state))
                    Text(activityText(for: session))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
        }
    }

    private func approvalControls(for approval: PendingApproval, in session: ClaudeSession) -> some View {
        VStack(alignment: .trailing, spacing: 4) {
            HStack(spacing: 6) {
                Button("Approve") { store.approve(approval) }
                    .buttonStyle(ApprovalPillStyle(color: Self.approveGreen))
                Button("Deny") { store.deny(approval) }
                    .buttonStyle(ApprovalPillStyle(color: .red))
                Menu {
                    Button("Approve all for 5 minutes") {
                        if let sid = session.sessionId {
                            store.approveAll(sessionId: sid, rule: .until(Date().addingTimeInterval(300)))
                        }
                    }
                    Button("Approve all for this session") {
                        if let sid = session.sessionId {
                            store.approveAll(sessionId: sid, rule: .forSession)
                        }
                    }
                } label: {
                    Image(systemName: "ellipsis.circle")
                        .foregroundStyle(.secondary)
                }
                .menuStyle(.borderlessButton)
                .fixedSize()
            }
            // Hover reveals the full request (newlines intact) after the
            // system's standard tooltip delay.
            Text(pendingSummary(approval, in: session))
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(maxWidth: 200, alignment: .trailing)
                .help(approval.detail)
        }
    }

    /// Approve-pill green: plain systemGreen is too washed out against the
    /// panel's near-white light-mode background, but full forest green read
    /// too rich — light mode sits halfway between the two. Dark mode keeps
    /// the brighter system green, which reads well on dark.
    private static let approveGreen = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            ? .systemGreen
            : NSColor(srgbRed: 0.16, green: 0.64, blue: 0.29, alpha: 1)
    })

    private func pendingSummary(_ approval: PendingApproval, in session: ClaudeSession) -> String {
        let more = store.pendingCount(for: session) - 1
        return more > 0 ? "\(approval.summary) · +\(more) more" : approval.summary
    }

    private var footer: some View {
        HStack(spacing: 10) {
            Spacer()
            Button(action: onNewSession) {
                Image(systemName: "plus.circle")
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
            .help("New Claude session in your home folder (auto mode), in a new terminal window")
            .accessibilityLabel("New session in your home folder")
            Menu {
                Toggle("Invert menu bar colors", isOn: $store.invertMenubarColors)
                    .help("On: solid color block behind the count. Off: colored text on the bare menu bar.")
                Divider()
                Button("Quit") { NSApp.terminate(nil) }
            } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
        }
    }

    // MARK: Derived

    private func stateColor(_ state: ClaudeSession.State) -> Color {
        switch state {
        case .busy: return .green
        case .shell: return .blue
        case .idle: return Color(nsColor: .secondaryLabelColor)
        case .waitingForInput: return .orange
        }
    }

    private func stateText(_ session: ClaudeSession) -> String {
        switch session.state {
        case .busy: return "Working"
        case .shell: return "Shell command"
        case .idle: return "Idle"
        case .waitingForInput:
            if let what = session.waitingFor, !what.isEmpty {
                return "Waiting: \(what)"
            }
            return "Waiting for input"
        }
    }

    /// "for 12s · up 2h 14m" — how long the current state has held, plus
    /// session age.
    private func activityText(for session: ClaudeSession) -> String {
        var parts: [String] = []
        if let since = session.stateSince {
            parts.append("for \(StatusFormat.compactAge(since: since, now: now))")
        }
        if let started = session.startedAt {
            parts.append("up \(StatusFormat.compactDuration(from: started, to: now))")
        }
        return parts.isEmpty ? "—" : parts.joined(separator: " · ")
    }

    private func abbreviatedPath(_ path: String) -> String {
        let home = NSHomeDirectory()
        if path.hasPrefix(home) {
            return "~" + path.dropFirst(home.count)
        }
        return path
    }
}

/// Compact tinted capsule for the inline Approve/Deny verdict buttons —
/// color-on-soft-color so the pair reads instantly (green = go, red = stop)
/// against the panel's vibrancy, with a darker fill while pressed. Hover
/// shows the pointing hand as the click affordance.
private struct ApprovalPillStyle: ButtonStyle {
    let color: Color

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.caption.weight(.semibold))
            .padding(.horizontal, 9)
            .padding(.vertical, 3.5)
            .foregroundStyle(color)
            .background(Capsule().fill(color.opacity(configuration.isPressed ? 0.35 : 0.15)))
            .overlay(Capsule().strokeBorder(color.opacity(0.35), lineWidth: 0.5))
            .contentShape(Capsule())
            .onHover { inside in
                if inside { NSCursor.pointingHand.push() } else { NSCursor.pop() }
            }
    }
}

// MARK: - Formatting helpers

enum StatusFormat {
    /// "just now" / "12s ago" / "3m ago" / "2h 5m ago"
    static func agoString(since past: Date, now: Date = Date()) -> String {
        let secs = max(0, Int(now.timeIntervalSince(past)))
        if secs < 5 { return "just now" }
        if secs < 60 { return "\(secs)s ago" }
        return "\(compactDuration(from: past, to: now)) ago"
    }

    /// "2h 34m" / "57m" / "0m" — same shape as ClaudeUsage's duration labels.
    static func compactDuration(from start: Date, to end: Date) -> String {
        let secs = max(0, Int(end.timeIntervalSince(start)))
        let h = secs / 3600
        let m = (secs % 3600) / 60
        if h > 0 { return "\(h)h \(m)m" }
        return "\(m)m"
    }

    /// "12s" / "3m" / "2h 5m" — seconds-precision only while young, for the
    /// "state held for …" label.
    static func compactAge(since past: Date, now: Date = Date()) -> String {
        let secs = max(0, Int(now.timeIntervalSince(past)))
        if secs < 60 { return "\(secs)s" }
        return compactDuration(from: past, to: now)
    }
}
