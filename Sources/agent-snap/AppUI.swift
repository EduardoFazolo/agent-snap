import AppKit
import SwiftUI
import ApplicationServices

// MARK: - Controller

@MainActor
final class AppController: ObservableObject {
    enum State: Equatable { case idle, starting, recording, building, done(String, Int), failed(String) }

    @Published var state: State = .idle
    @Published var options = Options.load() { didSet { options.save() } }
    @Published var elapsed: Double = 0
    @Published var stepCount = 0
    @Published var lastStep = ""
    @Published var screenOK = CGPreflightScreenCaptureAccess()
    @Published var axOK = AXIsProcessTrusted()

    struct SessionInfo: Identifiable, Equatable {
        var id: String { dir.path }
        var dir: URL
        var name: String
        var flow: URL
        var steps: Int?
        var tokens: Int?
        var duration: String?
    }
    /// Past recordings, newest first. nil `browse` = the live view (settings / last result).
    @Published var sessions: [SessionInfo] = []
    @Published var browse: Int? = nil

    func refreshSessions() {
        let root = URL(fileURLWithPath: options.outputDir).appendingPathComponent("sessions")
        let dirs = (try? FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)) ?? []
        var out: [SessionInfo] = []
        for d in dirs.sorted(by: { $0.lastPathComponent > $1.lastPathComponent }) {
            let flow = d.appendingPathComponent("flow.md")
            guard FileManager.default.fileExists(atPath: flow.path) else { continue }
            var info = SessionInfo(dir: d, name: d.lastPathComponent, flow: flow)
            if let text = try? String(contentsOf: flow, encoding: .utf8) {
                let head = text.prefix(1200)
                if let m = head.range(of: #"~(\d+) tokens"#, options: .regularExpression) {
                    info.tokens = Int(head[m].dropFirst().split(separator: " ").first ?? "")
                }
                if let m = head.range(of: #"- (\d+) steps"#, options: .regularExpression) {
                    info.steps = Int(head[m].dropFirst(2).split(separator: " ").first ?? "")
                }
                if let m = head.range(of: #"duration (\d\d:\d\d\.\d)"#, options: .regularExpression) {
                    info.duration = String(head[m].dropFirst(9))
                }
            }
            out.append(info)
        }
        sessions = out
        if let b = browse, b >= out.count { browse = out.isEmpty ? nil : out.count - 1 }
    }

    func browseOlder() {
        guard !sessions.isEmpty else { return }
        let next = (browse ?? -1) + 1
        if next < sessions.count { browse = next }
    }
    func browseNewer() {
        guard let b = browse else { return }
        browse = b == 0 ? nil : b - 1
    }
    var browsing: SessionInfo? { browse.flatMap { $0 < sessions.count ? sessions[$0] : nil } }

    private var recorder: Recorder?
    private var timer: Timer?
    private var startedAt: Date?
    var onStateChange: ((State) -> Void)?
    /// Global rect (points, top-left origin) of the status item, so clicks on it are not recorded.
    var statusItemRect: (() -> CGRect?)?

    func refreshPermissions() {
        screenOK = CGPreflightScreenCaptureAccess()
        axOK = AXIsProcessTrusted()
    }

    func requestScreen() {
        CGRequestScreenCaptureAccess()
        openSettings("Privacy_ScreenCapture")
    }
    func requestAX() {
        let opts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
        _ = AXIsProcessTrustedWithOptions(opts)
        openSettings("Privacy_Accessibility")
    }
    private func openSettings(_ pane: String) {
        if let u = URL(string: "x-apple.systempreferences:com.apple.preference.security?\(pane)") { NSWorkspace.shared.open(u) }
    }

    var canRecord: Bool { screenOK && axOK && (state == .idle || isDone) }
    var isDone: Bool { if case .done = state { return true }; if case .failed = state { return true }; return false }

    func start() {
        guard canRecord else { return }
        browse = nil
        let dir = options.newSessionDir()
        let rec = Recorder(outDir: dir, options: options)
        rec.ignoreBundleId = Bundle.main.bundleIdentifier
        rec.ignoreRect = statusItemRect?()
        rec.onStep = { [weak self] step in
            DispatchQueue.main.async {
                self?.stepCount += 1
                self?.lastStep = "\(fmtT(step.t)) \(step.label)"
            }
        }
        recorder = rec
        stepCount = 0; lastStep = ""; elapsed = 0
        setState(.starting)
        Task {
            do {
                // Let the popover close before the first frame.
                try await Task.sleep(nanoseconds: 300_000_000)
                try await rec.start()
                startedAt = Date()
                timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
                    Task { @MainActor in
                        guard let self = self, let s = self.startedAt else { return }
                        self.elapsed = Date().timeIntervalSince(s)
                    }
                }
                setState(.recording)
            } catch {
                setState(.failed(error.localizedDescription))
            }
        }
    }

    func stop() {
        guard let rec = recorder, state == .recording else { return }
        timer?.invalidate(); timer = nil
        setState(.building)
        Task {
            _ = await rec.stop()
            do {
                let builder = try Builder(dir: rec.outDir, options: options)
                let md = try builder.build()
                setState(.done(md.path, builder.stats.tokens))
                refreshSessions()
            } catch {
                setState(.failed("build failed: \(error.localizedDescription)"))
            }
            recorder = nil
        }
    }

    private func setState(_ s: State) { state = s; onStateChange?(s) }

    /// What gets pasted into an agent: the file to read and what it is.
    static func prompt(for path: String) -> String {
        "Read \(path) and view every image it links. It is a recording of what I did on screen."
    }

    func chooseOutputDir() {
        let p = NSOpenPanel()
        p.canChooseDirectories = true; p.canChooseFiles = false; p.canCreateDirectories = true
        p.directoryURL = URL(fileURLWithPath: options.outputDir)
        if p.runModal() == .OK, let u = p.url { options.outputDir = u.path }
    }
}

// MARK: - View

struct PopoverView: View {
    @ObservedObject var c: AppController
    @State private var showSettings = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                Text("AGENT SNAP").font(.system(.headline, design: .monospaced))
                Spacer()
                HStack(spacing: 2) {
                    Button { c.browseOlder() } label: { Image(systemName: "chevron.left") }
                        .buttonStyle(.plain)
                        .disabled(c.sessions.isEmpty || (c.browse ?? -1) >= c.sessions.count - 1)
                    Text(c.browse.map { "\($0 + 1)/\(c.sessions.count)" } ?? "now")
                        .font(.system(.caption, design: .monospaced)).foregroundStyle(.secondary)
                        .frame(minWidth: 34)
                    Button { c.browseNewer() } label: { Image(systemName: "chevron.right") }
                        .buttonStyle(.plain)
                        .disabled(c.browse == nil)
                }
                .help("Browse past recordings")
                Button { showSettings.toggle() } label: { Image(systemName: "gearshape") }.buttonStyle(.plain)
                Button { NSApp.terminate(nil) } label: { Image(systemName: "power") }.buttonStyle(.plain)
            }

            if !c.screenOK || !c.axOK {
                VStack(alignment: .leading, spacing: 6) {
                    permissionRow("Screen Recording", ok: c.screenOK) { c.requestScreen() }
                    permissionRow("Accessibility", ok: c.axOK) { c.requestAX() }
                    Text("Grant both, then reopen this popover.").font(.caption).foregroundStyle(.secondary)
                }
                .padding(10)
                .background(RoundedRectangle(cornerRadius: 8).fill(Color.yellow.opacity(0.12)))
            }

            if let past = c.browsing {
                pastSession(past)
            } else {
                if showSettings || c.state == .idle {
                    settings
                }
                status
            }

            Button(action: { c.state == .recording ? c.stop() : c.start() }) {
                HStack {
                    Image(systemName: c.state == .recording ? "stop.fill" : "record.circle")
                    Text(c.state == .recording ? "STOP" : "REC").font(.system(.title3, design: .monospaced).bold())
                }
                .frame(maxWidth: .infinity).padding(.vertical, 10)
            }
            .buttonStyle(.borderedProminent)
            .tint(c.state == .recording ? .gray : .red)
            .disabled(!(c.canRecord || c.state == .recording))
        }
        .padding(16)
        .frame(width: 360)
        .onAppear { c.refreshPermissions(); c.refreshSessions() }
    }

    private func pastSession(_ p: AppController.SessionInfo) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Image(systemName: "clock.arrow.circlepath").foregroundStyle(.secondary)
                Text(p.name).font(.system(.body, design: .monospaced))
            }
            HStack(spacing: 6) {
                if let d = p.duration { Text(d) }
                if let st = p.steps { Text("· \(st) steps") }
                if let t = p.tokens { Text("· ~\(t >= 1000 ? String(format: "%.1fk", Double(t) / 1000) : "\(t)") tokens") }
            }
            .font(.caption).foregroundStyle(.secondary)
            Text((p.flow.path as NSString).abbreviatingWithTildeInPath).font(.caption).lineLimit(1).truncationMode(.middle)
            HStack {
                Button("Copy prompt") { copy(AppController.prompt(for: p.flow.path)) }
                Button("Open") { NSWorkspace.shared.open(p.flow) }
                Button("Reveal") { NSWorkspace.shared.activateFileViewerSelecting([p.flow]) }
            }.controlSize(.small)
        }
    }

    private func permissionRow(_ name: String, ok: Bool, fix: @escaping () -> Void) -> some View {
        HStack {
            Image(systemName: ok ? "checkmark.circle.fill" : "xmark.circle").foregroundStyle(ok ? .green : .red)
            Text(name)
            Spacer()
            if !ok { Button("Grant…", action: fix).controlSize(.small) }
        }
    }

    private var settings: some View {
        VStack(alignment: .leading, spacing: 8) {
            row("Output") {
                HStack(spacing: 6) {
                    Text((c.options.outputDir as NSString).abbreviatingWithTildeInPath)
                        .font(.caption).lineLimit(1).truncationMode(.middle)
                    Spacer()
                    Button("Change…") { c.chooseOutputDir() }.controlSize(.small)
                }
            }
            row("Typed text") { Toggle("", isOn: $c.options.captureTypedText).toggleStyle(.switch).labelsHidden() }
            row("Browser URLs") { Toggle("", isOn: $c.options.captureURLs).toggleStyle(.switch).labelsHidden() }
            row("No-input updates") { Toggle("", isOn: $c.options.detectScreenUpdates).toggleStyle(.switch).labelsHidden() }
            row("Panels / image") {
                Picker("", selection: $c.options.panelsPerImage) { ForEach(2...4, id: \.self) { Text(verbatim: "\($0)") } }
                    .pickerStyle(.segmented).labelsHidden().frame(width: 140)
            }
            row("Max width") {
                Picker("", selection: $c.options.compositeWidth) { ForEach([1200, 1568, 2000], id: \.self) { Text(verbatim: "\($0)px") } }
                    .pickerStyle(.segmented).labelsHidden().frame(width: 200)
            }
            row("Settle quiet") {
                HStack {
                    Slider(value: Binding(get: { Double(c.options.quietMs) }, set: { c.options.quietMs = Int($0) }), in: 150...800, step: 50)
                    Text(verbatim: "\(c.options.quietMs)ms").font(.caption).monospacedDigit().frame(width: 48, alignment: .trailing)
                }
            }
        }
        .disabled(c.state == .recording || c.state == .starting || c.state == .building)
    }

    private func row<V: View>(_ label: String, @ViewBuilder _ v: () -> V) -> some View {
        HStack {
            Text(label).font(.system(.caption, design: .monospaced)).foregroundStyle(.secondary).frame(width: 110, alignment: .leading)
            v()
        }
    }

    @ViewBuilder private var status: some View {
        switch c.state {
        case .idle:
            EmptyView()
        case .starting:
            Label("Starting capture…", systemImage: "hourglass").font(.caption)
        case .recording:
            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    Circle().fill(.red).frame(width: 8, height: 8)
                    Text(fmtT(c.elapsed)).monospacedDigit()
                    Text("· \(c.stepCount) steps").foregroundStyle(.secondary)
                }
                if !c.lastStep.isEmpty { Text(c.lastStep).font(.caption).lineLimit(1).foregroundStyle(.secondary) }
            }
        case .building:
            Label("Building flow.md…", systemImage: "hourglass").font(.caption)
        case .done(let path, let tokens):
            VStack(alignment: .leading, spacing: 6) {
                Label("flow.md ready · \(c.stepCount) steps · ~\(tokens / 100 * 100 >= 1000 ? String(format: "%.1fk", Double(tokens) / 1000) : "\(tokens)") tokens", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
                Text((path as NSString).abbreviatingWithTildeInPath).font(.caption).lineLimit(1).truncationMode(.middle)
                HStack {
                    Button("Copy prompt") { copy(AppController.prompt(for: path)) }
                    Button("Open") { NSWorkspace.shared.open(URL(fileURLWithPath: path)) }
                    Button("Reveal") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)]) }
                }.controlSize(.small)
            }
        case .failed(let msg):
            Label(msg, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red).font(.caption)
        }
    }

    private func copy(_ s: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(s, forType: .string)
    }
}

// MARK: - Status item + popover

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate, NSPopoverDelegate {
    private var item: NSStatusItem!
    private let popover = NSPopover()
    private let controller = AppController()
    private var hosting: NSHostingController<PopoverView>!

    func applicationDidFinishLaunching(_ n: Notification) {
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        setIcon(recording: false)
        item.button?.action = #selector(toggle)
        item.button?.target = self
        popover.behavior = .transient
        popover.animates = false
        hosting = NSHostingController(rootView: PopoverView(c: controller))
        hosting.sizingOptions = [.preferredContentSize]
        popover.contentViewController = hosting
        popover.delegate = self
        controller.statusItemRect = { [weak self] in
            guard let b = self?.item.button, let w = b.window, let screen = NSScreen.screens.first else { return nil }
            let f = w.convertToScreen(b.convert(b.bounds, to: nil))
            // AppKit is bottom-left; global input coords are top-left of the main screen.
            return CGRect(x: f.minX, y: screen.frame.maxY - f.maxY, width: f.width, height: f.height).insetBy(dx: -4, dy: -4)
        }
        controller.onStateChange = { [weak self] s in
            self?.setIcon(recording: s == .recording || s == .starting)
            if s == .recording { self?.popover.performClose(nil) }
            if case .done = s { self?.show() }
        }
    }

    private func setIcon(recording: Bool) {
        guard let b = item.button else { return }
        let name = recording ? "record.circle.fill" : "camera.viewfinder"
        let img = NSImage(systemSymbolName: name, accessibilityDescription: "agent-snap")
        img?.isTemplate = !recording
        b.image = img
        b.contentTintColor = recording ? .systemRed : nil
    }

    @objc private func toggle() {
        if popover.isShown { popover.performClose(nil) } else { show() }
    }

    private func show() {
        guard let b = item.button else { return }
        controller.refreshPermissions()
        hosting.view.layoutSubtreeIfNeeded()
        popover.contentSize = hosting.view.fittingSize
        NSApp.activate(ignoringOtherApps: true)
        popover.show(relativeTo: b.bounds, of: b, preferredEdge: .minY)
        popover.contentViewController?.view.window?.makeKey()
    }
}

@MainActor
func runApp() -> Never {
    let app = NSApplication.shared
    app.setActivationPolicy(.accessory)
    let delegate = AppDelegate()
    app.delegate = delegate
    app.run()
    exit(0)
}
