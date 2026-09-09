//! Media inspection, decoding, rendering, and export.
//!
//! macOS defaults to AVFoundation. Portable decoding uses Symphonia for
//! supported audio and FFmpeg for other containers. Both use shared channel
//! selection and resampling. Pipeline workers are bounded and return results
//! in input order.

use std::path::Path;
use thiserror::Error;

pub mod aaf;
pub mod backend;
pub mod export;
pub mod ff;
mod ltc;
pub mod media_assets;
mod mix;
pub mod mono;
pub mod pipeline;
pub mod portable;
pub mod provider;
pub mod render;
pub mod sym;
pub mod timeline;

#[cfg(target_os = "macos")]
pub mod apple;

pub use backend::{BackendKind, MediaBackend, create, default_backend, default_backend_kind};

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("decoding cancelled")]
    Cancelled,
    #[error("no audio stream in {0}")]
    NoAudio(String),
    #[error("unsupported output")]
    UnsupportedOutput,
    #[error("decode incomplete: {0}")]
    Incomplete(String),
    #[error("invalid PCM")]
    InvalidPcm,
    #[error("ffmpeg missing: {0}")]
    FfmpegMissing(String),
    #[error("backend unavailable: {0}")]
    BackendUnavailable(&'static str),
    #[error("symphonia: {0}")]
    Symphonia(String),
    #[error("resample: {0}")]
    Resample(String),
    #[error("unseekable: {0}")]
    Unseekable(String),
    #[error("apple backend: {0}")]
    Apple(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Maximum concurrent decoder jobs.
pub const MAX_PARALLEL_DECODERS: usize = 4;
/// Fixed decode/resample block size in frames.
pub const RESAMPLE_BLOCK: usize = 32_768;

pub fn supported_extensions() -> &'static [&'static str] {
    &[
        "wav", "aif", "aiff", "m4a", "mp3", "m4v", "mov", "mp4", "mts", "mxf", "r3d",
    ]
}

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| supported_extensions().contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}
