//! Portable engine: Symphonia first, FFmpeg pipe as fallback.
//!
//! - inspect: video containers (MOV/MP4/MTS/MXF/R3D/M4V) go straight to
//!   ffprobe (headers only); audio formats try Symphonia, then ffprobe.
//! - decode: try Symphonia; anything it cannot demux falls through to the
//!   FFmpeg stdout pipe. Both tiers feed the same [`crate::mono`] chain,
//!   so numerics never depend on which tier won.

use std::path::Path;

use align_core::AudioAnalysisSource;

use crate::DecodeError;
use crate::backend::{AudioStreamProbe, BackendKind, MediaBackend, ProbeReport};

/// Extensions treated as video containers (audio + timing only, never
/// pixels — see the audio-only invariant in `backend.rs`).
pub const VIDEO_EXTENSIONS: &[&str] = &["mov", "mp4", "m4v", "mts", "mxf", "r3d"];

pub fn is_video_container(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

pub struct PortableBackend;

impl PortableBackend {
    /// Decode needs audio format headers only. Full video timing inspection
    /// belongs to import, never to each audio window or render block.
    fn audio_probe(&self, path: &Path) -> Result<ProbeReport, DecodeError> {
        if is_video_container(path) {
            crate::ff::inspect(path)
        } else {
            self.inspect(path)
        }
    }
}

impl MediaBackend for PortableBackend {
    fn decode_native(
        &self,
        path: &Path,
        stream_index: usize,
        range: Option<(f64, Option<f64>)>,
        consume: &mut dyn FnMut(crate::backend::NativeBlock) -> Result<(), DecodeError>,
    ) -> Result<f64, DecodeError> {
        use crate::backend::{NativeBlock, ProbeReport};
        let (start, duration) = range.unwrap_or((0.0, None));
        if crate::sym::can_decode(path, stream_index) {
            let probe = self.audio_probe(path).ok();
            let rate = probe
                .as_ref()
                .and_then(|p: &ProbeReport| p.audio_streams.get(stream_index))
                .map_or(48_000.0, |s| s.sample_rate);
            let limit = duration.map(|d| (d * rate).ceil() as u64 + 64);
            return crate::sym::decode_native(path, stream_index, start, limit, consume);
        }
        let probe = self.audio_probe(path)?;
        let stream = probe
            .audio_streams
            .get(stream_index)
            .ok_or_else(|| DecodeError::NoAudio(path.display().to_string()))?;
        let (sample_rate, channels) = (stream.sample_rate, stream.channels);
        if sample_rate <= 0.0 || channels == 0 {
            return Err(DecodeError::InvalidPcm);
        }
        let actual = start.max(0.0);
        let mut pipe = crate::ff::AudioPipe::spawn(
            path,
            stream_index,
            Some((actual, duration.unwrap_or(1e9))),
            channels,
        )?;
        let ok = pipe.pump(&mut |frames| {
            consume(NativeBlock {
                sample_rate,
                channels,
                frames: crate::mono::deinterleave_f32(frames, channels),
            })
        })?;
        if !ok {
            return Err(DecodeError::Incomplete(format!(
                "ffmpeg native of {}",
                path.display()
            )));
        }
        Ok(actual)
    }

    fn decode_native_i32(
        &self,
        path: &Path,
        stream_index: usize,
        range: Option<(f64, Option<f64>)>,
        consume: &mut dyn FnMut(crate::backend::NativeBlockI32) -> Result<(), DecodeError>,
    ) -> Result<f64, DecodeError> {
        use crate::backend::{NativeBlockI32, ProbeReport};
        let (start, duration) = range.unwrap_or((0.0, None));
        if crate::sym::can_decode(path, stream_index) {
            let probe = self.audio_probe(path).ok();
            let rate = probe
                .as_ref()
                .and_then(|p: &ProbeReport| p.audio_streams.get(stream_index))
                .map_or(48_000.0, |s| s.sample_rate);
            let limit = duration.map(|d| (d * rate).ceil() as u64 + 64);
            return crate::sym::decode_native_i32(path, stream_index, start, limit, consume);
        }
        // FFmpeg s32le pipe (bit-exact integer path for containers).
        let probe = self.audio_probe(path)?;
        let stream = probe
            .audio_streams
            .get(stream_index)
            .ok_or_else(|| DecodeError::NoAudio(path.display().to_string()))?;
        let (sample_rate, channels) = (stream.sample_rate, stream.channels);
        if sample_rate <= 0.0 || channels == 0 {
            return Err(DecodeError::InvalidPcm);
        }
        let actual = start.max(0.0);
        let mut pipe = crate::ff::AudioPipeI32::spawn(
            path,
            stream_index,
            Some((actual, duration.unwrap_or(1e9))),
            channels,
        )?;
        let ok = pipe.pump(&mut |frames| {
            consume(NativeBlockI32 {
                sample_rate,
                channels,
                frames: crate::mono::deinterleave_i32(frames, channels),
            })
        })?;
        if !ok {
            return Err(DecodeError::Incomplete(format!(
                "ffmpeg native of {}",
                path.display()
            )));
        }
        Ok(actual)
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Portable
    }

    fn inspect(&self, path: &Path) -> Result<ProbeReport, DecodeError> {
        if is_video_container(path) {
            // Headers + packet-timing walk + timecode tag (no frame decode).
            return crate::ff::inspect_full(path);
        }
        match crate::sym::inspect(path) {
            Ok(probe) if probe.duration_seconds.is_some() => Ok(ProbeReport {
                duration_seconds: probe.duration_seconds.unwrap_or(0.0),
                audio_streams: probe.streams,
                has_video: false,
                video: None,
            }),
            _ => crate::ff::inspect(path).or_else(|ff_err| {
                // No ffprobe binary? A Symphonia probe with streams but no
                // duration is still usable for decode (duration estimated
                // from decode). Prefer partial truth over failure.
                match crate::sym::inspect(path) {
                    Ok(probe) if !probe.streams.is_empty() => Ok(ProbeReport {
                        duration_seconds: probe.duration_seconds.unwrap_or(0.0),
                        audio_streams: probe.streams,
                        has_video: false,
                        video: None,
                    }),
                    _ => Err(ff_err),
                }
            }),
        }
    }

    fn decode_mono_8k(
        &self,
        path: &Path,
        source: AudioAnalysisSource,
        consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError> {
        if crate::sym::can_decode(path, source.stream_index()) {
            return crate::sym::decode_mono_8k(path, source, consume);
        }
        // Symphonia cannot demux this container: FFmpeg pipe with
        // the probed discrete channel count (stereo guess only when
        // even inspect failed; the pipe errors out honestly then).
        let channels = self
            .audio_probe(path)
            .ok()
            .and_then(|p| {
                p.audio_streams
                    .get(source.stream_index())
                    .map(|s| s.channels)
            })
            .unwrap_or(2);
        crate::ff::decode_mono_8k(
            path,
            source.stream_index(),
            channels,
            source.selected_channel(),
            source.mixes_channels(),
            consume,
        )
    }

    fn decode_window_16k(
        &self,
        path: &Path,
        start_seconds: f64,
        duration_seconds: f64,
        source: AudioAnalysisSource,
    ) -> Result<(f64, Vec<f32>), DecodeError> {
        match crate::sym::decode_window(path, start_seconds, duration_seconds, source) {
            Ok(win) => Ok(win),
            Err(_) => {
                let probe = self.audio_probe(path)?;
                let channels = probe
                    .audio_streams
                    .get(source.stream_index())
                    .map(|s: &AudioStreamProbe| s.channels)
                    .unwrap_or(2);
                crate::ff::decode_window(
                    path,
                    start_seconds,
                    duration_seconds,
                    source.stream_index(),
                    channels,
                    source.selected_channel(),
                    source.mixes_channels(),
                )
            }
        }
    }
}
