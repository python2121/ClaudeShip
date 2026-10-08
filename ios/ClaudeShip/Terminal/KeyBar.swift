import UIKit

/// The row of keys a phone keyboard lacks, shown above it, with a
/// keyboard-down key pinned at the left that stays put while the row
/// scrolls. A downward swipe anywhere on the bar does the same.
final class KeyBar: UIInputView, UIGestureRecognizerDelegate {
    enum Key: CaseIterable {
        /// `mode` cycles Claude Code's permission mode: it is shift+tab
        /// with a name, first in the row (the launch has no mode choice;
        /// the session's mode is changed here).
        case mode, escape, tab, shiftTab, controlC, up, down, left, right, pageUp, pageDown, enter

        var label: String {
            switch self {
            case .mode: return "mode"
            case .escape: return "esc"
            case .tab: return "tab"
            case .shiftTab: return "⇧tab"
            case .controlC: return "^C"
            case .up: return "↑"
            case .down: return "↓"
            case .left: return "←"
            case .right: return "→"
            case .pageUp: return "pgup"
            case .pageDown: return "pgdn"
            case .enter: return "⏎"
            }
        }

        func bytes(applicationCursor: Bool) -> [UInt8] {
            let arrow = applicationCursor ? "\u{1b}O" : "\u{1b}["
            switch self {
            case .mode: return Array("\u{1b}[Z".utf8)
            case .escape: return [0x1b]
            case .tab: return [0x09]
            case .shiftTab: return Array("\u{1b}[Z".utf8)
            case .controlC: return [0x03]
            case .up: return Array((arrow + "A").utf8)
            case .down: return Array((arrow + "B").utf8)
            case .left: return Array((arrow + "D").utf8)
            case .right: return Array((arrow + "C").utf8)
            case .pageUp: return Array("\u{1b}[5~".utf8)
            case .pageDown: return Array("\u{1b}[6~".utf8)
            case .enter: return [0x0d]
            }
        }
    }

    var onKey: ((Key) -> Void)?
    /// Put the keyboard away.
    var onDismiss: (() -> Void)?

    private let swipe = UIPanGestureRecognizer()
    private var swipeFired = false

    init() {
        super.init(frame: CGRect(x: 0, y: 0, width: 320, height: 44), inputViewStyle: .keyboard)
        allowsSelfSizing = true
        // The same key as the others, with a symbol where the label goes;
        // sized to the "esc" key below so the row reads as one set.
        var hide = Self.keyConfiguration()
        hide.image = UIImage(systemName: "keyboard.chevron.compact.down",
                             withConfiguration: UIImage.SymbolConfiguration(font: Self.keyFont, scale: .default))
        let hideButton = UIButton(configuration: hide)
        hideButton.accessibilityLabel = "Hide keyboard"
        hideButton.translatesAutoresizingMaskIntoConstraints = false
        hideButton.addAction(UIAction { [weak self] _ in self?.onDismiss?() }, for: .touchUpInside)
        addSubview(hideButton)
        let scroll = UIScrollView()
        scroll.showsHorizontalScrollIndicator = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        addSubview(scroll)
        let stack = UIStackView()
        stack.axis = .horizontal
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(stack)
        NSLayoutConstraint.activate([
            hideButton.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8),
            hideButton.centerYAnchor.constraint(equalTo: centerYAnchor),
            scroll.leadingAnchor.constraint(equalTo: hideButton.trailingAnchor, constant: 2),
            scroll.trailingAnchor.constraint(equalTo: trailingAnchor),
            scroll.topAnchor.constraint(equalTo: topAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            scroll.heightAnchor.constraint(equalToConstant: 44),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 8),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -8),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 5),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -5),
            stack.heightAnchor.constraint(equalTo: scroll.frameLayoutGuide.heightAnchor, constant: -10),
        ])
        var firstKey: UIButton?
        for key in Key.allCases {
            var config = Self.keyConfiguration()
            config.title = key.label
            let button = UIButton(configuration: config)
            if key == .mode { button.accessibilityLabel = "Cycle the permission mode" }
            button.addAction(UIAction { [weak self] _ in self?.onKey?(key) }, for: .touchUpInside)
            stack.addArrangedSubview(button)
            if firstKey == nil { firstKey = button }
        }
        if let firstKey {
            NSLayoutConstraint.activate([
                hideButton.widthAnchor.constraint(equalTo: firstKey.widthAnchor),
                hideButton.heightAnchor.constraint(equalTo: firstKey.heightAnchor),
            ])
        }
        swipe.addTarget(self, action: #selector(swiped(_:)))
        swipe.delegate = self
        addGestureRecognizer(swipe)
    }

    required init?(coder: NSCoder) { nil }

    static let keyFont = UIFont.monospacedSystemFont(ofSize: 14, weight: .semibold)

    /// One look for every key in the row.
    static func keyConfiguration() -> UIButton.Configuration {
        var config = UIButton.Configuration.gray()
        config.cornerStyle = .medium
        config.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 11, bottom: 4, trailing: 11)
        config.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attributes in
            var attributes = attributes
            attributes.font = keyFont
            return attributes
        }
        return config
    }

    @objc private func swiped(_ gesture: UIPanGestureRecognizer) {
        switch gesture.state {
        case .began:
            swipeFired = false
        case .changed:
            guard !swipeFired, gesture.translation(in: self).y > 24 else { return }
            swipeFired = true
            onDismiss?()
        default:
            break
        }
    }

    // Only a mostly-downward drag; sideways is the key row scrolling.
    override func gestureRecognizerShouldBegin(_ gesture: UIGestureRecognizer) -> Bool {
        guard gesture === swipe else { return true }
        let v = swipe.velocity(in: self)
        return v.y > 0 && v.y > abs(v.x) * 1.5
    }

    func gestureRecognizer(_ gesture: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        gesture === swipe
    }
}
