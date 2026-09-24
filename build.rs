fn main() {
    #[cfg(target_os = "macos")]
    {
        // Objective-C blocks compile in C mode on Apple Clang with -fblocks.
        // Blocks runtime is part of libSystem on macOS — no explicit link needed.
        cc::Build::new()
            .file("src/dispatch_shim.c")
            .flag("-fblocks")
            .flag_if_supported("-Wno-incompatible-pointer-types")
            .compile("dispatch_shim");
        // dispatch is part of libSystem, so nothing to link explicitly.
        println!("cargo:rerun-if-changed=src/dispatch_shim.c");
    }
}
