//! Platform backends: native Apple media stack on macOS, portable engine
//! everywhere else. Algorithms (matcher, graph, export), DSP (fingerprint,
//! GCC-PHAT) and UI (GPUI) stay shared — only media inspect/decode is
//! platform-specific.
//!
//! Why the split is exactly here:
//! - Decode/inspect is where the OS gives free hardware: VideoToolbox
//!   decoders, AVAudioConverter quality/speed, AVSampleCursor VFR walk,
//!   Sony/timecode metadata paths, battery-friendly I/O scheduling.
//! - FFT/DSP is NOT worth splitting: a 1024-pt real FFT is ~microseconds,
//!   the fingerprint stage runs ~16 FFT/s per file — decode dominates by
//!   orders of magnitude. Shared DSP also means bit-identical fingerprints,
//!   one cache format family and comparable result SHAs across OSes.
//!   (A vDSP FFT backend can be added later behind the same trait if
//!   profiling ever shows otherwise — the seam is reserved.)
//!
//! Selection: [`default_backend_kind`] returns AppleNative on macOS unless
//! `ALIGN_BACKEND=portable` (aka `pure-rust`) forces the portable engine —
//! the escape hatch for CI determinism checks and debugging. Off macOS the
//! portable engine is the only option; requesting native falls back with a
//! warning.
//!
//! ## Apple backend implementation (Rust + objc2, no Swift)
//!
//! The Apple backend re-implements `AudioDecoder.swift` / `inspect` /
//! `VideoTimingInspector` directly in Rust via `objc2-av-foundation`
//! without a Swift runtime or a second build toolchain.
//!
//! Swift -> objc2 call map (same contracts: 4-reader gate, 32k blocks,
//! 0.9-hysteresis adaptive mono, `.max` converter quality):
//! - `AVURLAsset(url:)` + `loadTracks(withMediaType:)` -> identical calls
//! - `AVAssetReader` + `AVAssetReaderTrackOutput` (LinearPCM f32) -> identical
//! - `MonoSampleRateConverter` (AVAudioConverter) -> identical, same quality
//! - `VideoTimingInspector` (AVSampleCursor full walk) -> identical
//! - channel/stream selection + hysteresis -> shared (`align_core::model`)
//! - Sony NonRealTimeMeta / BWF bext / R3D proxy notes -> shared pure file IO
//! - ClipID SHA256 -> shared `sha2` (identical digest, stable result JSON)
//!
//! Deliberately NOT kept as a Swift dylib + FFI: that would mean two
//! toolchains (Xcode + cargo), the Swift runtime inside the process, and a
//! packaging split (the portable backend is needed on Win/Linux anyway).
//! Pure Rust over native C/ObjC APIs is one toolchain and thinner.

use align_core::{AudioAnalysisSource, MediaTime, SourceTimecode, VideoFrameRateMode};
use std::path::Path;

use crate::DecodeError;

// ------------------------------------------------------------ audio-only
//
// INVARIANT: video tracks are never opened, video frames are never decoded,
// pixel buffers are never allocated. Containers (MOV/MP4/MTS/MXF/R3D) are
// opened for their *audio streams and timing metadata only* — exactly like
// Swift, where `AVAssetReaderTrackOutput` is attached to audio tracks and
// `VideoTimingInspector` walks `AVSampleCursor` timestamps without reading
// sample data.
//
// The trait enforces this structurally: there is no method returning video
// samples, only mono `f32` audio (`decode_*`) and scalar metadata
// (`ProbeReport`). Memory budgets (resident, worst case per job):
pub const ANALYSIS_SAMPLE_RATE_HZ: u32 = 8_000;
/// 8 kHz mono f32 = 32 KiB per second of audio, streamed in blocks.
pub const ANALYSIS_BYTES_PER_SEC: usize = 32_000;
pub const FINE_WINDOW_SAMPLE_RATE_HZ: u32 = 16_000;
/// GCC-PHAT caps at 131072 samples: 512 KiB per scratch buffer.
pub const MAX_FINE_WINDOW_SAMPLES: usize = 131_072;
// Streaming resample block: 32768 × 4 B = 128 KiB. Full files are never
// resident however long the take (a 3-hour recorder pass streams through
// the same 128 KiB + FFT scratch as a 10-second clip).

// ------------------------------------------------------------ selection

/// Which media engine to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// Symphonia + FFmpeg sidecar + rubato. Everywhere.
    Portable,
    /// AVFoundation + AVAudioConverter via objc2. macOS only.
    AppleNative,
}

impl BackendKind {
    /// Cache namespace. Bumped independently per engine so an Apple-decoded
    /// fingerprint never collides with a portable-decoded one (converter
    /// numerics differ in the last ulp; peak picking is robust but the cache
    /// must not assume it).
    pub fn cache_tag(&self) -> &'static str {
        match self {
            BackendKind::Portable => "portable2",
            BackendKind::AppleNative => "apple2",
        }
    }
}

/// Pure parser behind [`default_backend_kind`], kept separate so it is
/// unit-testable without touching process-global env vars.
pub fn kind_from_override(value: &str) -> Option<BackendKind> {
    match value.trim().to_lowercase().as_str() {
        "" | "auto" | "native" => None,
        "portable" | "pure-rust" | "pure_rust" => Some(BackendKind::Portable),
        "apple" | "apple-native" | "native-apple" => Some(BackendKind::AppleNative),
        _ => None,
    }
}

pub fn default_backend_kind() -> BackendKind {
    if cfg!(target_os = "macos") {
        let forced = std::env::var("ALIGN_BACKEND")
            .ok()
            .and_then(|v| kind_from_override(&v));
        match forced {
            Some(kind) => kind,
            None => BackendKind::AppleNative,
        }
    } else {
        BackendKind::Portable
    }
}

// ------------------------------------------------------------ traits

/// Minimal probe: enough to build `align_core::Clip` (duration, audio
/// layouts, video presence). Timecode/Sony/BWF enrichment stays shared.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioStreamProbe {
    pub sample_rate: f64,
    pub channels: usize,
    pub bit_depth: Option<u32>,
    pub is_float: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProbeReport {
    pub duration_seconds: f64,
    pub audio_streams: Vec<AudioStreamProbe>,
    pub has_video: bool,
    /// Present when the backend could read video timing without decoding
    /// frames (ffprobe packet walk / AVSampleCursor walk).
    pub video: Option<VideoProbe>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoProbe {
    pub width: u32,
    pub height: u32,
    pub frame_duration: Option<MediaTime>,
    pub mode: VideoFrameRateMode,
    pub source_timecode: Option<SourceTimecode>,
}

/// The single seam between shared algorithms and OS media stacks.
/// Both backends must honour the same contracts (mirrors Swift):
/// 8 kHz mono stream / 16 kHz windows, adaptive-mono hysteresis 0.9,
/// cooperative cancellation via the caller's `consume` returning Err.
pub trait MediaBackend: Send + Sync {
    fn kind(&self) -> BackendKind;

    fn inspect(&self, path: &Path) -> Result<ProbeReport, DecodeError>;

    fn decode_mono_8k(
        &self,
        path: &Path,
        source: AudioAnalysisSource,
        consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError>;

    fn decode_window_16k(
        &self,
        path: &Path,
        start_seconds: f64,
        duration_seconds: f64,
        source: AudioAnalysisSource,
    ) -> Result<(f64, Vec<f32>), DecodeError>;

    /// Full-rate discrete-channel streaming (render path): planar `f32`
    /// blocks at the source rate. `range`: optional (start, duration);
    /// callbacks begin at the nearest requested source sample, with seek lead
    /// discarded by the backend. Returns that selected stream time. Never
    /// loads whole files: blocks stream through the caller.
    fn decode_native(
        &self,
        path: &Path,
        stream_index: usize,
        range: Option<(f64, Option<f64>)>,
        consume: &mut dyn FnMut(NativeBlock) -> Result<(), DecodeError>,
    ) -> Result<f64, DecodeError>;

    /// Same, but 32-bit integer samples for bit-exact stems of 32-bit-int
    /// sources (other formats convert through f32, like Swift's int32
    /// reader path).
    fn decode_native_i32(
        &self,
        path: &Path,
        stream_index: usize,
        range: Option<(f64, Option<f64>)>,
        consume: &mut dyn FnMut(NativeBlockI32) -> Result<(), DecodeError>,
    ) -> Result<f64, DecodeError>;
}

/// One planar block of native-rate audio.
#[derive(Clone, Debug)]
pub struct NativeBlock {
    pub sample_rate: f64,
    pub channels: usize,
    /// Planar channels (`frames[ch][frame]`).
    pub frames: Vec<Vec<f32>>,
}

#[derive(Clone, Debug)]
pub struct NativeBlockI32 {
    pub sample_rate: f64,
    pub channels: usize,
    pub frames: Vec<Vec<i32>>,
}

// ------------------------------------------------------------ re-exports
// Concrete engines live in their own modules (`portable`, `apple`); the
// trait and selection stay here so `align-core` never names a backend.

// ------------------------------------------------------------ factory

/// Build a backend. Requesting AppleNative off macOS falls back to portable
/// with a warning (documented, never silent-panic).
pub fn create(kind: BackendKind) -> Box<dyn MediaBackend> {
    match kind {
        BackendKind::Portable => Box::new(crate::portable::PortableBackend),
        #[cfg(target_os = "macos")]
        BackendKind::AppleNative => Box::new(crate::apple::AppleNativeBackend),
        #[cfg(not(target_os = "macos"))]
        BackendKind::AppleNative => {
            eprintln!("warning: AppleNative backend requested off macOS; using portable");
            Box::new(crate::portable::PortableBackend)
        }
    }
}

/// Convenience: [`default_backend_kind`] + [`create`].
pub fn default_backend() -> Box<dyn MediaBackend> {
    create(default_backend_kind())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_parser() {
        assert_eq!(kind_from_override(""), None);
        assert_eq!(kind_from_override("auto"), None);
        assert_eq!(kind_from_override("native"), None);
        assert_eq!(kind_from_override("portable"), Some(BackendKind::Portable));
        assert_eq!(kind_from_override("pure-rust"), Some(BackendKind::Portable));
        assert_eq!(kind_from_override("PURE_RUST"), Some(BackendKind::Portable));
        assert_eq!(kind_from_override("apple"), Some(BackendKind::AppleNative));
        assert_eq!(kind_from_override("bogus"), None);
    }

    #[test]
    fn cache_tags_differ_per_engine() {
        assert_ne!(
            BackendKind::Portable.cache_tag(),
            BackendKind::AppleNative.cache_tag()
        );
    }

    #[test]
    fn factory_matches_kind() {
        assert_eq!(create(BackendKind::Portable).kind(), BackendKind::Portable);
        // On macOS returns AppleNative, elsewhere falls back to portable.
        let native = create(BackendKind::AppleNative);
        if cfg!(target_os = "macos") {
            assert_eq!(native.kind(), BackendKind::AppleNative);
        } else {
            assert_eq!(native.kind(), BackendKind::Portable);
        }
    }

    #[test]
    fn audio_only_memory_budgets() {
        // Compile-time enforced below (see const asserts); here we pin the
        // block size that bounds resident decode memory per job.
        assert_eq!(crate::RESAMPLE_BLOCK, 32_768);
        // The trait surface exposes no video-sample API by construction:
        // only mono f32 audio and scalar probe metadata (see docs above).
        fn assert_audio_only<T: MediaBackend>() {}
        assert_audio_only::<crate::portable::PortableBackend>();
    }

    // Streaming block + biggest FFT scratch stay far below 1 MiB per job;
    // a 3-hour take streams through the same resident set as a jingle.
    const _: () = {
        assert!(crate::RESAMPLE_BLOCK * 4 == 128 * 1024);
        assert!(MAX_FINE_WINDOW_SAMPLES * 4 <= 512 * 1024 + 1024);
        assert!(ANALYSIS_BYTES_PER_SEC == 8_000 * 4);
    };
}
