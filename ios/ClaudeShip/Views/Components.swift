import SwiftUI

/// The status glyphs of the menubar app and the web page: a green
/// half-disc turning while working, an orange dot when the session needs
/// you, a hollow ring at rest, a dashed ring while starting.
struct StatusGlyph: View {
    let status: String
    var size: CGFloat = 12

    var body: some View {
        switch status {
        case "busy":
            TimelineView(.periodic(from: .now, by: 0.25)) { context in
                let step = Int(context.date.timeIntervalSinceReferenceDate * 4) % 4
                ZStack {
                    Circle().stroke(Palette.green, lineWidth: 1.5)
                    HalfDisc().fill(Palette.green)
                }
                .rotationEffect(.degrees(Double(step) * 90))
                .frame(width: size, height: size)
            }
        case "waiting":
            Circle().fill(Palette.orange).frame(width: size, height: size)
        case "shell":
            Circle().stroke(Palette.green, lineWidth: 1.5).frame(width: size, height: size)
        case "starting":
            Circle().stroke(style: StrokeStyle(lineWidth: 1.5, dash: [2.5, 2.5])).foregroundStyle(.tertiary)
                .frame(width: size, height: size)
        default:
            Circle().stroke(Color.secondary.opacity(0.6), lineWidth: 1.5).frame(width: size, height: size)
        }
    }

    private struct HalfDisc: Shape {
        func path(in rect: CGRect) -> Path {
            var path = Path()
            path.addArc(center: CGPoint(x: rect.midX, y: rect.midY), radius: rect.width / 2,
                        startAngle: .degrees(90), endAngle: .degrees(270), clockwise: false)
            path.closeSubpath()
            return path
        }
    }
}

enum Palette {
    static let green = Color(light: UIColor(red: 0.18, green: 0.60, blue: 0.35, alpha: 1), dark: UIColor(red: 0.35, green: 0.72, blue: 0.48, alpha: 1))
    static let orange = Color(light: UIColor(red: 0.85, green: 0.51, blue: 0.04, alpha: 1), dark: UIColor(red: 0.95, green: 0.64, blue: 0.23, alpha: 1))
    static let terminalBackground = Color(red: 0.082, green: 0.078, blue: 0.075)
}

extension Color {
    init(light: UIColor, dark: UIColor) {
        self.init(uiColor: UIColor { traits in traits.userInterfaceStyle == .dark ? dark : light })
    }
}

enum StatusText {
    static func label(_ status: String) -> String {
        switch status {
        case "busy": return "Working"
        case "waiting": return "Needs you"
        case "idle": return "Idle"
        case "shell": return "Running a shell command"
        case "starting": return "Starting"
        default: return status
        }
    }

    static func color(_ status: String) -> Color {
        switch status {
        case "busy": return Palette.green
        case "waiting": return Palette.orange
        default: return .secondary
        }
    }
}

enum Ago {
    /// "12s", "4m", "2h 5m", "3d" — how long a state has held.
    static func age(ms: Int, now: Date) -> String {
        let s = max(0, Int(now.timeIntervalSince1970) - ms / 1000)
        if s < 60 { return "\(s)s" }
        if s < 3600 { return "\(s / 60)m" }
        if s < 86400 { return "\(s / 3600)h \((s % 3600) / 60)m" }
        return "\(s / 86400)d"
    }

    /// "14:32" today, else "3 Oct, 14:32" — a moment written on another
    /// clock (`now` is that clock's present), shown on this phone's.
    static func since(ms: Int, now: Date) -> String {
        let at = Date(timeIntervalSince1970: Double(ms) / 1000).addingTimeInterval(Date().timeIntervalSince(now))
        return Calendar.current.isDateInToday(at)
            ? at.formatted(date: .omitted, time: .shortened)
            : at.formatted(.dateTime.day().month(.abbreviated).hour().minute())
    }

    /// "just now", "5m ago", "3d ago", else a date.
    static func ago(ms: Int, now: Date) -> String {
        let s = max(0, Int(now.timeIntervalSince1970) - ms / 1000)
        if s < 60 { return "just now" }
        if s < 3600 { return "\(s / 60)m ago" }
        if s < 86400 { return "\(s / 3600)h ago" }
        if s < 86400 * 14 { return "\(s / 86400)d ago" }
        return Date(timeIntervalSince1970: Double(ms) / 1000).formatted(.dateTime.day().month(.abbreviated).year())
    }
}

/// A one-line notice at the top of a screen, gone when tapped.
struct NoticeBar: View {
    let text: String
    let dismiss: () -> Void

    var body: some View {
        Button(action: dismiss) {
            Text(text)
                .font(.footnote.weight(.medium))
                .foregroundStyle(Palette.orange)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                .background(Palette.orange.opacity(0.13), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        }
        .buttonStyle(.plain)
    }
}

extension View {
    /// A peer refused through a hub (409/502): an alert naming the
    /// machine. One, at the root, for whichever hub has one to show.
    func proxyAlert(_ store: HubStore?) -> some View {
        alert(store?.alert?.title ?? "", isPresented: Binding(
            get: { store?.alert != nil }, set: { if !$0 { store?.alert = nil } }
        )) {
            Button("OK", role: .cancel) { store?.alert = nil }
        } message: {
            Text(store?.alert?.message ?? "")
        }
    }
}
