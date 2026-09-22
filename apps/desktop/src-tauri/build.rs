#![deny(unsafe_code)]

fn main() {
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let attributes = if windows_msvc {
        tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest())
    } else {
        tauri_build::Attributes::new()
    };
    if let Err(error) = tauri_build::try_build(attributes) {
        eprintln!("Tauri build failed: {error}");
        std::process::exit(1);
    }
    // Supply one manifest to both the app and Rust test executables. Tauri's
    // binary-only resource otherwise leaves tests without Common Controls v6.
    if windows_msvc {
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
    stamp_build_hash();
}

/// Stamp `CLINCH_BUILD_HASH` for the Session Activity startup line, so a
/// running binary always names its exact build. Prefers the git short SHA;
/// source archives without git metadata fall back to a build nonce, and the
/// build itself never fails over identity.
fn stamp_build_hash() {
    for candidate in ["../../.git/HEAD", "../../.git/refs/heads"] {
        if std::path::Path::new(candidate).exists() {
            println!("cargo:rerun-if-changed={candidate}");
        }
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
