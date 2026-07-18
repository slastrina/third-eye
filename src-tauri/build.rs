fn main() {
    // The screencapturekit crate links a Swift bridge, so every binary this
    // crate produces (app + test runners) needs the system Swift runtime on
    // its rpath; without it dyld aborts at launch looking for
    // libswift_Concurrency.dylib. /usr/lib/swift resolves from the dyld
    // shared cache on every supported macOS.
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    tauri_build::build()
}
