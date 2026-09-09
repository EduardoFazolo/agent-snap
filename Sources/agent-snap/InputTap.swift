import Foundation
import CoreGraphics
import QuartzCore

enum RawKind { case leftDown, leftUp, rightDown, rightUp, moved, dragged, scroll, keyDown }

struct RawInput {
    let t: Double
    let kind: RawKind
    let point: CGPoint      // global points, top-left origin
    var keyCode: Int = 0
    var chars: String = ""
    var flags: CGEventFlags = []
    var clickState: Int = 1
    var scrollDY: Double = 0
}

final class InputTap {
    var onEvent: ((RawInput) -> Void)?
    private var tap: CFMachPort?
    private var source: CFRunLoopSource?

    func start() throws {
        let mask: CGEventMask =
            (1 << CGEventType.leftMouseDown.rawValue) | (1 << CGEventType.leftMouseUp.rawValue) |
            (1 << CGEventType.rightMouseDown.rawValue) | (1 << CGEventType.rightMouseUp.rawValue) |
            (1 << CGEventType.mouseMoved.rawValue) | (1 << CGEventType.leftMouseDragged.rawValue) |
            (1 << CGEventType.scrollWheel.rawValue) | (1 << CGEventType.keyDown.rawValue)

        let userInfo = Unmanaged.passUnretained(self).toOpaque()
        guard let tap = CGEvent.tapCreate(
            tap: .cgSessionEventTap, place: .headInsertEventTap, options: .listenOnly,
            eventsOfInterest: mask,
            callback: { _, type, event, info in
                guard let info = info else { return Unmanaged.passUnretained(event) }
                let me = Unmanaged<InputTap>.fromOpaque(info).takeUnretainedValue()
                me.handle(type: type, event: event)
                return Unmanaged.passUnretained(event)
            },
            userInfo: userInfo) else {
            throw NSError(domain: "agent-snap", code: 2, userInfo: [NSLocalizedDescriptionKey:
                "Could not create event tap. Grant Accessibility (and Input Monitoring) to your terminal in System Settings > Privacy & Security."])
        }
        self.tap = tap
        let src = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
        source = src
        CFRunLoopAddSource(CFRunLoopGetMain(), src, .commonModes)
        CGEvent.tapEnable(tap: tap, enable: true)
    }

    func stop() {
        if let tap = tap { CGEvent.tapEnable(tap: tap, enable: false) }
        if let src = source { CFRunLoopRemoveSource(CFRunLoopGetMain(), src, .commonModes) }
        tap = nil; source = nil
    }

    private func handle(type: CGEventType, event: CGEvent) {
        let t = CACurrentMediaTime()
        let p = event.location
        var raw: RawInput
        switch type {
        case .leftMouseDown: raw = RawInput(t: t, kind: .leftDown, point: p)
        case .leftMouseUp: raw = RawInput(t: t, kind: .leftUp, point: p)
        case .rightMouseDown: raw = RawInput(t: t, kind: .rightDown, point: p)
        case .rightMouseUp: raw = RawInput(t: t, kind: .rightUp, point: p)
        case .mouseMoved: raw = RawInput(t: t, kind: .moved, point: p)
        case .leftMouseDragged: raw = RawInput(t: t, kind: .dragged, point: p)
        case .scrollWheel:
            raw = RawInput(t: t, kind: .scroll, point: p)
            raw.scrollDY = Double(event.getIntegerValueField(.scrollWheelEventPointDeltaAxis1))
        case .keyDown:
            raw = RawInput(t: t, kind: .keyDown, point: p)
            raw.keyCode = Int(event.getIntegerValueField(.keyboardEventKeycode))
            raw.flags = event.flags
            var len = 0
            var buf = [UniChar](repeating: 0, count: 8)
            event.keyboardGetUnicodeString(maxStringLength: 8, actualStringLength: &len, unicodeString: &buf)
            raw.chars = String(utf16CodeUnits: buf, count: len)
        case .tapDisabledByTimeout, .tapDisabledByUserInput:
            if let tap = tap { CGEvent.tapEnable(tap: tap, enable: true) }
            return
        default: return
        }
        raw.clickState = Int(event.getIntegerValueField(.mouseEventClickState))
        onEvent?(raw)
    }
}

let keyNames: [Int: String] = [
    36: "Enter", 76: "Enter", 48: "Tab", 49: "Space", 51: "Backspace", 53: "Escape", 117: "Delete",
    123: "Left", 124: "Right", 125: "Down", 126: "Up", 115: "Home", 119: "End", 116: "PageUp", 121: "PageDown",
    122: "F1", 120: "F2", 99: "F3", 118: "F4", 96: "F5", 97: "F6", 98: "F7", 100: "F8", 101: "F9", 109: "F10", 103: "F11", 111: "F12",
]

func keyLabel(_ r: RawInput) -> String {
    var parts: [String] = []
    if r.flags.contains(.maskControl) { parts.append("Ctrl") }
    if r.flags.contains(.maskAlternate) { parts.append("Opt") }
    if r.flags.contains(.maskShift) { parts.append("Shift") }
    if r.flags.contains(.maskCommand) { parts.append("Cmd") }
    let base: String
    if let n = keyNames[r.keyCode] { base = n }
    else if let c = r.chars.unicodeScalars.first, c.value >= 32, c.value != 127 { base = r.chars.uppercased() }
    else { base = "key\(r.keyCode)" }
    parts.append(base)
    return parts.joined(separator: "+")
}

func isPrintable(_ r: RawInput) -> Bool {
    if r.flags.contains(.maskCommand) || r.flags.contains(.maskControl) { return false }
    if keyNames[r.keyCode] != nil && r.keyCode != 49 { return false } // Space is printable
    guard let c = r.chars.unicodeScalars.first else { return false }
    return c.value >= 32 && c.value != 127
}
