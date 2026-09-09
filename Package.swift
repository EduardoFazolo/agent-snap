// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "agent-snap",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "agent-snap",
            path: "Sources/agent-snap",
            swiftSettings: [.swiftLanguageMode(.v5)],
            linkerSettings: [
                .linkedFramework("ScreenCaptureKit"),
                .linkedFramework("VideoToolbox"),
                .linkedFramework("AppKit"),
                .linkedFramework("ApplicationServices"),
            ]
        )
    ]
)
