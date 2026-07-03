// swift-tools-version: 6.2
import PackageDescription
import Foundation

// The Rust staticlib is linked directly from cargo's target dir. `cargo egui-ios` exports
// EGUI_IOS_RUST_TARGET_DIR; the fallback matches the default `rust/target` layout.
let rustTargetDir = ProcessInfo.processInfo.environment["EGUI_IOS_RUST_TARGET_DIR"]
    ?? "\(FileManager.default.currentDirectoryPath)/rust/target"

let package = Package(
    name: "{{project_name}}",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "{{project_name}}", targets: ["{{project_name}}"]),
    ],
    dependencies: [
        // Git (default):
        .package(url: "https://github.com/shadowbrok3r/egui-ios", branch: "main"),
        // Local checkout (uncomment for development):
        //   .package(path: "../ios-egui"),
    ],
    targets: [
        .target(
            name: "{{project_name}}",
            dependencies: [.product(name: "EguiKit", package: "egui-ios")],
            linkerSettings: [
                .unsafeFlags(["-L", "\(rustTargetDir)/aarch64-apple-ios/release"]),
                .linkedLibrary("{{project_name}}"),
            ]
        ),
    ]
)
