//! Apple-native engine: AVFoundation demux + decode via `objc2`, same
//! contracts as `AudioDecoder.swift` + `SyncEngine.inspect` (4-reader gate
//! lives one layer up, 0.9-hysteresis adaptive mono, native-rate discrete
//! channels).
//!
//! Division of labour (deliberate): AVFoundation owns container parsing,
//! codec decode (incl. VideoToolbox hardware paths) and sample timing —
//! that is where ~all platform value sits. Resampling is the shared
//! [`crate::mono`] sinc (identical delay-compensated numerics on every
//! engine) instead of `AVAudioConverter`, so fingerprints never depend on
//! which engine decoded them.
//!
//! Audio-only: outputs attach to audio tracks only; video tracks are
//! enumerated for presence (`has_video`) and never read. VFR cursor walk
//! and Sony/timecode metadata land with the metadata milestone; until then
//! video timing reports through the same unknown-mode path as containers
//! without sample-timing info.
//!
//! Threading: `loadTracksWithMediaType:completionHandler:` crosses threads
//! via a raw-pointer channel (`NSArray`/`AVAssetTrack` are not `Send` in
//! objc2) with balanced retain/from_raw ownership — contained in
//! [`load_tracks`]. Everything else runs synchronously on the caller's
//! thread; all `unsafe` blocks document their contract inline.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use align_core::AudioAnalysisSource;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetReader, AVAssetReaderTrackOutput, AVAssetTrack, AVMediaTypeAudio, AVMediaTypeVideo,
    AVURLAsset,
};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatLinearPCM,
};
use objc2_core_foundation::CGSize;
use objc2_core_media::{
    CMAudioFormatDescription, CMAudioFormatDescriptionGetStreamBasicDescription, CMTime,
    CMTimeCodeFormatDescription, CMTimeCodeFormatDescriptionGetTimeCodeFlags, CMTimeFlags,
    CMTimeRange, kCMTimeCodeFlag_DropFrame,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};

use crate::DecodeError;
use crate::backend::{AudioStreamProbe, BackendKind, MediaBackend, ProbeReport, VideoProbe};

// ------------------------------------------------------------ asset plumbing

/// Real Foundation call (also proves `objc2` linkage without Swift).
pub fn file_url(path: &Path) -> Option<Retained<NSURL>> {
    NSURL::from_file_path(path)
}

fn apple_error(context: &str, err: Option<Retained<objc2_foundation::NSError>>) -> DecodeError {
    DecodeError::Apple(match err {
        Some(e) => format!("{context}: {}", e.localizedDescription()),
        None => context.to_string(),
    })
}

fn open_asset(path: &Path) -> Result<Retained<AVURLAsset>, DecodeError> {
    if !path.is_file() {
        return Err(DecodeError::NoAudio(path.display().to_string()));
    }
    let url = file_url(path).ok_or_else(|| DecodeError::NoAudio(path.display().to_string()))?;
    // SAFETY: url is a valid file URL; nil options = defaults.
    Ok(unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) })
}

/// Track loading crosses threads (completion handler fires on an internal
/// queue) but `NSArray<AVAssetTrack>` is not `Send`, so ownership hops as a
/// raw pointer: retained in the callback, `from_raw` on receipt. Balanced
/// by construction; a 60 s timeout turns a lost callback into an error.
fn load_tracks(
    asset: &AVURLAsset,
    media_type: &objc2_av_foundation::AVMediaType,
) -> Result<Vec<Retained<AVAssetTrack>>, DecodeError> {
    let (tx, rx) = mpsc::channel::<*mut NSArray<AVAssetTrack>>();
    let block: RcBlock<dyn Fn(*mut NSArray<AVAssetTrack>, *mut objc2_foundation::NSError)> =
        RcBlock::new(move |tracks, _error| {
            let ptr = unsafe { Retained::retain(tracks) }
                .map(Retained::into_raw)
                .unwrap_or_else(std::ptr::null_mut);
            let _ = tx.send(ptr);
        });
    // SAFETY: block outlives the call (held below until received); the
    // callback retains before sending, so the pointer stays valid.
    unsafe { asset.loadTracksWithMediaType_completionHandler(media_type, &block) };
    let ptr = rx
        .recv_timeout(Duration::from_secs(60))
        .map_err(|_| DecodeError::Apple("track load timed out".into()))?;
    drop(block);
    if ptr.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: non-null pointer retained in the callback; single ownership
    // transfer to this `Retained`.
    let retained: Retained<NSArray<AVAssetTrack>> =
        unsafe { Retained::from_raw(ptr) }.ok_or(DecodeError::InvalidPcm)?;
    Ok((0..retained.count())
        .map(|i| retained.objectAtIndex(i))
        .collect())
}

fn audio_tracks(asset: &AVURLAsset) -> Result<Vec<Retained<AVAssetTrack>>, DecodeError> {
    // SAFETY: framework-provided media-type constant (copied pointer).
    let mt_opt = unsafe { AVMediaTypeAudio };
    let mt = mt_opt
        .as_ref()
        .ok_or_else(|| DecodeError::Apple("AVMediaTypeAudio unavailable".into()))?;
    load_tracks(asset, mt)
}

/// Video timing without decoding frames: nominal rate + full
/// `AVSampleCursor` duration/PTS walk feeding the shared classifier.
/// Mirrors `VideoTimingInspector.inspect` (same thresholds, same early
/// evidence rules). Returns `None` when the asset has no video track.
fn video_probe(asset: &AVURLAsset) -> Result<Option<VideoProbe>, DecodeError> {
    use align_core::{RangeAccumulator, VideoFrameRateMode, canonical_frame_duration};

    let mt_opt = unsafe { AVMediaTypeVideo };
    let Some(mt) = mt_opt.as_ref() else {
        return Ok(None);
    };
    let tracks = load_tracks(asset, mt)?;
    let Some(track) = tracks.first() else {
        return Ok(None);
    };
    // SAFETY: plain-data property getters (may block on slow assets).
    let size: CGSize = unsafe { track.naturalSize() };
    let nominal = unsafe { track.nominalFrameRate() } as f64;
    let minimum = unsafe { track.minFrameDuration() };
    let fallback = (minimum.timescale > 0 && minimum.value > 0)
        .then(|| align_core::MediaTime::new(minimum.value, minimum.timescale));
    let frame_duration = canonical_frame_duration(nominal, fallback);

    let mode = if unsafe { track.canProvideSampleCursors() } {
        // SAFETY: cursor walk over plain-data CMTime getters.
        let range = unsafe { track.timeRange() };
        match unsafe { track.makeSampleCursorWithPresentationTimeStamp(range.start) } {
            None => VideoFrameRateMode::Unknown,
            Some(cursor) => unsafe {
                let mut durations = RangeAccumulator::new();
                let mut deltas = RangeAccumulator::new();
                let mut previous: Option<CMTime> = None;
                loop {
                    let duration = cursor.currentSampleDuration();
                    if is_numeric(duration) && duration.value > 0 {
                        durations.observe(duration.value as f64 / duration.timescale as f64);
                    }
                    let pts = cursor.presentationTimeStamp();
                    if let Some(prev) = previous {
                        if is_numeric(pts) && is_numeric(prev) {
                            // Plain-data arithmetic (outer block is unsafe).
                            let delta = pts.subtract(prev);
                            if is_numeric(delta) && delta.value > 0 {
                                deltas.observe(delta.value as f64 / delta.timescale as f64);
                            }
                        }
                    }
                    previous = Some(pts);
                    if cursor.stepInPresentationOrderByCount(1) != 1 {
                        break;
                    }
                }
                let enough = durations.count().max(deltas.count()) >= 2;
                if !enough {
                    VideoFrameRateMode::Unknown
                } else if durations.is_variable() || deltas.is_variable() {
                    VideoFrameRateMode::Variable
                } else {
                    VideoFrameRateMode::Constant
                }
            },
        }
    } else {
        VideoFrameRateMode::Unknown
    };
    Ok(Some(VideoProbe {
        width: size.width.abs().round() as u32,
        height: size.height.abs().round() as u32,
        frame_duration,
        mode,
        // Standard timecode track first (Sony tail enriches in pipeline).
        source_timecode: standard_timecode(asset),
    }))
}

/// Standard timecode track read: each tmcd sample is a big-endian frame
/// number (QuickTime File Format, Timecode media), interpreted as the
/// elapsed count at the description's true rate through the shared
/// `SourceTimecode` constructors — the same math and validation as the
/// ffprobe path. Returns None for files without a timecode track.
fn standard_timecode(asset: &AVURLAsset) -> Option<align_core::SourceTimecode> {
    // SAFETY: framework constant (copied pointer, bound locally).
    let mt_opt = unsafe { objc2_av_foundation::AVMediaTypeTimecode };
    let mt = mt_opt.as_ref()?;
    let tracks = load_tracks(asset, mt).ok()?;
    let track = tracks.first()?;
    // SAFETY: reader lifecycle mirrors open_reader (no gate needed: one
    // buffer, bounded work).
    let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(asset) }.ok()?;
    let output = unsafe {
        AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(track, None)
    };
    if unsafe { !reader.canAddOutput(&output) } {
        return None;
    }
    unsafe { reader.addOutput(&output) };
    if unsafe { !reader.startReading() } {
        return None;
    }
    // Take the first buffer carrying data (skip marker-only buffers).
    let sbuf = loop {
        let sbuf = (unsafe { output.copyNextSampleBuffer() })?;
        // SAFETY: plain-data getter on a live buffer.
        if unsafe { sbuf.data_buffer() }.is_some() {
            break sbuf;
        }
        unsafe { sbuf.invalidate() };
    };
    let result = (|| {
        // Track-level description (samples don't always carry one).
        let track_descs = unsafe { track.formatDescriptions() };
        if track_descs.count() == 0 {
            return None;
        }
        let first: Retained<AnyObject> = track_descs.objectAtIndex(0);
        // SAFETY: a timecode track's description is a
        // CMTimeCodeFormatDescription (same CF-pointer cast pattern as
        // the audio path); fields copied out immediately.
        let tc_desc: &CMTimeCodeFormatDescription =
            unsafe { &*(&*first as *const AnyObject as *const CMTimeCodeFormatDescription) };
        let block = unsafe { sbuf.data_buffer() }?;
        let total = unsafe { block.data_length() };
        if total < 4 {
            return None;
        }
        let mut word = [0u8; 4];
        let dest =
            unsafe { std::ptr::NonNull::new_unchecked(word.as_mut_ptr() as *mut std::ffi::c_void) };
        if unsafe { block.copy_data_bytes(0, 4, dest) } != 0 {
            return None;
        }
        let sample = u32::from_be_bytes(word);
        // SAFETY: plain-data getters.
        let duration = unsafe { CMTime::code_format_description_get_frame_duration(tc_desc) };
        if duration.timescale <= 0 {
            return None;
        }
        // A DF flag at a non-DF rate sanitizes to NDF inside the shared
        // constructor (the count stays authoritative).
        let duration = align_core::MediaTime::new(duration.value, duration.timescale);
        let flags = unsafe { CMTimeCodeFormatDescriptionGetTimeCodeFlags(tc_desc) };
        let drop_frame = flags & kCMTimeCodeFlag_DropFrame != 0;
        align_core::SourceTimecode::from_frame_number(sample as i64, duration, drop_frame)
    })();
    unsafe { sbuf.invalidate() };
    result
}

fn is_numeric(time: CMTime) -> bool {
    time.timescale != 0
        && time.flags.contains(CMTimeFlags::Valid)
        && !time.flags.intersects(
            CMTimeFlags::Indefinite | CMTimeFlags::PositiveInfinity | CMTimeFlags::NegativeInfinity,
        )
}
/// Native (rate, channels, bits, float?) from the track's first format
/// description. The array element is a `CMAudioFormatDescription`; the CF
/// pointer cast mirrors Swift's `as!` at the same call site.
fn stream_format(
    track: &AVAssetTrack,
) -> Result<(f64, usize, Option<u32>, Option<bool>), DecodeError> {
    let descs = unsafe { track.formatDescriptions() };
    if descs.count() == 0 {
        return Err(DecodeError::InvalidPcm);
    }
    let first: Retained<AnyObject> = descs.objectAtIndex(0);
    // SAFETY: element of an audio track's formatDescriptions is always a
    // CMAudioFormatDescription; raw CF pointers of distinct CF types share
    // representation, and we only read through it.
    let desc: &CMAudioFormatDescription =
        unsafe { &*(&*first as *const AnyObject as *const CMAudioFormatDescription) };
    let asbd_ptr = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(desc) };
    // SAFETY: non-null for audio descriptions (checked); fields copied out
    // immediately, no borrow retained.
    let asbd: AudioStreamBasicDescription = unsafe { asbd_ptr.as_ref() }
        .copied()
        .ok_or(DecodeError::InvalidPcm)?;
    if asbd.mSampleRate <= 0.0 || asbd.mChannelsPerFrame == 0 {
        return Err(DecodeError::InvalidPcm);
    }
    let is_float = if asbd.mFormatID == kAudioFormatLinearPCM {
        Some(asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0)
    } else {
        None
    };
    let bits = if asbd.mBitsPerChannel > 0 {
        Some(asbd.mBitsPerChannel)
    } else {
        None
    };
    Ok((
        asbd.mSampleRate,
        asbd.mChannelsPerFrame as usize,
        bits,
        is_float,
    ))
}

// ------------------------------------------------------------ reader

/// kAudioFormatLinearPCM + native rate/channels, f32 interleaved.
/// Key strings are created by value (AVFoundation compares string content,
/// so this is identical to referencing the missing `AVFormatIDKey` statics).
fn pcm_settings(
    sample_rate: f64,
    channels: usize,
    int32: bool,
) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys = [
        NSString::from_str("AVFormatIDKey"),
        NSString::from_str("AVSampleRateKey"),
        NSString::from_str("AVNumberOfChannelsKey"),
        NSString::from_str("AVLinearPCMBitDepthKey"),
        NSString::from_str("AVLinearPCMIsFloatKey"),
        NSString::from_str("AVLinearPCMIsBigEndianKey"),
        NSString::from_str("AVLinearPCMIsNonInterleaved"),
    ];
    let v_format = NSNumber::numberWithUnsignedInt(0x6C70636D); // kAudioFormatLinearPCM
    let v_rate = NSNumber::numberWithDouble(sample_rate);
    let v_channels = NSNumber::numberWithUnsignedInteger(channels);
    let v_32 = NSNumber::numberWithInt(32);
    let v_float = NSNumber::numberWithBool(!int32);
    let v_no = NSNumber::numberWithBool(false);
    let values: [&AnyObject; 7] = [
        &v_format,
        &v_rate,
        &v_channels,
        &v_32,
        &v_float,
        &v_no,
        &v_no,
    ];
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    NSDictionary::from_slices(&key_refs, &values)
}

struct Reader {
    reader: Retained<AVAssetReader>,
    output: Retained<AVAssetReaderTrackOutput>,
    channels: usize,
}

fn open_reader(
    path: &Path,
    stream_index: usize,
    window: Option<(f64, f64)>,
    int32: bool,
) -> Result<(Reader, f64), DecodeError> {
    let asset = open_asset(path)?;
    let tracks = audio_tracks(&asset)?;
    let track = tracks
        .get(stream_index)
        .ok_or_else(|| DecodeError::NoAudio(path.display().to_string()))?;
    let (sample_rate, channels, _, _) = stream_format(track)?;
    let settings = pcm_settings(sample_rate, channels, int32);
    // SAFETY: objects valid; reader creation copies settings.
    let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(&asset) }
        .map_err(|e| apple_error("AVAssetReader", Some(e)))?;
    // SAFETY: track belongs to reader's asset; settings dict well-formed.
    let output = unsafe {
        AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            track,
            Some(&settings),
        )
    };
    unsafe { output.setAlwaysCopiesSampleData(false) };
    if !unsafe { reader.canAddOutput(&output) } {
        return Err(DecodeError::UnsupportedOutput);
    }
    unsafe { reader.addOutput(&output) };
    if let Some((start, duration)) = window {
        // SAFETY: plain-data constructors; timescale/values finite.
        let range = unsafe {
            CMTimeRange::new(
                CMTime::new(
                    (start.max(0.0) * sample_rate).round() as i64,
                    sample_rate.round() as i32,
                ),
                CMTime::new(
                    (duration * sample_rate).round() as i64,
                    sample_rate.round() as i32,
                ),
            )
        };
        unsafe { reader.setTimeRange(range) };
    }
    if !unsafe { reader.startReading() } {
        let err = unsafe { reader.error() };
        return Err(apple_error("startReading", err));
    }
    Ok((
        Reader {
            reader,
            output,
            channels,
        },
        sample_rate,
    ))
}

fn check_completed(reader: &AVAssetReader, path: &Path) -> Result<(), DecodeError> {
    // SAFETY: status/error are thread-safe plain getters.
    let status = unsafe { reader.status() }.0;
    if status == 2 {
        // Completed.
        return Ok(());
    }
    let err = unsafe { reader.error() };
    Err(apple_error(
        &format!("reader status {status} for {}", path.display()),
        err,
    ))
}

/// Next data buffer: owned bytes; the first buffer's PTS (captured even
/// from marker-only buffers, like Swift's `actualStart`) flows through
/// `first_pts`. Marker-only buffers are skipped inside the loop. Owned
/// copy only — one memcpy per buffer is noise next to decode cost, and a
/// single path means fewer unsafe blocks than a zero-copy fast lane.
fn next_buffer(
    output: &AVAssetReaderTrackOutput,
    first_pts: &mut Option<f64>,
) -> Result<Option<Vec<u8>>, DecodeError> {
    loop {
        // SAFETY: output is reading; every taken buffer is invalidated below.
        let Some(sbuf) = (unsafe { output.copyNextSampleBuffer() }) else {
            return Ok(None);
        };
        if first_pts.is_none() {
            // SAFETY: plain-data getter on a live buffer.
            let pts = unsafe { sbuf.presentation_time_stamp() };
            if pts.timescale > 0 {
                *first_pts = Some(pts.value as f64 / pts.timescale as f64);
            }
        }
        let data = unsafe { sbuf.data_buffer() };
        // None = marker-only buffer (skipped below); InvalidPcm = corrupt.
        let result: Result<Option<Vec<u8>>, DecodeError> = (|| {
            let Some(block) = data.as_deref() else {
                return Ok(None);
            };
            // SAFETY: plain getter on a live buffer.
            let total = unsafe { block.data_length() };
            if total == 0 {
                return Ok(None);
            }
            let mut owned = vec![0u8; total];
            let dest = unsafe {
                std::ptr::NonNull::new_unchecked(owned.as_mut_ptr() as *mut std::ffi::c_void)
            };
            // SAFETY: dest is a valid writable allocation of total bytes.
            let status = unsafe { block.copy_data_bytes(0, total, dest) };
            if status != 0 {
                return Err(DecodeError::InvalidPcm);
            }
            Ok(Some(owned))
        })();
        unsafe { sbuf.invalidate() };
        match result {
            Ok(Some(bytes)) => return Ok(Some(bytes)),
            Ok(None) => continue, // marker-only buffer
            Err(e) => return Err(e),
        }
    }
}

// ------------------------------------------------------------ backend

pub struct AppleNativeBackend;

impl MediaBackend for AppleNativeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::AppleNative
    }

    fn inspect(&self, path: &Path) -> Result<ProbeReport, DecodeError> {
        let asset = open_asset(path)?;
        // SAFETY: plain-data property getter.
        let duration = unsafe { asset.duration() };
        if duration.timescale <= 0 {
            return Err(DecodeError::InvalidPcm);
        }
        let seconds = duration.value as f64 / duration.timescale as f64;
        // NaN/infinite durations are rejected (plain `<=` lets NaN through).
        if !seconds.is_finite() || seconds <= 0.0 {
            return Err(DecodeError::InvalidPcm);
        }
        let audio = audio_tracks(&asset)?;
        if audio.is_empty() {
            // Match Swift: a file with neither audio nor video is inaccessible.
            let no_video = video_probe(&asset)?.is_none();
            if no_video {
                return Err(DecodeError::NoAudio(path.display().to_string()));
            }
        }
        let mut audio_streams = Vec::with_capacity(audio.len());
        for track in &audio {
            let (sample_rate, channels, bit_depth, is_float) = stream_format(track)?;
            audio_streams.push(AudioStreamProbe {
                sample_rate,
                channels,
                bit_depth,
                is_float,
            });
        }
        let video = video_probe(&asset)?;
        Ok(ProbeReport {
            duration_seconds: seconds,
            audio_streams,
            has_video: video.is_some(),
            video,
        })
    }

    fn decode_mono_8k(
        &self,
        path: &Path,
        source: AudioAnalysisSource,
        consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError> {
        let (session, sample_rate) = open_reader(path, source.stream_index(), None, false)?;
        let mut pipe = crate::mono::MonoPipe::new(
            sample_rate,
            8000.0,
            session.channels,
            source.selected_channel(),
            source.mixes_channels(),
            consume,
        )?;
        let mut pts = None;
        while let Some(bytes) = next_buffer(&session.output, &mut pts)? {
            pipe.push_interleaved(&bytes_to_f32(&bytes, session.channels)?)?;
        }
        check_completed(&session.reader, path)?;
        pipe.finish()
    }

    fn decode_window_16k(
        &self,
        path: &Path,
        start_seconds: f64,
        duration_seconds: f64,
        source: AudioAnalysisSource,
    ) -> Result<(f64, Vec<f32>), DecodeError> {
        // AVAssetReader startup dominates sparse refinement windows. PCM/BWF
        // WAV is accurately seekable in-process through Symphonia and still
        // uses the identical shared mono/resample chain. Keep AVFoundation as
        // the fallback for unusual WAV variants and every media container.
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
            && let Ok(window) =
                crate::sym::decode_window(path, start_seconds, duration_seconds, source)
        {
            return Ok(window);
        }
        let (session, sample_rate) = open_reader(
            path,
            source.stream_index(),
            Some((start_seconds, duration_seconds)),
            false,
        )?;
        let want = (duration_seconds * 16_000.0).ceil() as usize + 64;
        let samples = std::cell::RefCell::new(Vec::with_capacity(want.min(16_000 * 12)));
        let mut pipe = crate::mono::MonoPipe::new(
            sample_rate,
            16_000.0,
            session.channels,
            source.selected_channel(),
            source.mixes_channels(),
            |s: &[f32]| {
                samples.borrow_mut().extend_from_slice(s);
                Ok(())
            },
        )?;
        // Presentation timestamps come from the buffers themselves (mirrors
        // Swift's `CMSampleBufferGetPresentationTimeStamp` actualStart).
        let mut actual_start = None::<f64>;
        while samples.borrow().len() < want {
            let Some(bytes) = next_buffer(&session.output, &mut actual_start)? else {
                break;
            };
            pipe.push_interleaved(&bytes_to_f32(&bytes, session.channels)?)?;
        }
        check_completed(&session.reader, path)?;
        pipe.finish()?;
        let mut samples = samples.into_inner();
        samples.truncate(want);
        Ok((actual_start.unwrap_or(start_seconds.max(0.0)), samples))
    }

    fn decode_native(
        &self,
        path: &Path,
        stream_index: usize,
        range: Option<(f64, Option<f64>)>,
        consume: &mut dyn FnMut(crate::backend::NativeBlock) -> Result<(), DecodeError>,
    ) -> Result<f64, DecodeError> {
        let window = range.map(|(start, duration)| (start, duration.unwrap_or(1e9)));
        let (session, sample_rate) = open_reader(path, stream_index, window, false)?;
        let mut actual = None;
        while let Some(bytes) = next_buffer(&session.output, &mut actual)? {
            let floats = bytes_to_f32(&bytes, session.channels)?;
            consume(crate::backend::NativeBlock {
                sample_rate,
                channels: session.channels,
                frames: crate::mono::deinterleave_f32(&floats, session.channels),
            })?;
        }
        check_completed(&session.reader, path)?;
        Ok(actual.unwrap_or(range.map_or(0.0, |(s, _)| s.max(0.0))))
    }

    fn decode_native_i32(
        &self,
        path: &Path,
        stream_index: usize,
        range: Option<(f64, Option<f64>)>,
        consume: &mut dyn FnMut(crate::backend::NativeBlockI32) -> Result<(), DecodeError>,
    ) -> Result<f64, DecodeError> {
        let window = range.map(|(start, duration)| (start, duration.unwrap_or(1e9)));
        let (session, sample_rate) = open_reader(path, stream_index, window, true)?;
        let mut actual = None;
        while let Some(bytes) = next_buffer(&session.output, &mut actual)? {
            if bytes.len() % (4 * session.channels) != 0 || bytes.is_empty() {
                return Err(DecodeError::InvalidPcm);
            }
            let ints: Vec<i32> = bytes
                .chunks_exact(4)
                .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            consume(crate::backend::NativeBlockI32 {
                sample_rate,
                channels: session.channels,
                frames: crate::mono::deinterleave_i32(&ints, session.channels),
            })?;
        }
        check_completed(&session.reader, path)?;
        Ok(actual.unwrap_or(range.map_or(0.0, |(s, _)| s.max(0.0))))
    }
}

/// Little-endian f32 frames from an interleaved PCM block.
fn bytes_to_f32(bytes: &[u8], channels: usize) -> Result<Vec<f32>, DecodeError> {
    if bytes.len() % (4 * channels) != 0 || bytes.is_empty() {
        return Err(DecodeError::InvalidPcm);
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

// ------------------------------------------------------------ traits used above
// (MonoPipe takes planar *or* interleaved; the reader vends interleaved.)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MediaBackend;

    fn fixture_wav(
        dir: &std::path::Path,
        name: &str,
        rate: u32,
        secs: u64,
        seed: u64,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut s = seed;
        let mut next = move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((s >> 33) as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..rate as u64 * secs {
            let v = next();
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        path
    }

    fn fixture_dir(tag: &str) -> std::path::PathBuf {
        // Unique per test: parallel tests share the process (and pid) and
        // must never delete each other's fixtures mid-decode.
        let dir = std::env::temp_dir().join(format!("align-apple-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn foundation_linkage_works() {
        let url = file_url(Path::new("/tmp/clip.wav")).expect("file url");
        assert!(url.isFileURL());
    }

    #[test]
    fn backend_kind_is_native() {
        assert_eq!(AppleNativeBackend.kind(), BackendKind::AppleNative);
    }

    #[test]
    fn standard_timecode_track_reads() {
        let ffmpeg = match crate::ff::ffmpeg_bin() {
            Some(b) => b,
            None => {
                eprintln!("SKIP: no ffmpeg binary");
                return;
            }
        };
        let dir = fixture_dir("tmcd");
        let mp4 = dir.join("tc.mp4");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=640x480:rate=25:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-timecode",
                "01:02:03:04",
                "-shortest",
            ])
            .arg(&mp4)
            .status()
            .expect("spawn ffmpeg");
        if !status.success() {
            eprintln!("SKIP: ffmpeg tmcd generation failed");
            return;
        }
        let probe = AppleNativeBackend.inspect(&mp4).expect("inspect");
        assert!(probe.has_video);
        let tc = probe
            .video
            .as_ref()
            .and_then(|v| v.source_timecode.clone())
            .expect("timecode");
        assert_eq!(tc.text, "01:02:03:04");
        assert!(!tc.drop_frame);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_frame_timecode_agrees_across_backends() {
        // Same 29.97 DF file through AVFoundation tmcd and ffprobe tags:
        // labels and elapsed must agree exactly (shared constructors).
        let ffmpeg = match crate::ff::ffmpeg_bin() {
            Some(b) => b,
            None => {
                eprintln!("SKIP: no ffmpeg binary");
                return;
            }
        };
        let dir = fixture_dir("tmcd-df");
        let mp4 = dir.join("tc-df.mp4");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=640x480:rate=30000/1001:duration=4",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=4",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-timecode",
                "01:00:00;00",
                "-shortest",
            ])
            .arg(&mp4)
            .status()
            .expect("spawn ffmpeg");
        if !status.success() {
            eprintln!("SKIP: ffmpeg DF tmcd generation failed");
            return;
        }
        let apple = AppleNativeBackend.inspect(&mp4).expect("apple inspect");
        let portable = crate::portable::PortableBackend
            .inspect(&mp4)
            .expect("portable inspect");
        let (Some(atc), Some(ptc)) = (
            apple.video.as_ref().and_then(|v| v.source_timecode.clone()),
            portable
                .video
                .as_ref()
                .and_then(|v| v.source_timecode.clone()),
        ) else {
            eprintln!("SKIP: DF tmcd not round-tripped by ffmpeg");
            return;
        };
        assert_eq!(atc, ptc, "apple={atc:?} portable={ptc:?}");
        assert!(atc.drop_frame);
        assert_eq!(atc.frame_duration, align_core::MediaTime::new(1001, 30_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inspect_matches_portable() {
        let dir = fixture_dir("probe");
        let wav = fixture_wav(&dir, "probe.wav", 44100, 3, 7);
        let apple = AppleNativeBackend.inspect(&wav).expect("apple inspect");
        let portable = crate::portable::PortableBackend
            .inspect(&wav)
            .expect("portable inspect");
        assert!((apple.duration_seconds - portable.duration_seconds).abs() < 1e-6);
        assert_eq!(apple.audio_streams, portable.audio_streams);
        assert_eq!(apple.has_video, portable.has_video);
        assert_eq!(apple.audio_streams.len(), 1);
        assert_eq!(apple.audio_streams[0].sample_rate, 44100.0);
        assert_eq!(apple.audio_streams[0].channels, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_decode_is_bit_identical_to_portable() {
        // Same PCM source + shared resample chain ⇒ identical streams.
        // This is the load-bearing conformance property: fingerprints and
        // caches stay comparable across engines.
        let dir = fixture_dir("stream");
        let wav = fixture_wav(&dir, "stream.wav", 48000, 5, 0xA);
        let mut a = Vec::new();
        AppleNativeBackend
            .decode_mono_8k(&wav, AudioAnalysisSource::Automatic, &mut |s| {
                a.extend_from_slice(s);
                Ok(())
            })
            .expect("apple decode");
        let mut p = Vec::new();
        crate::portable::PortableBackend
            .decode_mono_8k(&wav, AudioAnalysisSource::Automatic, &mut |s| {
                p.extend_from_slice(s);
                Ok(())
            })
            .expect("portable decode");
        assert_eq!(a.len(), p.len(), "lengths differ");
        assert_eq!(a, p, "streams differ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_matches_portable() {
        let dir = fixture_dir("win");
        let wav = fixture_wav(&dir, "win.wav", 44100, 10, 0xB);
        let (a_start, a_win) = AppleNativeBackend
            .decode_window_16k(&wav, 3.0, 2.0, AudioAnalysisSource::Automatic)
            .expect("apple window");
        let (p_start, p_win) = crate::portable::PortableBackend
            .decode_window_16k(&wav, 3.0, 2.0, AudioAnalysisSource::Automatic)
            .expect("portable window");
        assert_eq!(a_start, p_start);
        assert_eq!(a_win, p_win);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
