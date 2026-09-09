import Foundation
import AppKit
import ApplicationServices

/// Reads the frontmost app/window via AX, plus browser tab URL, plus AX hit-testing.
final class WindowTracker {
    var captureURLs = true
    private let systemWide = AXUIElementCreateSystemWide()
    private var urlCache: (key: String, url: String?, t: Double) = ("", nil, 0)

    static let browserScripts: [String: String] = [
        "com.google.Chrome": "tell application id \"com.google.Chrome\" to get URL of active tab of front window",
        "com.google.Chrome.canary": "tell application id \"com.google.Chrome.canary\" to get URL of active tab of front window",
        "com.brave.Browser": "tell application id \"com.brave.Browser\" to get URL of active tab of front window",
        "com.microsoft.edgemac": "tell application id \"com.microsoft.edgemac\" to get URL of active tab of front window",
        "company.thebrowser.Browser": "tell application id \"company.thebrowser.Browser\" to get URL of active tab of front window",
        "com.vivaldi.Vivaldi": "tell application id \"com.vivaldi.Vivaldi\" to get URL of active tab of front window",
        "com.apple.Safari": "tell application id \"com.apple.Safari\" to get URL of front document",
    ]

    /// Frontmost window. `toPixels` maps global points -> capture pixels.
    func front(toPixels: (CGRect) -> CGRect) -> WindowInfo {
        guard let app = NSWorkspace.shared.frontmostApplication else {
            return WindowInfo(app: "?", bundleId: nil, title: "", url: nil, isLocal: nil, bounds: nil)
        }
        let name = app.localizedName ?? "?"
        let bid = app.bundleIdentifier
        let axApp = AXUIElementCreateApplication(app.processIdentifier)
        var title = ""
        var bounds: RectI?
        if let win = attr(axApp, kAXFocusedWindowAttribute) {
            let w = win as! AXUIElement
            title = (attr(w, kAXTitleAttribute) as? String) ?? ""
            if let r = frame(of: w) { bounds = RectI(toPixels(r)) }
        }
        var url: String?
        if captureURLs, let bid = bid, WindowTracker.browserScripts[bid] != nil {
            url = tabURL(bundleId: bid, title: title)
        }
        var isLocal: Bool?
        if let u = url, let host = URL(string: u)?.host {
            isLocal = host == "localhost" || host == "127.0.0.1" || host == "0.0.0.0" || host.hasSuffix(".local") || host.hasSuffix(".localhost")
        }
        return WindowInfo(app: name, bundleId: bid, title: title, url: url, isLocal: isLocal, bounds: bounds)
    }

    private func tabURL(bundleId: String, title: String) -> String? {
        let key = "\(bundleId)|\(title)"
        let now = CFAbsoluteTimeGetCurrent()
        if urlCache.key == key, now - urlCache.t < 2 { return urlCache.url }
        guard let script = WindowTracker.browserScripts[bundleId] else { return nil }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        p.arguments = ["-e", script]
        let out = Pipe()
        p.standardOutput = out
        p.standardError = FileHandle.nullDevice
        var result: String?
        do {
            try p.run()
            let data = out.fileHandleForReading.readDataToEndOfFile()
            p.waitUntilExit()
            if p.terminationStatus == 0, let s = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines), !s.isEmpty {
                result = s
            }
        } catch {}
        urlCache = (key, result, now)
        return result
    }

    /// True when the element under the point belongs to this process (our own menu bar item / popover).
    func elementAtPointIsOwnProcess(_ p: CGPoint) -> Bool {
        var el: AXUIElement?
        guard AXUIElementCopyElementAtPosition(systemWide, Float(p.x), Float(p.y), &el) == .success, let e = el else { return false }
        var pid: pid_t = 0
        return AXUIElementGetPid(e, &pid) == .success && pid == getpid()
    }

    /// Diagnostic sink: one line per hit-test with the element chain the app exposed.
    var debugLog: ((String) -> Void)?

    static let interactiveRoles: Set<String> = [
        "AXButton", "AXPopUpButton", "AXMenuButton", "AXMenuItem", "AXMenuBarItem", "AXLink", "AXTextField",
        "AXTextArea", "AXComboBox", "AXCheckBox", "AXRadioButton", "AXSlider", "AXDisclosureTriangle",
        "AXCell", "AXRow", "AXIncrementor", "AXColorWell", "AXTabGroup", "AXToolbar",
    ]

    /// AX element under a global point: descends to the deepest child containing the point,
    /// then prefers the nearest interactive ancestor (button over its label).
    func hitTest(_ p: CGPoint, toPixels: (CGRect) -> CGRect) -> AXTarget? {
        hitTestOnce(p, toPixels: toPixels)
    }

    private func hitTestOnce(_ p: CGPoint, toPixels: (CGRect) -> CGRect) -> AXTarget? {
        var el: AXUIElement?
        guard AXUIElementCopyElementAtPosition(systemWide, Float(p.x), Float(p.y), &el) == .success, let root = el else {
            debugLog?("hit (\(Int(p.x)),\(Int(p.y))): no element")
            return nil
        }
        var chain = [root]
        var cur = root
        for _ in 0..<40 {
            guard let kids = attr(cur, kAXChildrenAttribute) as? [AXUIElement], !kids.isEmpty else { break }
            var best: (AXUIElement, CGFloat)?
            for k in kids.prefix(500) {
                guard let f = frame(of: k), f.contains(p) else { continue }
                let a = f.width * f.height
                if best == nil || a < best!.1 { best = (k, a) }
            }
            guard let b = best else { break }
            chain.append(b.0); cur = b.0
        }
        if let log = debugLog {
            let kids = (attr(root, kAXChildrenAttribute) as? [AXUIElement])?.count ?? 0
            let desc = chain.map { e -> String in
                let r = (attr(e, kAXRoleAttribute) as? String) ?? "?"
                let n = (attr(e, kAXTitleAttribute) as? String) ?? (attr(e, kAXDescriptionAttribute) as? String) ?? ""
                return n.isEmpty ? r : "\(r)(\(n.prefix(30)))"
            }.joined(separator: " > ")
            log("hit (\(Int(p.x)),\(Int(p.y))): rootKids=\(kids) chain=\(desc)")
        }
        for e in chain.reversed().prefix(6) {
            if let r = attr(e, kAXRoleAttribute) as? String, WindowTracker.interactiveRoles.contains(r) {
                return describe(e, toPixels: toPixels)
            }
        }
        for e in chain.reversed() {
            let d = describe(e, toPixels: toPixels)
            if d.name != nil || d.value != nil { return d }
        }
        return describe(chain.last!, toPixels: toPixels)
    }

    /// Currently focused AX element (for typing anchors).
    func focused(toPixels: (CGRect) -> CGRect) -> AXTarget? {
        guard let f = attr(systemWide, kAXFocusedUIElementAttribute) else { return nil }
        return describe(f as! AXUIElement, toPixels: toPixels)
    }

    private func describe(_ e: AXUIElement, toPixels: (CGRect) -> CGRect) -> AXTarget {
        let roleDesc = attr(e, kAXRoleDescriptionAttribute) as? String
        let role = attr(e, kAXRoleAttribute) as? String
        var name = attr(e, kAXTitleAttribute) as? String
        if (name ?? "").isEmpty { name = attr(e, kAXDescriptionAttribute) as? String }
        if (name ?? "").isEmpty, let ph = attr(e, kAXPlaceholderValueAttribute) as? String { name = ph }
        if (name ?? "").isEmpty, let labelUI = attr(e, "AXTitleUIElement") {
            name = attr(labelUI as! AXUIElement, kAXValueAttribute) as? String
        }
        if (name ?? "").isEmpty { name = innerText(e, depth: 0, budget: 24) }
        var value: String?
        if let v = attr(e, kAXValueAttribute) {
            if let s = v as? String { value = s } else if let n = v as? NSNumber { value = n.stringValue }
        }
        if let v = value, v.count > 200 { value = String(v.prefix(200)) + "…" }
        if let v = value, v.contains("\n") { value = v.components(separatedBy: "\n").first }
        var bounds: RectI?
        if let r = frame(of: e) { bounds = RectI(toPixels(r)) }
        if let n = name, n.count > 60 { name = String(n.prefix(60)).trimmingCharacters(in: .whitespaces) + "…" }
        if let v = value, v.count > 60 { value = String(v.prefix(60)).trimmingCharacters(in: .whitespaces) + "…" }
        return AXTarget(role: (roleDesc ?? role)?.replacingOccurrences(of: "AX", with: ""),
                        name: name?.isEmpty == true ? nil : name, value: value, bounds: bounds)
    }

    /// First static text inside an element (button labels, menu items).
    private func innerText(_ e: AXUIElement, depth: Int, budget: Int) -> String? {
        guard depth < 4, budget > 0, let kids = attr(e, kAXChildrenAttribute) as? [AXUIElement] else { return nil }
        var remaining = budget
        for k in kids {
            remaining -= 1
            if remaining <= 0 { return nil }
            let role = attr(k, kAXRoleAttribute) as? String
            if role == "AXStaticText", let v = attr(k, kAXValueAttribute) as? String, !v.trimmingCharacters(in: .whitespaces).isEmpty {
                return String(v.prefix(80))
            }
            if let t = attr(k, kAXTitleAttribute) as? String, !t.isEmpty { return t }
            if let t = innerText(k, depth: depth + 1, budget: remaining) { return t }
        }
        return nil
    }

    private func attr(_ e: AXUIElement, _ name: String) -> CFTypeRef? {
        var v: CFTypeRef?
        return AXUIElementCopyAttributeValue(e, name as CFString, &v) == .success ? v : nil
    }

    private func frame(of e: AXUIElement) -> CGRect? {
        guard let pv = attr(e, kAXPositionAttribute), let sv = attr(e, kAXSizeAttribute) else { return nil }
        var p = CGPoint.zero, s = CGSize.zero
        guard AXValueGetValue(pv as! AXValue, .cgPoint, &p), AXValueGetValue(sv as! AXValue, .cgSize, &s) else { return nil }
        return CGRect(origin: p, size: s)
    }

    /// True if the focused element is a password field.
    func focusedIsSecure() -> Bool {
        guard let f = attr(systemWide, kAXFocusedUIElementAttribute) else { return false }
        let e = f as! AXUIElement
        if (attr(e, kAXSubroleAttribute) as? String) == kAXSecureTextFieldSubrole { return true }
        if (attr(e, kAXRoleAttribute) as? String) == "AXSecureTextField" { return true }
        return false
    }
}
