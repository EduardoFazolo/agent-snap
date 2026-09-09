import Foundation

/// User-tunable settings, persisted in UserDefaults (shared by CLI + app).
struct Options: Codable, Equatable {
    var outputDir: String = NSString(string: "~/agent-snap").expandingTildeInPath
    var captureTypedText = true
    var captureURLs = true
    var detectScreenUpdates = true
    var panelsPerImage = 3
    var compositeWidth = 1568
    var quietMs = 350

    static let key = "agent-snap.options"

    static func load() -> Options {
        if let d = UserDefaults.standard.data(forKey: key), let o = try? JSONDecoder().decode(Options.self, from: d) { return o }
        return Options()
    }
    func save() {
        if let d = try? JSONEncoder().encode(self) { UserDefaults.standard.set(d, forKey: Options.key) }
    }
    func newSessionDir() -> URL {
        let f = DateFormatter(); f.dateFormat = "yyyyMMdd-HHmmss"
        return URL(fileURLWithPath: outputDir).appendingPathComponent("sessions/\(f.string(from: Date()))")
    }
}
