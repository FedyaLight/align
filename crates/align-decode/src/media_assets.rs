//! Ownership of audio extracted from AAF: temporary while in this process,
//! durable before a project or exported timeline crosses the process boundary.
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use align_core::{SyncProject, export_model::ExportTimeline};

static SESSION: Mutex<Option<tempfile::TempDir>> = Mutex::new(None);

/// A random, private directory prevents stale PID directories from becoming
/// another run's media. Merely inspecting external media does not create it.
pub fn extraction_directory() -> io::Result<PathBuf> {
    let mut session = SESSION.lock().unwrap_or_else(|error| error.into_inner());
    if session.is_none() {
        *session = Some(
            tempfile::Builder::new()
                .prefix("align-aaf-media-")
                .tempdir()?,
        );
    }
    session.as_ref().unwrap().path().canonicalize()
}

/// Called after workers stop or immediately before the application exits.
/// Only this process's owned directory is removed; saved results are untouched.
pub fn cleanup() {
    SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
}

pub fn preserve_project(project: &mut SyncProject, cancel: &AtomicBool) -> io::Result<()> {
    let Some(session) = active_directory()? else {
        return Ok(());
    };
    let destination = dirs::data_local_dir()
        .ok_or_else(|| io::Error::other("Cannot locate storage for saved AAF media"))?
        .join("Align")
        .join("Saved AAF Media");
    retain_from(
        &session,
        project.clips.iter_mut().map(|clip| &mut clip.url),
        &destination,
        cancel,
    )
}

/// Every export format gets durable original media, including mono/full-span
/// clips for which no correction, channel extraction or placement pad is needed.
pub(crate) fn preserve_export(
    timeline: &mut ExportTimeline,
    directory: &Path,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let Some(session) = active_directory()? else {
        return Ok(());
    };
    retain_from(
        &session,
        timeline
            .islands
            .iter_mut()
            .flat_map(|island| island.clips.iter_mut().map(|item| &mut item.clip.url)),
        &directory.join("Source Media"),
        cancel,
    )
}

fn active_directory() -> io::Result<Option<PathBuf>> {
    SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|directory| directory.path().canonicalize())
        .transpose()
}

fn retain_from<'a>(
    session: &Path,
    paths: impl Iterator<Item = &'a mut PathBuf>,
    destination: &Path,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let mut retained: HashMap<PathBuf, PathBuf> = HashMap::new();
    for path in paths {
        if path
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .as_deref()
            != Some(session)
        {
            continue;
        }
        if let Some(saved) = retained.get(path) {
            *path = saved.clone();
            continue;
        }
        check_cancel(cancel)?;
        std::fs::create_dir_all(destination)?;
        // Use absolute paths even when the export directory was relative.
        let saved = destination.canonicalize()?.join(
            path.file_name()
                .ok_or_else(|| io::Error::other("Embedded media has no filename"))?,
        );
        copy_atomic(path, &saved, cancel)?;
        retained.insert(path.clone(), saved.clone());
        *path = saved;
    }
    Ok(())
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled"))
    } else {
        Ok(())
    }
}

fn copy_atomic(source: &Path, destination: &Path, cancel: &AtomicBool) -> io::Result<()> {
    let mut source = std::fs::File::open(source)?;
    let mut copy = tempfile::NamedTempFile::new_in(destination.parent().unwrap())?;
    let mut buffer = vec![0; 1_048_576];
    loop {
        check_cancel(cancel)?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copy.write_all(&buffer[..count])?;
    }
    copy.as_file().sync_all()?;
    check_cancel(cancel)?;
    copy.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_media_outlives_session_and_external_sources_are_untouched() {
        let temporary = tempfile::tempdir().unwrap();
        let session = temporary.path().canonicalize().unwrap();
        let output = tempfile::tempdir().unwrap();
        let source = session.join("content.wav");
        std::fs::write(&source, b"embedded PCM").unwrap();
        let external = output.path().join("external.wav");
        std::fs::write(&external, b"external PCM").unwrap();
        let mut paths = [source.clone(), source, external.clone()];
        retain_from(
            &session,
            paths.iter_mut(),
            &output.path().join("saved"),
            &AtomicBool::new(false),
        )
        .unwrap();
        drop(temporary);
        assert_eq!(paths[0], paths[1]);
        assert_eq!(paths[2], external);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"embedded PCM");
        assert_eq!(std::fs::read(external).unwrap(), b"external PCM");
    }

    #[test]
    fn cancelled_copy_preserves_existing_destination_and_original_path() {
        let temporary = tempfile::tempdir().unwrap();
        let session = temporary.path().canonicalize().unwrap();
        let source = session.join("content.wav");
        std::fs::write(&source, b"new PCM").unwrap();
        let output = tempfile::tempdir().unwrap();
        let saved = output.path().join("content.wav");
        std::fs::write(&saved, b"previous PCM").unwrap();
        let mut path = source.clone();
        let error = retain_from(
            &session,
            std::iter::once(&mut path),
            output.path(),
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(path, source);
        assert_eq!(std::fs::read(saved).unwrap(), b"previous PCM");
        assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 1);
    }
}
