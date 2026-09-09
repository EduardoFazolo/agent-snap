import Foundation
import AppKit
import QuartzCore
import CoreMedia

/// Records a video of the main display plus a timestamped log of everything that happened:
/// every frame's arrival and dirty area, every gesture, every window switch. Accessibility
/// names are attached when an app answers quickly; nothing waits on them.
final class Recorder {
    let outDir: URL
    let options: Options
    /// Bundle id of the recorder UI itself; interactions with it are ignored.
    var ignoreBundleId: String?
    /// Global rect (points, top-left origin) of our own menu bar item; clicks inside are ignored.
    var ignoreRect: CGRect?
    var onStep: ((Step) -> Void)?

    private let capture = ScreenCapture()
    private var writer: VideoWriter?
    private let tap = InputTap()
    private let tracker = WindowTracker()
    private let q = DispatchQueue(label: "agent-snap.recorder")
    private let writerQ = DispatchQueue(label: "agent-snap.writer", qos: .userInitiated)
    private var ticker: DispatchSourceTimer?
    private var lastFrameHostT: Double = 0
    private lazy var coalescer = Coalescer(queue: q)

    private var session: Session
    private var t0: Double = 0
    private let frameLock = NSLock()
    private var frameLog: [FrameLog] = []
    private var cursorLog: [CursorSample] = []
    private var lastCursorT: Double = -1
    private var videoOffsetSet = false

    private struct Pending { var target: AXTarget?; var window: WindowInfo; var secure: Bool }
    private var pending: Pending?
    private var pendingIgnored = false

    private var currentWindow: WindowInfo?
    private var segmentStart: Double = 0
    private var segmentInputs = 0
    private var pollTimer: DispatchSourceTimer?
    private var stopped = false

    init(outDir: URL, options: Options = .load()) {
        self.outDir = outDir
        self.options = options
        self.session = Session(startedAt: Date(), width: 0, height: 0, scale: 1)
    }

    func start() async throws {
        try FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)
        tracker.captureURLs = options.captureURLs
        let logURL = outDir.appendingPathComponent("ax.log")
        FileManager.default.createFile(atPath: logURL.path, contents: nil)
        if let h = try? FileHandle(forWritingTo: logURL) {
            let start = CACurrentMediaTime()
            tracker.debugLog = { line in
                h.write((String(format: "%7.2f ", CACurrentMediaTime() - start) + line + "\n").data(using: .utf8)!)
            }
        }
        t0 = CACurrentMediaTime()
        try await capture.start()
        session.width = capture.width
        session.height = capture.height
        session.scale = capture.scale
        session.video = "recording.mov"
        writer = try VideoWriter(url: outDir.appendingPathComponent("recording.mov"), width: capture.width, height: capture.height)
        writer?.log = tracker.debugLog

        capture.onFrame = { [weak self] frame, dirty in self?.handleFrame(frame, dirty) }
        coalescer.onBegin = { [weak self] raw in self?.beginGesture(raw) }
        coalescer.onGesture = { [weak self] g in self?.finishGesture(g) }
        tap.onEvent = { [weak self] raw in
            self?.q.async {
                guard let self = self, !self.stopped else { return }
                if raw.kind == .moved || raw.kind == .dragged {
                    let t = raw.t - self.t0
                    if t - self.lastCursorT >= 0.05 {
                        let p = self.capture.toPixels(raw.point)
                        self.cursorLog.append(CursorSample(t: t, x: Int(p.x), y: Int(p.y)))
                        self.lastCursorT = t
                    }
                    self.coalescer.handle(raw)
                    return
                }
                self.segmentInputs += 1
                self.coalescer.handle(raw)
            }
        }
        try tap.start()

        // Constant frame rate: when the screen is static SCK sends nothing, so re-append the last frame.
        let tick = DispatchSource.makeTimerSource(queue: writerQ)
        tick.schedule(deadline: .now(), repeating: .milliseconds(Int(1000 / VideoWriter.fps)))
        tick.setEventHandler { [weak self] in
            guard let self = self, let w = self.writer else { return }
            let now = CACurrentMediaTime()
            if now - self.lastFrameHostT >= 1.0 / Double(VideoWriter.fps) - 0.004 {
                w.appendDuplicate(at: CMClockGetTime(CMClockGetHostTimeClock()))
            }
        }
        tick.resume()
        ticker = tick

        let timer = DispatchSource.makeTimerSource(queue: q)
        timer.schedule(deadline: .now(), repeating: .milliseconds(300))
        timer.setEventHandler { [weak self] in self?.pollWindow() }
        timer.resume()
        pollTimer = timer
        fputs("recording \(capture.width)x\(capture.height) @\(capture.scale)x → \(outDir.path)\nCtrl+C to stop.\n", stderr)
    }

    func stop() async -> Session {
        pollTimer?.cancel()
        tap.stop()
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            q.async {
                self.coalescer.flushAll()
                self.stopped = true
                let now = CACurrentMediaTime() - self.t0
                if let w = self.currentWindow {
                    self.session.timeline.append(WindowSegment(start: self.segmentStart, end: now, window: w, inputEvents: self.segmentInputs))
                }
                c.resume()
            }
        }
        ticker?.cancel()
        await capture.stop()
        writerQ.sync {}
        await writer?.finish()
        session.frames = frameLock.withLock { frameLog }
        let cursor: [CursorSample] = await withCheckedContinuation { c in q.async { c.resume(returning: self.cursorLog) } }
        session.cursor = cursor
        let snapshot: Session = await withCheckedContinuation { c in q.async { c.resume(returning: self.session) } }
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        enc.dateEncodingStrategy = .iso8601
        if let data = try? enc.encode(snapshot) {
            try? data.write(to: outDir.appendingPathComponent("session.json"))
        }
        fputs("video frames: \(writer?.frames ?? 0)\n", stderr)
        return snapshot
    }

    private var px: (CGRect) -> CGRect { { [capture] in capture.toPixels($0) } }

    // MARK: frames (capture queue)

    private func handleFrame(_ frame: Frame, _ dirty: [CGRect]) {
        guard let w = writer else { return }
        writerQ.async {
            w.append(frame)
            self.lastFrameHostT = frame.t
            if !self.videoOffsetSet, w.firstPTS.isValid {
                self.videoOffsetSet = true
                let off = w.firstPTS.seconds - self.t0
                self.q.async { self.session.videoOffset = off }
            }
        }
        let union = dirty.reduce(CGRect.null) { $0.union($1) }
        frameLock.lock()
        frameLog.append(FrameLog(t: frame.t - t0, dirty: union.isNull ? nil : RectI(union)))
        frameLock.unlock()
    }

    // MARK: gestures (q)

    private func beginGesture(_ raw: RawInput) {
        guard !stopped else { return }
        let win = tracker.front(toPixels: px)
        if let mine = ignoreBundleId, win.bundleId == mine { pendingIgnored = true; return }
        if let r = ignoreRect, r.contains(raw.point), raw.kind == .leftDown || raw.kind == .rightDown { pendingIgnored = true; return }
        pendingIgnored = false
        guard pending == nil else { return }
        var target: AXTarget?
        var secure = false
        switch raw.kind {
        case .leftDown, .rightDown:
            if tracker.elementAtPointIsOwnProcess(raw.point) { pendingIgnored = true; return }
            target = tracker.hitTest(raw.point, toPixels: px)
        case .keyDown:
            secure = tracker.focusedIsSecure()
            target = tracker.focused(toPixels: px)
        default: break
        }
        pending = Pending(target: target, window: win, secure: secure)
    }

    private func finishGesture(_ g: Gesture) {
        guard !stopped else { return }
        if g.kind == .cursor {
            let win = tracker.front(toPixels: px)
            if let mine = ignoreBundleId, win.bundleId == mine { return }
            let target = tracker.hitTest(g.point, toPixels: px)
            let pt = PointI(capture.toPixels(g.point))
            let secs = String(format: "%.1f", g.endT - g.startT)
            let r = Int((g.radius * capture.scale).rounded())
            let path = Int((g.pathLength * capture.scale).rounded())
            let over = target.map { " over \($0.describe)" } ?? ""
            var step = Step(index: 0, t: g.startT - t0, kind: .cursor,
                            label: "Cursor moved\(over) for \(secs)s, no click (path \(path)px, within \(r)px of (\(pt.x),\(pt.y)))",
                            point: pt, endPoint: nil, text: nil, target: target, window: win, before: nil, after: nil, dirty: nil)
            step.endT = g.endT - t0
            append(step)
            return
        }
        if pendingIgnored { pendingIgnored = false; pending = nil; return }
        guard let p = pending else { return }
        pending = nil
        let target = p.target
        let pt = PointI(capture.toPixels(g.point))
        var step = Step(index: 0, t: g.startT - t0, kind: .click, label: "", point: pt,
                        endPoint: g.endPoint.map { PointI(capture.toPixels($0)) }, text: nil, target: target,
                        window: p.window, before: nil, after: nil, dirty: nil)
        step.endT = g.endT - t0
        let tdesc = target.map { " " + $0.describe } ?? ""
        let anchorOK: Bool = {
            guard let b = target?.bounds else { return false }
            return b.w < capture.width / 2 && b.h < capture.height / 2
        }()
        switch g.kind {
        case .click: step.kind = .click; step.label = "Click\(tdesc.isEmpty ? " at (\(pt.x),\(pt.y))" : tdesc)"
        case .doubleClick: step.kind = .doubleClick; step.label = "Double-click\(tdesc)"
        case .rightClick: step.kind = .rightClick; step.label = "Right-click\(tdesc)"
        case .drag:
            step.kind = .drag
            let e = step.endPoint!
            step.label = "Drag\(tdesc) to (\(e.x),\(e.y))"
        case .type:
            step.kind = .type
            step.text = (p.secure || !options.captureTypedText) ? "[redacted]" : g.text
            step.label = "Type \"\(step.text!)\"" + (tdesc.isEmpty ? "" : " in\(tdesc)")
            if anchorOK, let b = target?.bounds { step.point = PointI(x: b.x + b.w / 2, y: b.y + b.h / 2) }
        case .key:
            step.kind = .key; step.text = g.text
            step.label = "Press \(g.text)"
            if anchorOK, let b = target?.bounds { step.point = PointI(x: b.x + b.w / 2, y: b.y + b.h / 2) }
        case .scroll:
            step.kind = .scroll
            step.label = "Scroll \(g.scrollDY < 0 ? "down" : "up")\(tdesc.isEmpty ? "" : " in\(tdesc)")"
        case .cursor:
            return
        }
        append(step)
    }

    private func append(_ step: Step) {
        var s = step
        s.index = session.steps.count + 1
        session.steps.append(s)
        fputs("[\(fmtT(s.t))] \(s.label)\n", stderr)
        onStep?(s)
    }

    // MARK: windows (q)

    private func pollWindow() {
        guard !stopped else { return }
        let w = tracker.front(toPixels: px)
        if let mine = ignoreBundleId, w.bundleId == mine { return }
        let now = CACurrentMediaTime() - t0
        if let cur = currentWindow {
            if cur.key == w.key {
                if cur != w { currentWindow = w }
                return
            }
            coalescer.flushAll()
            session.timeline.append(WindowSegment(start: segmentStart, end: now, window: cur, inputEvents: segmentInputs))
        }
        currentWindow = w
        segmentStart = now
        segmentInputs = 0
        if session.timeline.isEmpty && session.steps.isEmpty { return }
        let step = Step(index: 0, t: now, kind: .windowSwitch, label: "Switch to \(w.describe)", point: nil, endPoint: nil,
                        text: nil, target: nil, window: w, before: nil, after: nil, dirty: nil)
        append(step)
    }
}
