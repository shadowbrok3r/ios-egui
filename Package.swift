// swift-tools-version: 6.2
import PackageDescription

// This repository is both a Cargo workspace (Cargo.toml) and the EguiKit Swift package.
// Apps depend on it with: .package(url: "https://github.com/shadowbrok3r/ios-egui", branch: "master")
// and link their Rust staticlib via the -L flag emitted by `cargo egui-ios`.
let package = Package(
    name: "egui-ios",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "EguiKit", targets: ["EguiKit"]),
    ],
    targets: [
        .target(
            name: "EguiKitC",
            path: "EguiKit/Sources/EguiKitC",
            publicHeadersPath: "include"
        ),
        .target(
            name: "EguiKit",
            dependencies: ["EguiKitC"],
            path: "EguiKit/Sources/EguiKit"
        ),
    ]
)
