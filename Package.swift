// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "ClaudeShip",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "ClaudeShip",
            path: "Sources/ClaudeShip"
        )
    ]
)
