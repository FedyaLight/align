#![forbid(unsafe_code)]

mod read;
mod storage;
mod write;

use anyhow::{Context, Result, bail};
use num_rational::BigRational;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};

type Clock = BigRational;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Rate {
    numerator: i32,
    denominator: i32,
}

impl Rate {
    fn clock(self) -> Result<Clock> {
        anyhow::ensure!(
            self.numerator > 0 && self.denominator > 0,
            "Invalid AAF edit rate"
        );
        Ok(Clock::new(self.numerator.into(), self.denominator.into()))
    }

    fn from_clock(clock: &Clock) -> Result<Self> {
        Ok(Self {
            numerator: clock
                .numer()
                .to_i32()
                .context("AAF exact clock exceeds supported precision")?,
            denominator: clock
                .denom()
                .to_i32()
                .context("AAF exact clock exceeds supported precision")?,
        })
    }

    fn decode(data: &[u8]) -> Result<Clock> {
        let data: [u8; 8] = storage::bytes(data)?;
        Self {
            numerator: i32::from_le_bytes(storage::bytes(&data[..4])?),
            denominator: i32::from_le_bytes(storage::bytes(&data[4..])?),
        }
        .clock()
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Resolve {}", path.display()))?;
    // Win32 canonicalization can add a verbatim prefix, which file:// URLs
    // and editor-facing paths do not use.
    #[cfg(windows)]
    if let Some(path) = canonical.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        if let Some(unc) = path.strip_prefix(r"UNC\") {
            return Ok(PathBuf::from(format!(r"\\{unc}")));
        }
        return Ok(PathBuf::from(path));
    }
    Ok(canonical)
}

fn staged(destination: &Path, work: impl FnOnce(&mut fs::File) -> Result<()>) -> Result<PathBuf> {
    let destination = std::path::absolute(destination)?;
    let parent = destination
        .parent()
        .context("AAF output requires a file name")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".align-aaf-")
        .tempfile_in(parent)?;
    work(temporary.as_file_mut())?;
    temporary.as_file().sync_all()?;
    temporary.persist(&destination)?;
    absolute(&destination)
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let command = args
        .first()
        .and_then(|arg| arg.to_str())
        .unwrap_or_default();
    let result: Value = match (command, &args[1.min(args.len())..]) {
        ("--version", []) => json!({
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": 3,
            "platform": platform(),
        }),
        ("read-timeline", [path, rest @ ..]) if rest.len() <= 1 => {
            read::timeline(Path::new(path), rest.first().map(Path::new))?
        }
        ("repair-paths", [manifest, source, destination]) => read::repair(
            Path::new(manifest),
            Path::new(source),
            Path::new(destination),
        )?,
        ("write-audio" | "write-timeline", [manifest, destination]) => {
            write::write(Path::new(manifest), Path::new(destination))?
        }
        _ => bail!(
            "Usage: align-aaf read-timeline INPUT [EXTRACT-DIRECTORY] | repair-paths MANIFEST INPUT OUTPUT | write-timeline MANIFEST OUTPUT"
        ),
    };
    serde_json::to_writer(io::stdout().lock(), &result)?;
    println!();
    Ok(())
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn integer(value: i64) -> Clock {
    Clock::from_integer(value.into())
}

fn local_path(url: &str) -> Result<String> {
    use percent_encoding::percent_decode_str;
    let (scheme, tail) = url
        .split_once(':')
        .context("AAF source locator is not a local file")?;
    anyhow::ensure!(
        scheme.eq_ignore_ascii_case("file"),
        "AAF source locator is not a local file"
    );
    let tail = if let Some(remote) = tail.strip_prefix("//") {
        let split = remote.find('/').unwrap_or(remote.len());
        let (authority, tail) = remote.split_at(split);
        anyhow::ensure!(
            authority.is_empty() || authority == "localhost",
            "AAF source locator is not a local file"
        );
        tail
    } else {
        tail
    };
    let tail = tail.split(['?', '#']).next().unwrap_or_default();
    let path = percent_decode_str(tail).decode_utf8_lossy().into_owned();
    anyhow::ensure!(
        !path.is_empty() && !path.contains('\0'),
        "Invalid AAF media path"
    );
    let drive = matches!(path.as_bytes(), [b'/', letter, b':', ..] if letter.is_ascii_alphabetic());
    Ok(if drive { path[1..].to_owned() } else { path })
}

fn file_url(path: &Path) -> Result<String> {
    url::Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|()| anyhow::anyhow!("Cannot encode AAF media path {}", path.display()))
}

fn normalized(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir
                if clean
                    .file_name()
                    .is_some_and(|name| name != OsStr::new("..")) =>
            {
                clean.pop();
            }
            Component::ParentDir if clean.has_root() => {}
            _ => clean.push(part.as_os_str()),
        }
    }
    #[cfg(windows)]
    return PathBuf::from(clean.to_string_lossy().replace('/', "\\").to_lowercase());
    #[cfg(not(windows))]
    clean
}
