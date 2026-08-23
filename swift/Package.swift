// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Chisel",
    platforms: [.iOS(.v15), .macOS(.v12)],
    products: [
        .library(name: "Chisel", targets: ["Chisel"]),
    ],
    targets: [
        .binaryTarget(name: "ChiselFFI", path: "Chisel.xcframework"),
        .target(
            name: "Chisel",
            dependencies: ["ChiselFFI"],
            path: "Sources/Chisel"
        ),
        .testTarget(
            name: "ChiselTests",
            dependencies: ["Chisel"],
            path: "Tests/ChiselTests"
        ),
    ]
)
