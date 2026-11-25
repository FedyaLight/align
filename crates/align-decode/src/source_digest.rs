//! Strong-USN cached BLAKE3 source digests for Windows exports.
//!
//! Prepared export audio (drift corrections and AAF stems) must invalidate on
//! any in-place rewrite, including a same-length edit whose `LastWriteTime` is
//! restored. Windows `FILE_BASIC_INFO::ChangeTime` does not guarantee that:
//! NTFS populates it from the coarse system clock tick (~15.6 ms), and any
//! handle with `FILE_WRITE_ATTRIBUTES` can overwrite or freeze it.
//!
//! Instead, this module keeps the v0.3.5 full-content BLAKE3 identity (so
//! existing Windows sidecars remain valid and reusable) and avoids re-reading
//! unchanged files on repeat exports by caching the content digest behind an
//! NTFS/ReFS strong USN checkpoint (the BuildXL `FileContentTable` protocol):
//!
//! 1. Open the source read-only with `FILE_SHARE_READ | FILE_SHARE_DELETE`
//!    (omitting `FILE_SHARE_WRITE`), rejecting any active open writer.
//! 2. On lookup, read the file's 128-bit `FileId` + 64-bit `VolumeSerialNumber`
//!    (`FileIdInfo`), its current weak USN (`FSCTL_READ_FILE_USN_DATA`), and
//!    then the volume's active `UsnJournalID` + valid USN bounds
//!    (`FSCTL_QUERY_USN_JOURNAL` on the volume root directory handle, with a
//!    device-handle fallback for elevated processes). A cached digest hits only
//!    when `(volume_serial, usn_journal_id, file_id)` matches, `weak_usn`
//!    equals a previously recorded strong close-record USN within
//!    `[lowest_valid_usn, next_usn)`, and size/timestamps match.
//! 3. On miss, stream-hash the already-opened no-write-sharing handle with
//!    cancellation checks, then force a `USN_REASON_CLOSE` record via
//!    `FSCTL_WRITE_USN_CLOSE_RECORD` so NTFS resets the file's coalescing
//!    reason mask even if another reader handle remains open. Any subsequent
//!    data modification must allocate a strictly newer USN.
//! 4. Whenever any guarantee is unavailable (active open writer, FAT/exFAT,
//!    SMB/UNC share, disabled or resetting USN journal, read-only volume, or
//!    denied journal/FSCTL access), fall back directly to streaming full
//!    BLAKE3 without caching an unverified revision.
//!
//! Fast-path boundary: as with Unix `ctime` and `MAP_SHARED`, a process that
//! writes through a lingering writable memory mapping after closing its file
//! handle may not advance the file's USN until the modified pages are written
//! back to the filesystem.

#[cfg(any(windows, test))]
use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;
#[cfg(any(windows, test))]
use std::path::PathBuf;
#[cfg(any(windows, test))]
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use crate::export::{ExportError, check_cancel};

#[cfg(any(windows, test))]
const CACHE_VERSION: u32 = 1;
const DIGEST_HEX_LEN: usize = 32;
const HASH_CHUNK_BYTES: usize = 1024 * 1024;
#[cfg(any(windows, test))]
const MAX_MEMORY_ENTRIES: usize = 4_096;

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SourceFileKey {
    volume_serial: u64,
    usn_journal_id: u64,
    file_id: [u8; 16],
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SourceMetadataGuard {
    file_size: u64,
    creation_time: i64,
    last_write_time: i64,
    change_time: i64,
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JournalState {
    journal_id: u64,
    lowest_valid_usn: i64,
    next_usn: i64,
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UsnFileRecord {
    file_id: [u8; 16],
    usn: i64,
}

#[cfg(any(windows, test))]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct CachedSourceDigest {
    version: u32,
    volume_serial: u64,
    usn_journal_id: u64,
    file_id: [u8; 16],
    strong_usn: i64,
    metadata: SourceMetadataGuard,
    digest: String,
}

#[cfg(any(windows, test))]
impl CachedSourceDigest {
    fn key(&self) -> SourceFileKey {
        SourceFileKey {
            volume_serial: self.volume_serial,
            usn_journal_id: self.usn_journal_id,
            file_id: self.file_id,
        }
    }

    fn is_well_formed(&self) -> bool {
        self.version == CACHE_VERSION
            && self.usn_journal_id != 0
            && self.strong_usn > 0
            && is_valid_digest(&self.digest)
    }

    fn matches(
        &self,
        key: SourceFileKey,
        journal: JournalState,
        weak_usn: i64,
        metadata: SourceMetadataGuard,
    ) -> bool {
        self.is_well_formed()
            && self.volume_serial == key.volume_serial
            && self.usn_journal_id == key.usn_journal_id
            && self.usn_journal_id == journal.journal_id
            && self.file_id == key.file_id
            && weak_usn > 0
            && weak_usn >= journal.lowest_valid_usn
            && weak_usn < journal.next_usn
            && self.strong_usn == weak_usn
            && self.metadata == metadata
    }
}

#[cfg(any(windows, test))]
fn is_valid_digest(digest: &str) -> bool {
    digest.len() == DIGEST_HEX_LEN
        && digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(any(windows, test))]
struct SourceDigestCache {
    dir: PathBuf,
    memory: Mutex<HashMap<SourceFileKey, CachedSourceDigest>>,
}

#[cfg(any(windows, test))]
impl SourceDigestCache {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            memory: Mutex::new(HashMap::new()),
        }
    }

    fn entry_path(&self, key: SourceFileKey) -> PathBuf {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"export-source-digest-v1\0");
        hasher.update(&key.volume_serial.to_le_bytes());
        hasher.update(&key.usn_journal_id.to_le_bytes());
        hasher.update(&key.file_id);
        self.dir
            .join(format!("{}.json", &hasher.finalize().to_hex()[..32]))
    }

    fn load_disk_entry(&self, key: SourceFileKey) -> Option<CachedSourceDigest> {
        let bytes = std::fs::read(self.entry_path(key)).ok()?;
        let entry: CachedSourceDigest = serde_json::from_slice(&bytes).ok()?;
        (entry.is_well_formed() && entry.key() == key).then_some(entry)
    }

    fn remember_in_memory(&self, entry: CachedSourceDigest) {
        let key = entry.key();
        let mut memory = crate::lock_idle(&self.memory);
        if memory
            .get(&key)
            .is_some_and(|existing| existing.strong_usn > entry.strong_usn)
        {
            return;
        }
        if memory.len() >= MAX_MEMORY_ENTRIES && !memory.contains_key(&key) {
            memory.clear();
        }
        memory.insert(key, entry);
    }

    fn lookup(
        &self,
        key: SourceFileKey,
        journal: JournalState,
        weak_usn: i64,
        metadata: SourceMetadataGuard,
    ) -> Option<String> {
        {
            let memory = crate::lock_idle(&self.memory);
            if let Some(entry) = memory.get(&key)
                && entry.matches(key, journal, weak_usn, metadata)
            {
                return Some(entry.digest.clone());
            }
        }
        let disk = self.load_disk_entry(key)?;
        if !disk.matches(key, journal, weak_usn, metadata) {
            return None;
        }
        let digest = disk.digest.clone();
        self.remember_in_memory(disk);
        Some(digest)
    }

    fn store(&self, entry: CachedSourceDigest) {
        if !entry.is_well_formed() {
            return;
        }
        let key = entry.key();
        let winner = match self.load_disk_entry(key) {
            // Keep the newest close checkpoint if another export finished later.
            Some(existing) if existing.strong_usn > entry.strong_usn => existing,
            _ => {
                if let Ok(bytes) = serde_json::to_vec(&entry)
                    && std::fs::create_dir_all(&self.dir).is_ok()
                {
                    let _ = align_core::staged::write(&self.entry_path(key), &bytes);
                }
                entry
            }
        };
        self.remember_in_memory(winner);
    }
}

/// Parses a single `USN_RECORD_V2` or `USN_RECORD_V3` returned by
/// `FSCTL_READ_FILE_USN_DATA`.
#[cfg(any(windows, test))]
fn parse_usn_record(bytes: &[u8]) -> Option<UsnFileRecord> {
    if bytes.len() < 8 {
        return None;
    }
    let record_len = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    let major = u16::from_le_bytes(bytes[4..6].try_into().ok()?);
    if record_len != bytes.len() {
        return None;
    }
    match major {
        2 => {
            if record_len < 60 {
                return None;
            }
            let mut file_id = [0u8; 16];
            file_id[..8].copy_from_slice(&bytes[8..16]);
            let usn = i64::from_le_bytes(bytes[24..32].try_into().ok()?);
            Some(UsnFileRecord { file_id, usn })
        }
        3 => {
            if record_len < 76 {
                return None;
            }
            let file_id: [u8; 16] = bytes[8..24].try_into().ok()?;
            let usn = i64::from_le_bytes(bytes[40..48].try_into().ok()?);
            Some(UsnFileRecord { file_id, usn })
        }
        _ => None,
    }
}

/// Extracts the local volume root `\\?\Volume{...}\` (with trailing backslash)
/// from `GetFinalPathNameByHandleW(..., VOLUME_NAME_GUID)`. Non-volume paths
/// such as SMB/UNC shares return `None`.
#[cfg(any(windows, test))]
fn extract_volume_guid_root(wide_path: &[u16]) -> Option<Vec<u16>> {
    const PREFIX: &[u8] = b"\\\\?\\Volume{";
    if wide_path.len() <= PREFIX.len() + 2 {
        return None;
    }
    for (index, &expected) in PREFIX.iter().enumerate() {
        let actual = u8::try_from(wide_path[index]).ok()?;
        if !actual.eq_ignore_ascii_case(&expected) {
            return None;
        }
    }
    let body = &wide_path[PREFIX.len()..];
    let close_rel = body.iter().position(|&unit| unit == u16::from(b'}'))?;
    if close_rel == 0 {
        return None;
    }
    if body[..close_rel]
        .iter()
        .any(|&unit| unit == 0 || unit == u16::from(b'\\') || unit == u16::from(b'/'))
    {
        return None;
    }
    let close_idx = PREFIX.len() + close_rel;
    if wide_path.get(close_idx + 1).copied() != Some(u16::from(b'\\')) {
        return None;
    }
    Some(wide_path[..=close_idx + 1].to_vec())
}

pub(crate) fn hash_file_path(path: &Path, cancel: &AtomicBool) -> Result<String, ExportError> {
    let mut file = std::fs::File::open(path).map_err(ExportError::read(path))?;
    hash_reader(&mut file, path, cancel)
}

fn hash_reader(
    reader: &mut impl Read,
    path: &Path,
    cancel: &AtomicBool,
) -> Result<String, ExportError> {
    let mut hash = blake3::Hasher::new();
    let mut buffer = vec![0u8; HASH_CHUNK_BYTES];
    loop {
        check_cancel(cancel)?;
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ExportError::read(path)(error)),
        };
        hash.update(&buffer[..read]);
    }
    Ok(hash.finalize().to_hex()[..DIGEST_HEX_LEN].to_string())
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::OsString;
    use std::fs::{File, OpenOptions};
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx,
        GetFinalPathNameByHandleW, VOLUME_NAME_GUID,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_FILE_USN_DATA, FSCTL_WRITE_USN_CLOSE_RECORD,
        READ_FILE_USN_DATA, USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V1, USN_RECORD_COMMON_HEADER,
        USN_RECORD_V2, USN_RECORD_V3,
    };

    // Compile-time check that `parse_usn_record` matches the Windows SDK ABI.
    const _: () = {
        assert!(std::mem::offset_of!(USN_RECORD_COMMON_HEADER, RecordLength) == 0);
        assert!(std::mem::offset_of!(USN_RECORD_COMMON_HEADER, MajorVersion) == 4);
        assert!(std::mem::offset_of!(USN_RECORD_V2, FileReferenceNumber) == 8);
        assert!(std::mem::offset_of!(USN_RECORD_V2, Usn) == 24);
        assert!(std::mem::offset_of!(USN_RECORD_V2, FileName) == 60);
        assert!(std::mem::offset_of!(USN_RECORD_V3, FileReferenceNumber) == 8);
        assert!(std::mem::offset_of!(USN_RECORD_V3, Usn) == 40);
        assert!(std::mem::offset_of!(USN_RECORD_V3, FileName) == 76);
    };

    #[repr(C, align(8))]
    struct AlignedUsnBuffer([u8; 2048]);

    fn default_cache() -> &'static SourceDigestCache {
        static CACHE: OnceLock<SourceDigestCache> = OnceLock::new();
        CACHE.get_or_init(|| {
            let dir = dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("Align")
                .join("ExportDigests");
            SourceDigestCache::new(dir)
        })
    }

    pub(crate) fn source_revision_tag(
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<String, ExportError> {
        default_cache().tag_for_path(path, cancel)
    }

    fn open_no_write_sharing(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            // Omitting FILE_SHARE_WRITE fails with ERROR_SHARING_VIOLATION if
            // any writer handle is currently open, and prevents a writer from
            // opening the file while we verify or hash it.
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(path)
    }

    fn is_sharing_violation(error: &io::Error) -> bool {
        error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
    }

    fn query_file_id(file: &File) -> Option<(u64, [u8; 16])> {
        let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
        // `file` keeps the handle open and `info` is properly sized and aligned.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                info.as_mut_ptr().cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some((info.VolumeSerialNumber, info.FileId.Identifier))
    }

    fn query_metadata_guard(file: &File) -> Option<SourceMetadataGuard> {
        let file_size = file.metadata().ok()?.len();
        let mut info = std::mem::MaybeUninit::<FILE_BASIC_INFO>::uninit();
        // `file` keeps the handle open and `info` is properly sized and aligned.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileBasicInfo,
                info.as_mut_ptr().cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(SourceMetadataGuard {
            file_size,
            creation_time: info.CreationTime,
            last_write_time: info.LastWriteTime,
            change_time: info.ChangeTime,
        })
    }

    fn query_volume_guid_root(file: &File) -> Option<Vec<u16>> {
        let mut buf = vec![0u16; 260];
        loop {
            let cap = u32::try_from(buf.len()).ok()?;
            // `buf` stays valid for `cap` UTF-16 code units during the call.
            let len = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle(),
                    buf.as_mut_ptr(),
                    cap,
                    VOLUME_NAME_GUID,
                )
            };
            if len == 0 {
                return None;
            }
            let len = len as usize;
            if len < buf.len() {
                buf.truncate(len);
                return extract_volume_guid_root(&buf);
            }
            if len > 32_768 {
                return None;
            }
            buf.resize(len + 1, 0);
        }
    }

    fn query_journal_on_handle(volume: &File) -> Option<JournalState> {
        // Zero-initialize so reading V0 prefix fields is always defined even if
        // a driver only writes `USN_JOURNAL_DATA_V0` bytes into the V1 buffer.
        let mut v1: USN_JOURNAL_DATA_V1 = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        // `volume` stays open and `v1` holds one `USN_JOURNAL_DATA_V1`.
        let ok = unsafe {
            DeviceIoControl(
                volume.as_raw_handle(),
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                std::ptr::from_mut(&mut v1).cast(),
                size_of::<USN_JOURNAL_DATA_V1>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 && returned as usize >= size_of::<USN_JOURNAL_DATA_V0>() {
            return (v1.UsnJournalID != 0 && v1.NextUsn > 0 && v1.LowestValidUsn >= 0).then_some(
                JournalState {
                    journal_id: v1.UsnJournalID,
                    lowest_valid_usn: v1.LowestValidUsn,
                    next_usn: v1.NextUsn,
                },
            );
        }

        let mut v0: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
        returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                volume.as_raw_handle(),
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                std::ptr::from_mut(&mut v0).cast(),
                size_of::<USN_JOURNAL_DATA_V0>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || (returned as usize) < size_of::<USN_JOURNAL_DATA_V0>() {
            return None;
        }
        (v0.UsnJournalID != 0 && v0.NextUsn > 0 && v0.LowestValidUsn >= 0).then_some(JournalState {
            journal_id: v0.UsnJournalID,
            lowest_valid_usn: v0.LowestValidUsn,
            next_usn: v0.NextUsn,
        })
    }

    pub(super) fn query_volume_journal(file: &File) -> Option<JournalState> {
        let root = query_volume_guid_root(file)?;
        // Windows 10 RS2+ allows unprivileged FSCTL_QUERY_USN_JOURNAL on the
        // volume root directory handle (`\\?\Volume{...}\` with backup semantics).
        let root_path = OsString::from_wide(&root);
        if let Ok(volume) = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&root_path)
            && let Some(state) = query_journal_on_handle(&volume)
        {
            return Some(state);
        }
        // Fallback for elevated processes / environments expecting the volume
        // device path (`\\?\Volume{...}` without trailing backslash).
        let device_path = OsString::from_wide(&root[..root.len() - 1]);
        let volume = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&device_path)
            .ok()?;
        query_journal_on_handle(&volume)
    }

    fn read_file_usn(file: &File) -> Option<UsnFileRecord> {
        let input = READ_FILE_USN_DATA {
            MinMajorVersion: 2,
            MaxMajorVersion: 3,
        };
        let mut output = AlignedUsnBuffer([0u8; 2048]);
        let mut returned = 0u32;
        // `output` is 8-byte aligned and sized above the max V3 USN record.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_READ_FILE_USN_DATA,
                std::ptr::from_ref(&input).cast(),
                size_of::<READ_FILE_USN_DATA>() as u32,
                output.0.as_mut_ptr().cast(),
                output.0.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        let len = usize::try_from(returned).ok()?;
        if len > output.0.len() {
            return None;
        }
        parse_usn_record(&output.0[..len])
    }

    pub(super) fn write_usn_close_record(file: &File) -> Option<i64> {
        let mut strong_usn = 0i64;
        let mut returned = 0u32;
        // `strong_usn` is 8-byte aligned and sized for one 64-bit USN.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_WRITE_USN_CLOSE_RECORD,
                std::ptr::null(),
                0,
                std::ptr::from_mut(&mut strong_usn).cast(),
                size_of::<i64>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || returned as usize != size_of::<i64>() || strong_usn <= 0 {
            return None;
        }
        Some(strong_usn)
    }

    impl SourceDigestCache {
        pub(super) fn try_cached_digest(&self, file: &File) -> Option<String> {
            let (volume_serial, file_id) = query_file_id(file)?;
            let usn_record = read_file_usn(file)?;
            if usn_record.file_id != file_id || usn_record.usn <= 0 {
                return None;
            }
            // Query the journal *after* reading the file USN so `weak_usn < next_usn`
            // holds for any valid record in the active journal instance.
            let journal = query_volume_journal(file)?;
            let metadata = query_metadata_guard(file)?;
            let key = SourceFileKey {
                volume_serial,
                usn_journal_id: journal.journal_id,
                file_id,
            };
            self.lookup(key, journal, usn_record.usn, metadata)
        }

        fn record_strong_digest(&self, file: &File, digest: &str) {
            let Some((volume_serial, file_id)) = query_file_id(file) else {
                return;
            };
            let Some(strong_usn) = write_usn_close_record(file) else {
                return;
            };
            // Query the journal and metadata *after* writing the close record so
            // the recorded journal id, USN bounds, and timestamps match the
            // committed checkpoint.
            let Some(journal) = query_volume_journal(file) else {
                return;
            };
            if strong_usn < journal.lowest_valid_usn || strong_usn >= journal.next_usn {
                return;
            }
            let Some(metadata) = query_metadata_guard(file) else {
                return;
            };
            self.store(CachedSourceDigest {
                version: CACHE_VERSION,
                volume_serial,
                usn_journal_id: journal.journal_id,
                file_id,
                strong_usn,
                metadata,
                digest: digest.to_string(),
            });
        }

        pub(super) fn tag_for_path(
            &self,
            path: &Path,
            cancel: &AtomicBool,
        ) -> Result<String, ExportError> {
            check_cancel(cancel)?;
            match open_no_write_sharing(path) {
                Ok(mut file) => {
                    if let Some(digest) = self.try_cached_digest(&file) {
                        return Ok(digest);
                    }
                    let digest = hash_reader(&mut file, path, cancel)?;
                    check_cancel(cancel)?;
                    self.record_strong_digest(&file, &digest);
                    Ok(digest)
                }
                Err(error) if is_sharing_violation(&error) => {
                    // An active writer holds the file open; bypass the USN cache
                    // and stream the live content hash directly.
                    hash_file_path(path, cancel)
                }
                Err(error) => Err(ExportError::read(path)(error)),
            }
        }
    }
}

#[cfg(windows)]
pub(crate) use platform::source_revision_tag;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_digest(seed: &[u8]) -> String {
        blake3::hash(seed).to_hex()[..DIGEST_HEX_LEN].to_string()
    }

    fn sample_entry(strong_usn: i64, digest: String) -> CachedSourceDigest {
        CachedSourceDigest {
            version: CACHE_VERSION,
            volume_serial: 0x1122_3344_5566_7788,
            usn_journal_id: 0x99aa_bbcc_ddee_ff01,
            file_id: [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0],
            strong_usn,
            metadata: SourceMetadataGuard {
                file_size: 4 * 1024 * 1024,
                creation_time: 100,
                last_write_time: 200,
                change_time: 300,
            },
            digest,
        }
    }

    #[test]
    fn parse_usn_record_decodes_v2_and_v3_and_rejects_malformed() {
        let mut v2 = vec![0u8; 64];
        v2[0..4].copy_from_slice(&64u32.to_le_bytes());
        v2[4..6].copy_from_slice(&2u16.to_le_bytes());
        v2[8..16].copy_from_slice(&0x0001_0000_0000_1234u64.to_le_bytes());
        v2[24..32].copy_from_slice(&9_876_543i64.to_le_bytes());
        assert_eq!(
            parse_usn_record(&v2),
            Some(UsnFileRecord {
                file_id: [0x34, 0x12, 0, 0, 0, 0, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                usn: 9_876_543,
            })
        );

        let mut v3 = vec![0u8; 80];
        v3[0..4].copy_from_slice(&80u32.to_le_bytes());
        v3[4..6].copy_from_slice(&3u16.to_le_bytes());
        let refs_id = [
            0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0, 0xd0, 0xe0,
            0xf0, 0x01,
        ];
        v3[8..24].copy_from_slice(&refs_id);
        v3[40..48].copy_from_slice(&42_000i64.to_le_bytes());
        assert_eq!(
            parse_usn_record(&v3),
            Some(UsnFileRecord {
                file_id: refs_id,
                usn: 42_000,
            })
        );

        // Length mismatch, truncated record, or unsupported major version.
        assert_eq!(parse_usn_record(&v2[..63]), None);
        let mut truncated = vec![0u8; 56];
        truncated[0..4].copy_from_slice(&56u32.to_le_bytes());
        truncated[4..6].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(parse_usn_record(&truncated), None);
        let mut unknown_ver = v3.clone();
        unknown_ver[4..6].copy_from_slice(&4u16.to_le_bytes());
        assert_eq!(parse_usn_record(&unknown_ver), None);
    }

    #[test]
    fn extract_volume_guid_root_accepts_local_volumes_and_rejects_unc() {
        let to_utf16 = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
        let local =
            to_utf16(r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\Users\editor\clip.wav");
        let root = extract_volume_guid_root(&local).expect("local volume GUID root");
        assert_eq!(
            String::from_utf16(&root).unwrap(),
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\"
        );

        for rejected in [
            r"\\?\UNC\server\share\clip.wav",
            r"\\?\C:\Users\editor\clip.wav",
            r"\\?\Volume{}\clip.wav",
            r"\\?\Volume{bad\slash}\clip.wav",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}",
        ] {
            assert_eq!(
                extract_volume_guid_root(&to_utf16(rejected)),
                None,
                "{rejected}"
            );
        }
    }

    #[test]
    fn digest_cache_enforces_strong_usn_invariants_and_monotonic_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SourceDigestCache::new(dir.path().to_path_buf());
        let digest_v1 = sample_digest(b"version-1");
        let digest_v2 = sample_digest(b"version-2");
        let entry = sample_entry(5_000, digest_v1.clone());
        let key = entry.key();
        let journal = JournalState {
            journal_id: entry.usn_journal_id,
            lowest_valid_usn: 1_000,
            next_usn: 10_000,
        };
        let meta = entry.metadata;

        cache.store(entry.clone());
        assert_eq!(
            cache.lookup(key, journal, 5_000, meta),
            Some(digest_v1.clone())
        );

        // A fresh cache instance (separate process) reuses the persisted entry.
        let reloaded = SourceDigestCache::new(dir.path().to_path_buf());
        assert_eq!(
            reloaded.lookup(key, journal, 5_000, meta),
            Some(digest_v1.clone())
        );

        // Any USN advance, zero USN, journal reset, out-of-range USN, identity
        // change, or metadata drift invalidates the cached digest.
        assert_eq!(reloaded.lookup(key, journal, 5_064, meta), None);
        assert_eq!(reloaded.lookup(key, journal, 0, meta), None);
        assert_eq!(
            reloaded.lookup(
                key,
                JournalState {
                    lowest_valid_usn: 5_001,
                    ..journal
                },
                5_000,
                meta
            ),
            None
        );
        assert_eq!(
            reloaded.lookup(
                key,
                JournalState {
                    next_usn: 5_000,
                    ..journal
                },
                5_000,
                meta
            ),
            None
        );
        assert_eq!(
            reloaded.lookup(
                SourceFileKey {
                    usn_journal_id: key.usn_journal_id + 1,
                    ..key
                },
                JournalState {
                    journal_id: key.usn_journal_id + 1,
                    ..journal
                },
                5_000,
                meta
            ),
            None
        );
        assert_eq!(
            reloaded.lookup(
                SourceFileKey {
                    file_id: [9; 16],
                    ..key
                },
                journal,
                5_000,
                meta
            ),
            None
        );
        assert_eq!(
            reloaded.lookup(
                key,
                journal,
                5_000,
                SourceMetadataGuard {
                    file_size: meta.file_size + 1,
                    ..meta
                }
            ),
            None
        );

        // Monotonicity: newer strong USN wins over an out-of-order older store.
        let newer = sample_entry(6_000, digest_v2.clone());
        reloaded.store(newer);
        cache.store(entry);
        let verified = SourceDigestCache::new(dir.path().to_path_buf());
        assert_eq!(verified.lookup(key, journal, 6_000, meta), Some(digest_v2));
        assert_eq!(verified.lookup(key, journal, 5_000, meta), None);

        // Corrupt cache payloads recover cleanly as a miss.
        std::fs::write(verified.entry_path(key), b"not json").unwrap();
        let after_corrupt = SourceDigestCache::new(dir.path().to_path_buf());
        assert_eq!(after_corrupt.lookup(key, journal, 6_000, meta), None);
    }

    #[test]
    fn hash_reader_matches_blake3_and_honors_cancel_mid_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.wav");
        let payload: Vec<u8> = (0..(HASH_CHUNK_BYTES * 2 + 123))
            .map(|i| (i.wrapping_mul(31) & 0xff) as u8)
            .collect();
        std::fs::write(&path, &payload).unwrap();
        let cancel = AtomicBool::new(false);
        assert_eq!(
            hash_file_path(&path, &cancel).unwrap(),
            sample_digest(&payload)
        );

        struct CancelAfterFirstChunk<'a> {
            cancel: &'a AtomicBool,
            reads: usize,
        }
        impl Read for CancelAfterFirstChunk<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if self.reads == 1 {
                    self.cancel
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    buf[..16].fill(0xaa);
                    Ok(16)
                } else {
                    panic!("reader must not be polled after cancellation");
                }
            }
        }
        let mid_cancel = AtomicBool::new(false);
        let mut reader = CancelAfterFirstChunk {
            cancel: &mid_cancel,
            reads: 0,
        };
        assert!(matches!(
            hash_reader(&mut reader, &path, &mid_cancel),
            Err(ExportError::Cancelled)
        ));
        assert_eq!(reader.reads, 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_strong_usn_cache_reuses_unchanged_and_defeats_timestamp_restore_and_coalescing() {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo,
            GetFileInformationByHandleEx, SetFileInformationByHandle,
        };

        let dir = tempfile::tempdir().unwrap();
        let cache = SourceDigestCache::new(dir.path().join("export-digests"));
        let path = dir.path().join("source.wav");
        let mut payload = vec![0x2au8; 4 * 1024 * 1024];
        std::fs::write(&path, &payload).unwrap();

        let read_basic = |p: &Path| -> FILE_BASIC_INFO {
            let f = std::fs::File::open(p).unwrap();
            let mut info = std::mem::MaybeUninit::<FILE_BASIC_INFO>::uninit();
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    f.as_raw_handle(),
                    FileBasicInfo,
                    info.as_mut_ptr().cast(),
                    size_of::<FILE_BASIC_INFO>() as u32,
                )
            };
            assert_ne!(ok, 0);
            unsafe { info.assume_init() }
        };
        let restore_basic = |p: &Path, saved: FILE_BASIC_INFO| {
            let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
            let info = FILE_BASIC_INFO {
                FileAttributes: 0,
                ..saved
            };
            let ok = unsafe {
                SetFileInformationByHandle(
                    f.as_raw_handle(),
                    FileBasicInfo,
                    std::ptr::from_ref(&info).cast(),
                    size_of::<FILE_BASIC_INFO>() as u32,
                )
            };
            assert_ne!(ok, 0);
        };

        let saved_basic = read_basic(&path);
        let cancel = AtomicBool::new(false);

        // Cold export hashes once and produces the exact v0.3.5 BLAKE3 key.
        let expected_v1 = sample_digest(&payload);
        let tag_v1 = cache.tag_for_path(&path, &cancel).unwrap();
        assert_eq!(tag_v1, expected_v1);

        // Probe on a sibling file whether this volume supports both journal
        // queries and close-record checkpoints (standard local NTFS with an
        // active USN journal). When supported, repeat lookups — including from
        // a fresh cache instance — must hit the zero-read fast path directly.
        let cap_probe = dir.path().join("usn-capability.tmp");
        std::fs::write(&cap_probe, b"probe").unwrap();
        let cap_file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&cap_probe)
            .unwrap();
        let fast_path_supported = platform::write_usn_close_record(&cap_file).is_some()
            && platform::query_volume_journal(&cap_file).is_some();
        drop(cap_file);

        let probe_file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(&path)
            .unwrap();
        if fast_path_supported {
            assert_eq!(cache.try_cached_digest(&probe_file), Some(tag_v1.clone()));
            let reloaded = SourceDigestCache::new(dir.path().join("export-digests"));
            assert_eq!(
                reloaded.try_cached_digest(&probe_file),
                Some(tag_v1.clone())
            );
        }
        drop(probe_file);
        assert_eq!(cache.tag_for_path(&path, &cancel).unwrap(), tag_v1);

        // Keep a lingering reader handle open across two consecutive mid-file
        // edits with restored LastWriteTime AND ChangeTime. Without a strong
        // close-record checkpoint, NTFS suppresses USN_REASON_CLOSE and
        // coalesces the second write into the first write's USN.
        let lingering_reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&path)
            .unwrap();

        let mid = 2 * 1024 * 1024;
        for byte in [0x7fu8, 0x81u8] {
            payload[mid] = byte;
            let mut writer = std::fs::OpenOptions::new()
                .write(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&path)
                .unwrap();
            writer.seek(SeekFrom::Start(mid as u64)).unwrap();
            writer.write_all(&[byte]).unwrap();
            drop(writer);
            restore_basic(&path, saved_basic);

            let check_handle = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                .open(&path)
                .unwrap();
            assert_eq!(cache.try_cached_digest(&check_handle), None);
            drop(check_handle);

            let expected = sample_digest(&payload);
            let tag = cache.tag_for_path(&path, &cancel).unwrap();
            assert_eq!(tag, expected);
            assert_ne!(tag, tag_v1);
        }

        // An active open writer must trigger safe fallback without failing or
        // caching a coalesced USN across multiple writes on the same handle.
        let mut active_writer = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&path)
            .unwrap();
        for byte in [0x99u8, 0xaau8] {
            payload[mid] = byte;
            active_writer.seek(SeekFrom::Start(mid as u64)).unwrap();
            active_writer.write_all(&[byte]).unwrap();
            active_writer.flush().unwrap();
            assert_eq!(
                cache.tag_for_path(&path, &cancel).unwrap(),
                sample_digest(&payload)
            );
        }
        drop(active_writer);
        drop(lingering_reader);

        // Replacing the file via rename with restored timestamps invalidates.
        let replacement = dir.path().join("replacement.wav");
        payload[mid] = 0x55;
        std::fs::write(&replacement, &payload).unwrap();
        restore_basic(&replacement, saved_basic);
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(
            cache.tag_for_path(&path, &cancel).unwrap(),
            sample_digest(&payload)
        );
    }
}
