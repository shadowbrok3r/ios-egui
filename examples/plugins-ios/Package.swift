// swift-tools-version: 6.2
import PackageDescription
import Foundation

// `cargo egui-ios` exports EGUI_IOS_RUST_TARGET_DIR. This example is a workspace member, so its
// staticlib lands in the workspace target dir; the fallback points there for manual builds.
let rustTargetDir = ProcessInfo.processInfo.environment["EGUI_IOS_RUST_TARGET_DIR"]
    ?? "\(FileManager.default.currentDirectoryPath)/../../target"

let package = Package(
    name: "PluginsIos",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "PluginsIos", targets: ["PluginsIos"]),
    ],
    dependencies: [
        .package(path: "../.."),
    ],
    targets: [
        .target(
            name: "PluginsIos",
            dependencies: [.product(name: "EguiKit", package: "ios-egui")],
            linkerSettings: [
                .unsafeFlags(["-L", "\(rustTargetDir)/aarch64-apple-ios/release"]),
                .linkedLibrary("plugins_ios"),
            ]
        ),
    ]
)
