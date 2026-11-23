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
    align_core::model::canonical_path(session.as_ref().unwrap().path())
}

/// Called after workers stop or immediately before the application exits.
/// Only this process's owned directory is removed; saved results are untouched.
pub fn cleanup() {
    SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
}

/// Identifies a content-addressed WAV owned by this process's AAF extraction.
pub fn embedded_aaf_name(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let digest = name.strip_suffix(".wav")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let directory = active_directory().ok().flatten()?;
    (path.parent()? == directory).then_some(name)
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
        false,
    )
}

/// Every export format gets durable original media, including mono/full-span
/// clips for which no correction, channel extraction or placement pad is needed.
pub(crate) fn preserve_export(
    timeline: &mut ExportTimeline,
    directory: &Path,
    cancel: &AtomicBool,
    protect_existing: bool,
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
        protect_existing,
    )
}

fn active_directory() -> io::Result<Option<PathBuf>> {
    SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|directory| align_core::model::canonical_path(directory.path()))
        .transpose()
}

fn retain_from<'a>(
    session: &Path,
    paths: impl Iterator<Item = &'a mut PathBuf>,
    destination: &Path,
    cancel: &AtomicBool,
    protect_existing: bool,
) -> io::Result<()> {
    let mut retained: HashMap<PathBuf, PathBuf> = HashMap::new();
    for path in paths {
        if path
            .parent()
            .and_then(|parent| align_core::model::canonical_path(parent).ok())
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
        let saved = align_core::model::canonical_path(destination)?.join(
            path.file_name()
                .ok_or_else(|| io::Error::other("Embedded media has no filename"))?,
        );
        copy_atomic(path, &saved, cancel, protect_existing)?;
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

fn copy_atomic(
    source: &Path,
    destination: &Path,
    cancel: &AtomicBool,
    protect_existing: bool,
) -> io::Result<()> {
    if protect_existing {
        match std::fs::symlink_metadata(destination) {
            Ok(_) => return reuse_retained_media(source, destination, cancel),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let source_path = source;
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
    if !protect_existing {
        return copy
            .persist(destination)
            .map(drop)
            .map_err(|error| error.error);
    }
    match copy.persist_noclobber(destination) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            reuse_retained_media(source_path, destination, cancel)
        }
        Err(error) => Err(error.error),
    }
}

/// An extracted WAV is named by its contents. Check that a retained copy
/// still agrees before linking a new timeline to it.
fn reuse_retained_media(source: &Path, destination: &Path, cancel: &AtomicBool) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(destination)?;
    let mut source = std::fs::File::open(source)?;
    if metadata.is_file() && metadata.len() == source.metadata()?.len() {
        let mut saved = std::fs::File::open(destination)?;
        let mut original = vec![0; 1_048_576];
        let mut retained = vec![0; original.len()];
        loop {
            check_cancel(cancel)?;
            let count = source.read(&mut original)?;
            if count == 0 {
                return Ok(());
            }
            saved.read_exact(&mut retained[..count])?;
            if original[..count] != retained[..count] {
                break;
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "“{}” differs from the source audio. Choose another destination or move the existing file before trying again.",
            destination.display()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_media_outlives_session_and_external_sources_are_untouched() {
        let temporary = tempfile::tempdir().unwrap();
        let session = align_core::model::canonical_path(temporary.path()).unwrap();
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
            false,
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
        let session = align_core::model::canonical_path(temporary.path()).unwrap();
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
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(path, source);
        assert_eq!(std::fs::read(saved).unwrap(), b"previous PCM");
        assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 1);
    }

    #[test]
    fn protected_export_reuses_matching_retained_audio_and_preserves_a_changed_copy() {
        let session = tempfile::tempdir().unwrap();
        let session_path = align_core::model::canonical_path(session.path()).unwrap();
        let output = tempfile::tempdir().unwrap();
        let source = session_path.join("content.wav");
        let saved = output.path().join("content.wav");
        std::fs::write(&source, b"original").unwrap();
        let retain = |path: &mut PathBuf| {
            retain_from(
                &session_path,
                std::iter::once(path),
                output.path(),
                &AtomicBool::new(false),
                true,
            )
        };
        let mut path = source.clone();
        retain(&mut path).unwrap();
        let saved_revision = align_core::cache::media_revision(&saved).unwrap();
        path = source.clone();
        retain(&mut path).unwrap();
        assert_eq!(path, align_core::model::canonical_path(&saved).unwrap());
        assert_eq!(
            align_core::cache::media_revision(&saved).unwrap(),
            saved_revision
        );

        std::fs::write(&saved, b"modified").unwrap();
        path = source.clone();
        assert_eq!(
            retain(&mut path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(path, source);
        assert_eq!(std::fs::read(saved).unwrap(), b"modified");
    }
}
