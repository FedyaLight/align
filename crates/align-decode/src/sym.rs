//! Symphonia tier of the portable backend: streaming decode of WAV, AIFF,
//! MP3, ADTS, and MOV/MP4/M4A (AAC, ALAC, PCM). Other containers
//! (MTS/MXF/R3D) and MP4s Symphonia rejects fall through to the FFmpeg pipe
//! in [`crate::ff`].
//!
//! Audio-only: only tracks for which a decoder constructs are opened —
//! video tracks never decode (Symphonia 0.5 has no video decoders, so
//! `make()` failing is the filter). Sample flow per packet:
//! planar `AudioBuffer` → selected discrete channel → [`crate::mono`].
//! Full files are never resident: packets stream through a 128 KiB
//! resample block. A packet the decoder rejects (FFmpeg skips such packets
//! too) becomes silence of the same duration, so one damaged packet neither
//! stops the file nor shifts the audio after it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use symphonia::core::audio::{AudioBuffer, SignalSpec};
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

use crate::DecodeError;
use crate::backend::AudioStreamProbe;
use crate::pcm::PcmTrack;
use align_core::AudioAnalysisSource;

fn symphonia_error(error: SymphoniaError) -> DecodeError {
    DecodeError::Symphonia(error.to_string())
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// QuickTime and ISO media: Symphonia's MP4 reader, plus the movie header for
/// what that reader leaves out (AAC channel counts, edit lists).
fn is_iso_media(path: &Path) -> bool {
    matches!(extension(path).as_str(), "mov" | "mp4" | "m4a" | "m4v")
}

/// The container reader the file extension names. Symphonia 0.5 ignores the
/// probe hint and scans up to 1 MiB of any file for an MP3 or ADTS sync word;
/// inside MXF or MTS video it finds one and reports noise as audio, and when
/// the reader then rejects the match it may search to the end of the file.
/// Only elementary MP3 and ADTS streams, which can start with an ID3 tag,
/// are probed; every other container goes to FFmpeg.
fn container_reader(path: &Path, file: File) -> Result<Box<dyn FormatReader>, DecodeError> {
    use symphonia::default::formats::{AiffReader, IsoMp4Reader, WavReader};
    let extension = extension(path);
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let options = FormatOptions::default();
    if is_iso_media(path) {
        return Ok(Box::new(
            IsoMp4Reader::try_new(mss, &options).map_err(symphonia_error)?,
        ));
    }
    Ok(match extension.as_str() {
        "wav" | "wave" | "bwf" => {
            Box::new(WavReader::try_new(mss, &options).map_err(symphonia_error)?)
        }
        "aif" | "aiff" | "aifc" => {
            Box::new(AiffReader::try_new(mss, &options).map_err(symphonia_error)?)
        }
        "mp3" | "aac" => {
            symphonia::default::get_probe()
                .format(&Hint::new(), mss, &options, &MetadataOptions::default())
                .map_err(symphonia_error)?
                .format
        }
        _ => {
            return Err(DecodeError::Symphonia(format!(
                "{extension:?} files are decoded by FFmpeg"
            )));
        }
    })
}

/// Open + demux, returning the reader and the audio-track shortlist.
/// A track counts as audio iff a decoder constructs for it (video tracks
/// fail here and are never touched).
fn open(path: &Path) -> Result<(Box<dyn FormatReader>, Vec<u32>), DecodeError> {
    let mut file = File::open(path).map_err(DecodeError::Io)?;
    // Symphonia 0.5 recognizes RIFF WAVE but not RF64/BW64. Those files are
    // read by `crate::pcm` instead.
    let mut signature = [0u8; 4];
    if file.read_exact(&mut signature).is_ok() && matches!(&signature, b"RF64" | b"BW64") {
        return Err(DecodeError::Symphonia("RF64/BW64 is read directly".into()));
    }
    file.seek(SeekFrom::Start(0)).map_err(DecodeError::Io)?;
    let format = container_reader(path, file)?;
    let mut audio = Vec::new();
    for track in format.tracks() {
        if track.codec_params.codec == CODEC_TYPE_NULL {
            continue;
        }
        if symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .is_ok()
        {
            audio.push(track.id);
        }
    }
    if audio.is_empty() {
        return Err(DecodeError::NoAudio(path.display().to_string()));
    }
    Ok((format, audio))
}

fn track_by_stream(format: &dyn FormatReader, audio: &[u32], stream: usize) -> Option<Track> {
    let id = *audio.get(stream)?;
    format.tracks().iter().find(|t| t.id == id).cloned()
}

/// WAV fmt bits for stem fidelity (format tag + bits per sample).
/// Returns None for non-WAVE files; Symphonia demux stays authoritative
/// for rates/channels/duration.
fn wav_bits(path: &Path) -> Option<(u32, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header).ok()?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return None;
    }
    // Walk chunk headers to fmt (fmt is small and early; bounded scan).
    let mut offset = 12u64;
    for _ in 0..32 {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut chunk = [0u8; 8];
        file.read_exact(&mut chunk).ok()?;
        let size = u32::from_le_bytes(chunk[4..8].try_into().ok()?) as u64;
        if &chunk[0..4] == b"fmt " && size >= 16 {
            let mut fmt = vec![0u8; size.min(40) as usize];
            file.read_exact(&mut fmt).ok()?;
            let tag = u16::from_le_bytes(fmt[0..2].try_into().ok()?);
            let bits = u16::from_le_bytes(fmt[14..16].try_into().ok()?) as u32;
            if bits == 0 || bits > 32 {
                return None;
            }
            // WAVEFORMATEXTENSIBLE carries the real tag in the SubFormat GUID.
            let is_float = if tag == 0xFFFE && fmt.len() >= 40 {
                u32::from_le_bytes(fmt[24..28].try_into().ok()?) == 3
            } else {
                tag == 3
            };
            return Some((bits, is_float));
        }
        if size > 16 * 1024 * 1024 {
            return None;
        }
        offset += 8 + size + (size & 1);
    }
    None
}

/// Fast header probe: sample rates/channels/duration without decoding.
/// Duration needs `n_frames` (WAV/AIFF/FLAC…); stream formats without a
/// frame count report `None` and the caller falls back to ffprobe.
pub struct SymphoniaProbe {
    pub streams: Vec<AudioStreamProbe>,
    pub duration_seconds: Option<f64>,
}

pub fn inspect(path: &Path) -> Result<SymphoniaProbe, DecodeError> {
    let (format, audio) = open(path)?;
    let bits = wav_bits(path);
    let mut streams = Vec::with_capacity(audio.len());
    let mut duration_seconds = None;
    for id in &audio {
        let Some(track) = format.tracks().iter().find(|t| t.id == *id) else {
            // Listed by open() above; absence means concurrent mutation.
            return Err(DecodeError::InvalidPcm);
        };
        let p = &track.codec_params;
        streams.push(AudioStreamProbe {
            sample_rate: p.sample_rate.unwrap_or(0) as f64,
            channels: p.channels.map(|c| c.count()).unwrap_or(0),
            bit_depth: bits.map(|(b, _)| b),
            is_float: bits.map(|(_, f)| f),
        });
        if duration_seconds.is_none() {
            if let (Some(n), Some(rate)) = (p.n_frames, p.sample_rate) {
                if rate > 0 {
                    duration_seconds = Some(n as f64 / rate as f64);
                }
            }
        }
    }
    if streams
        .iter()
        .any(|s| s.sample_rate <= 0.0 || s.channels == 0)
    {
        return Err(DecodeError::Symphonia("unknown audio format".into()));
    }
    Ok(SymphoniaProbe {
        streams,
        duration_seconds,
    })
}

struct Stream {
    format: Box<dyn FormatReader>,
    track_id: u32,
    time_base: TimeBase,
    sample_rate: u32,
    channels: usize,
    /// Decoded frames before presentation time zero (MP4 edit list, e.g.
    /// AAC encoder priming). Symphonia 0.5 does not apply edit lists.
    lead: u64,
    /// Frames decoded before a seek target so lapped codecs settle.
    preroll: u64,
    decoder: Box<dyn Decoder>,
    scratch_spec: Option<SignalSpec>,
    scratch_cap: usize,
    scratch: AudioBuffer<f32>,
    /// Whether a decoded packet has been compared with the length its
    /// container records. PCM needs no comparison.
    timing_checked: bool,
    /// Shape of the last decoded packet: the frames of a damaged packet whose
    /// container records no duration, and the channels of its silence.
    last_frames: usize,
    last_channels: usize,
}

/// Outcome of reading the next packet of the selected track.
enum Step {
    /// Read the samples with `stream.decoder.last_decoded()`.
    Decoded,
    /// A damaged packet: this many frames of silence keep later audio at its
    /// recorded time.
    Silence(usize),
    End,
}

/// Frames covered by a packet duration in the track's time base.
fn packet_frames(time_base: TimeBase, sample_rate: u32, duration: u64) -> usize {
    let time = time_base.calc_time(duration);
    ((time.seconds as f64 + time.frac) * sample_rate as f64).round() as usize
}

fn decode_next(stream: &mut Stream) -> Result<Step, DecodeError> {
    loop {
        let packet = match stream.format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(Step::End);
            }
            Err(e) => return Err(symphonia_error(e)),
        };
        if packet.track_id() != stream.track_id {
            continue;
        }
        let recorded = (packet.dur() > 0)
            .then(|| packet_frames(stream.time_base, stream.sample_rate, packet.dur()));
        // Decoders read only the packet, so every error here describes this
        // packet: a truncated or corrupted bitstream ("unexpected end of
        // bitstream"), invalid data, or an element the decoder lacks. The
        // packet becomes silence of its recorded length.
        match stream.decoder.decode(&packet) {
            Ok(decoded) => {
                let frames = decoded.frames();
                stream.last_frames = frames;
                stream.last_channels = decoded.spec().channels.count();
                // A decoder that yields far fewer samples than the packet
                // spans is decoding another stream than the file holds:
                // HE-AAC gives half. Final packets may differ slightly either
                // way (PCM bookkeeping, AAC padding), which is fine.
                if let (false, Some(recorded)) = (stream.timing_checked, recorded) {
                    if frames * 4 < recorded * 3 {
                        return Err(DecodeError::Symphonia(format!(
                            "decoded {frames} of the {recorded} samples a packet records"
                        )));
                    }
                    stream.timing_checked = true;
                }
                return Ok(Step::Decoded);
            }
            Err(SymphoniaError::ResetRequired) => {
                stream.decoder.reset();
                continue;
            }
            Err(_) => return Ok(Step::Silence(recorded.unwrap_or(stream.last_frames))),
        }
    }
}

fn silence<T: Clone>(stream: &Stream, frames: usize, zero: T) -> Vec<Vec<T>> {
    let channels = match stream.last_channels {
        0 => stream.channels,
        decoded => decoded,
    };
    vec![vec![zero; frames]; channels]
}

fn open_stream(path: &Path, stream_index: usize) -> Result<Stream, DecodeError> {
    let (format, audio) = open(path)?;
    let track = track_by_stream(&*format, &audio, stream_index)
        .ok_or_else(|| DecodeError::NoAudio(path.display().to_string()))?;
    let decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| DecodeError::Symphonia(e.to_string()))?;
    // Codec time base is authoritative; PCM containers use 1/sample_rate.
    let time_base = track
        .codec_params
        .time_base
        .unwrap_or_else(|| TimeBase::new(1, 1));
    let sample_rate = track.codec_params.sample_rate.unwrap_or(0);
    // Symphonia's MP4 reader leaves the channel count of AAC tracks to the
    // decoder and ignores edit lists. The movie header (track ids are movie
    // track indices) supplies both, keeping camera AAC in-process.
    let movie_track = is_iso_media(path)
        .then(|| crate::isobmff::read(path))
        .flatten()
        .and_then(|movie| movie.tracks.into_iter().nth(track.id as usize));
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count())
        .or_else(|| Some(movie_track.as_ref()?.audio.as_ref()?.channels))
        .unwrap_or(0);
    // MP3's LAME header records the encoder and decoder delay, which
    // Symphonia reports and FFmpeg trims. Only the start is trimmed here:
    // Symphonia's gapless mode would also cut the end at a frame count that
    // files without a Xing header merely estimate.
    let lead = if is_iso_media(path) {
        movie_track
            .as_ref()
            .filter(|t| t.timescale > 0)
            .map_or(0, |t| {
                (t.media_start as f64 * sample_rate as f64 / t.timescale as f64).round() as u64
            })
    } else {
        u64::from(track.codec_params.delay.unwrap_or(0))
    };
    let is_pcm = symphonia::default::get_codecs()
        .get_codec(track.codec_params.codec)
        .is_some_and(|codec| codec.short_name.starts_with("pcm"));
    let preroll = if is_pcm { 0 } else { 4 * 1024 };
    if sample_rate == 0 || channels == 0 {
        return Err(DecodeError::Symphonia("unknown audio format".into()));
    }
    Ok(Stream {
        format,
        track_id: track.id,
        time_base,
        sample_rate,
        channels,
        lead,
        preroll,
        decoder,
        scratch_spec: None,
        scratch_cap: 0,
        scratch: AudioBuffer::unused(),
        timing_checked: is_pcm,
        last_frames: 0,
        last_channels: 0,
    })
}

/// Can Symphonia decode this stream? Used to pick the decode tier *before*
/// any sample flows, so the FFmpeg fallback never double-emits after a
/// partial Symphonia stream. Opening it is not enough: the first decodable
/// packet must also yield the samples its container records. HE-AAC does
/// not, since Symphonia decodes only its AAC core (half the samples), so
/// FFmpeg decodes it.
pub fn can_decode(path: &Path, stream_index: usize) -> bool {
    if PcmTrack::open(path, stream_index).is_some() {
        return true;
    }
    let Ok(mut stream) = open_stream(path, stream_index) else {
        return false;
    };
    // A few damaged packets at the start do not decide the tier.
    for _ in 0..16 {
        match decode_next(&mut stream) {
            Ok(Step::Decoded) => return true,
            Ok(Step::Silence(_)) => {}
            Ok(Step::End) | Err(_) => return false,
        }
    }
    false
}

/// Stream a whole file to 8 kHz mono through `consume` (fingerprint path).
/// Call [`can_decode`] first: an error can return *after* partial output
/// reached `consume`, so the tier must be fixed upfront.
pub fn decode_mono_8k(
    path: &Path,
    source: AudioAnalysisSource,
    consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
) -> Result<(), DecodeError> {
    if let Some(mut track) = PcmTrack::open(path, source.stream_index()) {
        let mut pipe = crate::mono::MonoPipe::new(
            track.sample_rate,
            8000.0,
            track.channels,
            source.selected_channel(),
            source.mixes_channels(),
            consume,
        )?;
        track.read_f32(0, None, &mut |planes| pipe.push_owned(&planes))?;
        return pipe.finish();
    }
    let mut stream = open_stream(path, source.stream_index())?;
    let explicit = source.selected_channel();
    let mut pipe = crate::mono::MonoPipe::new(
        stream.sample_rate as f64,
        8000.0,
        stream.channels,
        explicit,
        source.mixes_channels(),
        consume,
    )?;
    let mut skip = stream.lead;
    loop {
        match decode_packet_planes(&mut stream)? {
            None => break,
            Some(mut planes) => {
                trim_native_packet(&mut planes, &mut skip, None);
                let refs: Vec<&[f32]> = planes.iter().map(|v| v.as_slice()).collect();
                pipe.push_planar(&refs)?;
            }
        }
    }
    pipe.finish()
}

/// Decode one 16 kHz window (refine path). Returns actual start + samples.
pub fn decode_window(
    path: &Path,
    start: f64,
    duration: f64,
    source: AudioAnalysisSource,
) -> Result<(f64, Vec<f32>), DecodeError> {
    let actual_start = start.max(0.0);
    let want = (duration * 16_000.0).ceil() as usize + 64;
    if let Some(mut track) = PcmTrack::open(path, source.stream_index()) {
        let mut samples = Vec::with_capacity(want);
        let mut pipe = crate::mono::MonoPipe::new(
            track.sample_rate,
            16_000.0,
            track.channels,
            source.selected_channel(),
            source.mixes_channels(),
            |s: &[f32]| {
                samples.extend_from_slice(s);
                Ok(())
            },
        )?;
        let first = (actual_start * track.sample_rate).round() as u64;
        // Read past the window so the resampler's last outputs are filtered
        // from real audio, as with streamed packets, not from flush padding.
        let count = (want as f64 / 16_000.0 * track.sample_rate).ceil() as u64 + 8_192;
        track.read_f32(first, Some(count), &mut |planes| pipe.push_owned(&planes))?;
        pipe.finish()?;
        samples.truncate(want);
        return Ok((actual_start, samples));
    }
    let mut stream = open_stream(path, source.stream_index())?;
    let mut skip = position(&mut stream, actual_start)?;
    let explicit = source.selected_channel();
    // Shared collection: the closure owns the mutable borrow for the pipe's
    // lifetime, so length checks go through the cell (windows are small).
    let samples = std::cell::RefCell::new(Vec::with_capacity(want.min(16_000 * 12)));
    let mut pipe = crate::mono::MonoPipe::new(
        stream.sample_rate as f64,
        16_000.0,
        stream.channels,
        explicit,
        source.mixes_channels(),
        |s: &[f32]| {
            samples.borrow_mut().extend_from_slice(s);
            Ok(())
        },
    )?;
    // Pre-roll and seek-packet lead are discarded: the window starts at the
    // requested sample.
    loop {
        if samples.borrow().len() >= want {
            break;
        }
        match decode_packet_planes(&mut stream)? {
            None => break,
            Some(mut planes) => {
                trim_native_packet(&mut planes, &mut skip, None);
                let refs: Vec<&[f32]> = planes.iter().map(|v| v.as_slice()).collect();
                pipe.push_planar(&refs)?;
            }
        }
    }
    pipe.finish()?;
    let mut samples = samples.into_inner();
    samples.truncate(want);
    Ok((actual_start, samples))
}

/// Decode one packet into owned planar f32 channels. Owned (not borrowed)
/// so packet scratch can be reused without lifetime gymnastics; packets
/// are small (≤ a few thousand frames) and everything else stays
/// allocation-free.
fn decode_packet_planes(stream: &mut Stream) -> Result<Option<Vec<Vec<f32>>>, DecodeError> {
    use symphonia::core::audio::Signal;
    match decode_next(stream)? {
        Step::End => Ok(None),
        Step::Silence(frames) => Ok(Some(silence(stream, frames, 0.0))),
        Step::Decoded => {
            let decoded = stream.decoder.last_decoded();
            let spec = *decoded.spec();
            if stream.scratch_spec != Some(spec) || stream.scratch_cap != decoded.capacity() {
                stream.scratch = decoded.make_equivalent::<f32>();
                stream.scratch_spec = Some(spec);
                stream.scratch_cap = decoded.capacity();
            }
            decoded.convert(&mut stream.scratch);
            Ok(Some(
                (0..spec.channels.count())
                    .map(|ch| stream.scratch.chan(ch).to_vec())
                    .collect(),
            ))
        }
    }
}

// ------------------------------------------------------------ native render path

/// Position the stream for presentation time `start` (seconds). Seeks to
/// the pre-roll point (Accurate lands on/before it) and returns how many
/// decoded frames to discard before the requested sample.
fn position(stream: &mut Stream, start: f64) -> Result<u64, DecodeError> {
    let rate = stream.sample_rate as f64;
    let target = (start.max(0.0) * rate).round() as u64 + stream.lead;
    let seek_frame = target.saturating_sub(stream.preroll);
    if seek_frame == 0 {
        return Ok(target);
    }
    let seeked = stream
        .format
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::from(seek_frame as f64 / rate),
                track_id: Some(stream.track_id),
            },
        )
        .map_err(|e| DecodeError::Unseekable(e.to_string()))?;
    let t = stream.time_base.calc_time(seeked.actual_ts);
    let actual = ((t.seconds as f64 + t.frac) * rate).round() as u64;
    if actual > target {
        return Err(DecodeError::Unseekable(
            "seek passed requested sample".into(),
        ));
    }
    Ok(target - actual)
}

fn trim_native_packet<T>(planes: &mut [Vec<T>], skip: &mut u64, remaining: Option<u64>) {
    let n = planes.first().map_or(0, Vec::len);
    let lead = (*skip).min(n as u64) as usize;
    *skip -= lead as u64;
    let take = remaining.map_or(n - lead, |r| r.min((n - lead) as u64) as usize);
    for channel in planes {
        channel.drain(..lead);
        channel.truncate(take);
    }
}

/// Full-rate planar f32 streaming. `start`: seek target (0 = from head).
/// Discards seek-packet lead before callbacks and returns the selected sample time.
/// `limit_frames`: stop after this many selected frames (bounded stems).
pub fn decode_native(
    path: &Path,
    stream_index: usize,
    start: f64,
    limit_frames: Option<u64>,
    consume: &mut dyn FnMut(crate::backend::NativeBlock) -> Result<(), DecodeError>,
) -> Result<f64, DecodeError> {
    if let Some(mut track) = PcmTrack::open(path, stream_index) {
        let (rate, channels) = (track.sample_rate, track.channels);
        let wanted = (start.max(0.0) * rate).round();
        track.read_f32(wanted as u64, limit_frames, &mut |frames| {
            consume(crate::backend::NativeBlock {
                sample_rate: rate,
                channels,
                frames,
            })
        })?;
        return Ok(wanted / rate);
    }
    let mut stream = open_stream(path, stream_index)?;
    let wanted = (start.max(0.0) * stream.sample_rate as f64).round();
    let mut skip = position(&mut stream, start)?;
    let mut emitted = 0u64;
    loop {
        if limit_frames.is_some_and(|lim| emitted >= lim) {
            break;
        }
        match decode_packet_planes(&mut stream)? {
            None => break,
            Some(mut planes) => {
                trim_native_packet(
                    &mut planes,
                    &mut skip,
                    limit_frames.map(|n| n.saturating_sub(emitted)),
                );
                if planes.first().is_none_or(Vec::is_empty) {
                    continue;
                }
                emitted += planes.first().map_or(0, |p| p.len() as u64);
                consume(crate::backend::NativeBlock {
                    sample_rate: stream.sample_rate as f64,
                    channels: stream.channels,
                    frames: planes,
                })?;
            }
        }
    }
    Ok(wanted / stream.sample_rate as f64)
}

/// Full-rate planar i32 streaming. S32 buffers pass through bit-exactly;
/// other formats convert via f32 (exact for ≤24-bit integer).
pub fn decode_native_i32(
    path: &Path,
    stream_index: usize,
    start: f64,
    limit_frames: Option<u64>,
    consume: &mut dyn FnMut(crate::backend::NativeBlockI32) -> Result<(), DecodeError>,
) -> Result<f64, DecodeError> {
    use symphonia::core::audio::{AudioBufferRef, Signal};
    if let Some(mut track) = PcmTrack::open(path, stream_index) {
        let (rate, channels) = (track.sample_rate, track.channels);
        let wanted = (start.max(0.0) * rate).round();
        track.read_i32(wanted as u64, limit_frames, &mut |frames| {
            consume(crate::backend::NativeBlockI32 {
                sample_rate: rate,
                channels,
                frames,
            })
        })?;
        return Ok(wanted / rate);
    }
    let mut stream = open_stream(path, stream_index)?;
    let mut scratch_f32 = AudioBuffer::<f32>::unused();
    let wanted = (start.max(0.0) * stream.sample_rate as f64).round();
    let mut skip = position(&mut stream, start)?;
    let mut emitted = 0u64;
    loop {
        if limit_frames.is_some_and(|lim| emitted >= lim) {
            break;
        }
        let mut planes: Vec<Vec<i32>> = match decode_next(&mut stream)? {
            Step::End => break,
            Step::Silence(frames) => silence(&stream, frames, 0),
            Step::Decoded => {
                let decoded = stream.decoder.last_decoded();
                let nch = decoded.spec().channels.count();
                match &decoded {
                    AudioBufferRef::S32(buf) => (0..nch).map(|ch| buf.chan(ch).to_vec()).collect(),
                    _ => {
                        if scratch_f32.spec() != decoded.spec()
                            || scratch_f32.capacity() != decoded.capacity()
                        {
                            scratch_f32 = decoded.make_equivalent::<f32>();
                        }
                        decoded.convert(&mut scratch_f32);
                        (0..nch)
                            .map(|ch| {
                                scratch_f32
                                    .chan(ch)
                                    .iter()
                                    .map(|&v| {
                                        (v as f64 * 2_147_483_648.0)
                                            .round()
                                            .clamp(-2_147_483_648.0, 2_147_483_647.0)
                                            as i32
                                    })
                                    .collect()
                            })
                            .collect()
                    }
                }
            }
        };
        trim_native_packet(
            &mut planes,
            &mut skip,
            limit_frames.map(|n| n.saturating_sub(emitted)),
        );
        if planes.first().is_none_or(Vec::is_empty) {
            continue;
        }
        emitted += planes.first().map_or(0, |p| p.len() as u64);
        consume(crate::backend::NativeBlockI32 {
            sample_rate: stream.sample_rate as f64,
            channels: stream.channels,
            frames: planes,
        })?;
    }
    Ok(wanted / stream.sample_rate as f64)
}

#[cfg(test)]
mod native_range_tests {
    use super::*;

    #[test]
    fn native_seek_discards_packet_lead_for_float_and_integer() {
        let dir = std::env::temp_dir().join(format!("align-native-seek-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("ramp.wav");
        let mut writer = hound::WavWriter::create(
            &src,
            hound::WavSpec {
                channels: 2,
                sample_rate: 48000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for i in 0..4800i32 {
            writer.write_sample(i * 123456).unwrap();
            writer.write_sample(-i * 123456).unwrap();
        }
        writer.finalize().unwrap();
        let mut ints = Vec::new();
        decode_native_i32(&src, 0, 0.01, Some(960), &mut |block| {
            ints.extend(block.frames[0].iter().copied());
            assert!(
                block.frames[0]
                    .iter()
                    .zip(&block.frames[1])
                    .all(|(a, b)| *a == -*b)
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(ints, (480..1440).map(|i| i * 123456).collect::<Vec<_>>());
        let mut floats = Vec::new();
        decode_native(&src, 0, 0.01, Some(960), &mut |block| {
            floats.extend(block.frames[0].iter().copied());
            Ok(())
        })
        .unwrap();
        assert_eq!(floats.len(), 960);
        for (got, want) in floats.iter().zip(&ints) {
            assert!((*got - *want as f32 / 2147483648.0).abs() < 1e-7);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod edit_list_tests {
    use super::*;

    fn first_click(samples: &[f32]) -> Option<usize> {
        samples.iter().position(|s| s.abs() > 0.3)
    }

    /// Encoders prime their output: AAC in MP4 and M4A records it in the
    /// edit list, MP3 in its LAME header. In-process decoding must place
    /// audio exactly where FFmpeg (which applies both) does, from the head
    /// and after a seek.
    #[test]
    fn encoder_priming_matches_ffmpeg_timing() {
        let click = "aevalsrc='if(between(t,1.5,1.502),0.9,0)':s=48000:d=4";
        let dir = tempfile::tempdir().unwrap();
        let clips = [
            super::test_media::aac_clip(dir.path(), "camera.mp4", click),
            super::test_media::aac_clip(dir.path(), "voice.m4a", click),
            super::test_media::encoded_clip(
                dir.path(),
                "voice.mp3",
                click,
                &["-c:a", "libmp3lame", "-b:a", "192k"],
            ),
        ];
        for clip in clips.iter().flatten() {
            priming_case(clip);
        }
    }

    fn native_length(path: &Path) -> usize {
        let mut frames = 0;
        decode_native(path, 0, 0.0, None, &mut |block| {
            frames += block.frames[0].len();
            Ok(())
        })
        .unwrap();
        frames
    }

    fn ffmpeg_length(path: &Path) -> usize {
        let mut frames = 0;
        let mut pipe = crate::ff::AudioPipe::spawn(path, 0, None, 2).unwrap();
        pipe.pump(&mut |samples| {
            frames += samples.len() / 2;
            Ok(())
        })
        .unwrap();
        frames
    }

    /// Without a Xing header the frame count of a VBR MP3 is only an
    /// estimate from its first frames. Trimming the end by that estimate
    /// dropped the audio after a quiet stretch; the whole file must decode,
    /// and a frame count that contradicts the LAME header must not break it.
    #[test]
    fn mp3_frame_counts_never_cut_audio() {
        let dir = tempfile::tempdir().unwrap();
        // Loud, then silent, then loud: the estimate from the loud opening
        // is far too short.
        let source = "anoisesrc=d=4.5:r=44100:a=0.5,volume='if(between(t,0.5,3.8),0,1)':eval=frame";
        let Some(headerless) = super::test_media::encoded_clip(
            dir.path(),
            "headerless.mp3",
            source,
            &["-c:a", "libmp3lame", "-q:a", "2", "-write_xing", "0"],
        ) else {
            return;
        };
        let expected = ffmpeg_length(&headerless);
        let decoded = native_length(&headerless);
        assert!(
            decoded.abs_diff(expected) <= 1152,
            "{decoded} of {expected} frames"
        );

        let Some(tagged) = super::test_media::encoded_clip(
            dir.path(),
            "tagged.mp3",
            source,
            &["-c:a", "libmp3lame", "-q:a", "2"],
        ) else {
            return;
        };
        let full = native_length(&tagged);
        // The Xing/Info tag: identifier, flags, then the frame count.
        let mut data = std::fs::read(&tagged).unwrap();
        let tag = data
            .windows(4)
            .position(|w| w == b"Xing" || w == b"Info")
            .expect("Xing/Info tag");
        assert_eq!(data[tag + 7] & 1, 1, "the tag records a frame count");
        data[tag + 8..tag + 12].copy_from_slice(&1u32.to_be_bytes());
        std::fs::write(&tagged, data).unwrap();
        assert!(inspect(&tagged).is_ok());
        assert_eq!(native_length(&tagged), full);
    }

    fn priming_case(clip: &Path) {
        let clip = clip.to_path_buf();
        assert!(
            can_decode(&clip, 0),
            "{} must decode in-process",
            clip.display()
        );
        let reference = {
            let mut out = Vec::new();
            let mut pipe = crate::ff::AudioPipe::spawn(&clip, 0, None, 2).unwrap();
            pipe.pump(&mut |frames| {
                out.extend(frames.iter().step_by(2).copied());
                Ok(())
            })
            .unwrap();
            first_click(&out).unwrap()
        };
        assert!((71_990..72_010).contains(&reference), "{reference}");
        for start in [0.0, 0.7, 1.4] {
            let mut out = Vec::new();
            decode_native(&clip, 0, start, Some(96_000), &mut |block| {
                out.extend_from_slice(&block.frames[0]);
                Ok(())
            })
            .unwrap();
            let click = first_click(&out).unwrap() + (start * 48_000.0) as usize;
            assert!(
                click.abs_diff(reference) <= 2,
                "start {start}: {click} vs {reference}"
            );
        }
        let (window_start, window) =
            decode_window(&clip, 1.0, 1.0, AudioAnalysisSource::default()).unwrap();
        assert_eq!(window_start, 1.0);
        let click = first_click(&window).unwrap() as f64 / 16.0 + 1000.0;
        assert!((click - 1500.0).abs() < 0.3, "window click at {click} ms");
    }
}

/// AAC media made with FFmpeg, and damaged packets made by editing its
/// sample size table. Shared with the pipeline tests.
#[cfg(test)]
pub(crate) mod test_media {
    use std::path::{Path, PathBuf};

    /// Stereo AAC in MP4 from an FFmpeg `lavfi` source, or `None` without an
    /// AAC encoder.
    pub(crate) fn aac_clip(dir: &Path, name: &str, source: &str) -> Option<PathBuf> {
        // Without noise substitution, whose generator state would let one
        // skipped packet change every later packet.
        let codec = [
            "-movflags",
            "+faststart",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-aac_pns",
            "0",
        ];
        encoded_clip(dir, name, source, &codec)
    }

    /// Stereo audio from an FFmpeg `lavfi` source with the given codec
    /// arguments, or `None` without that encoder.
    pub(crate) fn encoded_clip(
        dir: &Path,
        name: &str,
        source: &str,
        codec: &[&str],
    ) -> Option<PathBuf> {
        let ffmpeg = crate::ff::ffmpeg_bin()?;
        let clip = dir.join(name);
        let ok = std::process::Command::new(ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", source, "-ac", "2"])
            .args(codec)
            .arg(&clip)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("SKIP: no encoder for {name}");
        }
        ok.then_some(clip)
    }

    /// Four seconds of pink noise.
    pub(crate) fn aac_noise(dir: &Path, name: &str, seed: u32) -> Option<PathBuf> {
        aac_clip(
            dir,
            name,
            &format!("anoisesrc=d=4:c=pink:r=48000:a=0.4:seed={seed}"),
        )
    }

    /// Rewrites the `stsz` entries of a single-track file.
    pub(crate) fn edit_sample_sizes(path: &Path, edit: impl FnOnce(&mut [u32])) {
        let mut data = std::fs::read(path).unwrap();
        let at = data.windows(4).position(|w| w == b"stsz").unwrap();
        let word = |data: &[u8], i: usize| u32::from_be_bytes(data[i..i + 4].try_into().unwrap());
        assert_eq!(word(&data, at + 8), 0, "variable sample sizes");
        let count = word(&data, at + 12) as usize;
        let first = at + 16;
        let mut sizes: Vec<u32> = (0..count).map(|i| word(&data, first + 4 * i)).collect();
        edit(&mut sizes);
        for (i, size) in sizes.iter().enumerate() {
            data[first + 4 * i..first + 4 * i + 4].copy_from_slice(&size.to_be_bytes());
        }
        std::fs::write(path, data).unwrap();
    }

    /// Doubles every `stts` duration of a single-track file.
    pub(crate) fn double_packet_durations(path: &Path) {
        let mut data = std::fs::read(path).unwrap();
        let at = data.windows(4).position(|w| w == b"stts").unwrap();
        let word = |data: &[u8], i: usize| u32::from_be_bytes(data[i..i + 4].try_into().unwrap());
        for entry in 0..word(&data, at + 8) as usize {
            let delta = at + 16 + 8 * entry;
            let doubled = word(&data, delta) * 2;
            data[delta..delta + 4].copy_from_slice(&doubled.to_be_bytes());
        }
        std::fs::write(path, data).unwrap();
    }

    /// Cuts packet `k` in half and gives the cut bytes to packet `k + 1`:
    /// both become unreadable, later packets keep their file offsets.
    pub(crate) fn truncate_packet(path: &Path, k: usize) {
        edit_sample_sizes(path, |sizes| {
            let cut = sizes[k] / 2;
            sizes[k] -= cut;
            sizes[k + 1] += cut;
        });
    }
}

#[cfg(test)]
mod damaged_packet_tests {
    use super::test_media::{aac_noise, double_packet_durations, truncate_packet};
    use super::*;

    fn decode_all(path: &Path) -> Result<Vec<f32>, DecodeError> {
        let mut samples = Vec::new();
        decode_native(path, 0, 0.0, None, &mut |block| {
            samples.extend_from_slice(&block.frames[0]);
            Ok(())
        })?;
        Ok(samples)
    }

    /// A truncated AAC packet makes Symphonia's decoder report "unexpected
    /// end of bitstream". It used to stop the whole file; it is silence of
    /// the packet's duration now, so later audio keeps its time.
    #[test]
    fn damaged_packet_becomes_silence_of_its_duration() {
        let dir = tempfile::tempdir().unwrap();
        let Some(clip) = aac_noise(dir.path(), "camera.mp4", 3) else {
            return;
        };
        let clean = decode_all(&clip).unwrap();
        let k = 80;
        truncate_packet(&clip, k);
        let damaged = decode_all(&clip).expect("damaged packets are skipped");
        assert_eq!(damaged.len(), clean.len(), "timeline length is kept");
        // Packets hold 1024 frames; the edit list trims 1024 priming frames.
        // The decoder's overlap mixes each packet with its predecessor.
        let before = (k - 2) * 1024;
        let after = (k + 4) * 1024;
        assert_eq!(damaged[..before], clean[..before]);
        assert_eq!(damaged[after..], clean[after..]);
        assert!(
            damaged[(k - 1) * 1024..k * 1024].iter().all(|s| *s == 0.0),
            "the truncated packet is silent"
        );
    }

    /// HE-AAC decodes to half the samples each packet records, because
    /// Symphonia has no SBR. Doubling the packet durations of AAC-LC gives
    /// Symphonia the same mismatch without an HE-AAC encoder.
    #[test]
    fn streams_decoded_shorter_than_their_packets_go_to_ffmpeg() {
        let dir = tempfile::tempdir().unwrap();
        let Some(clip) = aac_noise(dir.path(), "camera.mp4", 4) else {
            return;
        };
        assert!(can_decode(&clip, 0));
        double_packet_durations(&clip);
        assert!(!can_decode(&clip, 0), "FFmpeg decodes it");
        let error = decode_window(&clip, 1.0, 1.0, AudioAnalysisSource::default()).unwrap_err();
        assert!(
            error.to_string().contains("samples a packet records"),
            "{error}"
        );
    }

    /// Symphonia's AIFF reader can record a couple more frames for the final
    /// packet than it decodes. That is bookkeeping, not a wrong decoder, so
    /// exporting the last milliseconds must still work in process.
    #[test]
    fn short_final_pcm_packets_still_decode() {
        let Some(ffmpeg) = crate::ff::ffmpeg_bin() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let aiff = dir.path().join("recorder.aiff");
        let ok = std::process::Command::new(ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("anoisesrc=d=1:r=48000:a=0.3")
            .args(["-ac", "2", "-c:a", "pcm_s16be"])
            .arg(&aiff)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return;
        }
        let mut frames = 0;
        decode_native(&aiff, 0, 0.995, None, &mut |block| {
            frames += block.frames[0].len();
            Ok(())
        })
        .expect("the final packet decodes");
        assert_eq!(frames, 240);
    }

    /// Symphonia's probe ignores the extension and accepts MP3 sync words
    /// anywhere in the first megabyte, as compressed video essence contains.
    /// Only the containers the extension names are opened in process.
    #[test]
    fn mp3_frames_inside_other_containers_are_not_decoded_as_audio() {
        let dir = tempfile::tempdir().unwrap();
        // MPEG-1 Layer III, 128 kbit/s, 44.1 kHz: 417-byte silent frames.
        let frame = [[0xFF, 0xFB, 0x90, 0x00].as_slice(), &[0; 413]].concat();
        let frames = frame.repeat(100);
        let mp3 = dir.path().join("tone.mp3");
        std::fs::write(&mp3, &frames).unwrap();
        assert!(can_decode(&mp3, 0), "an MP3 file decodes in process");
        let mxf = dir.path().join("A001C001.MXF");
        std::fs::write(&mxf, [vec![0; 1000], frames].concat()).unwrap();
        assert!(!can_decode(&mxf, 0), "MXF goes to FFmpeg");
    }
}
