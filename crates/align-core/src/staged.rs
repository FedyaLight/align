//! Atomic file replacement.
//!
//! Generated files (cache entries, rendered audio, timelines, remuxed media,
//! settings) are written to a temporary sibling and moved over their
//! destination only once complete. Readers never observe a partial file, and
//! a failed or cancelled write leaves any previous destination untouched.
//!
//! That is the guarantee every writer gets. Files a user keeps and cannot
//! regenerate, such as fixed timelines and saved media copies, also call
//! [`StagedFile::sync`] so their contents reach storage before they replace
//! the destination. Project documents add their own ownership and locking
//! rules in the desktop application.

use std::io;
use std::path::{Path, PathBuf};

/// A temporary sibling of `destination` for publishing a completed file.
///
/// The staged file exists (empty) on creation and is removed when dropped
/// without committing. No handle is held open, so writers, external
/// processes, and in-place rewrites can all use [`path`] on every platform.
///
/// [`commit`]: StagedFile::commit
/// [`path`]: StagedFile::path
pub struct StagedFile {
    temporary: tempfile::TempPath,
    destination: PathBuf,
}

impl StagedFile {
    /// Creates the staged file in the destination's folder. Its name keeps the
    /// destination's extension, which tools such as FFmpeg use to pick the
    /// output format.
    pub fn new(destination: &Path) -> io::Result<Self> {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let stem = destination
            .file_stem()
            .map_or_else(|| "file".into(), |stem| stem.to_string_lossy());
        let suffix = destination.extension().map_or_else(
            || ".partial".into(),
            |ext| format!(".partial.{}", ext.to_string_lossy()),
        );
        let temporary = tempfile::Builder::new()
            .prefix(&format!(".{stem}-"))
            .suffix(&suffix)
            .tempfile_in(parent)?
            .into_temp_path();
        Ok(Self {
            temporary,
            destination: destination.to_path_buf(),
        })
    }

    /// Where to write the new contents.
    pub fn path(&self) -> &Path {
        &self.temporary
    }

    /// The file this one replaces on commit.
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Flushes the written contents to storage, so a crash after the commit
    /// cannot leave the destination empty or partial.
    pub fn sync(&self) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .open(self.path())?
            .sync_all()
    }

    /// Atomically replaces the destination. On Windows an existing file is
    /// replaced in the same operation, so the destination never disappears.
    pub fn commit(self) -> io::Result<()> {
        self.temporary
            .persist(&self.destination)
            .map_err(|error| error.error)
    }

    /// Publishes the completed file only if the destination is still free.
    pub fn commit_new(self) -> io::Result<()> {
        self.temporary
            .persist_noclobber(&self.destination)
            .map_err(|error| error.error)
    }
}

/// Writes `bytes` to `destination` atomically.
pub fn write(destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let staged = StagedFile::new(destination)?;
    std::fs::write(staged.path(), bytes)?;
    staged.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn commit_replaces_and_drop_discards() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("take.wav");
        std::fs::write(&destination, b"previous").unwrap();

        let staged = StagedFile::new(&destination).unwrap();
        assert!(staged.path().is_file());
        assert_eq!(staged.path().parent(), Some(dir.path()));
        assert!(staged.path().to_string_lossy().ends_with(".partial.wav"));
        std::fs::write(staged.path(), b"abandoned").unwrap();
        drop(staged);
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous");
        assert_eq!(entries(dir.path()), ["take.wav"]);

        write(&destination, b"replacement").unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"replacement");
        assert_eq!(entries(dir.path()), ["take.wav"]);
    }

    #[test]
    fn commit_new_preserves_a_destination_created_during_render() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("timeline.xml");
        let staged = StagedFile::new(&destination).unwrap();
        std::fs::write(staged.path(), b"finished render").unwrap();
        std::fs::write(&destination, b"already published").unwrap();

        assert_eq!(
            staged.commit_new().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(destination).unwrap(), b"already published");
        assert_eq!(entries(dir.path()), ["timeline.xml"]);
    }

    #[test]
    fn relative_destination_without_folder_is_staged_in_the_working_folder() {
        let staged = StagedFile::new(Path::new("align-staged-test")).unwrap();
        assert_eq!(
            staged.path().parent(),
            Some(std::env::current_dir().unwrap().as_path())
        );
        assert!(staged.path().to_string_lossy().ends_with(".partial"));
    }
}
