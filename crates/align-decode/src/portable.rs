//! Portable engine: Symphonia first, FFmpeg pipe as fallback.
//!
//! - inspect: MOV/MP4/M4V headers are read natively ([`crate::isobmff`]);
//!   other video containers (MTS/MXF/R3D) go to ffprobe (headers only);
//!   audio formats try Symphonia, then ffprobe.
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
        if is_video_container(path) && crate::isobmff::has_sbr_audio(path) {
            return crate::ff::inspect(path);
        }
        if is_video_container(path) {
            crate::isobmff::inspect_audio(path).map_or_else(|| crate::ff::inspect(path), Ok)
        } else {
            self.inspect(path)
        }
    }

    fn ffmpeg_native<T: crate::ff::PipeSample>(
        &self,
        path: &Path,
        stream_index: usize,
        start: f64,
        duration: Option<f64>,
        mut consume: impl FnMut(f64, usize, &[T]) -> Result<(), DecodeError>,
    ) -> Result<f64, DecodeError> {
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
        let mut pipe = crate::ff::PcmPipe::<T>::presented(
            path,
            stream_index,
            Some((actual, duration.unwrap_or(crate::ff::WHOLE_FILE))),
            channels,
            stream.decoder_delay,
        )?;
        let ok = pipe.pump(&mut |frames| consume(sample_rate, channels, frames))?;
        if !ok {
            return Err(DecodeError::Incomplete(format!(
                "ffmpeg native of {}",
                path.display()
            )));
        }
        Ok(pipe.actual_start(actual))
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
        use crate::backend::NativeBlock;
        let (start, duration) = range.unwrap_or((0.0, None));
        if let Ok(reader) = crate::sym::AudioReader::validated(path, stream_index) {
            let limit = duration.map(|d| (d * reader.rate()).ceil() as u64 + 64);
            return reader.decode_native(start, limit, consume);
        }
        self.ffmpeg_native(
            path,
            stream_index,
            start,
            duration,
            |sample_rate, channels, frames| {
                consume(NativeBlock {
                    sample_rate,
                    channels,
                    frames: crate::mono::deinterleave_f32(frames, channels),
                })
            },
        )
    }

    fn decode_native_i32(
        &self,
        path: &Path,
        stream_index: usize,
        range: Option<(f64, Option<f64>)>,
        consume: &mut dyn FnMut(crate::backend::NativeBlockI32) -> Result<(), DecodeError>,
    ) -> Result<f64, DecodeError> {
        use crate::backend::NativeBlockI32;
        let (start, duration) = range.unwrap_or((0.0, None));
        if let Ok(reader) = crate::sym::AudioReader::validated(path, stream_index) {
            let limit = duration.map(|d| (d * reader.rate()).ceil() as u64 + 64);
            return reader.decode_native_i32(start, limit, consume);
        }
        // FFmpeg s32le pipe (bit-exact integer path for containers).
        self.ffmpeg_native(
            path,
            stream_index,
            start,
            duration,
            |sample_rate, channels, frames| {
                consume(NativeBlockI32 {
                    sample_rate,
                    channels,
                    frames: crate::mono::deinterleave_i32(frames, channels),
                })
            },
        )
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Portable
    }

    fn inspect(&self, path: &Path) -> Result<ProbeReport, DecodeError> {
        // The sample entry can describe the AAC core alone. Ask the full
        // decoder for the output channel layout and rate (also used by LTC).
        if crate::isobmff::has_sbr_audio(path) {
            let mut probe = crate::ff::inspect(path)?;
            if is_video_container(path) {
                if let Some(header) = crate::isobmff::inspect(path) {
                    probe.video = header.video;
                } else {
                    return crate::ff::inspect_full(path);
                }
            }
            return Ok(probe);
        }
        if is_video_container(path) {
            // QuickTime/MP4 headers natively; other containers through
            // ffprobe (headers + packet-timing walk + timecode tag).
            return crate::isobmff::inspect(path).map_or_else(|| crate::ff::inspect_full(path), Ok);
        }
        match crate::sym::inspect(path) {
            Ok(probe) if probe.duration_seconds.is_some() => Ok(ProbeReport {
                duration_seconds: probe.duration_seconds.unwrap_or(0.0),
                audio_streams: probe.streams,
                has_video: false,
                video: None,
            }),
            // RF64/BW64 and other WAVE layouts Symphonia rejects.
            _ => match crate::pcm::PcmTrack::open(path, 0) {
                Some(track) => Ok(track.probe()),
                None => crate::ff::inspect(path).or_else(|ff_err| {
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
            },
        }
    }

    fn fingerprint_tracks(
        &self,
        path: &Path,
        sources: &[AudioAnalysisSource],
        consume: crate::backend::TrackConsumer<'_>,
    ) -> Option<Result<(), DecodeError>> {
        crate::pcm::fingerprint_tracks(path, sources, consume)
    }

    fn decode_mono_8k(
        &self,
        path: &Path,
        source: AudioAnalysisSource,
        consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError> {
        if source == AudioAnalysisSource::AllMixed {
            let stream_count = self.inspect(path)?.audio_streams.len();
            return crate::mix::decode_all_streams_mono_8k(self, path, stream_count, consume);
        }
        if let Ok(reader) = crate::sym::AudioReader::validated(path, source.stream_index()) {
            return reader.decode_mono_8k(source, consume);
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
        self.window(None, path, start_seconds, duration_seconds, source)
    }

    fn window_decoder(&self) -> crate::backend::WindowDecoder<'_> {
        let readers = crate::sym::WindowReaders::default();
        Box::new(move |path, start, duration, source| {
            self.window(Some(&readers), path, start, duration, source)
        })
    }
}

impl PortableBackend {
    fn window(
        &self,
        readers: Option<&crate::sym::WindowReaders>,
        path: &Path,
        start_seconds: f64,
        duration_seconds: f64,
        source: AudioAnalysisSource,
    ) -> Result<(f64, Vec<f32>), DecodeError> {
        if source == AudioAnalysisSource::AllMixed {
            let stream_count = self.inspect(path)?.audio_streams.len();
            return crate::mix::decode_all_streams_window_16k(
                self,
                path,
                stream_count,
                start_seconds,
                duration_seconds,
            );
        }
        match crate::sym::window_with_readers(
            readers,
            path,
            start_seconds,
            duration_seconds,
            source,
        ) {
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
