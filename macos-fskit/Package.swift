// swift-tools-version:5.10
//
// Swift package for the tzip FSKit extension. Builds a dynamic library
// that the top-level build.rs then wraps in a .fs bundle (Contents/
// Info.plist, Contents/MacOS/TzipFSExtension, Contents/Frameworks/
// libtzip_fskit_abi.dylib).

import PackageDescription

let package = Package(
    name: "TzipFSExtension",
    platforms: [
        // FSKit shipped the Swift protocols we use in macOS 15.4 (not 15.0).
        .macOS("15.4")
    ],
    products: [
        .library(
            name: "TzipFSExtension",
            type: .dynamic,
            targets: ["TzipFSExtension"]
        )
    ],
    targets: [
        .target(
            name: "TzipFSExtension",
            dependencies: [],
            path: "Sources/TzipFSExtension",
            linkerSettings: [
                .linkedFramework("FSKit"),
                .linkedFramework("Foundation"),
                // tzip_* symbols come from the Rust cdylib that the
                // outer build.rs bundles alongside this one in the .fs
                // bundle. Tell the linker to resolve them at dylib-load
                // time rather than at link time.
                .unsafeFlags(["-Xlinker", "-undefined", "-Xlinker", "dynamic_lookup"]),
            ]
        )
    ]
)
