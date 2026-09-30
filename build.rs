fn main() {
    // WinFsp is delay-loaded: the exe starts without it, and we load the
    // installed DLL ourselves (src/winfsp_loader.rs) so a missing WinFsp is a
    // friendly message instead of a loader crash. Only the app binary imports
    // WinFsp; tests/examples never mount, so they don't need the flag (and
    // the linker warns if it's given with nothing to delay).
    let dll = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "winfsp-a64.dll",
        Ok("x86") => "winfsp-x86.dll",
        _ => "winfsp-x64.dll",
    };
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg-bin=othcloud-drive=/DELAYLOAD:{dll}");
    println!("cargo:rerun-if-changed=build.rs");
}
