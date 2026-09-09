import Foundation
import CoreGraphics

/// A user-level gesture derived from raw input events.
struct Gesture {
    enum Kind { case click, doubleClick, rightClick, drag, type, key, scroll, cursor }
    let kind: Kind
    let startT: Double
    let endT: Double
    let point: CGPoint        // global points
    var endPoint: CGPoint? = nil
    var text: String = ""     // typed text or key label
    var scrollDY: Double = 0
    var pathLength: CGFloat = 0  // cursor gestures: total distance travelled (points)
    var radius: CGFloat = 0      // cursor gestures: half the bounding box diagonal (points)
}

/// Coalesces raw events into gestures. Calls `onBegin` when a new gesture starts
/// (so the recorder can grab a "before" frame) and `onGesture` when it completes.
final class Coalescer {
    var onBegin: ((RawInput) -> Void)?
    var onGesture: ((Gesture) -> Void)?

    private let queue: DispatchQueue
    private var downEvent: RawInput?
    private var typing: (start: RawInput, buf: String, lastT: Double)?
    private var scroll: (start: RawInput, dy: Double, lastT: Double)?
    private var typingGen = 0
    private var scrollGen = 0
    private var moveGen = 0
    private var moves: [(t: Double, p: CGPoint)] = []
    private(set) var lastPoint = CGPoint.zero

    /// Cursor movement without clicks counts once it lasts this long and travels this far.
    static let moveGap = 0.5
    static let cursorMinDuration = 0.7
    static let cursorMinPath: CGFloat = 400

    static let typingGap = 1.0
    static let scrollGap = 0.6
    static let dragThreshold: CGFloat = 6

    init(queue: DispatchQueue) { self.queue = queue }

    func handle(_ r: RawInput) {
        lastPoint = r.point
        switch r.kind {
        case .dragged:
            return
        case .moved:
            if let last = moves.last, r.t - last.t > Coalescer.moveGap { flushMoves() }
            moves.append((r.t, r.point))
            if moves.count > 20_000 { moves.removeFirst(10_000) }
            moveGen += 1
            let gen = moveGen
            queue.asyncAfter(deadline: .now() + Coalescer.moveGap) { [weak self] in
                guard let self = self, self.moveGen == gen else { return }
                self.flushMoves()
            }
            return
        case .leftDown, .rightDown:
            flushMoves(); flushTyping(); flushScroll()
            downEvent = r
            onBegin?(r)
        case .leftUp:
            guard let d = downEvent else { return }
            downEvent = nil
            let dist = hypot(r.point.x - d.point.x, r.point.y - d.point.y)
            if dist > Coalescer.dragThreshold {
                onGesture?(Gesture(kind: .drag, startT: d.t, endT: r.t, point: d.point, endPoint: r.point))
            } else {
                let kind: Gesture.Kind = r.clickState >= 2 ? .doubleClick : .click
                onGesture?(Gesture(kind: kind, startT: d.t, endT: r.t, point: d.point))
            }
        case .rightUp:
            guard let d = downEvent else { return }
            downEvent = nil
            onGesture?(Gesture(kind: .rightClick, startT: d.t, endT: r.t, point: d.point))
        case .scroll:
            flushMoves(); flushTyping()
            if scroll == nil { onBegin?(r); scroll = (r, 0, r.t) }
            scroll!.dy += r.scrollDY
            scroll!.lastT = r.t
            scrollGen += 1
            let gen = scrollGen
            queue.asyncAfter(deadline: .now() + Coalescer.scrollGap) { [weak self] in
                guard let self = self, self.scrollGen == gen else { return }
                self.flushScroll()
            }
        case .keyDown:
            flushMoves(); flushScroll()
            if isPrintable(r) {
                if typing == nil { onBegin?(r); typing = (r, "", r.t) }
                typing!.buf += r.chars
                typing!.lastT = r.t
                scheduleTypingFlush()
            } else if r.keyCode == 51, typing != nil, !typing!.buf.isEmpty {
                typing!.buf.removeLast()
                typing!.lastT = r.t
                scheduleTypingFlush()
            } else {
                flushTyping()
                onBegin?(r)
                onGesture?(Gesture(kind: .key, startT: r.t, endT: r.t, point: r.point, text: keyLabel(r)))
            }
        }
    }

    private func scheduleTypingFlush() {
        typingGen += 1
        let gen = typingGen
        queue.asyncAfter(deadline: .now() + Coalescer.typingGap) { [weak self] in
            guard let self = self, self.typingGen == gen else { return }
            self.flushTyping()
        }
    }

    func flushTyping() {
        guard let ty = typing else { return }
        typing = nil
        typingGen += 1
        if ty.buf.isEmpty { return }
        onGesture?(Gesture(kind: .type, startT: ty.start.t, endT: ty.lastT, point: ty.start.point, text: ty.buf))
    }

    func flushScroll() {
        guard let sc = scroll else { return }
        scroll = nil
        scrollGen += 1
        onGesture?(Gesture(kind: .scroll, startT: sc.start.t, endT: sc.lastT, point: sc.start.point, scrollDY: sc.dy))
    }

    /// Emits a cursor gesture for sustained movement without clicks. Reported as-is, never interpreted.
    func flushMoves() {
        let m = moves
        moves = []
        moveGen += 1
        guard m.count >= 3, let first = m.first, let last = m.last else { return }
        let duration = last.t - first.t
        var path: CGFloat = 0
        var minX = first.p.x, maxX = first.p.x, minY = first.p.y, maxY = first.p.y
        for i in 1..<m.count {
            path += hypot(m[i].p.x - m[i - 1].p.x, m[i].p.y - m[i - 1].p.y)
            minX = min(minX, m[i].p.x); maxX = max(maxX, m[i].p.x)
            minY = min(minY, m[i].p.y); maxY = max(maxY, m[i].p.y)
        }
        guard duration >= Coalescer.cursorMinDuration, path >= Coalescer.cursorMinPath else { return }
        let center = CGPoint(x: (minX + maxX) / 2, y: (minY + maxY) / 2)
        let radius = hypot(maxX - minX, maxY - minY) / 2
        onGesture?(Gesture(kind: .cursor, startT: first.t, endT: last.t, point: center, pathLength: path, radius: radius))
    }

    func flushAll() { flushMoves(); flushTyping(); flushScroll() }
}
