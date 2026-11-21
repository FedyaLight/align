//! Persistent fingerprint and video-timing cache.
//!
//! Fingerprint keys include the cache version, backend, analysis variant,
//! normalized media path, metadata, and samples from the first and last MiB.
//! Video-timing entries also identify the probe executable. Entries use
//! versioned bincode payloads and atomic writes in the OS cache directory.

use crate::fingerprint::Fingerprint;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CACHE_VERSION: u32 = 6;
pub const CACHE_MAX_AGE_DAYS: u64 = 30;
const LEGACY_CACHE_VERSION: u32 = 5;
const HEAD_TAIL_BYTES: u64 = 1_048_576;

#[derive(Serialize, Deserialize)]
struct Payload {
    version: u32,
    backend_tag: String,
    media_path: PathBuf,
    fingerprints: Vec<Fingerprint>,
}

#[derive(Serialize, Deserialize)]
struct TimingPayload {
    media_path: PathBuf,
    timing: crate::VideoTimingInspection,
}

#[derive(Serialize, Deserialize)]
struct LtcPayload {
    media_path: PathBuf,
    detection: Option<(usize, crate::SourceTimecode)>,
}

/// The same bytes and metadata used by the on-disk v5 and v6 formats.
/// Keep these only for the duration of one clip's fingerprint pass.
#[derive(PartialEq)]
pub struct MediaSample {
    revision: Option<String>,
    path: PathBuf,
    size: u64,
    mtime: f64,
    bytes: Vec<u8>,
}

impl MediaSample {
    pub fn read(media: &Path) -> std::io::Result<Self> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(media)?;
        let meta = file.metadata()?;
        let revision = file_revision(&file, &meta).ok();
        let size = meta.len();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0.0, |d| d.as_secs_f64());
        let head = size.min(HEAD_TAIL_BYTES) as usize;
        let tail = if size > HEAD_TAIL_BYTES {
            HEAD_TAIL_BYTES as usize
        } else {
            0
        };
        let mut bytes = vec![0; head + tail];
        file.read_exact(&mut bytes[..head])?;
        if tail > 0 {
            let overlap = (head + tail).saturating_sub(usize::try_from(size).unwrap_or(usize::MAX));
            if overlap > 0 {
                // Preserve the overlapping bytes in the v5/v6 hash input.
                bytes.copy_within(head - overlap..head, head);
            } else {
                file.seek(SeekFrom::End(-(tail as i64)))?;
            }
            file.read_exact(&mut bytes[head + overlap..])?;
        }
        Ok(Self {
            revision,
            path: normalized_path(media),
            size,
            mtime,
            bytes,
        })
    }
    fn is_current(&self) -> bool {
        match &self.revision {
            Some(revision) => media_revision(&self.path).as_ref().ok() == Some(revision),
            // Some filesystems do not expose a change time. Keep v5/v6
            // caching available and recheck the sampled identity before writing.
            None => Self::read(&self.path).is_ok_and(|sample| sample == *self),
        }
    }
}

pub struct FingerprintEntry<'a> {
    cache: &'a FingerprintCache,
    sample: &'a MediaSample,
    path: PathBuf,
    variant: &'a str,
}

/// v5 omitted the media path but its fingerprints remain valid. Keep this
/// reader so an upgrade does not re-analyse hours-long recordings.
#[derive(Serialize, Deserialize)]
struct LegacyPayload {
    version: u32,
    backend_tag: String,
    fingerprints: Vec<Fingerprint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSettings {
    /// `None` keeps cached analysis until the user clears it.
    pub retention_days: Option<u64>,
}

impl Default for CacheSettings {
    fn default() -> Self {
        Self {
            retention_days: Some(CACHE_MAX_AGE_DAYS),
        }
    }
}

impl CacheSettings {
    pub fn load() -> Self {
        Self::load_from(&settings_file())
    }

    pub fn save(self) {
        self.save_to(&settings_file());
    }

    pub fn load_from(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save_to(self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&self) {
            let _ = std::fs::write(path, bytes);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheStatistics {
    pub file_count: usize,
    pub total_bytes: u64,
}

pub struct FingerprintCache {
    dir: PathBuf,
    /// Namespaces entries per media engine (`portable1`, `apple1`, …).
    /// Decode converters differ in the last ulp across engines, so entries
    /// must never be shared between them even though peak picking is robust.
    backend_tag: String,
}

impl FingerprintCache {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self::with_backend(dir, "portable1")
    }

    pub fn with_backend(dir: Option<PathBuf>, backend_tag: &str) -> Self {
        let dir = dir.unwrap_or_else(default_dir);
        Self {
            dir,
            backend_tag: backend_tag.to_string(),
        }
    }

    pub fn statistics(&self) -> CacheStatistics {
        let mut st = CacheStatistics::default();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return st;
        };
        for e in entries.flatten() {
            let p = e.path();
            if !matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("bin" | "timing" | "ltc")
            ) {
                continue;
            }
            st.file_count += 1;
            st.total_bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            st.total_bytes += std::fs::metadata(p.with_extension("waveform"))
                .map(|metadata| metadata.len())
                .unwrap_or(0);
        }
        st
    }

    pub fn clear(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    /// Remove all backend and analysis variants belonging to `media`.
    pub fn clear_media(&self, media: &[PathBuf]) -> CacheStatistics {
        let media: std::collections::HashSet<PathBuf> =
            media.iter().map(|path| normalized_path(path)).collect();
        let mut removed = CacheStatistics::default();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return removed;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("bin" | "timing" | "ltc")
            ) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Some(media_path) = std::fs::read(&path).ok().and_then(|bytes| {
                if path.extension().and_then(|value| value.to_str()) == Some("timing") {
                    serde_json::from_slice::<TimingPayload>(&bytes)
                        .ok()
                        .map(|p| p.media_path)
                } else if path.extension().is_some_and(|e| e == "ltc") {
                    serde_json::from_slice::<LtcPayload>(&bytes)
                        .ok()
                        .map(|p| p.media_path)
                } else {
                    bincode::deserialize::<Payload>(&bytes)
                        .ok()
                        .filter(|p| p.version == CACHE_VERSION)
                        .map(|p| p.media_path)
                }
            }) else {
                continue;
            };
            if media.contains(&normalized_path(&media_path)) && std::fs::remove_file(path).is_ok() {
                removed.file_count += 1;
                removed.total_bytes += metadata.len();
                let waveform = entry.path().with_extension("waveform");
                if let Ok(metadata) = std::fs::metadata(&waveform)
                    && std::fs::remove_file(waveform).is_ok()
                {
                    removed.total_bytes += metadata.len();
                }
            }
        }
        removed
    }

    /// Remove expired fingerprint payloads without touching any other file
    /// in the cache directory. This is synchronous but runs only when a
    /// production pipeline is created, never on a timer.
    pub fn prune_older_than(&self, max_age: std::time::Duration) -> CacheStatistics {
        let mut removed = CacheStatistics::default();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return removed;
        };
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            if !matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("bin" | "timing" | "ltc")
            ) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let expired = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= max_age);
            if expired && std::fs::remove_file(path).is_ok() {
                removed.file_count += 1;
                removed.total_bytes += metadata.len();
                let waveform = entry.path().with_extension("waveform");
                if let Ok(metadata) = std::fs::metadata(&waveform)
                    && std::fs::remove_file(waveform).is_ok()
                {
                    removed.total_bytes += metadata.len();
                }
            }
        }
        removed
    }

    /// Packet timing shares cache clearing/retention with fingerprints.
    /// `decoder` includes the resolved ffprobe executable's identity.
    pub fn load_video_timing(
        &self,
        media: &Path,
        decoder: &str,
    ) -> Option<crate::VideoTimingInspection> {
        self.video_timing_entry(media, decoder)?.load()
    }

    pub fn save_video_timing(
        &self,
        media: &Path,
        decoder: &str,
        timing: crate::VideoTimingInspection,
    ) {
        if let Some(entry) = self.video_timing_entry(media, decoder) {
            entry.save(timing);
        }
    }

    pub fn video_timing_entry(&self, media: &Path, decoder: &str) -> Option<VideoTimingEntry> {
        let sample = MediaSample::read(media).ok()?;
        let revision = sample.revision.as_ref()?;
        let variant = format!("video-timing-v1:{decoder}:{revision}");
        let path = self
            .path_for_version(&sample, &variant, CACHE_VERSION)
            .with_extension("timing");
        Some(VideoTimingEntry {
            path,
            media_path: sample.path,
            revision: sample.revision?,
        })
    }

    pub fn entry<'a>(&'a self, sample: &'a MediaSample, variant: &'a str) -> FingerprintEntry<'a> {
        FingerprintEntry {
            cache: self,
            sample,
            variant,
            path: self.path_for_version(sample, variant, CACHE_VERSION),
        }
    }

    pub fn load(&self, media: &Path, variant: &str) -> Option<Vec<Fingerprint>> {
        self.entry(&MediaSample::read(media).ok()?, variant).load()
    }

    pub fn save(&self, fingerprints: &[Fingerprint], media: &Path, variant: &str) {
        if let Ok(sample) = MediaSample::read(media) {
            self.entry(&sample, variant).save(fingerprints);
        }
    }

    pub fn load_waveform(&self, media: &Path, variant: &str) -> Option<Vec<f32>> {
        self.entry(&MediaSample::read(media).ok()?, variant)
            .load_waveform()
    }

    pub fn save_waveform(&self, waveform: &[f32], media: &Path, variant: &str) {
        if let Ok(sample) = MediaSample::read(media) {
            self.entry(&sample, variant).save_waveform(waveform);
        }
    }

    fn path_for_version(&self, sample: &MediaSample, variant: &str, version: u32) -> PathBuf {
        let MediaSample {
            path,
            size,
            mtime,
            bytes,
            ..
        } = sample;
        let mut hasher = blake3::Hasher::new();
        let prefix = if version == LEGACY_CACHE_VERSION {
            format!(
                "fingerprints-{version}\0{}\0{variant}\0{size}\0{mtime}",
                self.backend_tag
            )
        } else {
            format!(
                "fingerprints-{version}\0{}\0{variant}\0{}\0{size}\0{mtime}",
                self.backend_tag,
                path.to_string_lossy()
            )
        };
        hasher.update(prefix.as_bytes());
        hasher.update(bytes);
        self.dir.join(format!("{}.bin", hasher.finalize().to_hex()))
    }

    /// LTC is independent of fingerprint accuracy and channel selection.
    /// Unix change time also detects rewrites with a restored mtime.
    pub fn ltc_entry(&self, media: &Path) -> Option<LtcEntry> {
        let media_path = normalized_path(media);
        let revision = media_revision(&media_path).ok()?;
        let mut key = blake3::Hasher::new();
        key.update(b"ltc-v1\0");
        key.update(self.backend_tag.as_bytes());
        key.update(&[0]);
        key.update(media_path.as_os_str().as_encoded_bytes());
        key.update(&[0]);
        key.update(revision.as_bytes());
        Some(LtcEntry {
            path: self.dir.join(format!("{}.ltc", key.finalize())),
            media_path,
            revision,
        })
    }
}

pub struct VideoTimingEntry {
    path: PathBuf,
    media_path: PathBuf,
    revision: String,
}

impl VideoTimingEntry {
    pub fn load(&self) -> Option<crate::VideoTimingInspection> {
        let bytes = std::fs::read(&self.path).ok()?;
        serde_json::from_slice::<TimingPayload>(&bytes)
            .ok()
            .map(|p| p.timing)
    }

    pub fn save(&self, timing: crate::VideoTimingInspection) {
        if media_revision(&self.media_path).ok().as_ref() != Some(&self.revision) {
            return;
        }
        let payload = TimingPayload {
            media_path: self.media_path.clone(),
            timing,
        };
        if let Ok(bytes) = serde_json::to_vec(&payload)
            && let Some(dir) = self.path.parent()
            && std::fs::create_dir_all(dir).is_ok()
        {
            let _ = crate::staged::write(&self.path, &bytes);
        }
    }
}

pub struct LtcEntry {
    path: PathBuf,
    media_path: PathBuf,
    revision: String,
}

impl LtcEntry {
    pub fn load(&self) -> Option<Option<(usize, crate::SourceTimecode)>> {
        Some(
            serde_json::from_slice::<LtcPayload>(&std::fs::read(&self.path).ok()?)
                .ok()?
                .detection,
        )
    }

    pub fn save(&self, detection: Option<(usize, crate::SourceTimecode)>) {
        if media_revision(&self.media_path).ok().as_ref() != Some(&self.revision) {
            return;
        }
        let payload = LtcPayload {
            media_path: self.media_path.clone(),
            detection,
        };
        if let Ok(bytes) = serde_json::to_vec(&payload)
            && let Some(dir) = self.path.parent()
            && std::fs::create_dir_all(dir).is_ok()
        {
            let _ = crate::staged::write(&self.path, &bytes);
        }
    }
}

impl FingerprintEntry<'_> {
    pub fn load(&self) -> Option<Vec<Fingerprint>> {
        if let Ok(bytes) = std::fs::read(&self.path)
            && let Ok(payload) = bincode::deserialize::<Payload>(&bytes)
            && payload.version == CACHE_VERSION
            && payload.backend_tag == self.cache.backend_tag
        {
            return Some(payload.fingerprints);
        }

        // Promote v5 in place after the first successful read. The legacy
        // key still validates backend, variant, metadata, and sampled bytes.
        let legacy_url =
            self.cache
                .path_for_version(self.sample, self.variant, LEGACY_CACHE_VERSION);
        let bytes = std::fs::read(&legacy_url).ok()?;
        let payload: LegacyPayload = bincode::deserialize(&bytes).ok()?;
        if payload.version != LEGACY_CACHE_VERSION || payload.backend_tag != self.cache.backend_tag
        {
            return None;
        }
        self.save(&payload.fingerprints);
        if self.path.exists() {
            let _ = std::fs::remove_file(legacy_url);
        }
        Some(payload.fingerprints)
    }

    pub fn save(&self, fingerprints: &[Fingerprint]) {
        if !self.sample.is_current() || std::fs::create_dir_all(&self.cache.dir).is_err() {
            return;
        }
        let payload = Payload {
            version: CACHE_VERSION,
            backend_tag: self.cache.backend_tag.clone(),
            media_path: self.sample.path.clone(),
            fingerprints: fingerprints.to_vec(),
        };
        let Ok(bytes) = bincode::serialize(&payload) else {
            return;
        };
        let _ = crate::staged::write(&self.path, &bytes);
    }

    /// Load the compact amplitude envelope stored beside a fingerprint entry.
    pub fn load_waveform(&self) -> Option<Vec<f32>> {
        let url = self.path.with_extension("waveform");
        let waveform: Vec<f32> = bincode::deserialize(&std::fs::read(url).ok()?).ok()?;
        (!waveform.is_empty()
            && waveform.len() <= 4_096
            && waveform
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value)))
        .then_some(waveform)
    }

    /// Store a session waveform without changing the fingerprint payload.
    pub fn save_waveform(&self, waveform: &[f32]) {
        if waveform.is_empty()
            || waveform.len() > 4_096
            || !waveform
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
        {
            return;
        }
        let url = self.path.with_extension("waveform");
        if !self.sample.is_current() || std::fs::create_dir_all(&self.cache.dir).is_err() {
            return;
        }
        let Ok(bytes) = bincode::serialize(waveform) else {
            return;
        };
        let _ = crate::staged::write(&url, &bytes);
    }
}

/// Cheap change stamp used before/after a packet walk. Cache keys additionally
/// sample file contents. Unix ctime detects rewrites with a restored mtime.
pub fn media_revision(path: &Path) -> std::io::Result<String> {
    #[cfg(windows)]
    {
        let file = std::fs::File::open(path)?;
        opened_media_revision(&file)
    }
    #[cfg(not(windows))]
    {
        metadata_revision(&std::fs::metadata(path)?)
    }
}

/// Revision of the opened object, including when its path has been replaced.
pub fn opened_media_revision(file: &std::fs::File) -> std::io::Result<String> {
    file_revision(file, &file.metadata()?)
}

fn file_revision(_file: &std::fs::File, metadata: &std::fs::Metadata) -> std::io::Result<String> {
    let stamp = metadata_revision(metadata)?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
        };
        let mut info = std::mem::MaybeUninit::<FILE_BASIC_INFO>::uninit();
        // The handle stays open and the OS fills one FILE_BASIC_INFO on success.
        let success = unsafe {
            GetFileInformationByHandleEx(
                _file.as_raw_handle(),
                FileBasicInfo,
                info.as_mut_ptr().cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        if success == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // ChangeTime includes writes followed by restoration of LastWriteTime.
        let change = unsafe { info.assume_init() }.ChangeTime;
        if change == 0 {
            return Err(std::io::Error::other("missing file change time"));
        }
        Ok(format!("{stamp}:{change}"))
    }
    #[cfg(not(windows))]
    {
        Ok(stamp)
    }
}

fn metadata_revision(metadata: &std::fs::Metadata) -> std::io::Result<String> {
    let stamp = format!(
        "{}:{:?}:{:?}",
        metadata.len(),
        metadata.modified()?,
        metadata.created().ok()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!(
            "{stamp}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ))
    }
    #[cfg(not(unix))]
    {
        Ok(stamp)
    }
}

fn default_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Align")
        .join("Fingerprints")
}

fn settings_file() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Align")
        .join("cache.json")
}

fn normalized_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_timing_invalidates_and_obeys_cache_management() {
        let root =
            std::env::temp_dir().join(format!("align-timing-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let media = root.join("media.mov");
        std::fs::write(&media, b"first payload").unwrap();
        let cache = FingerprintCache::with_backend(Some(root.join("cache")), "portable-timing1");
        let timing = crate::VideoTimingInspection {
            frame_duration: Some(crate::MediaTime::new(1, 25)),
            mode: crate::VideoFrameRateMode::Constant,
        };
        cache.save_video_timing(&media, "decoder1", timing);
        assert_eq!(cache.load_video_timing(&media, "decoder1"), Some(timing));
        assert_eq!(cache.load_video_timing(&media, "decoder2"), None);
        let old_time = std::fs::metadata(&media).unwrap().modified().unwrap();
        std::fs::write(&media, b"other payload").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&media)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        assert_eq!(
            cache.load_video_timing(&media, "decoder1"),
            None,
            "sampled bytes invalidate even with unchanged size/mtime"
        );
        cache.save_video_timing(&media, "decoder1", timing);
        let path = cache.video_timing_entry(&media, "decoder1").unwrap().path;
        std::fs::write(&path, b"broken cache").unwrap();
        assert_eq!(cache.load_video_timing(&media, "decoder1"), None);
        cache.save_video_timing(&media, "decoder1", timing);
        assert_eq!(cache.statistics().file_count, 2);
        assert_eq!(
            cache.clear_media(std::slice::from_ref(&media)).file_count,
            2
        );
        cache.save_video_timing(&media, "decoder1", timing);
        assert_eq!(
            cache.prune_older_than(std::time::Duration::ZERO).file_count,
            1
        );
        assert_eq!(cache.load_video_timing(&media, "decoder1"), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn roundtrip_and_invalidation() {
        let dir = std::env::temp_dir().join(format!("align-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = FingerprintCache::new(Some(dir.clone()));
        let media = dir.join("clip.wav");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&media, vec![1u8; 4096]).unwrap();
        let fps = vec![
            Fingerprint { hash: 1, frame: 2 },
            Fingerprint { hash: 3, frame: 4 },
        ];
        assert!(cache.load(&media, "automatic").is_none());
        cache.save(&fps, &media, "automatic");
        let waveform = vec![0.0, 0.25, 1.0, 0.5];
        cache.save_waveform(&waveform, &media, "automatic");
        assert_eq!(cache.load(&media, "automatic"), Some(fps.clone()));
        assert_eq!(cache.load_waveform(&media, "automatic"), Some(waveform));
        // Different variant must not collide.
        assert!(cache.load(&media, "stream-0-automatic").is_none());
        // Different engine must not collide either.
        let apple = FingerprintCache::with_backend(Some(dir.clone()), "apple1");
        assert!(apple.load(&media, "automatic").is_none());
        let st = cache.statistics();
        assert_eq!(st.file_count, 1);
        let removed = cache.prune_older_than(std::time::Duration::ZERO);
        assert_eq!(removed.file_count, 1);
        assert_eq!(cache.statistics().file_count, 0);
        assert!(cache.load_waveform(&media, "automatic").is_none());
        cache.save(&fps, &media, "automatic");
        // Content change invalidates.
        std::fs::write(&media, vec![2u8; 4096]).unwrap();
        // mtime granularity may be coarse on some FS; size change path:
        std::fs::write(&media, vec![2u8; 8192]).unwrap();
        assert!(cache.load(&media, "automatic").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_media_only_removes_matching_payloads() {
        let dir =
            std::env::temp_dir().join(format!("align-cache-current-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.wav");
        let second = dir.join("second.wav");
        let same_content = dir.join("first-copy.wav");
        std::fs::write(&first, vec![1u8; 128]).unwrap();
        std::fs::write(&second, vec![2u8; 128]).unwrap();
        std::fs::hard_link(&first, &same_content).unwrap();
        let cache = FingerprintCache::new(Some(dir.join("cache")));
        cache.save(&[Fingerprint { hash: 1, frame: 1 }], &first, "automatic");
        cache.save(
            &[Fingerprint { hash: 1, frame: 1 }],
            &same_content,
            "automatic",
        );
        cache.save(&[Fingerprint { hash: 2, frame: 2 }], &second, "automatic");

        let removed = cache.clear_media(std::slice::from_ref(&first));
        assert_eq!(removed.file_count, 1);
        assert!(cache.load(&first, "automatic").is_none());
        assert!(cache.load(&same_content, "automatic").is_some());
        assert!(cache.load(&second, "automatic").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_settings_roundtrip_and_default() {
        let dir =
            std::env::temp_dir().join(format!("align-cache-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cache.json");
        assert_eq!(CacheSettings::load_from(&path), CacheSettings::default());
        CacheSettings {
            retention_days: None,
        }
        .save_to(&path);
        assert_eq!(
            CacheSettings::load_from(&path),
            CacheSettings {
                retention_days: None
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_and_promotes_v5_payload() {
        let dir = std::env::temp_dir().join(format!("align-cache-v5-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join("clip.wav");
        let payload: Vec<u8> = (0..1_400_013).map(|i| (i % 251) as u8).collect();
        std::fs::write(&media, &payload).unwrap();
        let cache = FingerprintCache::new(Some(dir.join("cache")));
        let fingerprints = vec![Fingerprint { hash: 1, frame: 2 }];
        let legacy = LegacyPayload {
            version: LEGACY_CACHE_VERSION,
            backend_tag: "portable1".to_string(),
            fingerprints: fingerprints.clone(),
        };
        // Independent v5 key, including overlapping first and last MiB.
        let modified = std::fs::metadata(&media)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let mut hash = blake3::Hasher::new();
        hash.update(
            format!(
                "fingerprints-5\0portable1\0automatic\0{}\0{modified}",
                payload.len()
            )
            .as_bytes(),
        );
        hash.update(&payload[..1_048_576]);
        hash.update(&payload[payload.len() - 1_048_576..]);
        let legacy_path = dir.join("cache").join(format!("{}.bin", hash.finalize()));
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, bincode::serialize(&legacy).unwrap()).unwrap();

        assert_eq!(cache.load(&media, "automatic"), Some(fingerprints));
        assert!(
            cache
                .entry(&MediaSample::read(&media).unwrap(), "automatic")
                .path
                .exists()
        );
        assert!(!legacy_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
