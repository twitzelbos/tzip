// swift-tools-version:5.10
//
// Swift package for the tzip FSKit extension. Builds a standalone
// executable that `scripts/build-fskit-bundle.sh` wraps into a
// `.appex` bundle under `tzip.app/Contents/PlugIns/`. macOS 15.4+
// FSKit extensions are ExtensionKit XPC services, not dylibs; the
// executable needs `tzip_*` ABI symbols from the Rust cdylib
// bundled alongside it at `Contents/Frameworks/libtzip.dylib`.
//
// The Rust side's path isn't knowable at package-manifest time, so
// `scripts/build-fskit-bundle.sh` points us at it via
// `TZIP_RUST_LIBDIR`; we then feed `-L<libdir> -ltzip` into the
// linker and set an rpath so the finished binary looks for
// `libtzip.dylib` at `@executable_path/../Frameworks` at runtime.

import PackageDescription
import Foundation

let rustLibDir = ProcessInfo.processInfo.environment["TZIP_RUST_LIBDIR"] ?? ""
guard !rustLibDir.isEmpty else {
    fatalError(
        "TZIP_RUST_LIBDIR is not set. Run via scripts/build-fskit-bundle.sh, " +
        "or export it to point at the directory containing libtzip.dylib."
    )
}

let package = Package(
    name: "TzipFSExtension",
    platforms: [
        // FSKit's Swift protocols shipped in macOS 15.4.
        .macOS("15.4")
    ],
    products: [
        .executable(
            name: "TzipFSExtension",
            targets: ["TzipFSExtension"]
        )
    ],
    targets: [
        .executableTarget(
            name: "TzipFSExtension",
            dependencies: [],
            path: "Sources/TzipFSExtension",
            linkerSettings: [
                .linkedFramework("FSKit"),
                .linkedFramework("ExtensionFoundation"),
                .linkedFramework("Foundation"),
                .unsafeFlags([
                    "-L", rustLibDir,
                    "-ltzip",
                    "-Xlinker", "-rpath",
                    "-Xlinker", "@executable_path/../Frameworks",
                ]),
            ]
        )
    ]
)
