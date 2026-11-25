fn main() {
    #[cfg(target_os = "macos")]
    {
        // Objective-C blocks compile in C mode on Apple Clang with -fblocks.
        // Blocks runtime is part of libSystem on macOS — no explicit link needed.
        cc::Build::new()
            .file("src/platform/dispatch_shim.c")
            .flag("-fblocks")
            .flag_if_supported("-Wno-incompatible-pointer-types")
            .compile("dispatch_shim");
        println!("cargo:rerun-if-changed=src/platform/dispatch_shim.c");
    }

    // The FSKit install / uninstall shim is a small Obj-C file that wraps
    // SystemExtensions.framework's `OSSystemExtensionManager` + the
    // delegate pattern behind two blocking C entry points. Compiling
    // Obj-C inline is much simpler than wrestling `objc2 declare_class!`
    // for a tiny four-callback delegate.
    #[cfg(all(target_os = "macos", feature = "fskit"))]
    {
        cc::Build::new()
            .file("src/mount/fskit_install.m")
            .flag("-fobjc-arc")
            .flag("-fmodules")
            .compile("tzip_fskit_install");
        // `OSSystemExtensionManager` + friends live in
        // SystemExtensions.framework; Foundation covers NSString/NSError.
        println!("cargo:rustc-link-lib=framework=SystemExtensions");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rerun-if-changed=src/mount/fskit_install.m");
    }
}
