//! Persistent directory redirections for missing media.
//!
//! A redirection maps an old parent-directory prefix to a new location:
//! a missing file under the old prefix is looked up under the new one.
//! Redirections live as JSON in the OS config directory, load on every
//! run, and back the `--redirect` CLI flag. Manual `--relink` picks stay
//! one-shot: an explicit choice for this run is not silently persisted.

use serde::{Deserialize, Serialize};
use std::io;
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

/// The saved redirections. A missing or unreadable store reads as empty.
pub fn load_from(path: &Path) -> Vec<PathRedirection> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Applies `edit` to the saved redirections and returns the saved list.
///
/// The store stays locked from reading to replacing, so concurrent runs
/// cannot drop each other's changes. It is replaced atomically, and only
/// when `edit` changed the list.
pub fn update(
    path: &Path,
    edit: impl FnOnce(&mut Vec<PathRedirection>),
) -> io::Result<Vec<PathRedirection>> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut lock_name = path.file_name().unwrap_or_default().to_os_string();
    lock_name.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join(lock_name))?;
    lock.lock()?;
    let saved = load_from(path);
    let mut edited = saved.clone();
    edit(&mut edited);
    if edited != saved {
        let bytes = serde_json::to_vec_pretty(&edited).map_err(io::Error::other)?;
        crate::staged::write(path, &bytes)?;
    }
    Ok(edited)
}

/// Saves the changes an editor made from `base` to `edited`, keeping what
/// other runs saved meanwhile. An added redirection replaces a saved one
/// with the same old prefix; the list is kept sorted by old prefix.
pub fn save_edits(
    path: &Path,
    base: &[PathRedirection],
    edited: &[PathRedirection],
) -> io::Result<Vec<PathRedirection>> {
    let removed: Vec<_> = base
        .iter()
        .filter(|entry| !edited.contains(entry))
        .collect();
    let added: Vec<_> = edited
        .iter()
        .filter(|entry| !base.contains(entry))
        .collect();
    update(path, |saved| {
        saved.retain(|entry| {
            !removed.contains(&entry)
                && !added.iter().any(|new| new.from_prefix == entry.from_prefix)
        });
        saved.extend(added.into_iter().cloned());
        saved.sort_by(|left, right| left.from_prefix.cmp(&right.from_prefix));
    })
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
    fn store_is_written_only_when_an_update_changes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Align").join("redirections.json");
        assert!(update(&path, |_| {}).unwrap().is_empty());
        assert!(!path.exists(), "an unchanged list is not written");

        let redirections = vec![PathRedirection::new("/old", PathBuf::from("/new"))];
        let added = redirections.clone();
        assert_eq!(
            update(&path, |saved| saved.extend(added)).unwrap(),
            redirections
        );
        assert_eq!(load_from(&path), redirections);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        update(&path, |_| {}).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
    }

    #[test]
    fn concurrent_updates_keep_every_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redirections.json");
        std::thread::scope(|scope| {
            for index in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    let entry = PathRedirection::new(&format!("/old/{index}"), "/new".into());
                    update(path, |saved| saved.push(entry)).unwrap();
                });
            }
        });
        assert_eq!(load_from(&path).len(), 8);
    }

    #[test]
    fn saving_edits_keeps_redirections_saved_by_another_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redirections.json");
        let entry = |from: &str, to: &str| PathRedirection::new(from, PathBuf::from(to));
        let base = update(&path, |saved| saved.push(entry("/a", "/new/a"))).unwrap();
        // Another run adds /b while the editor removes /a and moves /c.
        update(&path, |saved| {
            saved.push(entry("/b", "/new/b"));
            saved.push(entry("/c", "/old/c"));
        })
        .unwrap();
        let edited = vec![entry("/c", "/new/c")];
        let saved = save_edits(&path, &base, &edited).unwrap();
        assert_eq!(saved, [entry("/b", "/new/b"), entry("/c", "/new/c")]);
        assert_eq!(load_from(&path), saved);
    }

    #[test]
    fn split_pair_rejects_bare_values() {
        assert!(split_pair("--redirect", "/only-old").is_err());
        let (left, right) = split_pair("--redirect", "/old=/new=extra").expect("pair");
        assert_eq!(left, "/old");
        assert_eq!(right, PathBuf::from("/new=extra"));
    }
}
