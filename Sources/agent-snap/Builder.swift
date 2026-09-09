import Foundation
import AppKit
import AVFoundation
import CoreGraphics
import Vision

/// Turns a recording (video + event log) into flow.md with focused composites.
final class Builder {
    let dir: URL
    var session: Session
    let options: Options

    static let pad: CGFloat = 60
    static let minSize = CGSize(width: 700, height: 450)
    static let maxSize = CGSize(width: 1800, height: 1200)
    static let diffRadius: CGFloat = 650
    static let cell = 16
    static let margin: CGFloat = 28, gap: CGFloat = 70, labelH: CGFloat = 46
    static let minPanelScale: CGFloat = 0.42
    static let briefDwell = 1.5
    static let settleMax = 2.0
    static let screenUpdateFraction = 0.15
    var compositeWidth: CGFloat { CGFloat(options.compositeWidth) }
    var panelsPerImage: Int { max(1, min(6, options.panelsPerImage)) }
    var quiet: Double { Double(options.quietMs) / 1000 }

    struct Panel {
        var image: CGImage
        var path: String
        var rect: CGRect
        var label: String?
        var marker: CGPoint?
        var marker2: CGPoint?
        var scale: CGFloat = 1
    }

    struct Run {
        var window: WindowInfo
        var steps: [Step] = []
        var start: Double
        var end: Double
        var inputs: Int
    }

    private var frames: FrameSource?
    private var imageCache: [String: CGImage] = [:]

    init(dir: URL, options: Options = .load()) throws {
        self.dir = dir
        self.options = options
        let data = try Data(contentsOf: dir.appendingPathComponent("session.json"))
        let dec = JSONDecoder()
        dec.dateDecodingStrategy = .iso8601
        session = try dec.decode(Session.self, from: data)
        if let v = session.video {
            let src = FrameSource(url: dir.appendingPathComponent(v), offset: session.videoOffset)
            if src.duration > 0 { frames = src }
        }
    }

    struct Stats {
        var steps = 0
        var images = 0
        var imageTokens = 0
        var textTokens = 0
        var tokens: Int { imageTokens + textTokens }
    }
    private(set) var stats = Stats()

    /// Rough Claude cost of one image: downscaled to ≤1568px long edge and ≤1.15MP, then w*h/750.
    static func imageTokens(width: Int, height: Int) -> Int {
        let w = Double(width), h = Double(height)
        let s = min(1, 1568 / max(w, h), (1_150_000 / (w * h)).squareRoot())
        return Int((w * s) * (h * s) / 750)
    }

    func build() throws -> URL {
        try FileManager.default.createDirectory(at: dir.appendingPathComponent("frames"), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: dir.appendingPathComponent("composites"), withIntermediateDirectories: true)
        for f in (try? FileManager.default.contentsOfDirectory(at: dir.appendingPathComponent("composites"), includingPropertiesForKeys: nil)) ?? [] {
            try? FileManager.default.removeItem(at: f)
        }

        if frames != nil {
            synthesizeScreenUpdates()
            session.steps.sort { $0.t < $1.t }
            for i in session.steps.indices { session.steps[i].index = i + 1 }
            for i in session.steps.indices {
                var s = session.steps[i]
                extractFrames(&s)
                labelByOCR(&s)
                session.steps[i] = s
            }
        } else {
            session.steps.sort { $0.t < $1.t }
            for i in session.steps.indices { session.steps[i].index = i + 1 }
        }

        let runs = groupRuns()
        var md = "# agent-snap session\n\n"
        let dur = session.timeline.last?.end ?? session.steps.last?.t ?? 0
        md += "- recorded: \(ISO8601DateFormatter().string(from: session.startedAt)), duration \(fmtT(dur))\n"
        md += "- screen: \(session.width)×\(session.height) px @\(session.scale)x (all coordinates below are pixels in that space)\n"
        if let v = session.video { md += "- video: \(dir.appendingPathComponent(v).path) (full recording; timestamps below index into it)\n" }
        md += "- \(session.steps.count) steps across \(runs.count) window visits\n{{COST}}\n"
        md += "The images shown inline are the flow. The full frames and video listed under each section are only for zooming in if a step is unclear.\n\n"

        var runNo = 0
        for r in runs {
            var run = r
            let realSteps = run.steps.filter { $0.kind != .windowSwitch }
            let dwell = run.end - run.start
            if realSteps.isEmpty && (dwell < Builder.briefDwell || run.inputs == 0) {
                md += "_\(fmtT(run.start)) briefly on \(run.window.describe) (\(String(format: "%.1f", dwell))s, no input)_\n\n"
                continue
            }
            runNo += 1
            md += "## \(runNo). \(run.window.describe)\n"
            md += "_\(fmtT(run.start)) → \(fmtT(run.end))_\n\n"

            run.steps = mergeScrolls(run.steps)
            let (panels, noChange) = makePanels(run)
            for (imgIdx, chunk) in pack(panels).enumerated() {
                let name = String(format: "composites/run-%02d-%c.png", runNo, 97 + imgIdx)
                if let img = compose(chunk) {
                    let url = dir.appendingPathComponent(name)
                    ImageIO.savePNG(img, to: url)
                    md += "![\(run.window.app) steps](\(url.path))\n\n"
                    stats.images += 1
                    stats.imageTokens += Builder.imageTokens(width: img.width, height: img.height)
                }
            }
            for s in run.steps where s.kind != .windowSwitch {
                var line = "\(s.index). `\(fmtT(s.t))` \(s.label)"
                if let p = s.point, ![.scroll, .screenUpdate, .cursor].contains(s.kind) { line += " @(\(p.x),\(p.y))" }
                if noChange.contains(s.index) { line += " _(no visible change)_" }
                md += line + "\n"
            }
            var seen = Set<String>()
            let files = run.steps.flatMap { [$0.before, $0.after].compactMap { $0 } }.filter { seen.insert($0).inserted }
            if !files.isEmpty {
                md += "\n<details><summary>full frames</summary>\n\n" + files.map { "- \(dir.appendingPathComponent($0).path)" }.joined(separator: "\n") + "\n\n</details>\n"
            }
            md += "\n"
        }
        stats.steps = session.steps.count
        stats.textTokens = md.count / 4 + 12
        md = md.replacingOccurrences(of: "{{COST}}", with: "- estimated prompt cost: ~\(stats.tokens) tokens (\(stats.images) images ~\(stats.imageTokens), text ~\(stats.textTokens))\n")
        let out = dir.appendingPathComponent("flow.md")
        try md.write(to: out, atomically: true, encoding: .utf8)
        return out
    }

    // MARK: video → frames

    static let cursorRadius: CGFloat = 70

    /// Cursor position at session time t (nearest earlier sample).
    private func cursorAt(_ t: Double) -> CGPoint? {
        let c = session.cursor
        guard !c.isEmpty else { return nil }
        var lo = 0, hi = c.count - 1
        while lo < hi {
            let mid = (lo + hi + 1) / 2
            if c[mid].t <= t { lo = mid } else { hi = mid - 1 }
        }
        return CGPoint(x: c[lo].x, y: c[lo].y)
    }

    /// A dirty rect that is just the cursor moving (small, on the cursor).
    private func isCursorOnly(_ d: RectI, at t: Double) -> Bool {
        guard d.w <= 110, d.h <= 110, let c = cursorAt(t) else { return false }
        let r = d.cg
        let dx = max(r.minX - c.x, 0, c.x - r.maxX), dy = max(r.minY - c.y, 0, c.y - r.maxY)
        return hypot(dx, dy) <= Builder.cursorRadius
    }

    /// Time the screen stopped changing after `from`: last real dirty frame before a quiet gap, capped.
    private func settleTime(after from: Double) -> Double {
        var last = from
        for f in session.frames where f.t > from {
            if f.t - last > quiet { break }
            if let d = f.dirty, !isCursorOnly(d, at: f.t) { last = f.t }
            if f.t - from > Builder.settleMax { break }
        }
        return last
    }

    private func dirtyUnion(_ a: Double, _ b: Double) -> RectI? {
        var u = CGRect.null
        for f in session.frames where f.t > a && f.t <= b {
            if let d = f.dirty, !isCursorOnly(d, at: f.t) { u = u.union(d.cg) }
        }
        return u.isNull ? nil : RectI(u)
    }

    /// Copies `before` cells onto `after` around the cursor positions, so the cursor never counts as change.
    private func maskCursor(_ ga: inout [UInt8], _ gb: [UInt8], gw: Int, gh: Int, times: [Double?]) {
        let c = Builder.cell
        for t in times.compactMap({ $0 }) {
            guard let p = cursorAt(t) else { continue }
            let r = Builder.cursorRadius
            let x0 = max(0, Int((p.x - r) / CGFloat(c))), x1 = min(gw - 1, Int((p.x + r) / CGFloat(c)))
            let y0 = max(0, Int((p.y - r) / CGFloat(c))), y1 = min(gh - 1, Int((p.y + r) / CGFloat(c)))
            guard x1 >= x0, y1 >= y0 else { continue }
            for y in y0...y1 { for x in x0...x1 { ga[y * gw + x] = gb[y * gw + x] } }
        }
    }

    private func extractFrames(_ s: inout Step) {
        guard let src = frames else { return }
        let n = String(format: "s%04d-%@", Int(s.t * 10), s.kind.rawValue)
        func save(_ t: Double, _ suffix: String) -> String? {
            guard let img = src.frame(at: t) else { return nil }
            let rel = "frames/\(n)-\(suffix).png"
            ImageIO.savePNG(img, to: dir.appendingPathComponent(rel))
            imageCache[rel] = img
            return rel
        }
        let end = s.endT ?? s.t
        switch s.kind {
        case .windowSwitch:
            s.afterT = settleTime(after: s.t)
            s.after = save(s.afterT!, "after")
        case .cursor:
            s.afterT = end
            s.after = save(end, "after")
        case .screenUpdate:
            s.beforeT = s.t - 0.03
            s.before = save(s.beforeT!, "before")
            s.afterT = settleTime(after: s.t)
            s.after = save(s.afterT!, "after")
            s.dirty = dirtyUnion(s.beforeT!, s.afterT!)
        default:
            s.beforeT = s.t - 0.03
            s.before = save(s.beforeT!, "before")
            s.afterT = settleTime(after: end)
            s.after = save(s.afterT!, "after")
            s.dirty = dirtyUnion(s.beforeT!, s.afterT!)
        }
    }

    /// Large repaints with no input nearby become "Screen updated" steps.
    private func synthesizeScreenUpdates() {
        let total = Double(session.width * session.height)
        var busy: [(Double, Double)] = []
        for s in session.steps {
            let end = s.endT ?? s.t
            busy.append((s.t - 0.1, settleTime(after: end) + 1.0))
        }
        let switches = session.steps.filter { $0.kind == .windowSwitch }.map { $0.t }
        var i = 0
        var extra: [Step] = []
        let fs = session.frames
        while i < fs.count {
            let f = fs[i]
            defer { i += 1 }
            guard let d = f.dirty, Double(d.w * d.h) / total >= Builder.screenUpdateFraction else { continue }
            guard f.t > 1.0, !switches.contains(where: { f.t - $0 > -0.1 && f.t - $0 < 1.5 }),
                  !busy.contains(where: { f.t >= $0.0 && f.t <= $0.1 }) else { continue }
            let win = session.timeline.first { $0.start <= f.t && f.t <= $0.end }?.window
                ?? session.steps.last(where: { $0.t <= f.t })?.window
                ?? session.timeline.first?.window
            guard let w = win else { continue }
            var s = Step(index: 0, t: f.t, kind: .screenUpdate, label: "Screen updated (no input)",
                         point: PointI(x: d.x + d.w / 2, y: d.y + d.h / 2), endPoint: nil, text: nil, target: nil,
                         window: w, before: nil, after: nil, dirty: nil)
            s.endT = f.t
            extra.append(s)
            let st = settleTime(after: f.t)
            busy.append((f.t, st + 1.0))
            while i + 1 < fs.count, fs[i + 1].t <= st + 1.0 { i += 1 }
        }
        session.steps.append(contentsOf: extra)
    }

    // MARK: OCR labels

    private static let weakRoles: Set<String> = ["scroll area", "group", "web area", "unknown", "application", "window", "layout area", "text"]

    private func targetIsWeak(_ t: AXTarget?) -> Bool {
        guard let t = t else { return true }
        if let n = t.name, !n.isEmpty { return false }
        if let v = t.value, !v.isEmpty { return false }
        return Builder.weakRoles.contains(t.role ?? "unknown")
    }

    /// Reads the text under/near the click from the pixels when the app gave no usable name.
    private func labelByOCR(_ s: inout Step) {
        guard [.click, .doubleClick, .rightClick, .drag].contains(s.kind), targetIsWeak(s.target),
              let p = s.point, let path = s.before, let img = loadImage(path) else { return }
        let w = CGFloat(img.width), h = CGFloat(img.height)
        let crop = CGRect(x: max(0, CGFloat(p.x) - 320), y: max(0, CGFloat(p.y) - 110), width: 640, height: 220)
            .intersection(CGRect(x: 0, y: 0, width: w, height: h))
        guard let ci = img.cropping(to: crop) else { return }
        let req = VNRecognizeTextRequest()
        req.recognitionLevel = .accurate
        req.usesLanguageCorrection = false
        let handler = VNImageRequestHandler(cgImage: ci, options: [:])
        guard (try? handler.perform([req])) != nil, let obs = req.results, !obs.isEmpty else { return }
        let local = CGPoint(x: CGFloat(p.x) - crop.minX, y: CGFloat(p.y) - crop.minY)
        var best: (String, CGFloat)?
        for o in obs {
            guard let c = o.topCandidates(1).first, c.confidence > 0.3 else { continue }
            let bb = o.boundingBox
            let r = CGRect(x: bb.minX * crop.width, y: (1 - bb.maxY) * crop.height, width: bb.width * crop.width, height: bb.height * crop.height)
            let dist: CGFloat = r.insetBy(dx: -12, dy: -12).contains(local) ? 0
                : hypot(max(r.minX - local.x, 0, local.x - r.maxX), max(r.minY - local.y, 0, local.y - r.maxY))
            if dist <= 90, best == nil || dist < best!.1 { best = (c.string, dist) }
        }
        guard let (text, dist) = best else { return }
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty else { return }
        let verb: String
        switch s.kind { case .doubleClick: verb = "Double-click"; case .rightClick: verb = "Right-click"; case .drag: verb = "Drag"; default: verb = "Click" }
        let near = dist == 0 ? "" : " near"
        var label = "\(verb)\(near) \"\(t)\""
        if s.kind == .drag, let e = s.endPoint { label += " to (\(e.x),\(e.y))" }
        s.label = label + " (text read from screen)"
        s.target = AXTarget(role: "text", name: t, value: nil, bounds: s.target?.bounds)
    }

    // MARK: grouping

    private func groupRuns() -> [Run] {
        var runs: [Run] = []
        for s in session.steps {
            let seg = session.timeline.first { $0.start <= s.t + 0.01 && s.t <= $0.end + 0.01 && $0.window.key == s.window.key }
            if let last = runs.last, last.window.key == s.window.key {
                if s.kind != .windowSwitch { runs[runs.count - 1].steps.append(s) }
                if let seg = seg {
                    runs[runs.count - 1].end = max(runs[runs.count - 1].end, seg.end)
                    runs[runs.count - 1].inputs += seg.inputEvents
                }
            } else {
                runs.append(Run(window: s.window, steps: [s], start: seg?.start ?? s.t, end: seg?.end ?? s.t, inputs: seg?.inputEvents ?? 1))
            }
        }
        for i in runs.indices {
            if let lastT = runs[i].steps.last?.t, lastT > runs[i].end { runs[i].end = lastT }
            if runs[i].steps.contains(where: { $0.kind != .windowSwitch }) { runs[i].inputs = max(runs[i].inputs, 1) }
        }
        return runs
    }

    /// Consecutive scroll steps become one step (first before, last after).
    private func mergeScrolls(_ steps: [Step]) -> [Step] {
        var out: [Step] = []
        var dirs: [String] = []
        for s in steps {
            if s.kind == .scroll, let last = out.last, last.kind == .scroll {
                dirs.append(s.label.hasPrefix("Scroll down") ? "down" : "up")
                out[out.count - 1].after = s.after
                out[out.count - 1].point = s.point
                var parts: [String] = []
                var i = 0
                while i < dirs.count {
                    var j = i
                    while j + 1 < dirs.count && dirs[j + 1] == dirs[i] { j += 1 }
                    let n = j - i + 1
                    parts.append(n > 1 ? "\(dirs[i]) ×\(n)" : dirs[i])
                    i = j + 1
                }
                out[out.count - 1].label = "Scroll " + parts.joined(separator: ", ")
            } else {
                if s.kind == .scroll { dirs = [s.label.hasPrefix("Scroll down") ? "down" : "up"] }
                out.append(s)
            }
        }
        return out
    }

    // MARK: panels

    private func loadImage(_ rel: String?) -> CGImage? {
        guard let r = rel else { return nil }
        if let c = imageCache[r] { return c }
        let img = ImageIO.loadPNG(dir.appendingPathComponent(r))
        if let i = img { imageCache[r] = i }
        return img
    }

    private func makePanels(_ run: Run) -> ([Panel], Set<Int>) {
        var panels: [Panel] = []
        var noChange = Set<Int>()
        var prevRect: CGRect?
        let real = run.steps.filter { $0.kind != .windowSwitch && $0.kind != .cursor }

        if real.isEmpty, let sw = run.steps.first, let path = sw.after, let img = loadImage(path) {
            let r = clamp(CGRect(x: 0, y: 0, width: CGFloat(img.width), height: CGFloat(img.height)), in: img, window: run.window)
            panels.append(Panel(image: img, path: path, rect: r, label: "Landed on \(run.window.app)", marker: nil))
            return (panels, noChange)
        }

        for (i, s) in real.enumerated() {
            let before = loadImage(s.before)
            let after = loadImage(s.after)
            guard let ref = after ?? before else { continue }
            if i > 0, let b = s.before, let a = s.after, sameImage(b, a, before, after, times: [s.beforeT, s.afterT]) {
                noChange.insert(s.index)
                continue
            }
            var rect = focusRect(step: s, before: before, after: after, ref: ref, window: run.window)
            if let p = prevRect {
                if p.insetBy(dx: -8, dy: -8).contains(rect) { rect = p }
                else if p.intersects(rect) {
                    let u = p.union(rect)
                    if u.width <= Builder.maxSize.width && u.height <= Builder.maxSize.height { rect = u }
                }
            }
            prevRect = rect
            if i > 0, ![.type, .key, .drag].contains(s.kind), let b = before, let a = after,
               b.width == a.width, b.height == a.height, changedFraction(before: b, after: a, in: rect, times: [s.beforeT, s.afterT]) < 0.02 {
                noChange.insert(s.index)
                continue
            }
            if i == 0, let b = before, let bp = s.before {
                panels.append(Panel(image: b, path: bp, rect: rect, label: "Before", marker: nil))
            }
            if let a = after, let ap = s.after {
                let showMarker = s.kind != .scroll && s.kind != .screenUpdate
                panels.append(Panel(image: a, path: ap, rect: rect, label: s.label,
                                    marker: showMarker ? s.point?.cg : nil, marker2: showMarker ? s.endPoint?.cg : nil))
            }
        }
        return (panels, noChange)
    }

    private var gridCache: [String: [UInt8]] = [:]

    private func sameImage(_ pa: String, _ pb: String, _ a: CGImage?, _ b: CGImage?, times: [Double?]) -> Bool {
        if pa == pb { return true }
        guard let a = a, let b = b, a.width == b.width, a.height == b.height else { return false }
        let c = Builder.cell
        let gw = a.width / c, gh = a.height / c
        guard let gb = grayGrid(a, gw, gh), var ga = grayGrid(b, gw, gh) else { return false }
        maskCursor(&ga, gb, gw: gw, gh: gh, times: times)
        var changed = 0
        for i in 0..<ga.count where abs(Int(ga[i]) - Int(gb[i])) > 8 { changed += 1 }
        return changed <= 3
    }

    private func pack(_ panels: [Panel]) -> [[Panel]] {
        var out: [[Panel]] = []
        var cur: [Panel] = []
        let maxH = compositeWidth * 0.6
        func fit(_ ps: [Panel]) -> [Panel] {
            var ps = ps
            for i in ps.indices { ps[i].scale = min(1, maxH / ps[i].rect.height) }
            let w = Builder.margin * 2 + Builder.gap * CGFloat(ps.count - 1) + ps.map { $0.rect.width * $0.scale }.reduce(0, +)
            if w > compositeWidth {
                let f = (compositeWidth - Builder.margin * 2 - Builder.gap * CGFloat(ps.count - 1)) / ps.map { $0.rect.width * $0.scale }.reduce(0, +)
                for i in ps.indices { ps[i].scale *= f }
            }
            return ps
        }
        for p in panels {
            let trial = fit(cur + [p])
            if cur.isEmpty || (trial.allSatisfy { $0.scale >= Builder.minPanelScale } && trial.count <= panelsPerImage) {
                cur.append(p)
            } else {
                out.append(fit(cur)); cur = [p]
            }
        }
        if !cur.isEmpty { out.append(fit(cur)) }
        return out
    }

    private func focusRect(step: Step, before: CGImage?, after: CGImage?, ref: CGImage, window: WindowInfo) -> CGRect {
        let W = CGFloat(ref.width), H = CGFloat(ref.height)
        let anchor = step.point?.cg ?? step.dirty?.cg.center ?? CGPoint(x: W / 2, y: H / 2)
        var box = CGRect(x: anchor.x - 40, y: anchor.y - 40, width: 80, height: 80)
        if let b = before, let a = after, b.width == a.width, b.height == a.height,
           let d = changedBBox(before: b, after: a, near: anchor, radius: Builder.diffRadius, times: [step.beforeT, step.afterT]) {
            box = box.union(d)
        } else if let d = step.dirty?.cg, d.width * d.height < W * H * 0.5 {
            box = box.union(d)
        }
        if let tb = step.target?.bounds?.cg, tb.width < W * 0.9, tb.height < H * 0.9 { box = box.union(tb) }
        if let e = step.endPoint?.cg { box = box.union(CGRect(x: e.x - 40, y: e.y - 40, width: 80, height: 80)) }
        box = box.insetBy(dx: -Builder.pad, dy: -Builder.pad)
        if box.width < Builder.minSize.width { box = box.insetBy(dx: -(Builder.minSize.width - box.width) / 2, dy: 0) }
        if box.height < Builder.minSize.height { box = box.insetBy(dx: 0, dy: -(Builder.minSize.height - box.height) / 2) }
        if box.width > Builder.maxSize.width {
            box = CGRect(x: anchor.x - Builder.maxSize.width / 2, y: box.minY, width: Builder.maxSize.width, height: box.height)
        }
        if box.height > Builder.maxSize.height {
            box = CGRect(x: box.minX, y: anchor.y - Builder.maxSize.height / 2, width: box.width, height: Builder.maxSize.height)
        }
        return clamp(box, in: ref, window: window)
    }

    private func clamp(_ r: CGRect, in img: CGImage, window: WindowInfo) -> CGRect {
        var bounds = CGRect(x: 0, y: 0, width: img.width, height: img.height)
        if let wb = window.bounds?.cg, wb.width > 200, wb.height > 200 { bounds = bounds.intersection(wb.insetBy(dx: -4, dy: -4)) }
        var out = r
        if out.width > bounds.width { out.size.width = bounds.width }
        if out.height > bounds.height { out.size.height = bounds.height }
        if out.minX < bounds.minX { out.origin.x = bounds.minX }
        if out.minY < bounds.minY { out.origin.y = bounds.minY }
        if out.maxX > bounds.maxX { out.origin.x = bounds.maxX - out.width }
        if out.maxY > bounds.maxY { out.origin.y = bounds.maxY - out.height }
        return out.integral
    }

    private func changedBBox(before: CGImage, after: CGImage, near: CGPoint, radius: CGFloat, times: [Double?] = []) -> CGRect? {
        let c = Builder.cell
        let gw = before.width / c, gh = before.height / c
        guard gw > 0, gh > 0, let gb = grayGrid(before, gw, gh), var ga = grayGrid(after, gw, gh) else { return nil }
        maskCursor(&ga, gb, gw: gw, gh: gh, times: times)
        var minX = Int.max, minY = Int.max, maxX = -1, maxY = -1
        let r2 = radius * radius
        for y in 0..<gh { for x in 0..<gw {
            let i = y * gw + x
            if abs(Int(gb[i]) - Int(ga[i])) > 10 {
                let cx = CGFloat(x * c + c / 2), cy = CGFloat(y * c + c / 2)
                let dx = cx - near.x, dy = cy - near.y
                if dx * dx + dy * dy <= r2 { minX = min(minX, x); minY = min(minY, y); maxX = max(maxX, x); maxY = max(maxY, y) }
            }
        } }
        guard maxX >= 0 else { return nil }
        return CGRect(x: minX * c, y: minY * c, width: (maxX - minX + 1) * c, height: (maxY - minY + 1) * c)
    }

    private func changedFraction(before: CGImage, after: CGImage, in rect: CGRect, times: [Double?] = []) -> CGFloat {
        let c = Builder.cell
        let gw = before.width / c, gh = before.height / c
        guard gw > 0, gh > 0, let gb = grayGrid(before, gw, gh), var ga = grayGrid(after, gw, gh) else { return 1 }
        maskCursor(&ga, gb, gw: gw, gh: gh, times: times)
        let x0 = max(0, Int(rect.minX) / c), x1 = min(gw - 1, Int(rect.maxX) / c)
        let y0 = max(0, Int(rect.minY) / c), y1 = min(gh - 1, Int(rect.maxY) / c)
        guard x1 >= x0, y1 >= y0 else { return 1 }
        var changed = 0, total = 0
        for y in y0...y1 { for x in x0...x1 {
            total += 1
            if abs(Int(gb[y * gw + x]) - Int(ga[y * gw + x])) > 10 { changed += 1 }
        } }
        return CGFloat(changed) / CGFloat(max(total, 1))
    }

    private var grayCache: [ObjectIdentifier: [UInt8]] = [:]
    private func grayGrid(_ img: CGImage, _ gw: Int, _ gh: Int) -> [UInt8]? {
        let key = ObjectIdentifier(img)
        if gw == img.width / Builder.cell, let g = grayCache[key] { return g }
        let cs = CGColorSpaceCreateDeviceGray()
        var buf = [UInt8](repeating: 0, count: gw * gh)
        let ok: Bool = buf.withUnsafeMutableBytes { raw in
            guard let ctx = CGContext(data: raw.baseAddress, width: gw, height: gh, bitsPerComponent: 8, bytesPerRow: gw,
                                      space: cs, bitmapInfo: CGImageAlphaInfo.none.rawValue) else { return false }
            ctx.interpolationQuality = .medium
            ctx.draw(img, in: CGRect(x: 0, y: 0, width: gw, height: gh))
            return true
        }
        if ok, gw == img.width / Builder.cell { grayCache[key] = buf }
        return ok ? buf : nil
    }

    // MARK: composite

    private func compose(_ panels: [Panel]) -> CGImage? {
        guard !panels.isEmpty else { return nil }
        let margin = Builder.margin, gap = Builder.gap, labelH = Builder.labelH, border: CGFloat = 1
        let scaled = panels.map { ($0, $0.scale, CGSize(width: $0.rect.width * $0.scale, height: $0.rect.height * $0.scale)) }
        let totalW = margin * 2 + scaled.map { $0.2.width }.reduce(0, +) + gap * CGFloat(panels.count - 1)
        let maxH = scaled.map { $0.2.height }.max() ?? 0
        let totalH = margin * 2 + labelH + maxH
        let W = Int(totalW.rounded(.up)), H = Int(totalH.rounded(.up))
        guard let ctx = CGContext(data: nil, width: W, height: H, bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: W, height: H))
        ctx.translateBy(x: 0, y: CGFloat(H))
        ctx.scaleBy(x: 1, y: -1)
        let ns = NSGraphicsContext(cgContext: ctx, flipped: true)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ns
        let labelAttrs: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: 24, weight: .semibold),
            .foregroundColor: NSColor(calibratedRed: 0.86, green: 0.30, blue: 0.28, alpha: 1),
        ]
        let red = CGColor(red: 0.90, green: 0.22, blue: 0.20, alpha: 1)
        var x = margin
        for (i, (p, s, size)) in scaled.enumerated() {
            let top = margin + labelH
            let frame = CGRect(x: x, y: top, width: size.width, height: size.height)
            if let crop = p.image.cropping(to: p.rect) {
                ctx.saveGState()
                ctx.translateBy(x: frame.minX, y: frame.maxY)
                ctx.scaleBy(x: 1, y: -1)
                ctx.draw(crop, in: CGRect(x: 0, y: 0, width: frame.width, height: frame.height))
                ctx.restoreGState()
            }
            ctx.setStrokeColor(CGColor(gray: 0.75, alpha: 1)); ctx.setLineWidth(border)
            ctx.stroke(frame.insetBy(dx: -0.5, dy: -0.5))
            if let l = p.label {
                let str = NSAttributedString(string: l, attributes: labelAttrs)
                let sz = str.size()
                str.draw(with: NSRect(x: frame.minX, y: margin + (labelH - sz.height) / 2, width: frame.width, height: sz.height + 4),
                         options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine])
            }
            for m in [p.marker, p.marker2].compactMap({ $0 }) where p.rect.contains(m) {
                drawArrow(ctx, to: CGPoint(x: frame.minX + (m.x - p.rect.minX) * s, y: frame.minY + (m.y - p.rect.minY) * s), color: red)
            }
            if i < scaled.count - 1 {
                let y = top + maxH / 2
                let from = CGPoint(x: frame.maxX + 12, y: y), to = CGPoint(x: frame.maxX + gap - 12, y: y)
                ctx.setStrokeColor(CGColor(gray: 0.45, alpha: 1)); ctx.setLineWidth(4)
                ctx.move(to: from); ctx.addLine(to: to); ctx.strokePath()
                ctx.setFillColor(CGColor(gray: 0.45, alpha: 1))
                ctx.move(to: CGPoint(x: to.x + 10, y: y)); ctx.addLine(to: CGPoint(x: to.x - 10, y: y - 10))
                ctx.addLine(to: CGPoint(x: to.x - 10, y: y + 10)); ctx.closePath(); ctx.fillPath()
            }
            x += size.width + gap
        }
        NSGraphicsContext.restoreGraphicsState()
        return ctx.makeImage()
    }

    private func drawArrow(_ ctx: CGContext, to p: CGPoint, color: CGColor) {
        let len: CGFloat = 110
        let dir = CGPoint(x: 1 / sqrt(2), y: 1 / sqrt(2))
        let tail = CGPoint(x: p.x - dir.x * len, y: p.y - dir.y * len)
        let head = CGPoint(x: p.x - dir.x * 22, y: p.y - dir.y * 22)
        ctx.setStrokeColor(color); ctx.setFillColor(color)
        ctx.setLineWidth(6); ctx.setLineCap(.round)
        ctx.move(to: tail); ctx.addLine(to: head); ctx.strokePath()
        let perp = CGPoint(x: -dir.y, y: dir.x)
        let base = CGPoint(x: p.x - dir.x * 34, y: p.y - dir.y * 34)
        ctx.move(to: CGPoint(x: p.x - dir.x * 12, y: p.y - dir.y * 12))
        ctx.addLine(to: CGPoint(x: base.x + perp.x * 13, y: base.y + perp.y * 13))
        ctx.addLine(to: CGPoint(x: base.x - perp.x * 13, y: base.y - perp.y * 13))
        ctx.closePath(); ctx.fillPath()
        ctx.setLineWidth(3)
        ctx.strokeEllipse(in: CGRect(x: p.x - 9, y: p.y - 9, width: 18, height: 18))
    }
}

/// Exact-time frame access into the recording.
final class FrameSource {
    private let asset: AVURLAsset
    private let gen: AVAssetImageGenerator
    private let offset: Double
    private var cache: [Int: CGImage] = [:]
    private(set) var duration: Double = 0

    init(url: URL, offset: Double) {
        asset = AVURLAsset(url: url)
        gen = AVAssetImageGenerator(asset: asset)
        gen.appliesPreferredTrackTransform = true
        self.offset = offset
        let d = CMTimeGetSeconds(asset.duration)
        duration = d.isFinite ? d : 0
    }

    /// Frame visible at session time `t` (seconds since t0).
    func frame(at t: Double) -> CGImage? {
        guard duration > 0, t.isFinite else { return nil }
        let vt = max(0, min(t - offset, max(0, duration - 0.001)))
        let key = Int((vt * 1000).rounded())
        if let c = cache[key] { return c }
        let time = CMTime(seconds: vt, preferredTimescale: 60000)
        for tol in [0.0, 0.5, 3.0, 60.0] {
            gen.requestedTimeToleranceBefore = CMTime(seconds: tol, preferredTimescale: 600)
            gen.requestedTimeToleranceAfter = .zero
            if let img = try? gen.copyCGImage(at: time, actualTime: nil) {
                if cache.count > 64 { cache.removeAll() }
                cache[key] = img
                return img
            }
        }
        return nil
    }
}

extension CGRect {
    var center: CGPoint { CGPoint(x: midX, y: midY) }
}
