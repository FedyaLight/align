//! Timeline file dispatch shared by CLI, GUI and the synchronization pipeline.
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use align_core::{TimelineSequenceSummary, xml::TimelineDraft};

#[derive(Debug, thiserror::Error)]
pub enum TimelineError {
    #[error(transparent)]
    Xml(#[from] align_core::xml::ImportError),
    #[error(transparent)]
    Aaf(#[from] crate::aaf::AafError),
    #[error("Cancelled")]
    Cancelled,
    #[error("Cannot write repaired project: {0}")]
    Write(String),
}

pub fn is_supported(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        ["xml", "fcpxml", "aaf"]
            .iter()
            .any(|extension| e.eq_ignore_ascii_case(extension))
    })
}

fn is_aaf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("aaf"))
}

/// Save a repaired project as a new file while leaving the imported source
/// untouched. XML is rewritten in-process; AAF delegates to the bundled
/// sidecar that owns its object graph.
pub fn write_relinked_copy(
    source: &Path,
    destination: &Path,
    replacements: &[(PathBuf, PathBuf)],
    cancel: &AtomicBool,
) -> Result<(), TimelineError> {
    let canonical_source = source.canonicalize().ok();
    let canonical_destination = destination.canonicalize().ok();
    if source == destination
        || (canonical_source.is_some() && canonical_source == canonical_destination)
    {
        return Err(TimelineError::Write(
            "choose a different destination so the imported project remains unchanged".into(),
        ));
    }
    if is_aaf(source) {
        crate::aaf::repair_paths(source, destination, replacements, cancel)?;
        return Ok(());
    }
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(TimelineError::Cancelled);
    }
    let bytes = std::fs::read(source).map_err(|error| TimelineError::Write(error.to_string()))?;
    let fixed = align_core::rewrite_media_paths(&bytes, replacements)?;
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| TimelineError::Write(error.to_string()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| TimelineError::Write(error.to_string()))?;
    temporary
        .write_all(&fixed)
        .and_then(|()| temporary.as_file_mut().sync_all())
        .map_err(|error| TimelineError::Write(error.to_string()))?;
    temporary
        .persist(destination)
        .map_err(|error| TimelineError::Write(error.error.to_string()))?;
    Ok(())
}

pub fn read(
    path: &Path,
    sequence: Option<usize>,
    cancel: &AtomicBool,
) -> Result<TimelineDraft, TimelineError> {
    read_with_proxies(path, sequence, cancel, false)
}

pub fn read_with_proxies(
    path: &Path,
    sequence: Option<usize>,
    cancel: &AtomicBool,
    prefer_proxies: bool,
) -> Result<TimelineDraft, TimelineError> {
    if is_aaf(path) {
        Ok(crate::aaf::read_timeline(path, cancel)?.into_draft(path, sequence)?)
    } else {
        Ok(align_core::xml::read_timeline_with_proxies(
            path,
            sequence,
            prefer_proxies,
        )?)
    }
}

pub fn sequences(
    path: &Path,
    cancel: &AtomicBool,
) -> Result<Vec<TimelineSequenceSummary>, TimelineError> {
    if is_aaf(path) {
        Ok(crate::aaf::read_timeline(path, cancel)?.summaries())
    } else {
        Ok(align_core::timeline_sequence_summaries(path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repaired_copy_cannot_replace_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("edit.xml");
        let original = b"<xmeml/>";
        std::fs::write(&source, original).unwrap();
        let error = write_relinked_copy(
            &source,
            &source,
            &[],
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(error, TimelineError::Write(_)));
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
}
