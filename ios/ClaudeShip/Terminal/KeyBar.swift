import UIKit

/// The row of keys a phone keyboard lacks, shown above it.
final class KeyBar: UIInputView {
    enum Key: CaseIterable {
        case escape, tab, shiftTab, controlC, up, down, left, right, pageUp, pageDown, enter

        var label: String {
            switch self {
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

    init() {
        super.init(frame: CGRect(x: 0, y: 0, width: 320, height: 44), inputViewStyle: .keyboard)
        allowsSelfSizing = true
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
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
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
        for key in Key.allCases {
            var config = UIButton.Configuration.gray()
            config.title = key.label
            config.cornerStyle = .medium
            config.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 11, bottom: 4, trailing: 11)
            config.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attributes in
                var attributes = attributes
                attributes.font = UIFont.monospacedSystemFont(ofSize: 14, weight: .semibold)
                return attributes
            }
            let button = UIButton(configuration: config)
            button.addAction(UIAction { [weak self] _ in self?.onKey?(key) }, for: .touchUpInside)
            stack.addArrangedSubview(button)
        }
    }

    required init?(coder: NSCoder) { nil }
}
