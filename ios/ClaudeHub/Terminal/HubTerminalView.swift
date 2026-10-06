import SwiftTerm
import UIKit

/// SwiftTerm's view with the scrolling a phone expects.
///
/// Claude Code runs as a full-screen TUI with mouse reporting on. In that
/// mode SwiftTerm turns a finger drag into mouse press/drag/release
/// reports for the program, so nothing scrolls. This view takes vertical
/// drags over while reporting is on and sends them as mouse-wheel events —
/// a line per cell of travel, continuing with momentum after the finger
/// lifts, the way a scroll view coasts — which is how the program's own
/// transcript is scrolled from a mouse. On the normal screen (mouse
/// reporting off) the view keeps SwiftTerm's native scrollback scrolling.
final class HubTerminalView: TerminalView {
    /// Finger travel, in lines, per wheel event. Claude Code moves a few
    /// lines per wheel notch, so one notch per line of travel runs away
    /// from the finger; two feels like 1:1.
    var linesPerWheelTick: CGFloat = 2

    private let scrollPan = UIPanGestureRecognizer()
    private var travel: CGFloat = 0
    private var lastY: CGFloat = 0
    private var at = CGPoint.zero
    private var velocity: CGFloat = 0
    private var coasting: CADisplayLink?
    private var lastFrame: CFTimeInterval = 0

    override init(frame: CGRect) {
        super.init(frame: frame)
        scrollPan.addTarget(self, action: #selector(dragged(_:)))
        scrollPan.maximumNumberOfTouches = 1
        scrollPan.isEnabled = false
        addGestureRecognizer(scrollPan)
    }

    required init?(coder: NSCoder) { nil }

    override func mouseModeChanged(source: Terminal) {
        super.mouseModeChanged(source: source)
        let tracking = source.mouseMode != .off
        let apply = { [self] in
            scrollPan.isEnabled = tracking
            // A full-screen program has no scrollback for the scroll view to
            // move, and its pan recognizer would cancel ours; the view scrolls
            // natively again as soon as reporting is off.
            isScrollEnabled = !tracking
            if !tracking { stopCoasting() }
            // SwiftTerm's own pan-as-mouse-drag recognizer (added just now by
            // super when reporting turned on) would eat the same drags.
            for recognizer in gestureRecognizers ?? [] {
                guard let pan = recognizer as? UIPanGestureRecognizer, pan !== scrollPan, pan !== panGestureRecognizer
                else { continue }
                pan.isEnabled = !tracking
            }
        }
        if Thread.isMainThread { apply() } else { DispatchQueue.main.async(execute: apply) }
    }

    private var lineHeight: CGFloat {
        let rows = getTerminal().rows
        return rows > 0 ? bounds.height / CGFloat(rows) : 16
    }

    @objc private func dragged(_ gesture: UIPanGestureRecognizer) {
        let y = gesture.translation(in: self).y
        switch gesture.state {
        case .began:
            stopCoasting()
            travel = 0
            lastY = y
            at = gesture.location(in: self)
        case .changed:
            travel += y - lastY
            lastY = y
            at = gesture.location(in: self)
            emitTicks()
        case .ended:
            travel += y - lastY
            emitTicks()
            velocity = gesture.velocity(in: self).y
            startCoasting()
        default:
            stopCoasting()
        }
    }

    /// Turn accumulated travel into wheel events: finger moving down shows
    /// earlier output (wheel up, button 4), up shows later (button 5).
    private func emitTicks() {
        let step = max(1, lineHeight * linesPerWheelTick)
        let terminal = getTerminal()
        let cellWidth = terminal.cols > 0 ? bounds.width / CGFloat(terminal.cols) : 8
        let col = max(0, min(terminal.cols - 1, Int(at.x / cellWidth)))
        let row = max(0, min(terminal.rows - 1, Int(at.y / lineHeight)))
        while travel >= step {
            travel -= step
            terminal.sendEvent(buttonFlags: 64, x: col, y: row)
        }
        while travel <= -step {
            travel += step
            terminal.sendEvent(buttonFlags: 65, x: col, y: row)
        }
    }

    // MARK: Momentum

    private func startCoasting() {
        guard abs(velocity) > 80 else { return }
        let link = CADisplayLink(target: self, selector: #selector(coast(_:)))
        link.add(to: .main, forMode: .common)
        coasting = link
        lastFrame = 0
    }

    private func stopCoasting() {
        coasting?.invalidate()
        coasting = nil
        velocity = 0
    }

    @objc private func coast(_ link: CADisplayLink) {
        if lastFrame == 0 {
            lastFrame = link.timestamp
            return
        }
        let dt = link.timestamp - lastFrame
        lastFrame = link.timestamp
        travel += velocity * dt
        // UIScrollView's normal deceleration rate, per millisecond.
        velocity *= pow(UIScrollView.DecelerationRate.normal.rawValue, dt * 1000)
        emitTicks()
        if abs(velocity) < 40 { stopCoasting() }
    }
}
