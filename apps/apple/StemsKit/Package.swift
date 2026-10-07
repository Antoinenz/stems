// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "StemsKit",
    defaultLocalization: "en",
    platforms: [.macOS(.v26), .iOS(.v26)],
    products: [
        .library(name: "StemsKit", targets: ["StemsKit"]),
        .library(name: "StemsUI", targets: ["StemsUI"]),
    ],
    targets: [
        // Models, the server connection and the playback clock. No UI.
        .target(name: "StemsKit"),
        // Screens shared by the Mac hub and the iOS app.
        .target(name: "StemsUI", dependencies: ["StemsKit"]),
        .testTarget(
            name: "StemsKitTests",
            dependencies: ["StemsKit"],
            resources: [.copy("Fixtures")]
        ),
    ]
)
