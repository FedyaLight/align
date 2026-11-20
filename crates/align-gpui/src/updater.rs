//! Application updates from GitHub Releases, on every platform.
//!
//! Windows Setup installs update through Velopack. The macOS app and the
//! Fedora and Arch packages are not Velopack installations: for those the
//! release's own installer asset is downloaded, checked against its
//! `.sha256` file and installed in place (the app bundle is replaced; the
//! package goes through the system package manager behind a polkit prompt).
//! Anything else — a portable copy, an app run from the DMG, a development
//! build — is told about the new version and offered the release page.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::{Digest, Sha256};
#[cfg(windows)]
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackAsset, sources::GithubSource};

pub const REPOSITORY_URL: &str = "https://github.com/FedyaLight/align";
/// One-time donation, the `custom` link in `.github/FUNDING.yml`.
pub const DONATION_URL: &str = "https://www.patreon.com/fedyalight/posts/support-once-170786295";
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/FedyaLight/align/releases/latest";

#[derive(Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    Current,
    Available(Update),
    Downloading {
        version: String,
    },
    Ready(Prepared),
    Installing {
        version: String,
    },
    Failed,
}

/// A newer release and how this copy of Align can reach it.
pub enum Update {
    #[cfg(windows)]
    Velopack {
        manager: UpdateManager,
        update: Box<UpdateInfo>,
    },
    Package {
        install: Install,
        version: String,
        asset: ReleaseAsset,
    },
    /// No in-place route: open the release page.
    Manual { version: String, page: String },
}

/// A downloaded update, ready to install.
pub enum Prepared {
    #[cfg(windows)]
    Velopack {
        manager: UpdateManager,
        asset: VelopackAsset,
    },
    Package {
        install: Install,
        version: String,
        file: tempfile::TempPath,
    },
}

/// How a non-Velopack installation replaces itself.
#[derive(Clone, Debug, PartialEq)]
pub enum Install {
    /// The app bundle at this path, in a folder the user can write to.
    MacApp(PathBuf),
    Rpm,
    Pacman,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub checksum_url: String,
}

impl UpdateState {
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Available(update) => Some(update.version()),
            Self::Downloading { version } | Self::Installing { version } => Some(version),
            #[cfg(windows)]
            Self::Ready(Prepared::Velopack { asset, .. }) => Some(&asset.Version),
            Self::Ready(Prepared::Package { version, .. }) => Some(version),
            _ => None,
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Downloading { .. } | Self::Installing { .. }
        )
    }
}

impl Update {
    pub fn version(&self) -> &str {
        match self {
            #[cfg(windows)]
            Self::Velopack { update, .. } => &update.TargetFullRelease.Version,
            Self::Package { version, .. } | Self::Manual { version, .. } => version,
        }
    }
}

pub fn check() -> UpdateState {
    #[cfg(windows)]
    if let Ok(manager) =
        UpdateManager::new(GithubSource::new(REPOSITORY_URL, None, false), None, None)
    {
        return match manager.check_for_updates() {
            Ok(UpdateCheck::UpdateAvailable(update)) => {
                UpdateState::Available(Update::Velopack { manager, update })
            }
            Ok(UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty) => UpdateState::Current,
            Err(error) => {
                eprintln!("Could not check for Align updates: {error}");
                UpdateState::Failed
            }
        };
    }
    match latest_release() {
        Ok(release) => evaluate(&release, env!("CARGO_PKG_VERSION"), detect_install()),
        Err(error) => {
            eprintln!("Could not check for Align updates: {error}");
            UpdateState::Failed
        }
    }
}

pub fn download(update: Update) -> UpdateState {
    match update {
        #[cfg(windows)]
        Update::Velopack { manager, update } => {
            let asset = update.TargetFullRelease.clone();
            match manager.download_updates(&update, None) {
                Ok(()) => UpdateState::Ready(Prepared::Velopack { manager, asset }),
                Err(error) => {
                    eprintln!("Could not download the Align update: {error}");
                    UpdateState::Failed
                }
            }
        }
        Update::Package {
            install,
            version,
            asset,
        } => match download_verified(&asset) {
            Ok(file) => UpdateState::Ready(Prepared::Package {
                install,
                version,
                file,
            }),
            Err(error) => {
                eprintln!("Could not download the Align update: {error}");
                UpdateState::Failed
            }
        },
        Update::Manual { .. } => UpdateState::Failed,
    }
}

/// Install a downloaded package in place; the caller relaunches.
pub fn install(install: &Install, file: &Path) -> Result<(), String> {
    match install {
        Install::MacApp(bundle) => replace_app_bundle(bundle, file),
        Install::Rpm => run(Command::new("/usr/bin/pkexec").args([
            "/usr/bin/dnf".as_ref(),
            "--assumeyes".as_ref(),
            "install".as_ref(),
            file.as_os_str(),
        ])),
        Install::Pacman => run(Command::new("/usr/bin/pkexec").args([
            "/usr/bin/pacman".as_ref(),
            "-U".as_ref(),
            "--noconfirm".as_ref(),
            file.as_os_str(),
        ])),
    }
}

/// Start the freshly installed copy. The caller quits this one.
pub fn relaunch(install: &Install) -> Result<(), String> {
    let mut command = match install {
        Install::MacApp(bundle) => {
            let mut command = Command::new("/usr/bin/open");
            command.arg("-n").arg(bundle);
            command
        }
        Install::Rpm | Install::Pacman => Command::new("/opt/align/align"),
    };
    command.spawn().map(drop).map_err(|error| error.to_string())
}

// ---------------------------------------------------------------- GitHub

#[derive(Debug, PartialEq)]
struct Release {
    version: String,
    page: String,
    assets: Vec<(String, String)>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .user_agent(concat!("Align/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn latest_release() -> Result<Release, String> {
    let body = agent()
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|error| error.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|error| error.to_string())?;
    parse_release(&body).ok_or_else(|| "unexpected release response".to_owned())
}

fn parse_release(body: &str) -> Option<Release> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = json["tag_name"].as_str()?;
    Some(Release {
        version: tag.trim_start_matches('v').to_owned(),
        page: json["html_url"].as_str()?.to_owned(),
        assets: json["assets"]
            .as_array()?
            .iter()
            .filter_map(|asset| {
                Some((
                    asset["name"].as_str()?.to_owned(),
                    asset["browser_download_url"].as_str()?.to_owned(),
                ))
            })
            .collect(),
    })
}

/// Compare the latest release with the running version and pick the
/// installer asset this installation can apply.
fn evaluate(release: &Release, current: &str, install: Option<Install>) -> UpdateState {
    let (Ok(latest), Ok(current)) = (
        semver::Version::parse(&release.version),
        semver::Version::parse(current),
    ) else {
        return UpdateState::Failed;
    };
    if latest <= current {
        return UpdateState::Current;
    }
    let version = release.version.clone();
    let wanted = install
        .as_ref()
        .map(|install| asset_name(install, &version));
    let asset = wanted.and_then(|name| {
        let url = |wanted: &str| {
            release
                .assets
                .iter()
                .find(|(name, _)| name == wanted)
                .map(|(_, url)| url.clone())
        };
        Some(ReleaseAsset {
            url: url(&name)?,
            checksum_url: url(&format!("{name}.sha256"))?,
            name,
        })
    });
    UpdateState::Available(match (install, asset) {
        (Some(install), Some(asset)) => Update::Package {
            install,
            version,
            asset,
        },
        _ => Update::Manual {
            version,
            page: release.page.clone(),
        },
    })
}

fn asset_name(install: &Install, version: &str) -> String {
    match install {
        Install::MacApp(_) => format!("Align-{version}-macOS-arm64.dmg"),
        Install::Rpm => format!("Align-{version}-Fedora-x86_64.rpm"),
        Install::Pacman => format!("align-{version}-1-x86_64.pkg.tar.zst"),
    }
}

fn download_verified(asset: &ReleaseAsset) -> Result<tempfile::TempPath, String> {
    let agent = agent();
    let expected = agent
        .get(&asset.checksum_url)
        .call()
        .map_err(|error| error.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|error| error.to_string())?;
    let expected = expected
        .split_whitespace()
        .next()
        .ok_or("empty checksum file")?
        .to_ascii_lowercase();
    // Preserve the package suffix for dnf/pacman. TempPath owns the
    // download until the installer process has finished consuming it.
    let mut file = tempfile::Builder::new()
        .prefix("align-update-")
        .suffix(&asset.name)
        .tempfile()
        .map_err(|e| e.to_string())?;
    let mut reader = agent
        .get(&asset.url)
        .call()
        .map_err(|error| error.to_string())?
        .into_body()
        .into_reader();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| error.to_string())?;
    }
    file.as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual != expected {
        return Err(format!("checksum mismatch for {}", asset.name));
    }
    Ok(file.into_temp_path())
}

// ---------------------------------------------------------- installation

/// How this copy was installed, when it can update itself in place.
fn detect_install() -> Option<Install> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    if cfg!(target_os = "macos") {
        let bundle = exe
            .ancestors()
            .find(|path| path.extension().is_some_and(|extension| extension == "app"))?;
        // A copy running from the mounted DMG or from a read-only folder
        // cannot be replaced; it is pointed at the download instead.
        if bundle.starts_with("/Volumes") || !writable(bundle.parent()?) {
            return None;
        }
        return Some(Install::MacApp(bundle.to_path_buf()));
    }
    if cfg!(target_os = "linux") && exe.starts_with("/opt/align") {
        let owns = |tool: &str, args: &[&str]| {
            Path::new(tool).exists()
                && Command::new(tool)
                    .args(args)
                    .output()
                    .is_ok_and(|output| output.status.success())
        };
        if !Path::new("/usr/bin/pkexec").exists() {
            return None;
        }
        if owns("/usr/bin/dnf", &["--version"]) && owns("/usr/bin/rpm", &["-q", "align"]) {
            return Some(Install::Rpm);
        }
        if owns("/usr/bin/pacman", &["-Q", "align"]) {
            return Some(Install::Pacman);
        }
    }
    None
}

fn writable(dir: &Path) -> bool {
    tempfile::tempdir_in(dir).is_ok()
}

fn run(command: &mut Command) -> Result<(), String> {
    let output = command.output().map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if detail.is_empty() {
        format!("installer exited with {}", output.status)
    } else {
        detail
    })
}

/// Copy the new bundle out of the disk image next to the current one, then
/// swap the two, so a failure leaves the installed app untouched.
fn replace_app_bundle(bundle: &Path, dmg: &Path) -> Result<(), String> {
    let parent = bundle.parent().ok_or("app bundle has no folder")?;
    let staging = tempfile::tempdir_in(parent).map_err(|e| e.to_string())?;
    let staged = staging.path().join("Align.app");
    let mount_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mount = mount_dir.path();
    run(Command::new("/usr/bin/hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noautoopen",
            "-mountpoint",
        ])
        .arg(mount)
        .arg(dmg))?;
    // Once attached, never recursively remove a live mount on an error.
    let mount = mount_dir.keep();
    let copied = run(Command::new("/usr/bin/ditto")
        .arg(mount.join("Align.app"))
        .arg(&staged));
    run(Command::new("/usr/bin/hdiutil")
        .args(["detach", "-force"])
        .arg(&mount))?;
    // hdiutil may already have removed the mount point.
    if let Err(e) = std::fs::remove_dir(&mount)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e.to_string());
    }
    copied?;
    let _ = run(Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&staged));
    let previous = staging.path().join("previous.app");
    std::fs::rename(bundle, &previous).map_err(|error| error.to_string())?;
    if let Err(error) = std::fs::rename(&staged, bundle) {
        if let Err(restore) = std::fs::rename(&previous, bundle) {
            let _ = staging.keep();
            return Err(format!(
                "{error}. Restore {} manually: {restore}",
                previous.display()
            ));
        }
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(version: &str, assets: &[&str]) -> Release {
        Release {
            version: version.into(),
            page: format!("{REPOSITORY_URL}/releases/tag/v{version}"),
            assets: assets
                .iter()
                .map(|name| (name.to_string(), format!("https://example.invalid/{name}")))
                .collect(),
        }
    }

    #[test]
    fn simultaneous_downloads_have_independent_lifetimes() {
        use std::io::{BufRead, BufReader};
        let server = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = server.local_addr().unwrap();
        let asset = ReleaseAsset {
            name: "Align.rpm".into(),
            url: format!("http://{address}/package"),
            checksum_url: format!("http://{address}/checksum"),
        };
        let worker = std::thread::spawn(move || {
            for connection in server.incoming().take(4) {
                let mut connection = connection.unwrap();
                let mut request = String::new();
                let mut reader = BufReader::new(&connection);
                reader.read_line(&mut request).unwrap();
                for line in reader.lines() {
                    if line.unwrap().is_empty() {
                        break;
                    }
                }
                let body = if request.starts_with("GET /checksum ") {
                    format!("{:x}", Sha256::digest(b"package"))
                } else {
                    "package".to_owned()
                };
                write!(
                    connection,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let first = download_verified(&asset).unwrap();
        let second = download_verified(&asset).unwrap();
        let path = first.to_path_buf();
        drop(first);
        assert!(!path.exists());
        assert_eq!(std::fs::read(&second).unwrap(), b"package");
        worker.join().unwrap();
    }

    #[test]
    fn transient_update_states_are_busy() {
        assert!(UpdateState::Checking.is_busy());
        assert!(
            UpdateState::Downloading {
                version: "1.2.3".into()
            }
            .is_busy()
        );
        assert!(
            UpdateState::Installing {
                version: "1.2.3".into()
            }
            .is_busy()
        );
        assert!(!UpdateState::Idle.is_busy());
        assert!(!UpdateState::Current.is_busy());
    }

    #[test]
    fn github_release_responses_parse() {
        let body = r#"{"tag_name":"v0.3.0","html_url":"https://github.com/FedyaLight/align/releases/tag/v0.3.0",
            "assets":[{"name":"Align-0.3.0-Fedora-x86_64.rpm","browser_download_url":"https://x/rpm"}]}"#;
        let release = parse_release(body).unwrap();
        assert_eq!(release.version, "0.3.0");
        assert_eq!(
            release.assets,
            vec![(
                "Align-0.3.0-Fedora-x86_64.rpm".into(),
                "https://x/rpm".into()
            )]
        );
        assert!(parse_release(r#"{"message":"Not Found"}"#).is_none());
    }

    #[test]
    fn only_newer_releases_are_offered() {
        let latest = release("0.2.0", &[]);
        assert!(matches!(
            evaluate(&latest, "0.2.0", None),
            UpdateState::Current
        ));
        assert!(matches!(
            evaluate(&latest, "0.10.0", None),
            UpdateState::Current
        ));
        assert!(matches!(
            evaluate(&latest, "0.1.0", None),
            UpdateState::Available(Update::Manual { .. })
        ));
    }

    #[test]
    fn installations_pick_their_own_verified_asset() {
        let latest = release(
            "0.3.0",
            &[
                "Align-0.3.0-macOS-arm64.dmg",
                "Align-0.3.0-macOS-arm64.dmg.sha256",
                "Align-0.3.0-Fedora-x86_64.rpm",
                "Align-0.3.0-Fedora-x86_64.rpm.sha256",
                "align-0.3.0-1-x86_64.pkg.tar.zst",
            ],
        );
        let UpdateState::Available(Update::Package { asset, .. }) =
            evaluate(&latest, "0.2.0", Some(Install::Rpm))
        else {
            panic!("the RPM should update in place");
        };
        assert_eq!(asset.name, "Align-0.3.0-Fedora-x86_64.rpm");
        assert!(asset.checksum_url.ends_with(".rpm.sha256"));
        let mac = Install::MacApp("/Applications/Align.app".into());
        assert!(matches!(
            evaluate(&latest, "0.2.0", Some(mac)),
            UpdateState::Available(Update::Package { .. })
        ));
        // Without a checksum file the package is never installed blindly.
        assert!(matches!(
            evaluate(&latest, "0.2.0", Some(Install::Pacman)),
            UpdateState::Available(Update::Manual { .. })
        ));
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    /// Hits the live GitHub API: `cargo test -p align-gpui -- --ignored`.
    #[test]
    #[ignore]
    fn the_latest_release_is_readable() {
        let release = latest_release().unwrap();
        assert!(semver::Version::parse(&release.version).is_ok());
        assert!(
            release
                .assets
                .iter()
                .any(|(name, _)| name.ends_with(".sha256"))
        );
        let asset = release
            .assets
            .iter()
            .find(|(name, _)| name.ends_with(".rpm"))
            .map(|(name, url)| ReleaseAsset {
                name: name.clone(),
                url: url.clone(),
                checksum_url: format!("{url}.sha256"),
            })
            .unwrap();
        let file = download_verified(&asset).unwrap();
        assert!(std::fs::metadata(&file).unwrap().len() > 1_000_000);
        let path = file.to_path_buf();
        drop(file);
        assert!(!path.exists());
    }
}
