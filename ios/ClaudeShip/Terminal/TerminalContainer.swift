import SwiftTerm
import SwiftUI

/// SwiftTerm's view, wired to a TerminalSession.
///
/// The terminal sits inside a plain host view rather than filling the
/// SwiftUI slot directly: when another screen owns the session's size, the
/// session shrinks the terminal's font and frames it to that screen's exact
/// grid inside the host (see `TerminalSession.layout`), so the host is what
/// tells the session how much room there is.
struct TerminalContainer: UIViewRepresentable {
    let session: TerminalSession

    func makeUIView(context: Context) -> TerminalHostView {
        let view = HubTerminalView(frame: .zero)
        view.font = UIFont.monospacedSystemFont(ofSize: TerminalSession.baseFontSize, weight: .regular)
        view.nativeBackgroundColor = UIColor(red: 0.082, green: 0.078, blue: 0.075, alpha: 1)
        view.nativeForegroundColor = UIColor(red: 0.914, green: 0.898, blue: 0.871, alpha: 1)
        view.caretColor = UIColor(red: 0.914, green: 0.898, blue: 0.871, alpha: 1)
        view.backgroundColor = view.nativeBackgroundColor
        view.allowMouseReporting = true
        view.optionAsMetaKey = true
        let bar = KeyBar()
        bar.onKey = { [weak session] key in session?.press(key) }
        // Tapping the terminal brings the keyboard back (SwiftTerm takes
        // focus on a tap when it doesn't have it).
        bar.onDismiss = { [weak view] in _ = view?.resignFirstResponder() }
        view.inputAccessoryView = bar
        // A downward drag on the terminal itself lowers the keyboard with
        // the finger, as in Messages — on the normal screen, where the
        // scroll view's pan is live; a full-screen program's drags scroll
        // its transcript instead, and the bar's key or swipe does it there.
        view.keyboardDismissMode = .interactive
        let host = TerminalHostView(frame: .zero)
        host.backgroundColor = view.nativeBackgroundColor
        host.clipsToBounds = true
        host.addSubview(view)
        host.onLayout = { [weak session] in session?.layout() }
        DispatchQueue.main.async {
            session.attach(view, in: host)
            _ = view.becomeFirstResponder()
        }
        return host
    }

    func updateUIView(_ view: TerminalHostView, context: Context) {}
}

/// The room the terminal has; reports every change of it.
final class TerminalHostView: UIView {
    var onLayout: (() -> Void)?

    override func layoutSubviews() {
        super.layoutSubviews()
        onLayout?()
    }
}
