// swift-tools-version:5.9
import PackageDescription
import Foundation

// The binary target has two modes because the artifact it names cannot exist in
// both places at once.
//
//   - CONSUMERS resolve `Chisel.xcframework.zip` from a GitHub release asset by
//     URL and checksum. That is the only form SwiftPM can fetch, and it is why
//     this manifest must live at the repository root rather than in `swift/`.
//   - THIS REPO'S CI and local development set CHISEL_LOCAL_XCFRAMEWORK=1 and
//     use the artifact `swift/build-xcframework.sh` just produced. Without this
//     branch, `swift test` on a PR would try to fetch a release that does not
//     exist yet — the repo has zero releases today — and a fresh clone could
//     not build at all.
//
// `swift/release/prepare.sh` rewrites the url and checksum below when a release
// is cut, so the tag carries a manifest describing its own asset.
let useLocalBinary = ProcessInfo.processInfo.environment["CHISEL_LOCAL_XCFRAMEWORK"] != nil

let binaryTarget: Target = useLocalBinary
    ? .binaryTarget(name: "ChiselFFI", path: "swift/Chisel.xcframework")
    : .binaryTarget(
        name: "ChiselFFI",
        url: "https://github.com/pgexperts/chisel/releases/download/v1.0.0/Chisel.xcframework.zip",
        checksum: "0000000000000000000000000000000000000000000000000000000000000000"
    )

let package = Package(
    name: "Chisel",
    platforms: [.iOS(.v15), .macOS(.v12)],
    products: [
        .library(name: "Chisel", targets: ["Chisel"]),
    ],
    targets: [
        binaryTarget,
        .target(
            name: "Chisel",
            dependencies: ["ChiselFFI"],
            path: "swift/Sources/Chisel"
        ),
        .testTarget(
            name: "ChiselTests",
            dependencies: ["Chisel"],
            path: "swift/Tests/ChiselTests"
        ),
    ]
)
