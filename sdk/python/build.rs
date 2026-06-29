//! Link/runtime setup for the native extension so it can find `libkrun`.
//!
//! Two concerns:
//!  * **Link time** — point the linker at a `libkrun` to link against
//!    (`SMOLVM_LIB_DIR` / `LIBKRUN_BUNDLE`, else the smolvm repo's `lib/`).
//!  * **Run time** — emit RELOCATABLE rpaths (`@loader_path` on macOS,
//!    `$ORIGIN` on Linux) so an *installed* extension finds `libkrun` /
//!    `libkrunfw` bundled next to it inside the wheel (the dylibs are staged
//!    into `python/smol/` by `scripts/bundle-native.sh`; maturin ships them
//!    alongside `_native`). An absolute rpath to the build-time lib dir is also
//!    added as a convenience for in-tree `maturin develop`.

use std::path::PathBuf;

fn main() {
    // Run time: relocatable rpaths FIRST. These let a shipped wheel load the
    // dylibs bundled alongside `_native` (smol/_native*.so → smol/libkrun.*),
    // independent of the machine that built it. Harmless during local dev (the
    // loader just falls through to the absolute rpath below).
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/lib");
    }
    #[cfg(target_os = "linux")]
    {
        // `$ORIGIN` is passed literally to the linker (cargo does not run a
        // shell over link args), so it resolves relative to the .so at load time.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/lib");
    }

    // Link time: locate libkrun to link against. Priority: explicit env, then
    // the smolvm repo's `lib/` (this crate is at smol/sdk/python → repo root is
    // three levels up).
    let lib_dir = std::env::var("SMOLVM_LIB_DIR")
        .or_else(|_| std::env::var("LIBKRUN_BUNDLE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            manifest
                .parent() // sdk
                .and_then(|p| p.parent()) // smol
                .and_then(|p| p.parent()) // repo root
                .map(|p| p.join("lib"))
                .unwrap_or_else(|| PathBuf::from("lib"))
        });

    if lib_dir.exists() {
        println!("cargo:rustc-link-search=native={}", lib_dir.display());
        // Dev convenience: absolute rpath so in-tree `maturin develop` loads
        // libkrun without bundling. Shipped wheels rely on the relocatable
        // rpaths above instead.
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
        println!(
            "cargo:warning=smol-py: using libkrun from {}",
            lib_dir.display()
        );
    } else {
        println!(
            "cargo:warning=smol-py: libkrun dir not found at {} — set SMOLVM_LIB_DIR (cloud transport still works without the native build)",
            lib_dir.display()
        );
    }

    println!("cargo:rerun-if-env-changed=SMOLVM_LIB_DIR");
    println!("cargo:rerun-if-env-changed=LIBKRUN_BUNDLE");
}
