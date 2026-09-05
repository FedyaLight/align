//! Symphonia tier of the portable backend: probe + streaming decode for
//! everything Symphonia demuxes (WAV/AIFF/MP3/M4A-ALAC-AAC/OGG/FLAC/MKV…).
//! Containers it cannot handle (MTS/MXF/R3D, odd MP4s) fall through to the
//! FFmpeg pipe in [`crate::ff`].
//!
//! Audio-only: only tracks for which a decoder constructs are opened —
//! video tracks never decode (Symphonia 0.5 has no video decoders, so
//! `make()` failing is the filter). Sample flow per packet:
//! planar `AudioBuffer` → selected discrete channel → [`crate::mono`].
//! Full files are never resident: packets stream through a 128 KiB
//! resample block.

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
use align_core::AudioAnalysisSource;

/// Open + demux, returning the reader and the audio-track shortlist.
/// A track counts as audio iff a decoder constructs for it (video tracks
/// fail here and are never touched).
fn open(path: &Path) -> Result<(Box<dyn FormatReader>, Vec<u32>), DecodeError> {
    let mut file = File::open(path).map_err(DecodeError::Io)?;
    // Symphonia 0.5 recognizes RIFF WAVE but not RF64/BW64. Its generic
    // signature search otherwise scans a multi-gigabyte file before
    // allowing the portable backend's FFmpeg fallback to handle it.
    let mut signature = [0u8; 4];
    if file.read_exact(&mut signature).is_ok() && matches!(&signature, b"RF64" | b"BW64") {
        return Err(DecodeError::Symphonia("RF64/BW64 requires FFmpeg".into()));
    }
    file.seek(SeekFrom::Start(0)).map_err(DecodeError::Io)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| DecodeError::Symphonia(e.to_string()))?;
    let format = probed.format;
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
    decoder: Box<dyn Decoder>,
    scratch_spec: Option<SignalSpec>,
    scratch_cap: usize,
    scratch: AudioBuffer<f32>,
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
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(0);
    if sample_rate == 0 || channels == 0 {
        return Err(DecodeError::Symphonia("unknown audio format".into()));
    }
    Ok(Stream {
        format,
        track_id: track.id,
        time_base,
        sample_rate,
        channels,
        decoder,
        scratch_spec: None,
        scratch_cap: 0,
        scratch: AudioBuffer::unused(),
    })
}

/// Header-only check: can Symphonia demux this stream? Used to pick the
/// decode tier *before* any sample flows, so the FFmpeg fallback never
/// double-emits after a partial Symphonia stream.
pub fn can_decode(path: &Path, stream_index: usize) -> bool {
    open_stream(path, stream_index).is_ok()
}

/// Stream a whole file to 8 kHz mono through `consume` (fingerprint path).
/// Call [`can_decode`] first: mid-stream errors return `Err` *after* partial
/// output reached `consume`, so the tier must be fixed upfront.
pub fn decode_mono_8k(
    path: &Path,
    source: AudioAnalysisSource,
    consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
) -> Result<(), DecodeError> {
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
    loop {
        match decode_packet_planes(&mut stream)? {
            None => break,
            Some(planes) => {
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
    let mut stream = open_stream(path, source.stream_index())?;
    let seeked = stream
        .format
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::from(start.max(0.0)),
                track_id: Some(stream.track_id),
            },
        )
        .map_err(|e| DecodeError::Unseekable(e.to_string()))?;
    let seek_time = stream.time_base.calc_time(seeked.actual_ts);
    let actual_start = seek_time.seconds as f64 + seek_time.frac;
    let explicit = source.selected_channel();
    let want = (duration * 16_000.0).ceil() as usize + 64;
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
    // Accurate seeks land on/before target; leading audio is truthfully
    // included (window.start reports it) and bounded by the want-cap.
    loop {
        if samples.borrow().len() >= want {
            break;
        }
        match decode_packet_planes(&mut stream)? {
            None => break,
            Some(planes) => {
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
    loop {
        let packet = match stream.format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(e) => return Err(DecodeError::Symphonia(e.to_string())),
        };
        if packet.track_id() != stream.track_id {
            continue;
        }
        let decoded = match stream.decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::ResetRequired) => {
                stream.decoder.reset();
                continue;
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(DecodeError::Symphonia(e.to_string())),
        };
        let spec = *decoded.spec();
        if stream.scratch_spec != Some(spec) || stream.scratch_cap != decoded.capacity() {
            stream.scratch = decoded.make_equivalent::<f32>();
            stream.scratch_spec = Some(spec);
            stream.scratch_cap = decoded.capacity();
        }
        decoded.convert(&mut stream.scratch);
        let planes: Vec<Vec<f32>> = (0..spec.channels.count())
            .map(|ch| stream.scratch.chan(ch).to_vec())
            .collect();
        return Ok(Some(planes));
    }
}

// ------------------------------------------------------------ native render path

/// Seek helper shared by window + native decode. Returns actual stream
/// time (Accurate lands on/before target).
fn seek_to(stream: &mut Stream, start: f64) -> Result<f64, DecodeError> {
    let seeked = stream
        .format
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::from(start.max(0.0)),
                track_id: Some(stream.track_id),
            },
        )
        .map_err(|e| DecodeError::Unseekable(e.to_string()))?;
    let t = stream.time_base.calc_time(seeked.actual_ts);
    Ok(t.seconds as f64 + t.frac)
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
    let mut stream = open_stream(path, stream_index)?;
    let actual = if start > 0.0 {
        seek_to(&mut stream, start).unwrap_or(0.0)
    } else {
        0.0
    };
    let wanted = (start.max(0.0) * stream.sample_rate as f64).round();
    let actual_frame = (actual * stream.sample_rate as f64).round();
    if actual_frame > wanted {
        return Err(DecodeError::Unseekable(
            "native seek passed requested sample".into(),
        ));
    }
    let mut skip = (wanted - actual_frame) as u64;
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
    let mut stream = open_stream(path, stream_index)?;
    let actual = if start > 0.0 {
        seek_to(&mut stream, start).unwrap_or(0.0)
    } else {
        0.0
    };
    let mut scratch_f32 = AudioBuffer::<f32>::unused();
    let wanted = (start.max(0.0) * stream.sample_rate as f64).round();
    let actual_frame = (actual * stream.sample_rate as f64).round();
    if actual_frame > wanted {
        return Err(DecodeError::Unseekable(
            "native seek passed requested sample".into(),
        ));
    }
    let mut skip = (wanted - actual_frame) as u64;
    let mut emitted = 0u64;
    loop {
        if limit_frames.is_some_and(|lim| emitted >= lim) {
            break;
        }
        let packet = match stream.format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => return Err(DecodeError::Symphonia(e.to_string())),
        };
        if packet.track_id() != stream.track_id {
            continue;
        }
        let decoded = match stream.decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::ResetRequired) => {
                stream.decoder.reset();
                continue;
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(DecodeError::Symphonia(e.to_string())),
        };
        let spec = *decoded.spec();
        let nch = spec.channels.count();
        let mut planes: Vec<Vec<i32>> = match &decoded {
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
