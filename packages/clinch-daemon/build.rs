#![deny(unsafe_code)]

//! Minimal build script: stamps `CLINCH_BUILD_HASH` the same way
//! `apps/desktop/src-tauri/build.rs` does, so the engine's Session Activity
//! startup line names its exact build. No `tauri_build` — the daemon is a
//! plain binary.

fn main() {
    // Build scripts run with the package root (`packages/clinch-daemon`) as
    // CWD, so the worktree's git dir is two levels up. Re-stamp whenever the
    // checkout moves: `git pull --ff-only` rewrites the branch ref without
    // touching HEAD's symref, so watch HEAD, the ref it points at, and
    // packed-refs (which covers packed branch refs).
    let git_dir = std::path::Path::new("../../.git");
    let head = git_dir.join("HEAD");
    if head.exists() {
        println!("cargo:rerun-if-changed={}", head.display());
        if let Ok(target) = std::fs::read_to_string(&head)
            && let Some(ref_path) = target.strip_prefix("ref:")
        {
            let ref_file = git_dir.join(ref_path.trim());
            if ref_file.exists() {
                println!("cargo:rerun-if-changed={}", ref_file.display());
            }
        }
    }
    let packed = git_dir.join("packed-refs");
    if packed.exists() {
        println!("cargo:rerun-if-changed={}", packed.display());
    }
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|sha| sha.trim().to_owned())
        .filter(|sha| !sha.is_empty());
    let hash = hash.unwrap_or_else(|| {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(elapsed) => format!("build-{:x}", elapsed.as_nanos()),
            Err(_) => "build-unknown".to_owned(),
        }
    });
    println!("cargo:rustc-env=CLINCH_BUILD_HASH={hash}");
}
