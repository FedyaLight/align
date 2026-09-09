//! Persistent directory redirections for missing media.
//!
//! A redirection maps an old parent-directory prefix to a new location:
//! a missing file under the old prefix is looked up under the new one.
//! Redirections live as JSON in the OS config directory, load on every
//! run, and back the `--redirect` CLI flag. Manual `--relink` picks stay
//! one-shot: an explicit choice for this run is not silently persisted.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathRedirection {
    /// Old parent-directory prefix, verbatim (trailing slashes trimmed).
    pub from_prefix: String,
    /// New location looked up instead.
    pub to_dir: PathBuf,
}

impl PathRedirection {
    pub fn new(from_prefix: &str, to_dir: PathBuf) -> Self {
        Self {
            from_prefix: from_prefix.trim_end_matches('/').to_string(),
            to_dir,
        }
    }
}

/// Production store location (OS config directory).
pub fn config_file() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Align")
        .join("redirections.json")
}

pub fn load_from(path: &Path) -> Vec<PathRedirection> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save_to(path: &Path, redirections: &[PathRedirection]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_string_pretty(redirections) {
        let _ = std::fs::write(path, bytes);
    }
}

/// Rewrite `missing` through the longest matching prefix. Existence is
/// checked by the caller, so pure mapping stays unit-testable.
pub fn rewrite(missing: &Path, redirections: &[PathRedirection]) -> Option<PathBuf> {
    redirections
        .iter()
        .filter_map(|redirection| {
            missing
                .strip_prefix(&redirection.from_prefix)
                .ok()
                .map(|rest| (redirection.from_prefix.len(), redirection.to_dir.join(rest)))
        })
        .max_by_key(|(prefix_len, _)| *prefix_len)
        .map(|(_, path)| path)
}

/// Parse one `--redirect OLD=NEW` / `--relink NAME=PATH` pair on the first `=`.
pub fn split_pair(flag: &str, value: &str) -> Result<(String, PathBuf), String> {
    value
        .split_once('=')
        .map(|(left, right)| (left.to_string(), PathBuf::from(right)))
        .ok_or_else(|| format!("{flag} requires OLD=NEW (got {value:?})."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_prefers_longest_prefix() {
        let redirections = vec![
            PathRedirection::new("/media", PathBuf::from("/new/media")),
            PathRedirection::new("/media/day-1", PathBuf::from("/new/day-1")),
        ];
        assert_eq!(
            rewrite(Path::new("/media/day-1/card-a/a.mov"), &redirections),
            Some(PathBuf::from("/new/day-1/card-a/a.mov"))
        );
        assert_eq!(
            rewrite(Path::new("/media/day-2/b.mov"), &redirections),
            Some(PathBuf::from("/new/media/day-2/b.mov"))
        );
        assert_eq!(rewrite(Path::new("/other/c.mov"), &redirections), None);
    }

    #[test]
    fn store_roundtrips_through_json() {
        let dir = std::env::temp_dir().join(format!("align-redirect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("redirections.json");
        assert!(load_from(&path).is_empty());
        let redirections = vec![PathRedirection::new("/old", PathBuf::from("/new"))];
        save_to(&path, &redirections);
        assert_eq!(load_from(&path), redirections);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_pair_rejects_bare_values() {
        assert!(split_pair("--redirect", "/only-old").is_err());
        let (left, right) = split_pair("--redirect", "/old=/new=extra").expect("pair");
        assert_eq!(left, "/old");
        assert_eq!(right, PathBuf::from("/new=extra"));
    }
}
