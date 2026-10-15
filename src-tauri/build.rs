fn main() {
    // On Windows, embed an application manifest that declares a dependency on
    // Common Controls v6. Without it, any binary that links `tauri` — including
    // the `cargo test` harness for this library — fails to start with
    // STATUS_ENTRYPOINT_NOT_FOUND (exit code 0xc0000139), because `tauri`
    // references comctl32 v6 entry points that are not activated otherwise.
    // This mirrors Tauri's own fix (tauri-apps/tauri PR #4383 / issue #11028)
    // and makes `cargo test` runnable on the Windows CI runner.
    //
    // The MSVC-only `/MANIFEST:EMBED` + `/MANIFESTINPUT` link args apply to the
    // test/binary targets built here; the packaged app bundle gets its manifest
    // through the normal Tauri bundling path, so this is additive and harmless
    // off-Windows (the block is compiled out on every other target).
    #[cfg(target_os = "windows")]
    {
        static WINDOWS_MANIFEST_FILE: &str = "Windows Manifest.xml";
        let mut manifest = std::env::current_dir().unwrap();
        manifest.push(WINDOWS_MANIFEST_FILE);
        println!("cargo:rerun-if-changed={WINDOWS_MANIFEST_FILE}");
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg=/MANIFESTINPUT:{}",
            manifest.to_str().unwrap()
        );
        // The embedded manifest is authoritative; silence the linker's default.
        println!("cargo:rustc-link-arg=/MANIFESTUAC:NO");
    }

    tauri_build::build()
}
