//! Build identifier for the diagnostics panel (proves which binary is running),
//! and on Windows the executable's icon, manifest and version information.

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
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources();
    }
}

/// Embed the icon Explorer, the taskbar and installer shortcuts show, and
/// version details from Cargo.toml. GPUI links the application manifest
/// (per-monitor DPI, common controls) itself; a second one would collide.
#[cfg(windows)]
fn windows_resources() {
    use std::path::PathBuf;
    let support = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Support/Installer");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let version = env!("CARGO_PKG_VERSION");
    let mut numbers: Vec<&str> = version.split(['.', '-', '+']).take(3).collect();
    numbers.resize(3, "0");
    let [major, minor, patch] = [numbers[0], numbers[1], numbers[2]];
    println!(
        "cargo:rerun-if-changed={}",
        support.join("Align.ico").display()
    );
    let icon = support.join("Align.ico").canonicalize().unwrap();
    // rc.exe accepts forward slashes; the verbatim prefix it does not.
    let icon = align_path(&icon);
    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "Align contributors\0"
      VALUE "FileDescription", "Align\0"
      VALUE "FileVersion", "{version}\0"
      VALUE "InternalName", "align\0"
      VALUE "OriginalFilename", "align.exe\0"
      VALUE "ProductName", "Align\0"
      VALUE "ProductVersion", "{version}\0"
      VALUE "LegalCopyright", "Copyright 2026 Align contributors. GPL-3.0-only.\0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let rc_path = out.join("Align.rc");
    std::fs::write(&rc_path, rc).unwrap();
    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}

#[cfg(windows)]
fn align_path(path: &std::path::Path) -> String {
    let text = path.display().to_string().replace('\\', "/");
    text.strip_prefix("//?/").map(str::to_owned).unwrap_or(text)
}

/// Cross-compiled Windows builds skip resources (no resource compiler).
#[cfg(not(windows))]
fn windows_resources() {}
