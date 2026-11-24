//! Local desktop projects. Placements carry the source revisions from the
//! synchronization that produced them and never depend on the analysis cache.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Condvar, Mutex, atomic::AtomicBool},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use align_core::{SyncResult, cache::media_revision};
use align_decode::media_assets::embedded_aaf_name;
use futures::channel::oneshot;
use serde::{Deserialize, Serialize};

use crate::state::ProjectSession;

const FORMAT: &str = "align-project";
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 128 * 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const RECENT_LIMIT: usize = 12;
const MANAGED_PREFIX: &str = "autosave-";
const EMBEDDED: &str = "embedded-aaf";
const SETTINGS_KEY: &str = "projects";

pub fn is_project(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("align"))
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preferences {
    pub disabled: bool,
    pub folder: Option<PathBuf>,
    previous_folders: Vec<PathBuf>,
    pub recent: Vec<Recent>,
}

impl Preferences {
    pub fn load(path: &Path) -> Self {
        let mut settings: Self = crate::settings::read(path, SETTINGS_KEY).unwrap_or_default();
        settings.folder = settings.autosave_folder();
        let mut roots: HashSet<_> = settings.previous_folders.iter().cloned().collect();
        roots.extend(settings.autosave_folder());
        let mut managed = Vec::new();
        for root in roots {
            managed.extend(managed_projects(&root, SystemTime::now()));
        }
        settings.recent.retain(|recent| recent.path.is_file());
        let mut seen: HashSet<_> = settings
            .recent
            .iter()
            .map(|recent| recent.path.clone())
            .collect();
        managed.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        settings.recent.extend(
            managed
                .into_iter()
                .map(|(_, recent)| recent)
                .filter(|recent| seen.insert(recent.path.clone())),
        );
        settings.recent.truncate(RECENT_LIMIT);
        settings
    }

    pub fn autosave_folder(&self) -> Option<PathBuf> {
        self.folder.clone().or_else(|| {
            dirs::document_dir()
                .or_else(|| dirs::home_dir().map(|home| home.join("Documents")))
                .map(|dir| {
                    align_core::model::canonical_path(&dir)
                        .unwrap_or(dir)
                        .join("Align")
                })
        })
    }

    pub fn set_folder(&mut self, folder: PathBuf) {
        if let Some(previous) = self.autosave_folder()
            && previous != folder
            && !self.previous_folders.contains(&previous)
        {
            self.previous_folders.push(previous);
        }
        self.folder = Some(folder);
    }

    pub fn remember(&mut self, recent: Recent) {
        self.recent.retain(|old| old.path != recent.path);
        self.recent.insert(0, recent);
        self.recent.truncate(RECENT_LIMIT);
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        crate::settings::write(path, SETTINGS_KEY, Some(self))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    pub path: PathBuf,
    pub label: String,
}

impl Recent {
    pub fn new(destination: Destination, title: String) -> Self {
        let label = if destination.managed {
            format!("{title} (Autosave)")
        } else {
            align_core::model::file_name(&destination.path)
        };
        Self {
            path: destination.path,
            label,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    pub path: PathBuf,
    pub managed: bool,
}

impl Destination {
    pub fn autosave(folder: &Path) -> Self {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = NEXT_SAVE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            path: folder.join(format!(
                "{MANAGED_PREFIX}{time:x}-{:x}-{counter:x}.align",
                std::process::id()
            )),
            managed: true,
        }
    }
}

static NEXT_SAVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    format: String,
    version: u32,
    title: String,
    /// The original path is part of ownership. A copy of an autosave is a
    /// regular document, even if its filename is preserved.
    managed_path: Option<PathBuf>,
}

impl Header {
    fn supported(&self) -> bool {
        self.format == FORMAT && self.version == VERSION
    }

    fn owns(&self, path: &Path) -> bool {
        self.supported() && managed_name(path) && self.managed_path.as_deref() == Some(path)
    }
}

#[derive(Deserialize)]
struct Document {
    header: Header,
    session: ProjectSession,
    placements: Option<Arc<Placements>>,
}

pub struct Opened {
    pub destination: Destination,
    pub title: String,
    pub session: ProjectSession,
    pub placements: Option<Arc<Placements>>,
    pub results: Vec<SyncResult>,
    pub changed: Vec<PathBuf>,
    pub embedded_unavailable: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Placements {
    results: Vec<SyncResult>,
    revisions: Revisions,
}

pub type Revisions = HashMap<PathBuf, Option<String>>;

pub fn record_revision(revisions: &mut Revisions, path: &Path) {
    revisions
        .entry(path.to_path_buf())
        .or_insert_with(|| revision(path));
}

fn revision(path: &Path) -> Option<String> {
    let document = align_decode::timeline::is_bundle(path).then(|| path.join("Info.fcpxml"));
    let path = document.as_deref().unwrap_or(path);
    fs::metadata(path)
        .ok()?
        .is_file()
        .then(|| media_revision(path).ok())
        .flatten()
}

impl Placements {
    pub fn from_run(results: &[SyncResult], mut revisions: Revisions) -> Self {
        let mut results = results.to_vec();
        for result in &mut results {
            for clip in &mut result.project.clips {
                if let Some(name) = embedded_aaf_name(&clip.url) {
                    revisions.remove(&clip.url);
                    clip.url = Path::new(EMBEDDED).join(name);
                }
            }
            for warning in &mut result.project.warnings {
                if let Some(name) = embedded_aaf_name(&warning.url) {
                    let relative = Path::new(EMBEDDED).join(name);
                    warning.message = warning.message.replace(
                        warning.url.to_string_lossy().as_ref(),
                        relative.to_string_lossy().as_ref(),
                    );
                    warning.url = relative;
                }
            }
        }
        revisions.retain(|path, _| embedded_aaf_name(path).is_none());
        Self { results, revisions }
    }

    fn restore(
        &self,
        session: &ProjectSession,
        changed: &mut Vec<PathBuf>,
    ) -> (Vec<SyncResult>, bool) {
        let mut results = self.results.clone();
        for path in session
            .timeline_inputs()
            .chain(self.results.iter().flat_map(|r| {
                r.project
                    .clips
                    .iter()
                    .map(|clip| clip.url.as_path())
                    .filter(|path| path.is_absolute())
            }))
        {
            let current = revision(path);
            if current.is_none() || self.revisions.get(path) != Some(&current) {
                changed.push(path.to_path_buf());
            }
        }
        let has_embedded = results
            .iter()
            .any(|r| r.project.clips.iter().any(|clip| embedded_path(&clip.url)));
        if !has_embedded {
            return (results, false);
        }
        let mut extracted = HashMap::new();
        for aaf in session.timeline_inputs().filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("aaf"))
                && !changed.iter().any(|old| old == path)
        }) {
            if let Ok(imported) = align_decode::aaf::read_timeline(aaf, &AtomicBool::new(false))
                && self.revisions.get(aaf) == Some(&revision(aaf))
            {
                for clip in imported
                    .sequences
                    .into_iter()
                    .flat_map(|s| s.tracks)
                    .flat_map(|t| t.clips)
                {
                    if let Some(name) = embedded_aaf_name(&clip.path) {
                        extracted.insert(Path::new(EMBEDDED).join(name), clip.path);
                    }
                }
            }
        }
        let mut unavailable = false;
        for result in &mut results {
            for clip in &mut result.project.clips {
                if embedded_path(&clip.url) {
                    if let Some(path) = extracted.get(&clip.url) {
                        clip.url.clone_from(path);
                    } else {
                        unavailable = true;
                    }
                }
            }
        }
        (results, unavailable)
    }
}

pub fn open(path: &Path) -> io::Result<Opened> {
    let path = align_core::model::canonical_path(path)?;
    let metadata = fs::metadata(&path)?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(io::Error::other(
            "The project is not a supported file size.",
        ));
    }
    let mut file = File::open(&path)?;
    let mut document = read_document(&mut file)?;
    if !document.header.supported() {
        return Err(io::Error::other(
            "This Align project format is unsupported.",
        ));
    }
    let valid_source = |path: &Path| path.is_absolute() && !is_project(path);
    if !document.session.valid_paths(valid_source) {
        return Err(io::Error::other(
            "The project contains an invalid source path.",
        ));
    }
    if document.placements.as_ref().is_some_and(|placements| {
        placements
            .results
            .iter()
            .flat_map(|r| &r.project.clips)
            .any(|clip| {
                let path = &clip.url;
                !valid_source(path) && !embedded_path(path)
            })
    }) {
        return Err(io::Error::other(
            "The project contains an invalid media path.",
        ));
    }
    document.session.adopt_fcp7_document_order();
    let mut changed: Vec<_> = document
        .session
        .inputs
        .iter()
        .filter(|path| !path.exists())
        .cloned()
        .collect();
    let (results, embedded_unavailable) = document
        .placements
        .as_ref()
        .map(|placements| placements.restore(&document.session, &mut changed))
        .unwrap_or_default();
    changed.sort_unstable();
    changed.dedup();
    Ok(Opened {
        destination: Destination {
            managed: document.header.owns(&path),
            path,
        },
        title: document.header.title,
        session: document.session,
        placements: document.placements,
        results,
        changed,
        embedded_unavailable,
    })
}

pub fn manual_path(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Choose a project folder."))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("Choose a project filename."))?;
    let mut name = name.to_os_string();
    if !is_project(path) {
        name.push(".align");
    }
    Ok(align_core::model::canonical_path(parent)?.join(name))
}

struct Snapshot {
    session: ProjectSession,
    placements: Option<Arc<Placements>>,
}

fn embedded_path(path: &Path) -> bool {
    path.parent() == Some(Path::new(EMBEDDED))
        && path.extension() == Some(std::ffi::OsStr::new("wav"))
        && path.file_stem().is_some_and(|digest| {
            let digest = digest.as_encoded_bytes();
            digest.len() == 64 && digest.iter().all(u8::is_ascii_hexdigit)
        })
}

fn managed_name(path: &Path) -> bool {
    is_project(path)
        && path
            .file_stem()
            .and_then(|part| part.to_str())
            .is_some_and(|name| {
                name.strip_prefix(MANAGED_PREFIX).is_some_and(|id| {
                    id.split('-').count() == 3
                        && id.split('-').all(|part| {
                            !part.is_empty() && part.bytes().all(|b| b.is_ascii_hexdigit())
                        })
                })
            })
}

// The first line is always the small ownership header. Startup does not
// deserialize entire timelines to decide whether an autosave is ours.
fn read_header(file: &File) -> Option<Header> {
    let mut first = Vec::new();
    BufReader::new(file.take(128 * 1024))
        .read_until(b'\n', &mut first)
        .ok()?;
    let json = first.strip_prefix(b"{\"header\":")?.strip_suffix(b",\n")?;
    serde_json::from_slice(json).ok()
}

fn read_document(file: &mut File) -> io::Result<Document> {
    if file.metadata()?.len() > MAX_BYTES {
        return Err(io::Error::other(
            "The project exceeds the supported file size.",
        ));
    }
    file.rewind()?;
    Ok(serde_json::from_reader(BufReader::new(
        file.take(MAX_BYTES),
    ))?)
}

fn write(destination: &Destination, snapshot: &Snapshot, mode: SaveMode) -> io::Result<()> {
    let path = &destination.path;
    let parent = path
        .parent()
        .filter(|dir| dir.is_absolute())
        .ok_or_else(|| io::Error::other("Choose an absolute project location."))?;
    if !is_project(path) {
        return Err(io::Error::other("Project files must end in .align."));
    }
    if destination.managed {
        fs::create_dir_all(parent)?;
    }

    // Serialize under the same directory lock as startup expiry, so another
    // instance cannot expire an autosave while we are updating it.
    let managed_dir = managed_name(path)
        .then(|| ManagedDirectory::open(parent))
        .transpose()?;
    let _lock = managed_dir.as_ref().map(|dir| dir.lock(true)).transpose()?;
    let replacing = if mode == SaveMode::Create {
        false
    } else {
        let existing = if let Some(dir) = &managed_dir {
            dir.open_file(path.file_name().expect("project filename"))
        } else {
            fs::symlink_metadata(path).and_then(|metadata| {
                if metadata.is_file() {
                    File::open(path)
                } else {
                    Err(io::Error::other(
                        "The project destination is not a regular file.",
                    ))
                }
            })
        };
        match existing {
            Ok(mut file) => {
                let header = read_header(&file).or_else(|| {
                    read_document(&mut file)
                        .ok()
                        .map(|document| document.header)
                });
                if destination.managed {
                    if !header.is_some_and(|header| header.owns(path)) {
                        return Err(io::Error::other("The autosave location is already in use."));
                    }
                } else if !header.is_some_and(|header| header.supported()) {
                    return Err(io::Error::other("The destination is not an Align project."));
                }
                true
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        }
    };
    let header = Header {
        format: FORMAT.into(),
        version: VERSION,
        title: snapshot.session.title(),
        managed_path: destination.managed.then(|| path.clone()),
    };
    let temp = tempfile::Builder::new()
        .prefix(".align-project-")
        .tempfile_in(parent)?;
    {
        let mut output = BufWriter::new(temp.as_file());
        output.write_all(b"{\"header\":")?;
        serde_json::to_writer(&mut output, &header)?;
        output.write_all(b",\n\"session\":")?;
        serde_json::to_writer(&mut output, &snapshot.session)?;
        output.write_all(b",\n\"placements\":")?;
        serde_json::to_writer(&mut output, &snapshot.placements)?;
        output.write_all(b"}\n")?;
        output.flush()?;
    }
    if temp.as_file().metadata()?.len() > MAX_BYTES {
        return Err(io::Error::other(
            "The project exceeds the supported file size.",
        ));
    }
    temp.as_file().sync_all()?;
    if replacing {
        temp.persist(path).map_err(|error| error.error)?;
    } else {
        temp.persist_noclobber(path).map_err(|error| error.error)?;
    }
    Ok(())
}

fn managed_projects(root: &Path, now: SystemTime) -> Vec<(SystemTime, Recent)> {
    let Ok(dir) = ManagedDirectory::open(root) else {
        return Vec::new();
    };
    let Ok(_lock) = dir.lock(false) else {
        return Vec::new();
    };
    dir.names()
        .into_iter()
        .filter_map(|name| {
            let path = root.join(&name);
            if !managed_name(&path) {
                return None;
            }
            let file = dir.open_file(&name).ok()?;
            let meta = file.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let header = read_header(&file)?;
            if !header.owns(&path) {
                return None;
            }
            let updated = meta.modified().ok()?;
            if now
                .duration_since(updated)
                .is_ok_and(|age| age >= RETENTION)
                && dir.remove(&name).is_ok()
            {
                return None;
            }
            Some((
                updated,
                Recent::new(
                    Destination {
                        path,
                        managed: true,
                    },
                    header.title,
                ),
            ))
        })
        .collect()
}

struct ManagedDirectory {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    #[cfg(windows)]
    path: PathBuf,
    #[cfg(windows)]
    _ancestors: Vec<File>,
}

#[cfg(unix)]
impl ManagedDirectory {
    fn open(path: &Path) -> io::Result<Self> {
        use rustix::fs::{Mode, OFlags, open, openat};
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut components = path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(io::Error::other("Invalid autosave folder."));
        }
        let mut fd = open("/", flags, Mode::empty())?;
        for part in components {
            let Component::Normal(name) = part else {
                return Err(io::Error::other("Invalid autosave folder."));
            };
            fd = openat(&fd, name, flags, Mode::empty())?;
        }
        Ok(Self { fd })
    }

    fn open_lock(&self) -> io::Result<File> {
        use rustix::fs::{Mode, OFlags, openat};
        Ok(File::from(openat(
            &self.fd,
            ".align-autosaves.lock",
            OFlags::CREATE | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?))
    }

    fn names(&self) -> Vec<std::ffi::OsString> {
        use std::os::unix::ffi::OsStrExt;
        rustix::fs::Dir::read_from(&self.fd)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| std::ffi::OsStr::from_bytes(entry.file_name().to_bytes()).to_owned())
            .collect()
    }

    fn open_file(&self, name: &std::ffi::OsStr) -> io::Result<File> {
        use rustix::fs::{Mode, OFlags, openat};
        Ok(File::from(openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?))
    }

    fn remove(&self, name: &std::ffi::OsStr) -> io::Result<()> {
        Ok(rustix::fs::unlinkat(
            &self.fd,
            name,
            rustix::fs::AtFlags::empty(),
        )?)
    }
}

#[cfg(windows)]
impl ManagedDirectory {
    fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            return Err(io::Error::other("Invalid autosave folder."));
        }
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };
        let mut parents: Vec<_> = path.ancestors().collect();
        parents.reverse();
        let mut handles = Vec::with_capacity(parents.len());
        for ancestor in parents {
            let directory = fs::OpenOptions::new()
                .read(true)
                // Pin the ancestors while expiry and atomic replacement still
                // use their paths. Allowing delete sharing also allows renames.
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(ancestor)?;
            if !directory.metadata()?.is_dir() || !is_local_or_cloud_entry(&directory)? {
                return Err(io::Error::other("Invalid autosave folder."));
            }
            handles.push(directory);
        }
        Ok(Self {
            path: path.to_path_buf(),
            _ancestors: handles,
        })
    }

    fn open_lock(&self) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.path.join(".align-autosaves.lock"))?;
        if !is_local_or_cloud_entry(&file)? {
            return Err(io::Error::other("Invalid autosave lock."));
        }
        Ok(file)
    }

    fn names(&self) -> Vec<std::ffi::OsString> {
        fs::read_dir(&self.path)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect()
    }

    fn open_file(&self, name: &std::ffi::OsStr) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.path.join(name))?;
        if !is_local_or_cloud_entry(&file)? {
            return Err(io::Error::other("Invalid autosave file."));
        }
        Ok(file)
    }

    fn remove(&self, name: &std::ffi::OsStr) -> io::Result<()> {
        fs::remove_file(self.path.join(name))
    }
}

#[cfg(windows)]
fn is_local_or_cloud_entry(file: &File) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FileAttributeTagInfo,
            GetFileInformationByHandleEx,
        },
        System::SystemServices::{
            IO_REPARSE_TAG_CLOUD, IO_REPARSE_TAG_CLOUD_MASK, IO_REPARSE_TAG_ONEDRIVE,
        },
    };

    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // The file owns the handle for the duration of the call and the buffer
    // points to a properly sized FILE_ATTRIBUTE_TAG_INFO.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
        || info.ReparseTag & !IO_REPARSE_TAG_CLOUD_MASK == IO_REPARSE_TAG_CLOUD
        || info.ReparseTag == IO_REPARSE_TAG_ONEDRIVE)
}

impl ManagedDirectory {
    fn lock(&self, blocking: bool) -> io::Result<File> {
        let file = self.open_lock()?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("Invalid autosave lock."));
        }
        if blocking {
            file.lock()?;
        } else {
            file.try_lock()?;
        }
        Ok(file)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SaveMode {
    Coalesced,
    Replace,
    Create,
}

type SaveReceipt = oneshot::Receiver<Result<(), String>>;

struct SaveJob {
    destination: Destination,
    snapshot: Snapshot,
    mode: SaveMode,
    deadline: Instant,
    receipts: Vec<oneshot::Sender<Result<(), String>>>,
    autosave_receipt: Option<oneshot::Sender<Result<(), String>>>,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<SaveJob>,
    running: bool,
}

#[derive(Clone, Default)]
pub struct Writer(Arc<(Mutex<Queue>, Condvar)>);

impl Writer {
    pub fn submit(
        &self,
        destination: Destination,
        session: ProjectSession,
        placements: Option<Arc<Placements>>,
        mode: SaveMode,
    ) -> SaveReceipt {
        let (tx, rx) = oneshot::channel();
        let (queue, signal) = &*self.0;
        let mut queue = queue.lock().unwrap();
        let snapshot = Snapshot {
            session,
            placements,
        };
        if let Some(job) = queue
            .jobs
            .iter_mut()
            .find(|job| job.destination == destination)
        {
            job.snapshot = snapshot;
            if mode != SaveMode::Coalesced {
                job.mode = mode;
                job.deadline = Instant::now();
                job.receipts.push(tx);
            } else {
                job.autosave_receipt = Some(tx);
            }
        } else {
            // Only a new project can introduce a new destination. Keep a
            // hard bound even if the filesystem stops responding.
            if queue.jobs.len() == 8 {
                let _ = tx.send(Err("Project storage is busy. Try saving again.".into()));
                return rx;
            }
            let now = Instant::now();
            let mut job = SaveJob {
                destination,
                snapshot,
                mode,
                deadline: now,
                receipts: Vec::new(),
                autosave_receipt: None,
            };
            if mode != SaveMode::Coalesced {
                job.receipts.push(tx);
            } else {
                job.deadline += Duration::from_millis(250);
                job.autosave_receipt = Some(tx);
            }
            queue.jobs.push_back(job);
        }
        if !queue.running {
            queue.running = true;
            let writer = self.clone();
            std::thread::spawn(move || writer.work());
        }
        signal.notify_all();
        rx
    }

    fn work(&self) {
        let _worker = WriterGuard(self);
        let (queue, signal) = &*self.0;
        let mut state = queue.lock().unwrap();
        loop {
            let Some(job) = state.jobs.front() else {
                state.running = false;
                signal.notify_all();
                return;
            };
            let delay = job.deadline.saturating_duration_since(Instant::now());
            if !delay.is_zero() {
                state = signal.wait_timeout(state, delay).unwrap().0;
                continue;
            }
            let job = state.jobs.pop_front().unwrap();
            drop(state);
            let result =
                write(&job.destination, &job.snapshot, job.mode).map_err(|error| error.to_string());
            for receipt in job.receipts.into_iter().chain(job.autosave_receipt) {
                let _ = receipt.send(result.clone());
            }
            state = queue.lock().unwrap();
        }
    }

    pub fn flush(&self) {
        let (queue, signal) = &*self.0;
        let mut queue = queue.lock().unwrap();
        for job in &mut queue.jobs {
            job.deadline = Instant::now();
        }
        signal.notify_all();
        drop(
            signal
                .wait_while(queue, |queue| queue.running)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}

struct WriterGuard<'a>(&'a Writer);

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let Writer(inner) = self.0;
            let (queue, signal) = &**inner;
            let mut state = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.running = false;
            for job in state.jobs.drain(..) {
                for receipt in job.receipts.into_iter().chain(job.autosave_receipt) {
                    let _ = receipt.send(Err("The project writer stopped unexpectedly.".into()));
                }
            }
            queue.clear_poison();
            signal.notify_all();
        }
    }
}

#[cfg(test)]
mod tests;
