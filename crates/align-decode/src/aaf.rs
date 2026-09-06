//! Process boundary for the bundled AAF writer.
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Serialize)]
pub struct AudioManifest {
    pub version: u32,
    pub name: String,
    pub tracks: Vec<AudioTrack>,
}
#[derive(Debug, Serialize)]
pub struct AudioTrack {
    pub name: String,
    pub sample_rate: u32,
    pub clips: Vec<AudioClip>,
}
#[derive(Debug, Serialize)]
pub struct AudioClip {
    pub path: PathBuf,
    pub start: u64,
    pub source_in: u64,
    pub length: u64,
    pub source_frames: u64,
    pub channels: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum AafError {
    #[error("AAF support module is missing; reinstall the complete Align package")]
    Missing,
    #[error("AAF export cancelled")]
    Cancelled,
    #[error("AAF writer failed: {0}")]
    Writer(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Invoke a manifest already staged by the export job. No shell interpretation.
/// The helper atomically commits the destination only after completing the AAF.
pub fn write_audio(
    manifest: &Path,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<(), AafError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(AafError::Cancelled);
    }
    let executable = crate::ff::resolve_bin("ALIGN_AAF", "align-aaf").ok_or(AafError::Missing)?;
    let mut child = Command::new(executable)
        .arg("write-audio")
        .arg(manifest)
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let status = crate::export::wait_render_process(&mut child, cancel).map_err(|error| {
        if cancel.load(Ordering::Relaxed) {
            AafError::Cancelled
        } else {
            AafError::Writer(error.to_string())
        }
    })?;
    if !status.success() {
        return Err(AafError::Writer(status.to_string()));
    }
    if !destination.is_file() {
        return Err(AafError::Writer("no output file".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_export_does_not_start_writer() {
        assert!(matches!(
            write_audio(
                Path::new("missing.json"),
                Path::new("missing.aaf"),
                &AtomicBool::new(true)
            ),
            Err(AafError::Cancelled)
        ));
    }
}
