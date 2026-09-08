//! Content-addressed fingerprint cache.
//! Port of Sources/AlignCore/FingerprintCache.swift.
//!
//! Differences (deliberate, documented):
//! - format: bincode instead of binary plist (plist has no stable Rust
//!   writer; bincode is smaller and faster on all 3 OSes);
//! - hash: BLAKE3 over (version, backend, variant, normalized path, size,
//!   mtime, head+tail 1 MiB) instead of SHA256 (same security margin for
//!   cache keys, ~8x faster, less CPU wake on laptops);
//! - version bumped so Swift plist and older Rust entries are never misread.
//! - location: OS cache dir via `dirs` (`~/Library/Caches`,
//!   `%LOCALAPPDATA%`, `~/.cache`), same semantics as Swift.

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
            if p.extension().and_then(|s| s.to_str()) != Some("bin") {
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
            if path.extension().and_then(|value| value.to_str()) != Some("bin") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Some(payload) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| bincode::deserialize::<Payload>(&bytes).ok())
            else {
                continue;
            };
            if payload.version == CACHE_VERSION
                && media.contains(&normalized_path(&payload.media_path))
                && std::fs::remove_file(path).is_ok()
            {
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
            if path.extension().and_then(|value| value.to_str()) != Some("bin") {
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

    pub fn load(&self, media: &Path, variant: &str) -> Option<Vec<Fingerprint>> {
        if let Ok(url) = self.cache_path(media, variant)
            && let Ok(bytes) = std::fs::read(url)
            && let Ok(payload) = bincode::deserialize::<Payload>(&bytes)
            && payload.version == CACHE_VERSION
            && payload.backend_tag == self.backend_tag
        {
            return Some(payload.fingerprints);
        }

        // Promote v5 in place after the first successful read. The legacy
        // key still validates backend, variant, metadata, and sampled bytes.
        let legacy_url = self.legacy_cache_path(media, variant).ok()?;
        let bytes = std::fs::read(&legacy_url).ok()?;
        let payload: LegacyPayload = bincode::deserialize(&bytes).ok()?;
        if payload.version != LEGACY_CACHE_VERSION || payload.backend_tag != self.backend_tag {
            return None;
        }
        self.save(&payload.fingerprints, media, variant);
        if self
            .cache_path(media, variant)
            .is_ok_and(|path| path.exists())
        {
            let _ = std::fs::remove_file(legacy_url);
        }
        Some(payload.fingerprints)
    }

    pub fn save(&self, fingerprints: &[Fingerprint], media: &Path, variant: &str) {
        let Ok(url) = self.cache_path(media, variant) else {
            return;
        };
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        // Atomic write: tmp + rename, so a crash never leaves a half file
        // (same guarantee as Swift `.atomic`).
        let payload = Payload {
            version: CACHE_VERSION,
            backend_tag: self.backend_tag.clone(),
            media_path: normalized_path(media),
            fingerprints: fingerprints.to_vec(),
        };
        let Ok(bytes) = bincode::serialize(&payload) else {
            return;
        };
        let tmp = url.with_extension(format!("tmp-{}", std::process::id()));
        if std::fs::write(&tmp, &bytes).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if std::fs::rename(&tmp, &url).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Load the compact amplitude envelope stored beside a fingerprint entry.
    pub fn load_waveform(&self, media: &Path, variant: &str) -> Option<Vec<f32>> {
        let url = self
            .cache_path(media, variant)
            .ok()?
            .with_extension("waveform");
        let waveform: Vec<f32> = bincode::deserialize(&std::fs::read(url).ok()?).ok()?;
        (!waveform.is_empty()
            && waveform.len() <= 4_096
            && waveform
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value)))
        .then_some(waveform)
    }

    /// Store a session waveform without changing the fingerprint payload.
    pub fn save_waveform(&self, waveform: &[f32], media: &Path, variant: &str) {
        if waveform.is_empty()
            || waveform.len() > 4_096
            || !waveform
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
        {
            return;
        }
        let Ok(url) = self
            .cache_path(media, variant)
            .map(|path| path.with_extension("waveform"))
        else {
            return;
        };
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let Ok(bytes) = bincode::serialize(waveform) else {
            return;
        };
        let tmp = url.with_extension(format!("waveform-tmp-{}", std::process::id()));
        if std::fs::write(&tmp, bytes).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if std::fs::rename(&tmp, &url).is_err() {
            let _ = std::fs::remove_file(tmp);
        }
    }

    fn cache_path(&self, media: &Path, variant: &str) -> std::io::Result<PathBuf> {
        self.cache_path_for_version(media, variant, CACHE_VERSION, true)
    }

    fn legacy_cache_path(&self, media: &Path, variant: &str) -> std::io::Result<PathBuf> {
        self.cache_path_for_version(media, variant, LEGACY_CACHE_VERSION, false)
    }

    fn cache_path_for_version(
        &self,
        media: &Path,
        variant: &str,
        version: u32,
        include_path: bool,
    ) -> std::io::Result<PathBuf> {
        let meta = std::fs::metadata(media)?;
        let size = meta.len();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let mut hasher = blake3::Hasher::new();
        let prefix = if include_path {
            format!(
                "fingerprints-{version}\0{}\0{variant}\0{}\0{size}\0{mtime}",
                self.backend_tag,
                normalized_path(media).to_string_lossy()
            )
        } else {
            format!(
                "fingerprints-{version}\0{}\0{variant}\0{size}\0{mtime}",
                self.backend_tag
            )
        };
        hasher.update(prefix.as_bytes());
        // Head + tail sampling: content change invalidates without hashing
        // 119 GB corpora in full.
        if let Ok(f) = std::fs::File::open(media) {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = f;
            let mut buf = vec![0u8; HEAD_TAIL_BYTES as usize];
            if let Ok(n) = f.read(&mut buf) {
                hasher.update(&buf[..n]);
            }
            if size > HEAD_TAIL_BYTES
                && f.seek(SeekFrom::End(-(HEAD_TAIL_BYTES as i64))).is_ok()
                && let Ok(n) = f.read(&mut buf)
            {
                hasher.update(&buf[..n]);
            }
        }
        Ok(self.dir.join(format!("{}.bin", hasher.finalize().to_hex())))
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
        std::fs::write(&media, vec![1u8; 4096]).unwrap();
        let cache = FingerprintCache::new(Some(dir.join("cache")));
        let fingerprints = vec![Fingerprint { hash: 1, frame: 2 }];
        let legacy = LegacyPayload {
            version: LEGACY_CACHE_VERSION,
            backend_tag: "portable1".to_string(),
            fingerprints: fingerprints.clone(),
        };
        let legacy_path = cache.legacy_cache_path(&media, "automatic").unwrap();
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, bincode::serialize(&legacy).unwrap()).unwrap();

        assert_eq!(cache.load(&media, "automatic"), Some(fingerprints));
        assert!(cache.cache_path(&media, "automatic").unwrap().exists());
        assert!(!legacy_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
