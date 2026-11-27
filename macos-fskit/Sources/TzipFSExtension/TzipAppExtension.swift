//
// TzipAppExtension.swift
//
// Modern ExtensionKit / FSKit entry point. macOS 15.4+ FSKit
// extensions are standalone XPC services discovered via Launch
// Services when the containing `.app` is in `/Applications/`;
// the system then launches this binary and FSKit calls into our
// `fileSystem` getter for the `FSUnaryFileSystem` subclass that
// implements probe / load / unload.

import FSKit
import ExtensionFoundation
import Darwin

@main
struct TzipAppExtension: UnaryFileSystemExtension {
    // Static initialiser runs before `main()` — before anything in
    // the extension references a `tzip_*` symbol. The Rust cdylib
    // sits at `<extension>.appex/Contents/Frameworks/libtzip.dylib`
    // (see `scripts/build-fskit-bundle.sh`), i.e. one directory up
    // from this executable. We dlopen it so dyld binds the
    // `-undefined dynamic_lookup` references in the Swift binary
    // against it at load time.
    //
    // Failures are logged but non-fatal: the @main entry will still
    // run, and the first call that needs a `tzip_*` symbol will
    // surface a clearer "symbol not found" error for debugging.
    static let _loaded: Bool = {
        let exe = CommandLine.arguments.first.map { URL(fileURLWithPath: $0) }
        guard let exe else { return false }
        let dylib = exe
            .deletingLastPathComponent()      // Contents/MacOS/
            .deletingLastPathComponent()      // Contents/
            .appendingPathComponent("Frameworks/libtzip.dylib")
            .path
        if dlopen(dylib, RTLD_GLOBAL | RTLD_NOW) == nil {
            let err = String(cString: dlerror())
            FileHandle.standardError.write(
                Data("tzip.appex: dlopen \(dylib) failed: \(err)\n".utf8))
            return false
        }
        return true
    }()

    var fileSystem: TzipFSExtension {
        _ = TzipAppExtension._loaded
        return TzipFSExtension()
    }
}
