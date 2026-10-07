import Foundation
import Observation
import SwiftTerm
import UIKit

/// One attachment to a hub session: the WebSocket, the SwiftTerm view it
/// feeds, and the size handshake with the hub (the same rules as the web
/// page, see CLAUDE.md "One pty, one size").
@Observable
@MainActor
final class TerminalSession: NSObject, TerminalViewDelegate {
    let hubId: String
    /// The swarm peer the session runs on (nil: the hub's own machine).
    let hostId: String?
    /// What to call that machine when the hub won't relay to it.
    @ObservationIgnored var hostName: () -> String = { "the other machine" }
    private let connection: HubConnection
    @ObservationIgnored private weak var view: TerminalView?
    /// The room the terminal has (TerminalContainer's host view).
    @ObservationIgnored private weak var host: UIView?

    private(set) var note: String?
    private(set) var reconnecting = false
    /// Set once the session is over: heading and detail for the user.
    private(set) var ended: (heading: String, detail: String)?
    /// Whether the session is sized for this screen (the hub says).
    private(set) var owner = true
    /// The session's grid when it is some other screen's (nil when ours).
    private(set) var foreignSize: (cols: Int, rows: Int)?

    @ObservationIgnored private var task: URLSessionWebSocketTask?
    @ObservationIgnored private var connected = false
    @ObservationIgnored private var attempts = 0
    @ObservationIgnored private var refusals = 0
    @ObservationIgnored private var heard = Date()
    @ObservationIgnored private var pulse: Timer?
    @ObservationIgnored private var retry: Task<Void, Never>?
    /// What this screen fits at the normal font, and what the hub was told.
    @ObservationIgnored private var own = (cols: 80, rows: 24)
    @ObservationIgnored private var sent = (cols: 0, rows: 0)
    @ObservationIgnored private var sessionSize = (cols: 0, rows: 0)
    /// The handshake has completed on the current task; only then do
    /// control messages go out (a task is "running" from resume()).
    @ObservationIgnored private var isOpen = false
    @ObservationIgnored private var closed = false
    /// Whether the hub is answering the directory poll — a connection
    /// refused while it isn't is a network problem, not the hub saying no.
    @ObservationIgnored var hubReachable: () -> Bool = { true }
    static let baseFontSize: CGFloat = 12
    /// How far the font shrinks to mirror another screen's grid; past
    /// this the view is scaled down instead (glyphs rasterized smaller
    /// than this lose their shape, a scaled-down 5 pt keeps it). A wide
    /// terminal ends up unreadable but whole and correct, which beats
    /// clipping or wrapping it; sideways it is legible again.
    static let minimumFontSize: CGFloat = 5
    /// The font size, grid, and scale the view is currently laid out for.
    @ObservationIgnored private var applied: (font: CGFloat, cols: Int, rows: Int, scale: CGFloat, room: CGSize, mirroring: Bool)?
    /// The font size in force when mirroring (nil at the normal size).
    private(set) var mirrorFontSize: CGFloat?

    init(hubId: String, host: String? = nil, connection: HubConnection) {
        self.hubId = hubId
        self.hostId = host
        self.connection = connection
    }

    // MARK: Lifecycle

    func attach(_ terminalView: TerminalView, in hostView: UIView) {
        guard !closed else { return }
        if let old = view, old !== terminalView { old.terminalDelegate = nil }
        let first = view == nil
        view = terminalView
        host = hostView
        applied = nil
        terminalView.terminalDelegate = self
        measure()
        layout()
        guard first else { return }  // SwiftUI rebuilt the view; the socket stays
        connect()
        let timer = Timer(timeInterval: 15, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.heartbeat() }
        }
        RunLoop.main.add(timer, forMode: .common)
        pulse = timer
    }

    func close() {
        closed = true
        pulse?.invalidate()
        pulse = nil
        retry?.cancel()
        task?.cancel(with: .normalClosure, reason: nil)
        task = nil
        isOpen = false
    }

    /// The app came back to the front: a socket that died meanwhile still
    /// says it is open, so ask it, and start over if it doesn't answer. And
    /// a phone being looked at is the screen the session should fit — take
    /// the size rather than ask.
    func revive() {
        guard ended == nil, !closed else { return }
        if task == nil || task?.state != .running {
            reconnectNow()
            return
        }
        claim()
        let asked = Date()
        heard = asked
        sendControl(["type": "ping"])
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(5))
            guard let self, self.ended == nil, self.heard <= asked else { return }
            self.reconnectNow()
        }
    }

    private func heartbeat() {
        guard ended == nil, !closed, UIApplication.shared.applicationState == .active,
              let task, task.state == .running else { return }
        if Date().timeIntervalSince(heard) > 45 { reconnectNow() } else { sendControl(["type": "ping"]) }
    }

    private func reconnectNow() {
        guard ended == nil, !closed else { return }
        retry?.cancel()
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
        isOpen = false
        attempts = 0
        refusals = 0
        reconnecting = true
        connect()
    }

    private func connect() {
        guard ended == nil, !closed else { return }
        // Attaching while the phone is in front sizes the session for it: a
        // phone being looked at is the screen that matters. A socket that
        // flaps while the app is in the background rejoins as a spectator, so
        // a pocketed phone doesn't reflow the session under whoever is using
        // it (the hub hands the size over anyway if nobody else is attached).
        let claiming = UIApplication.shared.applicationState == .active
        owner = claiming
        sent = own
        guard let request = connection.terminalRequest(hubId: hubId, host: hostId, cols: own.cols, rows: own.rows, claim: claiming) else { return }
        let task = connection.session.webSocketTask(with: request)
        self.task = task
        isOpen = false
        heard = Date()
        task.resume()
        receive(on: task, opened: false)
    }

    private func receive(on task: URLSessionWebSocketTask, opened: Bool) {
        task.receive { [weak self] result in
            Task { @MainActor in
                guard let self, self.task === task else { return }
                switch result {
                case .success(let message):
                    if !opened { self.didOpen() }
                    self.heard = Date()
                    self.handle(message)
                    self.receive(on: task, opened: true)
                case .failure:
                    // A home hub that won't relay to a peer of another build
                    // says so with a 409 on the upgrade: no point retrying.
                    if !opened, self.hostId != nil, (task.response as? HTTPURLResponse)?.statusCode == 409 {
                        self.refused(ProxyRefusal(mismatch: true))
                        return
                    }
                    self.didClose(opened: opened)
                }
            }
        }
    }

    private func didOpen() {
        isOpen = true
        attempts = 0
        refusals = 0
        reconnecting = false
        note = nil
        if connected {
            // A reconnect brings the whole replay again; start from a blank screen.
            view?.feed(text: "\u{1b}c")
        }
        connected = true
        measure()
        syncFit()
    }

    private func refused(_ refusal: ProxyRefusal) {
        guard ended == nil, !closed else { return }
        task = nil
        isOpen = false
        reconnecting = false
        note = refusal.message(hostName: hostName())
    }

    private func didClose(opened: Bool) {
        guard ended == nil, !closed else { return }
        task = nil
        isOpen = false
        // Refused again and again while the hub is otherwise answering is
        // the hub saying no (not a flaky network): stop and say so.
        if !opened, hubReachable() {
            refusals += 1
            if refusals >= 5 {
                reconnecting = false
                note = "Can't connect to this session."
                return
            }
        }
        reconnecting = true
        let delay = min(5.0, 0.4 * pow(2, Double(attempts)))
        attempts += 1
        retry = Task { [weak self] in
            try? await Task.sleep(for: .seconds(delay))
            guard !Task.isCancelled else { return }
            self?.connect()
        }
    }

    func retryNow() {
        reconnectNow()
    }

    // MARK: Messages

    private func handle(_ message: URLSessionWebSocketTask.Message) {
        switch message {
        case .data(let data):
            view?.feed(byteArray: [UInt8](data)[...])
        case .string(let text):
            guard let object = try? JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any],
                  let type = object["type"] as? String
            else { return }
            switch type {
            case "size":
                if let cols = object["cols"] as? Int, let rows = object["rows"] as? Int {
                    sessionSize = (cols, rows)
                    if let mine = object["owner"] as? Bool { owner = mine }
                    applySize()
                }
            case "exit":
                let code = object["code"] as? Int ?? 0
                finish("Session ended", code == 0 ? "Claude exited." : "Claude exited with code \(code).")
            case "gone":
                finish("Session not found", "It has ended, or the hub was restarted.")
            default:
                break
            }
        @unknown default:
            break
        }
    }

    private func finish(_ heading: String, _ detail: String) {
        ended = (heading, detail)
        close()
    }

    private func sendControl(_ object: [String: Any]) {
        guard isOpen, let task, task.state == .running,
              let data = try? JSONSerialization.data(withJSONObject: object),
              let text = String(data: data, encoding: .utf8)
        else { return }
        task.send(.string(text)) { _ in }
    }

    private func sendInput(_ bytes: [UInt8]) {
        guard isOpen, let task, task.state == .running, ended == nil else { return }
        task.send(.data(Data(bytes))) { _ in }
    }

    /// Keys from the bar above the keyboard.
    func press(_ key: KeyBar.Key) {
        let applicationCursor = view?.getTerminal().applicationCursor ?? false
        sendInput(key.bytes(applicationCursor: applicationCursor))
    }

    // MARK: Size

    /// The grid this screen fits at the normal font — computed from the
    /// room, not read from the terminal, which may be laid out for another
    /// screen's grid at the time. A sliver (landscape with the keyboard up)
    /// is not a size worth imposing on the session; keep the last real one.
    private func measure() {
        guard let host, host.bounds.width > 0, host.bounds.height > 0 else { return }
        let dims = Self.grid(fitting: host.bounds.size, fontSize: Self.baseFontSize)
        if dims.cols >= 20, dims.rows >= 6 { own = dims }
    }

    /// SwiftTerm's cell for a monospaced system font of this size: line
    /// height and the advance of "W", each snapped to the pixel grid, the
    /// way `computeFontDimensions` does it (internal to the package).
    /// Memoized: `fit` asks for up to 15 sizes per layout, and layout runs
    /// on every frame of a keyboard or rotation animation.
    nonisolated(unsafe) private static var cellSizes: [CGFloat: CGSize] = [:]
    static func cellSize(fontSize: CGFloat) -> CGSize {
        if let hit = cellSizes[fontSize] { return hit }
        let size = computeCellSize(fontSize: fontSize)
        cellSizes[fontSize] = size
        return size
    }

    private static func computeCellSize(fontSize: CGFloat) -> CGSize {
        let font = UIFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        let height = ceil(CTFontGetAscent(font) + CTFontGetDescent(font) + CTFontGetLeading(font))
        let width = "W".size(withAttributes: [.font: font]).width
        let scale = UIScreen.main.scale
        return CGSize(width: max(1, (width * scale).rounded() / scale), height: max(1, ceil(height * scale) / scale))
    }

    /// The grid SwiftTerm would derive for a view of this size and font.
    static func grid(fitting size: CGSize, fontSize: CGFloat) -> (cols: Int, rows: Int) {
        let cell = cellSize(fontSize: fontSize)
        return (Int(size.width / cell.width), Int(size.height / cell.height))
    }

    /// How to show this grid in the room: the smallest font (half-point
    /// steps from the floor up to the normal size) at which the grid
    /// overflows the room, and the scale (≤ 1) that then makes it fit
    /// exactly. Cell sizes are snapped to whole pixels, so no font size
    /// lands the grid on the room's edge — with 155 columns each step is
    /// a ~50 pt jump — and the scale is what fills the remaining width or
    /// height; rendering a little large and shrinking keeps the glyphs
    /// crisper than the other way round. A grid that fits at the normal
    /// size is shown at it, unscaled.
    static func fit(_ grid: (cols: Int, rows: Int), in room: CGSize) -> (fontSize: CGFloat, scale: CGFloat) {
        var size = minimumFontSize
        while size < baseFontSize {
            let cell = cellSize(fontSize: size)
            if CGFloat(grid.cols) * cell.width > room.width || CGFloat(grid.rows) * cell.height > room.height { break }
            size += 0.5
        }
        let cell = cellSize(fontSize: size)
        let scale = min(1, room.width / (CGFloat(grid.cols) * cell.width), room.height / (CGFloat(grid.rows) * cell.height))
        return (size, scale)
    }

    /// Lay the terminal out for whoever owns the session's size. Ours: the
    /// normal font, filling the room. Another screen's: that screen's exact
    /// grid, at whatever font size makes it fit — zoomed out, never
    /// reflowed, so the bytes the hub relays land where the other screen
    /// drew them. Idempotent; called on every change of room or ownership.
    func layout() {
        guard let view, let host else { return }
        let room = host.bounds.size
        guard room.width > 0, room.height > 0 else { return }
        let mirroring = !owner && sessionSize.cols >= 2 && sessionSize.rows >= 2
            && (sessionSize.cols, sessionSize.rows) != (own.cols, own.rows)
        // Nothing that feeds the layout changed: out before any measuring.
        if let applied, applied.room == room, applied.mirroring == mirroring,
           !mirroring || (applied.cols, applied.rows) == (sessionSize.cols, sessionSize.rows) { return }
        let (fontSize, scale) = mirroring ? Self.fit(sessionSize, in: room) : (Self.baseFontSize, 1)
        let target = mirroring ? sessionSize : Self.grid(fitting: room, fontSize: Self.baseFontSize)
        if let applied, applied.font == fontSize, applied.cols == target.cols, applied.rows == target.rows,
           applied.scale == scale, applied.room == room { return }
        applied = (fontSize, target.cols, target.rows, scale, room, mirroring)
        mirrorFontSize = mirroring ? fontSize : nil
        if view.font.pointSize != fontSize {
            view.font = UIFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        }
        view.transform = .identity
        if mirroring {
            // Exactly the grid: SwiftTerm floors width / cell width, so a
            // hair over the last cell's edge rounds to the right count.
            // Then shrunk about the top-left corner to fill the room.
            let cell = Self.cellSize(fontSize: fontSize)
            view.layer.anchorPoint = .zero
            view.bounds = CGRect(x: 0, y: 0, width: CGFloat(target.cols) * cell.width + 0.5, height: CGFloat(target.rows) * cell.height + 0.5)
            view.layer.position = .zero
            view.transform = CGAffineTransform(scaleX: scale, y: scale)
        } else {
            view.layer.anchorPoint = CGPoint(x: 0.5, y: 0.5)
            view.frame = host.bounds
        }
    }

    /// Bring the hub up to date with what this screen fits: resizing the
    /// session if it is sized for this screen, just noting it otherwise.
    private func syncFit() {
        guard isOpen, (own.cols, own.rows) != (sent.cols, sent.rows) else { return }
        sent = own
        sendControl(["type": owner ? "resize" : "fit", "rows": own.rows, "cols": own.cols])
    }

    /// Take the session's size for this screen. The hub always answers, so
    /// the view goes back to the normal font at once rather than a
    /// round-trip later.
    func claim() {
        guard isOpen else { return }
        measure()
        sent = own
        sendControl(["type": "resize", "rows": own.rows, "cols": own.cols])
        owner = true
        sessionSize = own
        applySize()
    }

    /// The session's real size arrived, or whose it is changed: show that
    /// grid, zoomed out if it isn't ours (`layout`), and say so.
    private func applySize() {
        foreignSize = !owner && (sessionSize.cols, sessionSize.rows) != (own.cols, own.rows) ? sessionSize : nil
        layout()
    }

    // MARK: TerminalViewDelegate

    nonisolated func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
        Task { @MainActor in
            self.measure()
            self.syncFit()
            self.applySize()
        }
    }

    nonisolated func send(source: TerminalView, data: ArraySlice<UInt8>) {
        let bytes = Array(data)
        Task { @MainActor in self.sendInput(bytes) }
    }

    nonisolated func setTerminalTitle(source: TerminalView, title: String) {}
    nonisolated func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
    nonisolated func scrolled(source: TerminalView, position: Double) {}
    nonisolated func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {
        guard let url = URL(string: link), ["http", "https"].contains(url.scheme ?? "") else { return }
        Task { @MainActor in UIApplication.shared.open(url) }
    }
    nonisolated func bell(source: TerminalView) {}
    nonisolated func clipboardCopy(source: TerminalView, content: Data) {
        Task { @MainActor in UIPasteboard.general.string = String(decoding: content, as: UTF8.self) }
    }
    nonisolated func clipboardRead(source: TerminalView) -> Data? { nil }
    nonisolated func iTermContent(source: TerminalView, content: ArraySlice<UInt8>) {}
    nonisolated func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
}
