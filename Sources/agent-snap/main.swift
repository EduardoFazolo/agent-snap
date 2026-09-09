import Foundation
import AppKit
import CoreGraphics

func usage() -> Never {
    print("""
    agent-snap — token-efficient screen recording for coding agents (macOS)

    usage:
      agent-snap record [--out DIR] [--duration SECONDS] [--no-build]
      agent-snap build DIR
      agent-snap app                      # menu bar UI (or launch AgentSnap.app)

    record: captures keyframes around every click/keystroke/scroll/window switch,
            then builds DIR/flow.md with focused composites.
    build:  (re)generate flow.md + composites from an existing session DIR.
    """)
    exit(1)
}

var args = Array(CommandLine.arguments.dropFirst())
// Launched as an .app bundle (no args) → menu bar UI.
let inBundle = Bundle.main.bundleURL.pathExtension == "app"
if args.isEmpty && inBundle { MainActor.assumeIsolated { runApp() } }
guard let cmd = args.first else { usage() }
args.removeFirst()

func opt(_ name: String) -> String? {
    guard let i = args.firstIndex(of: name), i + 1 < args.count else { return nil }
    let v = args[i + 1]; args.removeSubrange(i...i + 1); return v
}
func flag(_ name: String) -> Bool {
    guard let i = args.firstIndex(of: name) else { return false }
    args.remove(at: i); return true
}

switch cmd {
case "app":
    MainActor.assumeIsolated { runApp() }

case "build":
    guard let dir = args.first else { usage() }
    do {
        let b = try Builder(dir: URL(fileURLWithPath: dir))
        let md = try b.build()
        print(md.path)
        fputs("~\(b.stats.tokens) tokens (\(b.stats.images) images ~\(b.stats.imageTokens), text ~\(b.stats.textTokens))\n", stderr)
    } catch {
        fputs("build failed: \(error)\n", stderr); exit(1)
    }

case "record":
    let outPath = opt("--out") ?? Options.load().newSessionDir().path
    let duration = opt("--duration").flatMap(Double.init)
    let noBuild = flag("--no-build")
    let outDir = URL(fileURLWithPath: outPath)

    if !CGPreflightScreenCaptureAccess() {
        CGRequestScreenCaptureAccess()
        fputs("Screen Recording permission needed. Grant it to your terminal in System Settings > Privacy & Security > Screen Recording, then rerun.\n", stderr)
        exit(2)
    }
    let axOpts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
    if !AXIsProcessTrustedWithOptions(axOpts) {
        fputs("Accessibility permission needed (for input events + element names). Grant it to your terminal in System Settings > Privacy & Security > Accessibility, then rerun.\n", stderr)
        exit(2)
    }

    let recorder = Recorder(outDir: outDir)
    var stopping = false
    func finish() {
        guard !stopping else { return }
        stopping = true
        Task {
            let session = await recorder.stop()
            fputs("stopped. \(session.steps.count) steps, \(session.timeline.count) window segments.\n", stderr)
            if !noBuild {
                do {
                    let b = try Builder(dir: outDir)
                    let md = try b.build()
                    print(md.path)
                    fputs("~\(b.stats.tokens) tokens (\(b.stats.images) images ~\(b.stats.imageTokens), text ~\(b.stats.textTokens))\n", stderr)
                } catch { fputs("build failed: \(error)\n", stderr) }
            }
            exit(0)
        }
    }
    signal(SIGINT, SIG_IGN)
    let sig = DispatchSource.makeSignalSource(signal: SIGINT, queue: .main)
    sig.setEventHandler { finish() }
    sig.resume()
    if let d = duration {
        DispatchQueue.main.asyncAfter(deadline: .now() + d) { finish() }
    }
    Task {
        do { try await recorder.start() } catch {
            fputs("start failed: \(error.localizedDescription)\n", stderr); exit(1)
        }
    }
    RunLoop.main.run()

default:
    usage()
}
