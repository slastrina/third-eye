fn main() {
    // The screencapturekit crate links a Swift bridge, so every binary this
    // crate produces (app + test runners) needs the system Swift runtime on
    // its rpath; without it dyld aborts at launch looking for
    // libswift_Concurrency.dylib. /usr/lib/swift resolves from the dyld
    // shared cache on every supported macOS.
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    // Build stamp (user request 2026-07-31): Settings shows exactly which
    // build is running — the "did my /Applications copy actually update"
    // question answered from inside the app.
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=THIRD_EYE_BUILD_EPOCH={epoch}");
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=THIRD_EYE_GIT_HASH={hash}");
    // Re-stamp when the checked-out commit moves. Commits update the
    // BRANCH REF (refs/heads/<branch>) or packed-refs — .git/HEAD only
    // changes on branch switch, so watching it alone left the stamp stale
    // across commits (the 5711f53-forever bug). Watch all three.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string("../.git/HEAD") {
        if let Some(reference) = head.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed=../.git/{reference}");
        }
    }

    tauri_build::build()
}
