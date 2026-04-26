// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "ClipBridge",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "ClipBridgeApp", targets: ["ClipBridgeApp"]),
    ],
    targets: [
        .binaryTarget(
            name: "ClipbridgeCoreFFI",
            path: "ClipbridgeCore.xcframework"
        ),
        .target(
            name: "ClipbridgeCore",
            dependencies: ["ClipbridgeCoreFFI"],
            path: "Sources/ClipbridgeCore"
        ),
        .executableTarget(
            name: "ClipBridgeApp",
            dependencies: ["ClipbridgeCore"],
            path: "Sources/ClipBridgeApp"
        ),
    ]
)
