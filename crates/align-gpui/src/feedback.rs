//! User-initiated issue reporting. Only the environment shown in the report
//! panel is prefilled; recordings, paths, logs, and error text are never sent.

use std::path::PathBuf;
use std::sync::OnceLock;

pub const ISSUES_URL: &str = "https://github.com/FedyaLight/align/issues";

/// `.github/ISSUE_TEMPLATE` form whose `environment` field the link fills.
const ISSUE_FORM: &str = "bug_report.yml";

/// Display name of the running operating system family.
pub fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    }
}

/// Align version, OS release, and architecture. Read once: the report panel
/// renders it every frame while open.
pub fn environment() -> &'static str {
    static ENVIRONMENT: OnceLock<String> = OnceLock::new();
    ENVIRONMENT.get_or_init(|| {
        format!(
            "Align {}\nOS: {}\nArchitecture: {}",
            env!("CARGO_PKG_VERSION"),
            os_release().unwrap_or_else(|| os_name().to_owned()),
            std::env::consts::ARCH,
        )
    })
}

/// The ProductVersion `sw_vers` prints, read without spawning a process.
#[cfg(target_os = "macos")]
fn os_release() -> Option<String> {
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    let (_, rest) = plist.split_once("<key>ProductVersion</key>")?;
    let (_, rest) = rest.split_once("<string>")?;
    let (version, _) = rest.split_once("</string>")?;
    Some(format!("macOS {}", version.trim()))
}

/// GetVersionEx reports a manifest-dependent compatibility version;
/// RtlGetVersion reports the installed one.
#[cfg(windows)]
fn os_release() -> Option<String> {
    use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
    // SAFETY: OSVERSIONINFOW is plain data; RtlGetVersion fills the struct
    // whose size is set in dwOSVersionInfoSize.
    let info = unsafe {
        let mut info: OSVERSIONINFOW = std::mem::zeroed();
        info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
        (RtlGetVersion(&mut info) == 0).then_some(info)?
    };
    let build = info.dwBuildNumber;
    let release = match (info.dwMajorVersion, info.dwMinorVersion) {
        // Windows 11 kept the 10.0 kernel version; its builds start at 22000.
        (10, 0) if build >= 22_000 => "11".to_owned(),
        (10, 0) => "10".to_owned(),
        (major, minor) => format!("{major}.{minor}"),
    };
    Some(format!("Windows {release} (build {build})"))
}

/// PRETTY_NAME names the distribution and release, e.g. "Fedora Linux 43".
#[cfg(not(any(target_os = "macos", windows)))]
fn os_release() -> Option<String> {
    ["/etc/os-release", "/usr/lib/os-release"]
        .into_iter()
        .find_map(|path| {
            let text = std::fs::read_to_string(path).ok()?;
            let name = text
                .lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))?;
            Some(name.trim().trim_matches('"').to_owned())
        })
        .filter(|name| !name.is_empty())
}

/// New-issue link that opens the bug form with the environment filled in.
/// Labels come from the form itself: a `labels` query parameter needs
/// triage permission, and GitHub answers 404 to reporters without it.
pub fn issue_url() -> String {
    let mut url = url::Url::parse(&format!("{ISSUES_URL}/new")).expect("static GitHub issue URL");
    url.query_pairs_mut()
        .append_pair("template", ISSUE_FORM)
        .append_pair("environment", environment());
    url.into()
}

fn notice_file() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Align")
        .join("feedback-notice-version")
}

/// A small card once per installed release, including the first installation.
/// Closing it persists the dismissal; the Help command remains available.
pub fn notice_pending() -> bool {
    !cfg!(debug_assertions)
        && std::fs::read_to_string(notice_file()).ok().as_deref() != Some(env!("CARGO_PKG_VERSION"))
}

pub fn dismiss_notice() {
    let path = notice_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, env!("CARGO_PKG_VERSION"));
}

#[cfg(test)]
mod tests {
    /// GitHub silently ignores an unknown template or field id, so a renamed
    /// form or field would leave reports without their environment.
    #[test]
    fn report_link_prefills_fields_of_the_repository_bug_form() {
        let form = include_str!("../../../.github/ISSUE_TEMPLATE/bug_report.yml");
        let url = url::Url::parse(&super::issue_url()).expect("valid issue URL");
        let mut fields = 0;
        for (name, value) in url.query_pairs() {
            match name.as_ref() {
                "template" => assert_eq!(value, super::ISSUE_FORM),
                field => {
                    // Line by line: Windows checkouts may convert to CRLF.
                    let id = format!("id: {field}");
                    assert!(
                        form.lines().any(|line| line.trim() == id),
                        "the bug form has no `{field}` field"
                    );
                    assert!(value.contains(env!("CARGO_PKG_VERSION")));
                    fields += 1;
                }
            }
        }
        assert_eq!(fields, 1, "the environment is the only prefilled field");
    }
}
