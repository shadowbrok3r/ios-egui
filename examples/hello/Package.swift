// swift-tools-version: 6.2
import PackageDescription
import Foundation

// `cargo egui-ios` exports EGUI_IOS_RUST_TARGET_DIR. This example is a workspace member, so its
// staticlib lands in the workspace target dir; the fallback points there for manual builds.
let rustTargetDir = ProcessInfo.processInfo.environment["EGUI_IOS_RUST_TARGET_DIR"]
    ?? "\(FileManager.default.currentDirectoryPath)/../../target"

let package = Package(
    name: "Hello",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "Hello", targets: ["Hello"]),
    ],
    dependencies: [
        .package(path: "../.."),
    ],
    targets: [
        .target(
            // Path-dependency identity is the directory name (ios-egui); the git-based template
            // uses package "egui-ios" instead.
            name: "Hello",
            dependencies: [.product(name: "EguiKit", package: "ios-egui")],
            linkerSettings: [
                .unsafeFlags(["-L", "\(rustTargetDir)/aarch64-apple-ios/release"]),
                .linkedLibrary("hello"),
            ]
        ),
    ]
)
