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

/// The document inside an FCPXML bundle.
const BUNDLE_DOCUMENT: &str = "Info.fcpxml";

/// Final Cut Pro exports FCPXML as a bundle: a `.fcpxmld` directory whose
/// `Info.fcpxml` is the document.
pub fn is_bundle(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("fcpxmld"))
}

/// The XML document a timeline path names.
fn document(path: &Path) -> PathBuf {
    if is_bundle(path) {
        path.join(BUNDLE_DOCUMENT)
    } else {
        path.to_path_buf()
    }
}

/// Errors and warnings name the bundle the user added, not its document.
fn named(path: &Path, error: align_core::xml::ImportError) -> align_core::xml::ImportError {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) if is_bundle(path) => error.naming(name),
        _ => error,
    }
}

/// AAF, FCPXML (file or bundle), and XML whose root is an FCP 7 or FCPXML
/// document. Camera metadata XML beside clips is not a timeline and is
/// skipped like any other file that is not media.
pub fn is_supported(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    if ["fcpxml", "fcpxmld", "aaf"]
        .iter()
        .any(|timeline| extension.eq_ignore_ascii_case(timeline))
    {
        return true;
    }
    extension.eq_ignore_ascii_case("xml") && align_core::xml::is_timeline_document(path)
}

fn is_aaf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("aaf"))
}

/// A path as the filesystem resolves it; a path that does not exist yet
/// resolves through its parent.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    match (parent.canonicalize(), path.file_name()) {
        (Ok(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

/// Save a repaired project as a new file while leaving the imported source
/// untouched. XML is rewritten in-process; AAF delegates to the bundled
/// sidecar that owns its object graph. A `.fcpxmld` destination is written
/// as a bundle: a copy of the source bundle with its document repaired.
pub fn write_relinked_copy(
    source: &Path,
    destination: &Path,
    replacements: &[(PathBuf, PathBuf)],
    cancel: &AtomicBool,
) -> Result<(), TimelineError> {
    // Neither may contain the other: writing into a bundle would change the
    // imported project, and replacing a folder around it would delete it.
    let (from, to) = (resolved(source), resolved(destination));
    if from.starts_with(&to) || to.starts_with(&from) {
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
    let document_path = document(source);
    let bytes =
        std::fs::read(&document_path).map_err(|error| TimelineError::Write(error.to_string()))?;
    let source_folder = document_path.parent().unwrap_or_else(|| Path::new(""));
    let destination_folder = if is_bundle(destination) {
        destination
    } else {
        destination.parent().unwrap_or_else(|| Path::new(""))
    };
    let same_folder = resolved(source_folder) == resolved(destination_folder);
    let relocation = align_core::Relocation {
        from: source_folder,
        to: if same_folder {
            source_folder
        } else {
            destination_folder
        },
        copied: (is_bundle(source) && is_bundle(destination)).then_some(source),
    };
    let fixed = align_core::rewrite_media_paths(&bytes, replacements, &relocation)?;
    if is_bundle(destination) {
        return write_bundle(source, destination, &fixed, cancel);
    }
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

/// Builds the bundle beside `destination`, then swaps it in. An existing
/// destination must itself be a bundle; it is moved aside first and moved
/// back if the new bundle cannot take its place, so a failure never loses
/// either. Directory renames onto an existing path are not portable, which
/// the move aside also avoids.
fn write_bundle(
    source: &Path,
    destination: &Path,
    document: &[u8],
    cancel: &AtomicBool,
) -> Result<(), TimelineError> {
    let write_error = |error: std::io::Error| TimelineError::Write(error.to_string());
    let existing = std::fs::symlink_metadata(destination).ok();
    if existing
        .as_ref()
        .is_some_and(|meta| !meta.is_dir() || !destination.join(BUNDLE_DOCUMENT).is_file())
    {
        return Err(TimelineError::Write(format!(
            "{} exists and is not an FCPXML bundle",
            destination.display()
        )));
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(write_error)?;
    // Removed on every early return; renamed away on success.
    let staging = tempfile::Builder::new()
        .prefix(".align-bundle")
        .tempdir_in(parent)
        .map_err(write_error)?;
    if is_bundle(source) {
        copy_tree(source, staging.path(), Some(BUNDLE_DOCUMENT), cancel)?;
    }
    let mut file =
        std::fs::File::create(staging.path().join(BUNDLE_DOCUMENT)).map_err(write_error)?;
    file.write_all(document)
        .and_then(|()| file.sync_all())
        .map_err(write_error)?;
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(TimelineError::Cancelled);
    }
    let Some(previous) = existing.map(|_| aside(destination)) else {
        return std::fs::rename(staging.path(), destination).map_err(write_error);
    };
    std::fs::rename(destination, &previous).map_err(write_error)?;
    if let Err(error) = std::fs::rename(staging.path(), destination) {
        let _ = std::fs::rename(&previous, destination);
        return Err(write_error(error));
    }
    let _ = std::fs::remove_dir_all(previous);
    Ok(())
}

/// An unused name beside `path` for moving it aside.
fn aside(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    path.with_file_name(format!(".{name}.{}-{stamp}.previous", std::process::id()))
}

/// Copies a directory tree, leaving out one top-level entry. Symbolic links
/// stay links on Unix, and folders keep their permissions.
fn copy_tree(
    from: &Path,
    to: &Path,
    skip: Option<&str>,
    cancel: &AtomicBool,
) -> Result<(), TimelineError> {
    let write_error = |error: std::io::Error| TimelineError::Write(error.to_string());
    for entry in std::fs::read_dir(from).map_err(write_error)? {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(TimelineError::Cancelled);
        }
        let entry = entry.map_err(write_error)?;
        if skip.is_some_and(|skip| entry.file_name() == skip) {
            continue;
        }
        let target = to.join(entry.file_name());
        let kind = entry.file_type().map_err(write_error)?;
        #[cfg(unix)]
        if kind.is_symlink() {
            let link = std::fs::read_link(entry.path()).map_err(write_error)?;
            std::os::unix::fs::symlink(link, &target).map_err(write_error)?;
            continue;
        }
        if entry.path().is_dir() {
            std::fs::create_dir(&target).map_err(write_error)?;
            copy_tree(&entry.path(), &target, None, cancel)?;
            let permissions = entry.metadata().map_err(write_error)?.permissions();
            std::fs::set_permissions(&target, permissions).map_err(write_error)?;
        } else {
            std::fs::copy(entry.path(), target).map_err(write_error)?;
        }
    }
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
        let document = document(path);
        let mut draft =
            align_core::xml::read_timeline_with_proxies(&document, sequence, prefer_proxies)
                .map_err(|error| named(path, error))?;
        draft.source_url = path.to_path_buf();
        for warning in &mut draft.warnings {
            if warning.url == document {
                warning.url = path.to_path_buf();
            }
        }
        Ok(draft)
    }
}

pub fn sequences(
    path: &Path,
    cancel: &AtomicBool,
) -> Result<Vec<TimelineSequenceSummary>, TimelineError> {
    if is_aaf(path) {
        Ok(crate::aaf::read_timeline(path, cancel)?.summaries())
    } else {
        Ok(align_core::timeline_sequence_summaries(&document(path))
            .map_err(|error| named(path, error))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FCPXML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<fcpxml version="1.10">
<resources>
<format id="r1" frameDuration="1/25s"/>
<asset id="a1" hasVideo="1" hasAudio="1"><media-rep src="file:///old/card/c.mov"/></asset>
</resources>
<library><event><project name="Proj">
<sequence format="r1" name="Cut">
<spine>
<asset-clip ref="a1" name="Clip C" offset="0s" duration="100/25s" start="0s"/>
</spine>
</sequence>
</project></event></library>
</fcpxml>"#;

    fn bundle(dir: &Path, name: &str, document: &str) -> PathBuf {
        let bundle = dir.join(name);
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join(BUNDLE_DOCUMENT), document).unwrap();
        bundle
    }

    /// Final Cut Pro exports `.fcpxmld` bundles; their `Info.fcpxml` is the
    /// timeline, and messages name the bundle the user added.
    #[test]
    fn fcpxml_bundles_are_read_through_their_document() {
        let dir = tempfile::tempdir().unwrap();
        let cut = bundle(dir.path(), "Cut.fcpxmld", FCPXML);
        assert!(is_supported(&cut));
        let cancel = AtomicBool::new(false);
        assert_eq!(sequences(&cut, &cancel).unwrap().len(), 1);
        let draft = read(&cut, None, &cancel).unwrap();
        assert_eq!(draft.source_url, cut);
        assert_eq!(draft.media_urls(), vec![PathBuf::from("/old/card/c.mov")]);

        let empty = bundle(dir.path(), "Empty.fcpxmld", "<fcpxml version=\"1.10\"/>");
        let error = read(&empty, None, &cancel).unwrap_err().to_string();
        assert!(error.contains("Empty.fcpxmld"), "{error}");
        assert!(!error.contains(BUNDLE_DOCUMENT), "{error}");
    }

    fn relative_bundle(dir: &Path, name: &str) -> PathBuf {
        let bundle = bundle(
            dir,
            name,
            &FCPXML.replace("file:///old/card/c.mov", "./Media/my%20clip.wav"),
        );
        std::fs::create_dir(bundle.join("Media")).unwrap();
        std::fs::write(bundle.join("Media").join("my clip.wav"), b"").unwrap();
        bundle
    }

    /// FCPXML may locate media relative to its document; inside a bundle that
    /// is the bundle root, not the working directory.
    #[test]
    fn bundle_media_resolve_against_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let cut = relative_bundle(dir.path(), "Cut.fcpxmld");
        let draft = read(&cut, None, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            draft.media_urls(),
            vec![cut.join("Media").join("my clip.wav")]
        );
    }

    /// Relative media must still resolve after the repaired document moves:
    /// a bundle copy carries its own media along; a document written elsewhere
    /// points back at the originals.
    #[test]
    fn repaired_copies_keep_relative_media_resolving() {
        let dir = tempfile::tempdir().unwrap();
        let source = relative_bundle(dir.path(), "Cut.fcpxmld");
        let cancel = AtomicBool::new(false);
        let elsewhere = dir.path().join("exports");
        std::fs::create_dir(&elsewhere).unwrap();

        let bundle_copy = elsewhere.join("Cut-fixed.fcpxmld");
        write_relinked_copy(&source, &bundle_copy, &[], &cancel).unwrap();
        let copied = std::fs::read_to_string(bundle_copy.join(BUNDLE_DOCUMENT)).unwrap();
        assert!(copied.contains("src=\"./Media/my%20clip.wav\""), "{copied}");
        assert!(bundle_copy.join("Media").join("my clip.wav").is_file());

        let single = elsewhere.join("Cut-fixed.fcpxml");
        write_relinked_copy(&source, &single, &[], &cancel).unwrap();
        let draft = read(&single, None, &cancel).unwrap();
        assert_eq!(
            draft.media_urls(),
            vec![source.join("Media").join("my clip.wav")]
        );
    }

    /// The destination may not lie inside the source, nor contain it, nor be
    /// a folder that is not a bundle; each would destroy data.
    #[test]
    fn repaired_bundles_never_overwrite_the_source_or_other_folders() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let cut = bundle(dir.path(), "Cut.fcpxmld", FCPXML);
        for inside in [cut.join(BUNDLE_DOCUMENT), cut.join("Nested.fcpxmld")] {
            assert!(write_relinked_copy(&cut, &inside, &[], &cancel).is_err());
        }
        assert_eq!(
            std::fs::read_to_string(cut.join(BUNDLE_DOCUMENT)).unwrap(),
            FCPXML
        );
        assert!(!cut.join("Nested.fcpxmld").exists());

        let outer = bundle(dir.path(), "Outer.fcpxmld", FCPXML);
        let inner = bundle(&outer, "Inner.fcpxmld", FCPXML);
        assert!(write_relinked_copy(&inner, &outer, &[], &cancel).is_err());
        assert!(inner.join(BUNDLE_DOCUMENT).is_file());

        let folder = dir.path().join("Folder.fcpxmld");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("unique.txt"), "kept").unwrap();
        assert!(write_relinked_copy(&cut, &folder, &[], &cancel).is_err());
        assert!(folder.join("unique.txt").is_file());
    }

    /// Cancelling while a bundle is copied stops the copy and keeps the
    /// existing destination, without leaving the staging folder behind.
    #[test]
    fn a_cancelled_bundle_copy_leaves_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = bundle(dir.path(), "Cut.fcpxmld", FCPXML);
        std::fs::write(source.join("note.txt"), "kept").unwrap();
        let existing = bundle(
            dir.path(),
            "Cut-fixed.fcpxmld",
            "<fcpxml version=\"1.10\"/>",
        );
        let error = write_bundle(
            &source,
            &existing,
            FCPXML.as_bytes(),
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert!(matches!(error, TimelineError::Cancelled));
        assert_eq!(
            std::fs::read_to_string(existing.join(BUNDLE_DOCUMENT)).unwrap(),
            "<fcpxml version=\"1.10\"/>"
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn bundle_copies_keep_symbolic_links() {
        let dir = tempfile::tempdir().unwrap();
        let source = bundle(dir.path(), "Cut.fcpxmld", FCPXML);
        std::os::unix::fs::symlink("../shared.mov", source.join("link.mov")).unwrap();
        let copy = dir.path().join("Cut-fixed.fcpxmld");
        write_relinked_copy(&source, &copy, &[], &AtomicBool::new(false)).unwrap();
        assert_eq!(
            std::fs::read_link(copy.join("link.mov")).unwrap(),
            PathBuf::from("../shared.mov")
        );
    }

    /// A repaired copy of a bundle is a bundle: every other file is copied
    /// and the document is rewritten; the source stays as it was.
    #[test]
    fn repaired_bundle_copies_keep_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let source = bundle(dir.path(), "Cut.fcpxmld", FCPXML);
        std::fs::create_dir(source.join("Resources")).unwrap();
        std::fs::write(source.join("Resources").join("note.txt"), "kept").unwrap();
        let replacements = [(
            PathBuf::from("/old/card/c.mov"),
            PathBuf::from("/new/card/c.mov"),
        )];
        let cancel = AtomicBool::new(false);

        let copy = bundle(
            dir.path(),
            "Cut-fixed.fcpxmld",
            "<fcpxml version=\"1.10\"/>",
        );
        std::fs::write(copy.join("stale.txt"), "replaced").unwrap();
        write_relinked_copy(&source, &copy, &replacements, &cancel).unwrap();
        let fixed = std::fs::read_to_string(copy.join(BUNDLE_DOCUMENT)).unwrap();
        assert!(fixed.contains("file:///new/card/c.mov"), "{fixed}");
        assert_eq!(
            std::fs::read_to_string(copy.join("Resources").join("note.txt")).unwrap(),
            "kept"
        );
        assert!(
            !copy.join("stale.txt").exists(),
            "the old bundle is replaced"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert_eq!(
            std::fs::read_to_string(source.join(BUNDLE_DOCUMENT)).unwrap(),
            FCPXML
        );

        let single = dir.path().join("Cut-fixed.fcpxml");
        write_relinked_copy(&source, &single, &replacements, &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(&single).unwrap(), fixed);
    }

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
