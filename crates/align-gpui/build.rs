//! Build identifier for the diagnostics panel (proves which binary is running).

fn main() {
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "nogit".to_string());
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    println!(
        "cargo:rustc-env=ALIGN_BUILD_ID={hash}{}",
        if dirty { "-dirty" } else { "" }
    );
    println!("cargo:rerun-if-changed=build.rs");
    // Rebuild the id when the tree changes (best effort; timestamp fallback).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
