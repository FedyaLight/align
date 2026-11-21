//! Adapter from [`crate::MediaBackend`] to [`WindowProvider`].
//!
//! The refine stage addresses clips by [`ClipId`]; this maps ids to media
//! paths and uses the requested audio source, preserving
//! the 16 kHz contract the fine matcher relies on.

use std::collections::HashMap;
use std::path::PathBuf;

use align_core::{AudioAnalysisSource, AudioWindow, ClipId, FINE_SAMPLE_RATE, WindowProvider};

use crate::backend::WindowDecoder;

pub struct BackendWindowProvider<'a> {
    pub worker_cap: usize,
    pub decode: WindowDecoder<'a>,
    pub clips: HashMap<ClipId, PathBuf>,
}

impl<'a> WindowProvider for BackendWindowProvider<'a> {
    fn worker_cap(&self) -> usize {
        self.worker_cap
    }

    fn window(
        &self,
        clip: &ClipId,
        start: f64,
        duration: f64,
        source: AudioAnalysisSource,
    ) -> Option<AudioWindow> {
        let path = self.clips.get(clip)?;
        let (actual_start, samples) = (self.decode)(path, start, duration, source).ok()?;
        Some(AudioWindow {
            start: actual_start,
            sample_rate: FINE_SAMPLE_RATE,
            samples,
        })
    }
}
