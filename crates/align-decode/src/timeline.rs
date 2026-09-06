//! Timeline file dispatch shared by CLI, GUI and the synchronization pipeline.
use std::{path::Path, sync::atomic::AtomicBool};

use align_core::{TimelineSequenceSummary, xml::TimelineDraft};

#[derive(Debug, thiserror::Error)]
pub enum TimelineError {
    #[error(transparent)]
    Xml(#[from] align_core::xml::ImportError),
    #[error(transparent)]
    Aaf(#[from] crate::aaf::AafError),
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

pub fn read(
    path: &Path,
    sequence: Option<usize>,
    cancel: &AtomicBool,
) -> Result<TimelineDraft, TimelineError> {
    if is_aaf(path) {
        Ok(crate::aaf::read_audio(path, cancel)?.into_draft(path, sequence)?)
    } else {
        Ok(align_core::read_timeline(path, sequence)?)
    }
}

pub fn sequences(
    path: &Path,
    cancel: &AtomicBool,
) -> Result<Vec<TimelineSequenceSummary>, TimelineError> {
    if is_aaf(path) {
        Ok(crate::aaf::read_audio(path, cancel)?.summaries())
    } else {
        Ok(align_core::timeline_sequence_summaries(path)?)
    }
}
