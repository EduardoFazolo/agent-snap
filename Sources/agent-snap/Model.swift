import Foundation

struct RectI: Codable, Equatable {
    var x: Int, y: Int, w: Int, h: Int
    var cg: CGRect { CGRect(x: x, y: y, width: w, height: h) }
    init(x: Int, y: Int, w: Int, h: Int) { self.x = x; self.y = y; self.w = w; self.h = h }
    init(_ r: CGRect) {
        x = Int(r.origin.x.rounded()); y = Int(r.origin.y.rounded())
        w = Int(r.width.rounded()); h = Int(r.height.rounded())
    }
}

struct PointI: Codable, Equatable {
    var x: Int, y: Int
    var cg: CGPoint { CGPoint(x: x, y: y) }
    init(x: Int, y: Int) { self.x = x; self.y = y }
    init(_ p: CGPoint) { x = Int(p.x.rounded()); y = Int(p.y.rounded()) }
}

struct WindowInfo: Codable, Equatable {
    var app: String
    var bundleId: String?
    var title: String
    var url: String?
    var isLocal: Bool?
    /// Window bounds in captured-image pixels.
    var bounds: RectI?

    /// Identity used to decide "same window" for grouping steps.
    var key: String { "\(bundleId ?? app)|\(url ?? title)" }
    var describe: String {
        var s = app
        if !title.isEmpty { s += " · \(title)" }
        if let u = url { s += " · \(u)" }
        if isLocal == true { s += " (local)" }
        return s
    }
}

struct AXTarget: Codable, Equatable {
    var role: String?
    var name: String?
    var value: String?
    /// Element bounds in captured-image pixels.
    var bounds: RectI?

    var describe: String {
        var parts: [String] = []
        if let r = role { parts.append(r) }
        if let n = name, !n.isEmpty { parts.append("\"\(n)\"") }
        else if let v = value, !v.isEmpty { parts.append("\"\(v)\"") }
        return parts.joined(separator: " ")
    }
}

enum StepKind: String, Codable {
    case click, doubleClick, rightClick, drag, type, key, scroll, cursor, windowSwitch, screenUpdate
}

/// One captured frame: when it arrived and what changed, in pixels. Written for every frame.
struct FrameLog: Codable {
    var t: Double
    var dirty: RectI?
}

/// Sampled cursor position (pixels) so the builder can ignore the cursor when diffing frames.
struct CursorSample: Codable {
    var t: Double
    var x: Int
    var y: Int
}

struct Step: Codable {
    var index: Int
    /// Seconds since session start.
    var t: Double
    /// When the input ended (mouse up, last key). Same as t for instant events.
    var endT: Double? = nil
    var kind: StepKind
    var label: String
    /// Anchor point in captured-image pixels (click point, or focused element center).
    var point: PointI?
    var endPoint: PointI?
    var text: String?
    var target: AXTarget?
    var window: WindowInfo
    var before: String?
    var after: String?
    var beforeT: Double? = nil
    var afterT: Double? = nil
    /// Union of OS-reported dirty rects between before and after (pixels).
    var dirty: RectI?
}

struct WindowSegment: Codable {
    var start: Double
    var end: Double
    var window: WindowInfo
    var inputEvents: Int
    var dwell: Double { end - start }
}

struct Session: Codable {
    var startedAt: Date
    var width: Int
    var height: Int
    var scale: Double
    /// Relative path of the recording. Source of truth for every frame.
    var video: String? = nil
    /// Host time of the first video frame minus session t0: videoTime = t - videoOffset.
    var videoOffset: Double = 0
    var frames: [FrameLog] = []
    var cursor: [CursorSample] = []
    var steps: [Step] = []
    var timeline: [WindowSegment] = []
}

func fmtT(_ t: Double) -> String {
    let m = Int(t) / 60, s = t - Double(m * 60)
    return String(format: "%02d:%04.1f", m, s)
}
