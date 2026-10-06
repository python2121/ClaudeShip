import SwiftTerm
import SwiftUI

/// SwiftTerm's view, wired to a TerminalSession.
struct TerminalContainer: UIViewRepresentable {
    let session: TerminalSession

    func makeUIView(context: Context) -> TerminalView {
        let view = TerminalView(frame: .zero)
        view.font = UIFont.monospacedSystemFont(ofSize: TerminalSession.baseFontSize, weight: .regular)
        view.nativeBackgroundColor = UIColor(red: 0.082, green: 0.078, blue: 0.075, alpha: 1)
        view.nativeForegroundColor = UIColor(red: 0.914, green: 0.898, blue: 0.871, alpha: 1)
        view.caretColor = UIColor(red: 0.914, green: 0.898, blue: 0.871, alpha: 1)
        view.backgroundColor = view.nativeBackgroundColor
        view.allowMouseReporting = true
        view.optionAsMetaKey = true
        let bar = KeyBar()
        bar.onKey = { [weak session] key in session?.press(key) }
        view.inputAccessoryView = bar
        DispatchQueue.main.async {
            session.attach(view)
            view.becomeFirstResponder()
        }
        return view
    }

    func updateUIView(_ view: TerminalView, context: Context) {}
}
