//! The desktop settings file. Each setting reads and replaces its own key and
//! keeps the others, so the appearance and the export defaults never
//! overwrite each other. A missing or unreadable value reads as absent.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

const LAST_EXPORT_DIRECTORY: &str = "lastExportDirectory";

pub fn file() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Align")
        .join("settings.json")
}

fn entries(path: &Path) -> Map<String, Value> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn read<T: DeserializeOwned>(path: &Path, key: &str) -> Option<T> {
    entries(path)
        .remove(key)
        .and_then(|value| serde_json::from_value(value).ok())
}

/// Store `value` under `key`; `None` removes the key.
pub fn write<T: Serialize + ?Sized>(
    path: &Path,
    key: &str,
    value: Option<&T>,
) -> std::io::Result<()> {
    let mut entries = entries(path);
    match value {
        Some(value) => {
            entries.insert(key.to_string(), serde_json::to_value(value)?);
        }
        None => {
            entries.remove(key);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    align_core::staged::write(path, &serde_json::to_vec_pretty(&entries)?)
}

pub fn last_export_directory(settings: &Path) -> Option<PathBuf> {
    read::<PathBuf>(settings, LAST_EXPORT_DIRECTORY)
        .filter(|path| align_decode::export::check_export_directory(path).is_ok())
}

pub fn remember_export_directory(settings: &Path, directory: &Path) -> std::io::Result<()> {
    write(settings, LAST_EXPORT_DIRECTORY, Some(directory))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_written_without_disturbing_each_other() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Align").join("settings.json");
        assert_eq!(read::<String>(&path, "appearance"), None);
        write(&path, "appearance", Some(&"dark")).unwrap();
        write(&path, "exportDefaults", Some(&vec![1, 2])).unwrap();
        assert_eq!(read::<String>(&path, "appearance").as_deref(), Some("dark"));
        write::<()>(&path, "exportDefaults", None).unwrap();
        assert_eq!(read::<Vec<u8>>(&path, "exportDefaults"), None);
        assert_eq!(read::<String>(&path, "appearance").as_deref(), Some("dark"));
        // A value of another shape reads as absent.
        assert_eq!(read::<u32>(&path, "appearance"), None);
    }

    #[test]
    fn remembered_export_folder_is_restored_only_while_usable() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        let destination = root.path().join("destination");
        std::fs::create_dir(&destination).unwrap();
        remember_export_directory(&settings, &destination).unwrap();
        assert_eq!(last_export_directory(&settings), Some(destination.clone()));

        std::fs::remove_dir(&destination).unwrap();
        assert_eq!(last_export_directory(&settings), None);
        assert!(!destination.exists());
        std::fs::write(&destination, b"regular file").unwrap();
        assert_eq!(last_export_directory(&settings), None);
    }
}
