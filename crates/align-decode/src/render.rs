//! Streaming drift correction, channel stems, and placement padding.
//!
//! The source is decoded once at native rate. Per-segment resampling maps
//! source ranges to their validated timeline durations; adjacent segments
//! meet without a crossfade. Buffers are bounded by decode blocks and
//! resampler scratch. Broadcast Wave metadata is handled by `align_core::wav`.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

use align_core::drift::segments;
use align_core::model::file_name;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use crate::DecodeError;
use crate::backend::{MediaBackend, NativeBlock, NativeBlockI32};

#[derive(Clone, Debug, PartialEq)]
pub enum RenderError {
    Cancelled,
    NoAudio(String),
    UnknownFormat(String),
    CannotRead,
    CannotWrite,
    InvalidMapping,
    PlacementPad(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "Audio rendering cancelled."),
            Self::NoAudio(name) => write!(f, "No audio track was found in {name}."),
            Self::UnknownFormat(name) => write!(f, "Cannot read the audio format of {name}."),
            Self::CannotRead => write!(f, "Cannot decode audio for drift correction."),
            Self::CannotWrite => write!(f, "Cannot write drift-corrected audio."),
            Self::InvalidMapping => write!(f, "The drift-correction time map is invalid."),
            Self::PlacementPad(message) => {
                write!(f, "Cannot render placement-pad audio: {message}.")
            }
        }
    }
}

impl std::error::Error for RenderError {}

impl From<DecodeError> for RenderError {
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Cancelled => Self::Cancelled,
            DecodeError::NoAudio(name) => Self::NoAudio(name),
            DecodeError::InvalidPcm | DecodeError::UnsupportedOutput => {
                Self::UnknownFormat(String::new())
            }
            _ => Self::CannotRead,
        }
    }
}

fn decode_render_error(error: RenderError) -> DecodeError {
    match error {
        RenderError::Cancelled => DecodeError::Cancelled,
        other => DecodeError::Incomplete(other.to_string()),
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), RenderError> {
    if cancel.load(Ordering::Relaxed) {
        Err(RenderError::Cancelled)
    } else {
        Ok(())
    }
}

/// A camera's last audio sample may precede its last video frame. After a
/// successful decode, fill only that sub-frame gap at the container's end.
/// Never conceal truncated audio-only files or missing audio inside an edit.
fn finish_video_tail(
    writer: &mut WavWriter,
    probe: &crate::backend::ProbeReport,
    end_seconds: f64,
    remaining: u64,
    sample_rate: f64,
    channels: usize,
) -> Result<(), RenderError> {
    if remaining == 0 {
        return Ok(());
    }
    let frame_duration = probe
        .video
        .as_ref()
        .and_then(|v| v.frame_duration)
        .map_or(0.001, |t| t.as_seconds());
    if !probe.has_video
        || (end_seconds - probe.duration_seconds).abs() > 1.0 / sample_rate
        || remaining as f64 > (frame_duration * sample_rate).ceil()
    {
        return Err(RenderError::CannotRead);
    }
    writer.write_silence(channels, remaining as usize)
}

// ------------------------------------------------------------ wav writer

/// Minimal streaming WAV writer (f32le / s32le interleaved). Headers are
/// patched on finalize — no full-file buffering, no hound dependency in
/// the hot path.
struct WavWriter {
    file: std::fs::File,
    frames_written: u64,
    bytes: Vec<u8>,
}

impl WavWriter {
    fn create(
        path: &Path,
        sample_rate: u32,
        channels: usize,
        int32: bool,
    ) -> Result<Self, RenderError> {
        let mut file = std::fs::File::create(path).map_err(|_| RenderError::CannotWrite)?;
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&[0u8; 4]);
        header.extend_from_slice(b"WAVE");
        // Reserve ds64 space so finalization can promote a long recording
        // to RF64 without moving or buffering its audio payload.
        header.extend_from_slice(b"JUNK");
        header.extend_from_slice(&28u32.to_le_bytes());
        header.extend_from_slice(&[0u8; 28]);
        header.extend_from_slice(b"fmt ");
        header.extend_from_slice(&16u32.to_le_bytes());
        // tag 3 = FLOAT, 1 = PCM.
        header.extend_from_slice(&(if int32 { 1u16 } else { 3u16 }).to_le_bytes());
        header.extend_from_slice(&(channels as u16).to_le_bytes());
        header.extend_from_slice(&sample_rate.to_le_bytes());
        let block_align = (channels * 4) as u16;
        header.extend_from_slice(&(sample_rate * block_align as u32).to_le_bytes());
        header.extend_from_slice(&block_align.to_le_bytes());
        header.extend_from_slice(&32u16.to_le_bytes());
        header.extend_from_slice(b"data");
        header.extend_from_slice(&[0u8; 4]);
        file.write_all(&header)
            .map_err(|_| RenderError::CannotWrite)?;
        Ok(Self {
            file,
            frames_written: 0,
            bytes: Vec::new(),
        })
    }

    fn write_pcm<T: Copy>(
        &mut self,
        channels: usize,
        planes: &[impl AsRef<[T]>],
        encode: fn(T) -> [u8; 4],
    ) -> Result<(), RenderError> {
        let n = planes.first().map_or(0, |p| p.as_ref().len());
        if planes.len() < channels || planes.iter().take(channels).any(|p| p.as_ref().len() < n) {
            return Err(RenderError::CannotRead);
        }
        self.bytes.clear();
        self.bytes.reserve(n * channels * 4);
        for i in 0..n {
            for plane in planes.iter().take(channels) {
                self.bytes.extend_from_slice(&encode(plane.as_ref()[i]));
            }
        }
        self.flush_frames(n)
    }

    fn write_silence(&mut self, channels: usize, frames: usize) -> Result<(), RenderError> {
        self.bytes.clear();
        self.bytes.resize(frames * channels * 4, 0);
        self.flush_frames(frames)
    }

    fn flush_frames(&mut self, frames: usize) -> Result<(), RenderError> {
        self.file
            .write_all(&self.bytes)
            .map_err(|_| RenderError::CannotWrite)?;
        self.frames_written += frames as u64;
        Ok(())
    }

    fn finalize(mut self) -> Result<(), RenderError> {
        let len = self
            .file
            .stream_position()
            .map_err(|_| RenderError::CannotWrite)?;
        // 12-byte RIFF header, 36-byte reserved ds64, 24-byte fmt,
        // and 8-byte data header. Audio starts at byte 80.
        let data_size = len.checked_sub(80).ok_or(RenderError::CannotWrite)?;
        let riff_size = len - 8;
        let rf64 = riff_size >= u64::from(u32::MAX);
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| RenderError::CannotWrite)?;
        self.file
            .write_all(if rf64 { b"RF64" } else { b"RIFF" })
            .map_err(|_| RenderError::CannotWrite)?;
        self.file
            .write_all(&(if rf64 { u32::MAX } else { riff_size as u32 }).to_le_bytes())
            .map_err(|_| RenderError::CannotWrite)?;
        if rf64 {
            self.file
                .seek(SeekFrom::Start(12))
                .map_err(|_| RenderError::CannotWrite)?;
            self.file
                .write_all(b"ds64")
                .map_err(|_| RenderError::CannotWrite)?;
            self.file
                .seek(SeekFrom::Start(20))
                .map_err(|_| RenderError::CannotWrite)?;
            for value in [riff_size, data_size, self.frames_written] {
                self.file
                    .write_all(&value.to_le_bytes())
                    .map_err(|_| RenderError::CannotWrite)?;
            }
            // The reserved final u32 remains zero: no ds64 table entries.
        }
        self.file
            .seek(SeekFrom::Start(76))
            .map_err(|_| RenderError::CannotWrite)?;
        self.file
            .write_all(&(if rf64 { u32::MAX } else { data_size as u32 }).to_le_bytes())
            .map_err(|_| RenderError::CannotWrite)?;
        self.file.flush().map_err(|_| RenderError::CannotWrite)?;
        Ok(())
    }
}

// ------------------------------------------------------------ drift render

/// Rendered segment: source range stretched to the island range.
struct Segment {
    resampler: Vec<SincFixedIn<f32>>,
    pending: Vec<Vec<f32>>,
    skip: usize,
    emitted: usize,
    expected: usize,
    flush_rounds: usize,
}

impl Segment {
    fn new(
        _source_start: f64,
        source_duration: f64,
        target_duration: f64,
        sample_rate: f64,
        channels: usize,
    ) -> Result<Self, RenderError> {
        let ratio = target_duration / source_duration;
        if !(ratio.is_finite() && ratio > 0.0) {
            return Err(RenderError::InvalidMapping);
        }
        // Arbitrary clock ratios must not determine the FFT size or be
        // rounded to whole ppm: that can accumulate milliseconds on long takes.
        // A fixed sinc kernel bounds work and latency independently of ratio.
        let mut resampler = Vec::with_capacity(channels);
        for _ in 0..channels {
            resampler.push(
                SincFixedIn::new(
                    ratio,
                    1.0,
                    SincInterpolationParameters {
                        sinc_len: 128,
                        f_cutoff: 0.95,
                        oversampling_factor: 256,
                        interpolation: SincInterpolationType::Linear,
                        window: WindowFunction::BlackmanHarris2,
                    },
                    4096,
                    1,
                )
                .map_err(|_| RenderError::CannotWrite)?,
            );
        }
        // rubato 0.15 SincFixedIn starts its interpolation index at -kernel/2:
        // the first returned sample is already at source time zero. Its
        // output_delay() describes buffered lookahead, not leading samples
        // to discard (unlike FftFixedIn). Analytic phase tests cover this.
        let skip = 0;
        Ok(Self {
            resampler,
            pending: vec![Vec::new(); channels],
            skip,
            flush_rounds: ((4.0 * skip as f64 / ratio).ceil() as usize).div_ceil(4096) + 4,
            emitted: 0,
            expected: (target_duration * sample_rate).round() as usize,
        })
    }

    /// Feed source-rate planar frames; returns time-true output planes.
    /// The shared drop (group delay) and cap (exact length) apply
    /// UNIFORMLY to every channel (see `uniform_slice`) — per-channel
    /// shared counters would starve later channels and ragged planes
    /// would corrupt the interleaved writer.
    fn push(&mut self, planes: Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>, RenderError> {
        let n = planes.len();
        let mut out: Vec<Vec<f32>> = (0..n).map(|_| Vec::new()).collect();
        for ch in 0..n {
            self.pending[ch].extend_from_slice(&planes[ch]);
            while self.pending[ch].len() >= 4096 {
                let chunk: Vec<f32> = self.pending[ch].drain(..4096).collect();
                let produced: Vec<Vec<f32>> = self.resampler[ch]
                    .process(&[chunk], None)
                    .map_err(|_| RenderError::CannotWrite)?;
                out[ch].extend_from_slice(&produced[0]);
            }
        }
        self.uniform_slice(&mut out);
        Ok(out)
    }

    /// Apply the shared drop+cap to equally-sized channel planes.
    fn uniform_slice(&mut self, out: &mut [Vec<f32>]) {
        if out.is_empty() {
            return;
        }
        debug_assert!(out.iter().all(|p| p.len() == out[0].len()));
        let drop = self.skip.min(out[0].len());
        self.skip -= drop;
        let allow = self.expected.saturating_sub(self.emitted);
        let take = (out[0].len() - drop).min(allow);
        for plane in out.iter_mut() {
            plane.drain(..drop);
            plane.truncate(take);
        }
        self.emitted += take;
    }

    fn finish(&mut self, cancel: &AtomicBool) -> Result<Vec<Vec<f32>>, RenderError> {
        check_cancel(cancel)?;
        // Remainder through process_partial, then bounded tail flush.
        let mut out: Vec<Vec<f32>> = (0..self.resampler.len()).map(|_| Vec::new()).collect();
        for ((pending, rs), dst) in self
            .pending
            .iter_mut()
            .zip(self.resampler.iter_mut())
            .zip(out.iter_mut())
        {
            if pending.is_empty() {
                continue;
            }
            let rem = std::mem::take(pending);
            let produced: Vec<Vec<f32>> = rs
                .process_partial(Some(&[rem]), None)
                .map_err(|_| RenderError::CannotWrite)?;
            dst.extend_from_slice(&produced[0]);
        }
        self.uniform_slice(&mut out);
        // Flush the bounded filter delay and any partial input block.
        for _ in 0..self.flush_rounds {
            check_cancel(cancel)?;
            if self.emitted >= self.expected {
                break;
            }
            let mut round: Vec<Vec<f32>> = (0..self.resampler.len()).map(|_| Vec::new()).collect();
            for (ch, rs) in self.resampler.iter_mut().enumerate() {
                let produced: Vec<Vec<f32>> = rs
                    .process_partial::<&[f32]>(None, None)
                    .map_err(|_| RenderError::CannotWrite)?;
                round[ch].extend_from_slice(&produced[0]);
            }
            self.uniform_slice(&mut round);
            for (ch, r) in round.into_iter().enumerate() {
                out[ch].extend_from_slice(&r);
            }
        }
        if self.emitted != self.expected {
            return Err(RenderError::CannotWrite);
        }
        Ok(out)
    }
}

/// Drift-corrected sidecar render (32-bit float, native rate/channels).
/// `points`: (source_seconds, island_seconds) mapping, ≥ 2 entries.
pub fn render_drift(
    backend: &dyn MediaBackend,
    source: &Path,
    destination: &Path,
    points: &[(f64, f64)],
) -> Result<(), RenderError> {
    render_drift_cancellable(backend, source, destination, points, &NEVER_CANCELLED)
}

pub fn render_drift_cancellable(
    backend: &dyn MediaBackend,
    source: &Path,
    destination: &Path,
    points: &[(f64, f64)],
    cancel: &AtomicBool,
) -> Result<(), RenderError> {
    check_cancel(cancel)?;
    let model_points: Vec<align_core::MappingPoint> = points
        .iter()
        .map(|(s, i)| align_core::MappingPoint {
            source: align_core::MediaTime::seconds(*s),
            island: align_core::MediaTime::seconds(*i),
        })
        .collect();
    let segments = segments(&model_points).ok_or(RenderError::InvalidMapping)?;
    // Probe format first (writer needs rate/channels upfront).
    let probe = backend.inspect(source).map_err(RenderError::from)?;
    let stream = probe
        .audio_streams
        .first()
        .ok_or_else(|| RenderError::NoAudio(file_name(source)))?;
    let (sample_rate, channels) = (stream.sample_rate, stream.channels);
    if sample_rate <= 0.0 || channels == 0 {
        return Err(RenderError::UnknownFormat(file_name(source)));
    }

    let tmp = destination.with_extension(format!(
        "tmp-{}.wav",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ));
    let result = (|| {
        let mut writer = WavWriter::create(&tmp, sample_rate as u32, channels, false)?;
        for &(source_start, source_duration, target_duration) in &segments {
            check_cancel(cancel)?;
            let ratio = target_duration / source_duration;
            // Supply real neighboring samples to each independent filter.
            // Zero-padding at an internal knot otherwise creates an impulse
            // whenever the signal is nonzero there. Guard audio is filtered
            // but excluded from the emitted segment length.
            let start_frame = (source_start * sample_rate).round() as usize;
            let guard_frames = start_frame.min(4096);
            let decode_start = (start_frame - guard_frames) as f64 / sample_rate;
            let decode_end =
                (source_start + source_duration + 4096.0 / sample_rate).min(probe.duration_seconds);
            let mut segment = Segment::new(
                source_start,
                source_duration,
                target_duration,
                sample_rate,
                channels,
            )?;
            segment.skip += (guard_frames as f64 * ratio).round() as usize;
            backend
                .decode_native(
                    source,
                    0,
                    Some((decode_start, Some((decode_end - decode_start).max(0.0)))),
                    &mut |block: NativeBlock| {
                        check_cancel(cancel).map_err(decode_render_error)?;
                        let out = segment.push(block.frames).map_err(decode_render_error)?;
                        writer
                            .write_pcm(channels, &out, f32::to_le_bytes)
                            .map_err(decode_render_error)
                    },
                )
                .map_err(RenderError::from)?;
            let tail = segment.finish(cancel)?;
            writer.write_pcm(channels, &tail, f32::to_le_bytes)?;
        }
        check_cancel(cancel)?;
        writer.finalize()?;
        Ok::<(), RenderError>(())
    })();
    match result {
        Ok(()) => {
            if align_core::wav::preserve_broadcast_extension(
                source,
                &tmp,
                sample_rate,
                channels,
                32,
                "drift correction",
                0,
            )
            .is_err()
            {
                let _ = std::fs::remove_file(&tmp);
                return Err(RenderError::CannotWrite);
            }
            #[cfg(windows)]
            if destination.exists() && std::fs::remove_file(destination).is_err() {
                let _ = std::fs::remove_file(&tmp);
                return Err(RenderError::CannotWrite);
            }
            if std::fs::rename(&tmp, destination).is_err() {
                let _ = std::fs::remove_file(&tmp);
                return Err(RenderError::CannotWrite);
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// ------------------------------------------------------------ placement pad

/// Placement-pad sidecar render (32-bit float, native rate/channels).
///
/// Prepends exactly `pad_samples` silence samples through the native-rate
/// streaming decoder and WAV writer, equally on every channel. No resampling
/// or external FFmpeg process is needed when the backend decodes the source.
/// An integer-frame FCP7 XML placement of this file lands fractional
/// content sample-accurately in Premiere, whose importer floors audio
/// `start`/`end` and ignores `<subframeoffset>`. BWF extension handling
/// mirrors drift sidecars. `selection` is a native-sample half-open range;
/// when present, only that range follows the prepend.
pub fn render_pad(
    backend: &dyn MediaBackend,
    source: &Path,
    destination: &Path,
    pad_samples: u64,
    selection: Option<(u64, u64)>,
) -> Result<(), RenderError> {
    render_pad_cancellable(
        backend,
        source,
        destination,
        pad_samples,
        selection,
        &NEVER_CANCELLED,
    )
}

pub fn render_pad_cancellable(
    backend: &dyn MediaBackend,
    source: &Path,
    destination: &Path,
    pad_samples: u64,
    selection: Option<(u64, u64)>,
    cancel: &AtomicBool,
) -> Result<(), RenderError> {
    check_cancel(cancel)?;
    if pad_samples == 0 {
        return Err(RenderError::PlacementPad("empty prepend".into()));
    }
    let probe = backend.inspect(source).map_err(RenderError::from)?;
    let stream = probe
        .audio_streams
        .first()
        .ok_or_else(|| RenderError::NoAudio(file_name(source)))?;
    let (sample_rate, channels) = (stream.sample_rate, stream.channels);
    if sample_rate <= 0.0 || channels == 0 {
        return Err(RenderError::UnknownFormat(file_name(source)));
    }
    if selection.is_some_and(|(start, end)| end <= start) {
        return Err(RenderError::InvalidMapping);
    }
    let reference_shift = i64::try_from(
        i128::from(selection.map_or(0, |(start, _)| start)) - i128::from(pad_samples),
    )
    .map_err(|_| RenderError::InvalidMapping)?;
    let tmp = destination.with_extension(format!("tmp-{}.wav", std::process::id()));
    let result = (|| {
        let mut writer = WavWriter::create(&tmp, sample_rate as u32, channels, false)?;
        let mut silence = pad_samples;
        while silence > 0 {
            check_cancel(cancel)?;
            let n = silence.min(4096) as usize;
            writer.write_silence(channels, n)?;
            silence -= n as u64;
        }
        let range = selection.map(|(start, end)| {
            (
                start as f64 / sample_rate,
                Some((end - start) as f64 / sample_rate),
            )
        });
        let mut remaining = selection.map(|(start, end)| end - start);
        backend
            .decode_native(source, 0, range, &mut |block| {
                check_cancel(cancel).map_err(decode_render_error)?;
                let available = block.frames.first().map_or(0, Vec::len);
                let take = remaining.map_or(available, |n| n.min(available as u64) as usize);
                let selected: Vec<&[f32]> = block.frames.iter().map(|ch| &ch[..take]).collect();
                writer
                    .write_pcm(channels, &selected, f32::to_le_bytes)
                    .map_err(decode_render_error)?;
                if let Some(ref mut n) = remaining {
                    *n -= take as u64;
                }
                Ok(())
            })
            .map_err(RenderError::from)?;
        if let (Some(n), Some((_, end))) = (remaining, selection) {
            finish_video_tail(
                &mut writer,
                &probe,
                end as f64 / sample_rate,
                n,
                sample_rate,
                channels,
            )?;
        }
        check_cancel(cancel)?;
        writer.finalize()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if align_core::wav::preserve_broadcast_extension(
        source,
        &tmp,
        sample_rate,
        channels,
        32,
        "placement pad",
        reference_shift,
    )
    .is_err()
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(RenderError::CannotWrite);
    }
    #[cfg(windows)]
    if destination.exists() && std::fs::remove_file(destination).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err(RenderError::CannotWrite);
    }
    if std::fs::rename(&tmp, destination).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err(RenderError::CannotWrite);
    }
    Ok(())
}

// ------------------------------------------------------------ channel stem

/// Sample-exact mono stem extraction (precision audio for Resolve).
/// Uses int32 for 32-bit integer sources, otherwise float32. BWF
/// TimeReference shifts by the
/// trim offset, CodingHistory gains a channel-extraction line.
#[allow(clippy::too_many_arguments)]
pub fn render_channel(
    backend: &dyn MediaBackend,
    source: &Path,
    destination: &Path,
    channel: usize,
    bit_depth: Option<u32>,
    is_float: bool,
    source_start: f64,
    duration: f64,
) -> Result<(), RenderError> {
    render_channel_with_tail(
        backend,
        source,
        destination,
        channel,
        bit_depth,
        is_float,
        source_start,
        duration,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_channel_with_tail(
    backend: &dyn MediaBackend,
    source: &Path,
    destination: &Path,
    channel: usize,
    bit_depth: Option<u32>,
    is_float: bool,
    source_start: f64,
    duration: f64,
    tail_samples: u64,
) -> Result<(), RenderError> {
    render_channel_cancellable(
        backend,
        source,
        destination,
        channel,
        bit_depth,
        is_float,
        source_start,
        duration,
        tail_samples,
        &NEVER_CANCELLED,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_channel_cancellable(
    backend: &dyn MediaBackend,
    source: &Path,
    destination: &Path,
    channel: usize,
    bit_depth: Option<u32>,
    is_float: bool,
    source_start: f64,
    duration: f64,
    tail_samples: u64,
    cancel: &AtomicBool,
) -> Result<(), RenderError> {
    check_cancel(cancel)?;
    let int32 = matches!(bit_depth, Some(32)) && !is_float;
    let probe = backend.inspect(source).map_err(RenderError::from)?;
    let stream = probe
        .audio_streams
        .first()
        .ok_or_else(|| RenderError::NoAudio(file_name(source)))?;
    let (sample_rate, channels) = (stream.sample_rate, stream.channels);
    if sample_rate <= 0.0 || channels == 0 || channel >= channels {
        return Err(RenderError::UnknownFormat(file_name(source)));
    }
    let start_frame = ((source_start.max(0.0)) * sample_rate).round() as i64;
    let take_frames = ((duration * sample_rate).round() as i64).max(0);

    let tmp = destination.with_extension(format!(
        "tmp-{}.wav",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ));
    let result = (|| {
        let mut writer = WavWriter::create(&tmp, sample_rate as u32, 1, int32)?;
        let mut remaining = take_frames;
        if int32 {
            backend
                .decode_native_i32(
                    source,
                    0,
                    Some((source_start.max(0.0), Some(duration))),
                    &mut |block: NativeBlockI32| {
                        check_cancel(cancel).map_err(decode_render_error)?;
                        if remaining <= 0 {
                            return Ok(());
                        }
                        let got = block.frames[channel].len() as i64;
                        let take = got.min(remaining) as usize;
                        writer
                            .write_pcm(1, &[&block.frames[channel][..take]], i32::to_le_bytes)
                            .map_err(decode_render_error)?;
                        remaining -= take as i64;
                        Ok(())
                    },
                )
                .map_err(RenderError::from)?;
        } else {
            backend
                .decode_native(
                    source,
                    0,
                    Some((source_start.max(0.0), Some(duration))),
                    &mut |block: NativeBlock| {
                        check_cancel(cancel).map_err(decode_render_error)?;
                        if remaining <= 0 {
                            return Ok(());
                        }
                        let got = block.frames[channel].len() as i64;
                        let take = got.min(remaining) as usize;
                        writer
                            .write_pcm(1, &[&block.frames[channel][..take]], f32::to_le_bytes)
                            .map_err(decode_render_error)?;
                        remaining -= take as i64;
                        Ok(())
                    },
                )
                .map_err(RenderError::from)?;
        }
        finish_video_tail(
            &mut writer,
            &probe,
            source_start.max(0.0) + duration,
            remaining as u64,
            sample_rate,
            1,
        )?;
        let mut tail_remaining = tail_samples;
        while tail_remaining > 0 {
            check_cancel(cancel)?;
            let n = tail_remaining.min(4096) as usize;
            writer.write_silence(1, n)?;
            tail_remaining -= n as u64;
        }
        check_cancel(cancel)?;
        writer.finalize()?;
        Ok::<(), RenderError>(())
    })();
    match result {
        Ok(()) => {
            if align_core::wav::preserve_broadcast_extension(
                source,
                &tmp,
                sample_rate,
                1,
                32,
                &format!("channel {} extraction", channel + 1),
                start_frame,
            )
            .is_err()
            {
                let _ = std::fs::remove_file(&tmp);
                return Err(RenderError::CannotWrite);
            }
            #[cfg(windows)]
            if destination.exists() && std::fs::remove_file(destination).is_err() {
                let _ = std::fs::remove_file(&tmp);
                return Err(RenderError::CannotWrite);
            }
            if std::fs::rename(&tmp, destination).is_err() {
                let _ = std::fs::remove_file(&tmp);
                return Err(RenderError::CannotWrite);
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_stereo_wav(path: &Path, rate: u32, secs: u64, seed: u64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut s = seed;
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for _ in 0..rate as u64 * secs {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let v = (((s >> 33) as f32 / u32::MAX as f32) * 60000.0 - 30000.0) as i16;
            w.write_sample(v).unwrap();
            w.write_sample((v as i32 / 2) as i16).unwrap();
        }
        w.finalize().unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("align-render-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn wav_writer_preserves_pcm_bits_and_rejects_short_planes() {
        let dir = tmp("pcm-bits");
        for int32 in [false, true] {
            let path = dir.join(if int32 { "int.wav" } else { "float.wav" });
            let mut writer = WavWriter::create(&path, 48000, 2, int32).unwrap();
            let expected = if int32 {
                let left = [i32::MIN, 0, i32::MAX];
                let right = [1, -1, 123456789];
                writer
                    .write_pcm(2, &[&left[..], &right[..1]], i32::to_le_bytes)
                    .unwrap_err();
                writer
                    .write_pcm(2, &[left, right], i32::to_le_bytes)
                    .unwrap();
                left.into_iter()
                    .zip(right)
                    .flat_map(|(l, r)| [l, r])
                    .flat_map(i32::to_le_bytes)
                    .collect::<Vec<_>>()
            } else {
                let left = [-0.0f32, f32::from_bits(0x7fc01234), 1.0];
                let right = [f32::INFINITY, -1.0, f32::MIN_POSITIVE];
                writer
                    .write_pcm(2, &[&left[..], &right[..1]], f32::to_le_bytes)
                    .unwrap_err();
                writer
                    .write_pcm(2, &[left, right], f32::to_le_bytes)
                    .unwrap();
                left.into_iter()
                    .zip(right)
                    .flat_map(|(l, r)| [l, r])
                    .flat_map(f32::to_le_bytes)
                    .collect::<Vec<_>>()
            };
            let capacity = writer.bytes.capacity();
            writer.write_silence(2, 2).unwrap();
            assert_eq!(writer.bytes.capacity(), capacity);
            assert_eq!(writer.frames_written, 5);
            writer.finalize().unwrap();
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(&bytes[80..104], expected);
            assert_eq!(&bytes[104..], &[0; 16]);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wav_writer_promotes_large_sparse_output_to_rf64() {
        use std::io::Read;
        let dir = tmp("rf64-size");
        let path = dir.join("large.wav");
        let mut writer = WavWriter::create(&path, 48000, 2, false).unwrap();
        // Sparse file crosses the actual RIFF boundary without allocating
        // 4 GiB of RAM or writing hours of silence during a unit test.
        let frames = (u64::from(u32::MAX) / 8) + 1;
        let data_bytes = frames * 8;
        writer.file.set_len(80 + data_bytes).unwrap();
        writer.file.seek(SeekFrom::End(0)).unwrap();
        writer.frames_written = frames;
        writer.finalize().unwrap();
        let mut header = [0u8; 80];
        std::fs::File::open(&path)
            .unwrap()
            .read_exact(&mut header)
            .unwrap();
        assert_eq!(&header[0..4], b"RF64");
        assert_eq!(&header[4..8], &u32::MAX.to_le_bytes());
        assert_eq!(&header[12..16], b"ds64");
        assert_eq!(u32::from_le_bytes(header[16..20].try_into().unwrap()), 28);
        assert_eq!(
            u64::from_le_bytes(header[20..28].try_into().unwrap()),
            data_bytes + 72
        );
        assert_eq!(
            u64::from_le_bytes(header[28..36].try_into().unwrap()),
            data_bytes
        );
        assert_eq!(
            u64::from_le_bytes(header[36..44].try_into().unwrap()),
            frames
        );
        assert_eq!(&header[44..48], &[0; 4]);
        assert_eq!(&header[72..76], b"data");
        assert_eq!(&header[76..80], &u32::MAX.to_le_bytes());
        let probe = crate::portable::PortableBackend.inspect(&path).unwrap();
        assert!((probe.duration_seconds - frames as f64 / 48000.0).abs() < 1.0 / 48000.0);
        assert_eq!(probe.audio_streams[0].channels, 2);
        assert_eq!(probe.audio_streams[0].sample_rate, 48000.0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn drift_render_identity_mapping_is_transparent() {
        let dir = tmp("drift");
        let src = dir.join("src.wav");
        write_stereo_wav(&src, 44100, 5, 0xD1);
        let dst = dir.join("fixed.wav");
        let backend = crate::portable::PortableBackend;
        // Identity map (rate 1): output must equal input signal.
        render_drift(&backend, &src, &dst, &[(0.0, 0.0), (5.0, 5.0)]).expect("render");
        let got =
            crate::sym::decode_window(&dst, 0.0, 5.0, align_core::AudioAnalysisSource::Automatic)
                .expect("read back");
        assert!(
            (got.1.len() as isize - 5 * 16000).abs() < 200,
            "len={}",
            got.1.len()
        );
        // Compare against the source window: near-identical (sinc at 1.0).
        let want =
            crate::sym::decode_window(&src, 0.0, 5.0, align_core::AudioAnalysisSource::Automatic)
                .expect("read src");
        let drift = got
            .1
            .iter()
            .zip(want.1.iter())
            .take(5 * 16000)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(drift < 0.02, "drift={drift}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_pad_samples(path: &Path) -> Vec<f32> {
        let mut samples = Vec::new();
        crate::portable::PortableBackend
            .decode_native(path, 0, None, &mut |block| {
                for i in 0..block.frames[0].len() {
                    for channel in &block.frames {
                        samples.push(channel[i]);
                    }
                }
                Ok(())
            })
            .unwrap();
        samples
    }

    fn add_test_bext(path: &Path, reference: u64) {
        let mut wav = std::fs::read(path).unwrap();
        let mut bext = vec![0u8; 602];
        bext[338..346].copy_from_slice(&reference.to_le_bytes());
        wav.extend_from_slice(b"bext");
        wav.extend_from_slice(&602u32.to_le_bytes());
        wav.extend_from_slice(&bext);
        let size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&size.to_le_bytes());
        std::fs::write(path, wav).unwrap();
    }

    #[test]
    fn placement_pad_prepends_exact_silence() {
        let dir = tmp("pad");
        let src = dir.join("src.wav");
        // f32 stereo with a click at sample 1000 (both channels).
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&src, spec).unwrap();
        for i in 0..4800 {
            let v = if i == 1000 { 0.75 } else { 0.0 };
            w.write_sample(v).unwrap();
            w.write_sample(-v).unwrap();
        }
        w.finalize().unwrap();
        add_test_bext(&src, 48000);
        let dst = dir.join("pad.wav");
        let backend = crate::portable::PortableBackend;
        render_pad(&backend, &src, &dst, 192, None).expect("pad");
        assert_eq!(
            align_core::meta::read_bwf(&dst).unwrap().time_reference,
            Some(48000 - 192)
        );
        let samples = read_pad_samples(&dst);
        assert_eq!(samples.len(), (4800 + 192) * 2);
        // Leading prepend is digital silence on both channels.
        assert!(samples[..192 * 2].iter().all(|&v| v == 0.0));
        // Body is the untouched source: click moved by exactly 192 samples.
        assert_eq!(samples[192 * 2 + 1000 * 2], 0.75);
        assert_eq!(samples[192 * 2 + 1000 * 2 + 1], -0.75);
        assert!(samples[192 * 2 + 999 * 2] == 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn placement_pad_preserves_selected_head_and_tail_only() {
        let dir = tmp("pad-range");
        let src = dir.join("src.wav");
        let mut w = hound::WavWriter::create(
            &src,
            hound::WavSpec {
                channels: 2,
                sample_rate: 48000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for i in 0..9600 {
            let v = match i {
                100 => 0.9,
                2400 => 0.6,
                7199 => 0.7,
                8000 => 0.8,
                _ => 0.0,
            };
            w.write_sample(v).unwrap();
            w.write_sample(-v).unwrap();
        }
        w.finalize().unwrap();
        add_test_bext(&src, 48000);
        let dst = dir.join("selected.wav");
        render_pad(
            &crate::portable::PortableBackend,
            &src,
            &dst,
            192,
            Some((2400, 7200)),
        )
        .unwrap();
        let samples = read_pad_samples(&dst);
        assert_eq!(samples.len(), 4992 * 2);
        assert_eq!(
            align_core::meta::read_bwf(&dst).unwrap().time_reference,
            Some(48000 + 2400 - 192)
        );
        assert!(samples[..384].iter().all(|v| *v == 0.0));
        assert_eq!(&samples[384..386], &[0.6, -0.6]);
        assert_eq!(&samples[samples.len() - 2..], &[0.7, -0.7]);
        assert_eq!(samples.iter().filter(|v| **v != 0.0).count(), 4);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn camera_tail_gap_is_silent_but_truncated_audio_is_rejected() {
        let dir = tmp("camera-tail");
        let src = dir.join("source.wav");
        write_stereo_wav(&src, 48000, 1, 42);
        let mut probe = crate::portable::PortableBackend.inspect(&src).unwrap();
        let dst = dir.join("tail.wav");
        let mut writer = WavWriter::create(&dst, 48000, 2, false).unwrap();
        assert_eq!(
            finish_video_tail(&mut writer, &probe, 1.0, 6, 48000.0, 2),
            Err(RenderError::CannotRead)
        );
        probe.has_video = true;
        assert_eq!(
            finish_video_tail(&mut writer, &probe, 0.9, 6, 48000.0, 2),
            Err(RenderError::CannotRead)
        );
        assert_eq!(
            finish_video_tail(&mut writer, &probe, 1.0, 4800, 48000.0, 2),
            Err(RenderError::CannotRead)
        );
        finish_video_tail(&mut writer, &probe, 1.0, 6, 48000.0, 2).unwrap();
        writer.finalize().unwrap();
        assert_eq!(read_pad_samples(&dst), vec![0.0; 12]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn placement_pad_invalid_clock_preserves_existing_destination() {
        let dir = tmp("pad-clock-underflow");
        let src = dir.join("src.wav");
        write_stereo_wav(&src, 48000, 1, 42);
        add_test_bext(&src, 100);
        let dst = dir.join("existing.wav");
        std::fs::write(&dst, b"existing output").unwrap();
        assert!(render_pad(&crate::portable::PortableBackend, &src, &dst, 192, None).is_err());
        assert_eq!(std::fs::read(&dst).unwrap(), b"existing output");
        assert!(
            !dst.with_extension(format!("tmp-{}.wav", std::process::id()))
                .exists()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    struct CancelOnDecode<'a>(&'a AtomicBool);
    impl MediaBackend for CancelOnDecode<'_> {
        fn kind(&self) -> crate::backend::BackendKind {
            crate::backend::BackendKind::Portable
        }
        fn inspect(&self, path: &Path) -> Result<crate::backend::ProbeReport, DecodeError> {
            crate::portable::PortableBackend.inspect(path)
        }
        fn decode_mono_8k(
            &self,
            _: &Path,
            _: align_core::AudioAnalysisSource,
            _: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
        ) -> Result<(), DecodeError> {
            unreachable!()
        }
        fn decode_window_16k(
            &self,
            _: &Path,
            _: f64,
            _: f64,
            _: align_core::AudioAnalysisSource,
        ) -> Result<(f64, Vec<f32>), DecodeError> {
            unreachable!()
        }
        fn decode_native(
            &self,
            path: &Path,
            stream: usize,
            range: Option<(f64, Option<f64>)>,
            consume: &mut dyn FnMut(NativeBlock) -> Result<(), DecodeError>,
        ) -> Result<f64, DecodeError> {
            crate::portable::PortableBackend.decode_native(path, stream, range, &mut |block| {
                self.0.store(true, Ordering::Relaxed);
                consume(block)
            })
        }
        fn decode_native_i32(
            &self,
            path: &Path,
            stream: usize,
            range: Option<(f64, Option<f64>)>,
            consume: &mut dyn FnMut(NativeBlockI32) -> Result<(), DecodeError>,
        ) -> Result<f64, DecodeError> {
            crate::portable::PortableBackend.decode_native_i32(path, stream, range, &mut |block| {
                self.0.store(true, Ordering::Relaxed);
                consume(block)
            })
        }
    }

    #[test]
    fn cancellation_during_decode_discards_partial_outputs() {
        let dir = tmp("cancel-mid-decode");
        let src = dir.join("source.wav");
        write_stereo_wav(&src, 48000, 1, 42);
        let dst = dir.join("existing.wav");
        std::fs::write(&dst, b"existing output").unwrap();
        let cancel = AtomicBool::new(false);
        let backend = CancelOnDecode(&cancel);
        assert_eq!(
            render_drift_cancellable(&backend, &src, &dst, &[(0.0, 0.0), (1.0, 1.001)], &cancel),
            Err(RenderError::Cancelled)
        );
        for integer in [false, true] {
            cancel.store(false, Ordering::Relaxed);
            assert_eq!(
                render_channel_cancellable(
                    &backend,
                    &src,
                    &dst,
                    0,
                    Some(if integer { 32 } else { 16 }),
                    false,
                    0.0,
                    1.0,
                    0,
                    &cancel
                ),
                Err(RenderError::Cancelled)
            );
        }
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            render_pad_cancellable(&backend, &src, &dst, 192, None, &cancel),
            Err(RenderError::Cancelled)
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"existing output");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelled_render_never_reads_or_replaces_output() {
        let dir = tmp("cancelled-render");
        let dst = dir.join("existing.wav");
        std::fs::write(&dst, b"existing output").unwrap();
        let backend = crate::portable::PortableBackend;
        let cancel = AtomicBool::new(true);
        let source = dir.join("missing.wav");
        assert_eq!(
            render_pad_cancellable(&backend, &source, &dst, 192, None, &cancel),
            Err(RenderError::Cancelled)
        );
        assert_eq!(
            render_drift_cancellable(
                &backend,
                &source,
                &dst,
                &[(0.0, 0.0), (1.0, 1.001)],
                &cancel
            ),
            Err(RenderError::Cancelled)
        );
        assert_eq!(
            render_channel_cancellable(
                &backend,
                &source,
                &dst,
                0,
                Some(16),
                false,
                0.0,
                1.0,
                0,
                &cancel
            ),
            Err(RenderError::Cancelled)
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"existing output");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancellation_terminates_running_ffmpeg_and_reaps_it() {
        let Some(bin) = crate::ff::ffmpeg_bin() else {
            return;
        };
        let mut child = std::process::Command::new(bin)
            .args([
                "-v",
                "error",
                "-re",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=mono",
                "-f",
                "null",
                "-",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                cancel.store(true, Ordering::Relaxed);
            });
            let start = std::time::Instant::now();
            assert_eq!(
                crate::export::wait_render_process(&mut child, &cancel),
                Err(RenderError::Cancelled)
            );
            assert!(start.elapsed() < std::time::Duration::from_secs(3));
        });
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn placement_pad_rejects_empty_prepend() {
        let backend = crate::portable::PortableBackend;
        let dir = tmp("pad-empty");
        let dst = dir.join("pad.wav");
        assert!(render_pad(&backend, Path::new("/nonexistent.wav"), &dst, 0, None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_stem_trims_sample_exact() {
        let dir = tmp("stem");
        let src = dir.join("src.wav");
        write_stereo_wav(&src, 48000, 4, 0xC4);
        let dst = dir.join("ch1.wav");
        let backend = crate::portable::PortableBackend;
        render_channel(&backend, &src, &dst, 1, Some(16), false, 1.0, 2.0).expect("stem");
        let reader = hound::WavReader::open(&dst).expect("open stem");
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.duration(), 2 * 48000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apple_fractional_trim_uses_nearest_native_sample() {
        let dir = tmp("apple-fractional-trim");
        let src = dir.join("src.wav");
        write_stereo_wav(&src, 48000, 3, 0xC4);
        let dst = dir.join("trim.wav");
        render_channel(
            &crate::apple::AppleNativeBackend,
            &src,
            &dst,
            0,
            Some(16),
            false,
            0.4004,
            (80080.0 - 0.00001) / 48000.0,
        )
        .unwrap();
        assert_eq!(hound::WavReader::open(&dst).unwrap().duration(), 80080);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn drift_stretch_produces_exact_target_length() {
        // 10 s source mapped to 10.1 s island (+1% clock): output length
        // must be exact and content must be the stretched signal, not
        // truncated or padded.
        let dir = tmp("ratio");
        let src = dir.join("src.wav");
        write_stereo_wav(&src, 48000, 10, 0xD1);
        let dst = dir.join("fixed.wav");
        let backend = crate::portable::PortableBackend;
        render_drift(&backend, &src, &dst, &[(0.0, 0.0), (10.0, 10.1)]).expect("render");
        let reader = hound::WavReader::open(&dst).expect("open render");
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.duration(), (10.1f64 * 48000.0).round() as u32);
        // Tail content exists (not zero-padded): last second has energy.
        let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
        let tail: f64 = samples[samples.len() - 48000 * 2..]
            .iter()
            .map(|&v| (v as f64).powi(2))
            .sum::<f64>()
            / (48000 * 2) as f64;
        assert!(tail > 1e-6, "tail silent: {tail}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn piecewise_render_preserves_independent_channels_and_bwf() {
        verify_piecewise_render(8);
        verify_piecewise_render(600);
    }

    fn verify_piecewise_render(duration: u32) {
        let midpoint = f64::from(duration) / 2.0;
        let knot = midpoint * 1.001;
        let dir = tmp("piecewise-tones");
        let src = dir.join("src.wav");
        let dst = dir.join("fixed.wav");
        let rate = 48000;
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let tone = |time: f64, channel: usize| {
            let frequency = [317.0, 733.0][channel];
            0.4 * (std::f64::consts::TAU * frequency * time + [0.37, 0.83][channel]).sin()
        };
        let mut writer = hound::WavWriter::create(&src, spec).unwrap();
        for frame in 0..duration * rate {
            for channel in 0..2 {
                writer
                    .write_sample(tone(f64::from(frame) / f64::from(rate), channel) as f32)
                    .unwrap();
            }
        }
        writer.finalize().unwrap();
        add_test_bext(&src, 123456789);
        render_drift(
            &crate::portable::PortableBackend,
            &src,
            &dst,
            &[
                (0.0, 0.0),
                (midpoint, knot),
                (f64::from(duration), f64::from(duration)),
            ],
        )
        .unwrap();
        let reader = hound::WavReader::open(&dst).unwrap();
        assert_eq!(reader.spec(), spec);
        assert_eq!(reader.duration(), duration * rate);
        assert_eq!(
            align_core::meta::read_bwf(&dst).unwrap().time_reference,
            Some(123456789)
        );
        let samples: Vec<f32> = reader.into_samples().map(Result::unwrap).collect();
        // Independent analytic oracle: each segment's output time maps
        // back to its original sine phase. Exclude 20 ms at resampler
        // boundaries; this measures steady-state content, not seam quality.
        for channel in 0..2 {
            let mut squared_error = 0.0;
            let mut count = 0;
            for frame in 0..(duration * rate) as usize {
                let time = frame as f64 / f64::from(rate);
                if time < 0.02 || (time - knot).abs() < 0.02 || time > f64::from(duration) - 0.02 {
                    continue;
                }
                let source = if time < knot {
                    time / 1.001
                } else {
                    midpoint + (time - knot) / 0.999
                };
                squared_error +=
                    (f64::from(samples[frame * 2 + channel]) - tone(source, channel)).powi(2);
                count += 1;
            }
            let rms = (squared_error / f64::from(count)).sqrt();
            // A one-sample phase displacement of a sine has this exact
            // RMS difference. FFT group-delay removal is integer-sampled.
            let one_sample_rms = 0.4
                * 2.0_f64.sqrt()
                * (std::f64::consts::PI * [317.0, 733.0][channel] / f64::from(rate)).sin();
            assert!(
                rms < one_sample_rms,
                "channel {channel}: analytic RMS error {rms}, one-sample bound {one_sample_rms}"
            );
        }
        // Inspect the previously excluded 40 ms around the rate change.
        // Bound instantaneous error by one source-sample sine displacement,
        // allowing 0.001 for numerical filter error.
        for channel in 0..2 {
            let mut peak_error = 0.0_f64;
            for frame in (((knot - 0.02) * f64::from(rate)) as usize)
                ..(((knot + 0.02) * f64::from(rate)) as usize)
            {
                let time = frame as f64 / f64::from(rate);
                let source = if time < knot {
                    time / 1.001
                } else {
                    midpoint + (time - knot) / 0.999
                };
                peak_error = peak_error
                    .max((f64::from(samples[frame * 2 + channel]) - tone(source, channel)).abs());
            }
            let bound = 0.8
                * (std::f64::consts::PI * [317.0, 733.0][channel] / f64::from(rate)).sin()
                + 0.001;
            assert!(
                peak_error < bound,
                "channel {channel}: seam peak {peak_error}, bound {bound}"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fractional_ppm_preserves_phase_at_end_of_long_take() {
        let dir = tmp("fractional-ppm");
        let src = dir.join("source.wav");
        let dst = dir.join("corrected.wav");
        let sr = 8000;
        let duration = 600.0;
        let ratio = 1.00001745;
        let tone = |time: f64| 0.4 * (std::f64::consts::TAU * 997.0 * time).sin();
        let mut writer = hound::WavWriter::create(
            &src,
            hound::WavSpec {
                channels: 1,
                sample_rate: sr,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for i in 0..(duration * sr as f64) as usize {
            writer
                .write_sample(tone(i as f64 / sr as f64) as f32)
                .unwrap();
        }
        writer.finalize().unwrap();
        render_drift(
            &crate::portable::PortableBackend,
            &src,
            &dst,
            &[(0.0, 0.0), (duration, duration * ratio)],
        )
        .unwrap();
        let reader = hound::WavReader::open(&dst).unwrap();
        assert_eq!(
            reader.duration(),
            (duration * ratio * sr as f64).round() as u32
        );
        let mut squared_error = 0.0;
        let mut count = 0;
        for (i, sample) in reader.into_samples::<f32>().enumerate() {
            let time = i as f64 / sr as f64;
            if (590.0..599.0).contains(&time) {
                squared_error += (sample.unwrap() as f64 - tone(time / ratio)).powi(2);
                count += 1;
            }
        }
        assert!((squared_error / count as f64).sqrt() < 0.005);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clock_ratio_has_bounded_streaming_latency() {
        for ratio in [1.000801, 0.9999977, 1.000002161] {
            let mut segment = Segment::new(0.0, 60.0, 60.0 * ratio, 48000.0, 2).unwrap();
            assert!(segment.skip < 4096, "ratio {ratio}: delay {}", segment.skip);
            let out = segment.push(vec![vec![0.25; 4096]; 2]).unwrap();
            assert!(!out[0].is_empty(), "ratio {ratio}: first block buffered");
        }
    }

    #[test]
    fn coprime_clock_ratio_flushes_buffered_fft_tail() {
        let dir = tmp("coprime");
        let src = dir.join("src.wav");
        write_stereo_wav(&src, 44100, 3, 0xD1);
        let dst = dir.join("fixed.wav");
        let duration = 3.0 * 1.000801;
        render_drift(
            &crate::portable::PortableBackend,
            &src,
            &dst,
            &[(0.0, 0.0), (3.0, duration)],
        )
        .unwrap();
        let reader = hound::WavReader::open(&dst).unwrap();
        assert_eq!(reader.duration(), (duration * 44100.0).round() as u32);
        let samples: Vec<f32> = reader.into_samples::<f32>().map(Result::unwrap).collect();
        let tail = &samples[samples.len() - 44100 * 2..];
        assert!(
            tail.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / tail.len() as f64 > 0.001
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn drift_rejects_bad_mapping() {
        let dir = tmp("bad");
        let src = dir.join("src.wav");
        write_stereo_wav(&src, 44100, 2, 1);
        let backend = crate::portable::PortableBackend;
        let err = render_drift(&backend, &src, &dir.join("o.wav"), &[(0.0, 0.0)])
            .expect_err("single point");
        assert_eq!(err, RenderError::InvalidMapping);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
