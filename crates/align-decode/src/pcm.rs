//! Direct linear-PCM reads from WAVE (RIFF, RF64, BW64) and QuickTime/MP4.
//!
//! Uncompressed camera audio (Sony, Panasonic, Canon, Blackmagic, ARRI) is
//! stored one frame per sample. Generic demuxing then yields one packet per
//! audio frame; reading whole chunks through the sample tables instead is
//! sample-accurate, seek-free and allocation-light. Stream indices count
//! every sound track in file order, like the header probe.
//!
//! Recorder files are read the same way. This also covers RF64/BW64, which
//! Symphonia does not demux and which Align itself writes for renders over
//! 4 GiB, so no external decoder is needed for any uncompressed WAVE.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::DecodeError;
use crate::isobmff::{self, TrackKind};

/// Frames converted per callback block.
const BLOCK_FRAMES: usize = 8_192;

/// Analysis borrows the planes so the next block reuses their capacity.
/// Native render callbacks take ownership of a block handed to the backend.
type ConsumePlanes<'a, T> = dyn FnMut(&mut Vec<Vec<T>>) -> Result<(), DecodeError> + 'a;

pub struct PcmTrack {
    file: File,
    pub sample_rate: f64,
    pub channels: usize,
    bits: u32,
    float: bool,
    big_endian: bool,
    /// (file offset, first frame, frame count) per chunk.
    chunks: Vec<(u64, u64, u64)>,
    total: u64,
    complete: bool,
}

#[derive(Default)]
struct PcmBuffer {
    bytes: Vec<u8>,
    samples: Vec<f32>,
    next: Option<u64>,
}

/// Interleave the requested physical tracks by ascending file offset. A
/// single file description (including its kernel read-ahead state) advances
/// through the movie. Each track has independent channel and SRC state.
pub(crate) fn fingerprint_tracks(
    path: &Path,
    sources: &[align_core::AudioAnalysisSource],
    consume: crate::backend::TrackConsumer<'_>,
) -> Option<Result<(), DecodeError>> {
    if !(2..=16).contains(&sources.len()) || !isobmff::is_candidate(path) {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let movie = isobmff::shared_movie_for_file(path, &mut file)?;
    let consume = std::cell::RefCell::new(consume);
    let mut tracks = Vec::new();
    for (slot, source) in sources.iter().enumerate() {
        let desc = movie.audio().nth(source.stream_index())?;
        let track = PcmTrack::from_movie_track(file.try_clone().ok()?, desc, &movie.media_ranges)?;
        if track.chunks.windows(2).any(|w| w[0].0 > w[1].0) || source.selected_channel().is_some() {
            return None;
        }
        let sink = &consume;
        let pipe = crate::mono::MonoPipe::new(
            track.sample_rate,
            8000.,
            track.channels,
            None,
            source.mixes_channels(),
            move |block: &[f32]| (sink.borrow_mut())(slot, block),
        )
        .ok()?;
        tracks.push((track, pipe, 0usize));
    }
    Some((|| {
        let mut buffer = PcmBuffer::default();
        while let Some((offset, count, slot)) = tracks
            .iter()
            .enumerate()
            .filter_map(|(slot, (track, _, index))| {
                track
                    .chunks
                    .get(*index)
                    .map(|&(offset, _, count)| (offset, count, slot))
            })
            .min()
        {
            let (track, pipe, index) = &mut tracks[slot];
            track.read_chunk(offset, count, None, &mut buffer, &mut |s| {
                pipe.push_interleaved(s)
            })?;
            *index += 1;
        }
        for (_, pipe, _) in tracks {
            pipe.finish()?;
        }
        Ok(())
    })())
}

impl PcmTrack {
    /// `None` when the stream is not plain interleaved PCM in a WAVE,
    /// QuickTime or MP4 file (callers then use the general decoder).
    pub fn open(path: &Path, stream_index: usize) -> Option<Self> {
        if let Some(wave) = Self::open_wave(path) {
            return (stream_index == 0).then_some(wave);
        }
        if !isobmff::is_candidate(path) {
            return None;
        }
        let mut file = File::open(path).ok()?;
        let movie = isobmff::shared_movie_for_file(path, &mut file)?;
        let track = movie
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .nth(stream_index)?;
        Self::from_movie_track(file, track, &movie.media_ranges)
    }

    fn from_movie_track(
        file: File,
        track: &isobmff::Track,
        ranges: &[std::ops::Range<u64>],
    ) -> Option<Self> {
        let audio = track.audio.as_ref()?;
        let (bits, float) = (audio.bit_depth?, audio.is_float?);
        if !matches!((bits, float), (16 | 24 | 32, false) | (32 | 64, true))
            || !track.linear_pcm_layout
            || audio.channels > 64
            || !track.composition_offsets.is_empty()
            || audio.sample_rate != f64::from(track.timescale)
            || track.time_to_sample.iter().any(|&(_, delta)| delta != 1)
            || !one_frame_per_sample(&track.sample_entry, audio.channels, bits, float)
        {
            return None;
        }
        let bytes_per_frame = audio.channels as u64 * bits as u64 / 8;
        let matching_sizes = match &track.sample_sizes {
            isobmff::SampleSizes::Constant { size, .. } => u64::from(*size) == bytes_per_frame,
            isobmff::SampleSizes::Table(sizes) => {
                sizes.iter().all(|&size| u64::from(size) == bytes_per_frame)
            }
        };
        if !matching_sizes
            || track.media_duration != track.sample_count()
            || track.media_duration != track.sample_sizes.count() as u64
        {
            return None;
        }
        let mut chunks = Vec::with_capacity(track.chunk_offsets.len());
        let mut frame = 0u64;
        for chunk in track.chunk_layout() {
            let (offset, count) = chunk?;
            let frames = u64::from(count);
            chunks.push((offset, frame, frames));
            frame += frames;
        }
        if frame != track.sample_count()
            || chunks.iter().any(|&(offset, _, frames)| {
                let i = ranges.partition_point(|r| r.end <= offset);
                !ranges.get(i).is_some_and(|r| {
                    r.start <= offset
                        && offset
                            .checked_add(frames * bytes_per_frame)
                            .is_some_and(|end| end <= r.end)
                })
            })
        {
            return None;
        }
        Some(Self {
            file,
            sample_rate: audio.sample_rate,
            channels: audio.channels,
            bits,
            float,
            big_endian: audio.big_endian,
            chunks,
            total: frame,
            complete: true,
        })
    }

    /// RIFF/RF64/BW64 WAVE with 16/24/32-bit integer or 32/64-bit float
    /// samples (`WAVE_FORMAT_EXTENSIBLE` included).
    fn open_wave(path: &Path) -> Option<Self> {
        let mut file = File::open(path).ok()?;
        let length = file.metadata().ok()?.len();
        let mut header = [0u8; 12];
        file.read_exact(&mut header).ok()?;
        let large = match &header[..4] {
            b"RIFF" => false,
            b"RF64" | b"BW64" => true,
            _ => return None,
        };
        if &header[8..] != b"WAVE" {
            return None;
        }
        let mut ds64_data = None;
        let mut format = None;
        let mut offset = 12u64;
        while offset + 8 <= length {
            file.seek(SeekFrom::Start(offset)).ok()?;
            let mut chunk = [0u8; 8];
            file.read_exact(&mut chunk).ok()?;
            let size = u32::from_le_bytes(chunk[4..].try_into().ok()?) as u64;
            let body = offset + 8;
            match &chunk[..4] {
                b"ds64" if large && size >= 24 => {
                    let mut sizes = [0u8; 16];
                    file.read_exact(&mut sizes).ok()?;
                    ds64_data = Some(u64::from_le_bytes(sizes[8..].try_into().ok()?));
                }
                b"fmt " if size >= 16 => {
                    let mut fmt = vec![0u8; size.min(40) as usize];
                    file.read_exact(&mut fmt).ok()?;
                    format = Some(fmt);
                }
                b"data" => {
                    let fmt = format?;
                    let field = |at: usize| u16::from_le_bytes([fmt[at], fmt[at + 1]]);
                    let mut tag = field(0);
                    if tag == 0xfffe {
                        if fmt.len() < 40
                            || field(16) < 22
                            || field(18) != field(14)
                            || fmt[26..40]
                                != [0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]
                        {
                            return None;
                        }
                        tag = field(24);
                    }
                    let channels = field(2) as usize;
                    let sample_rate = u32::from_le_bytes(fmt[4..8].try_into().ok()?);
                    let block_align = field(12) as u64;
                    if channels == 0
                        || channels > 64
                        || sample_rate == 0
                        || !block_align.is_multiple_of(channels as u64)
                    {
                        return None;
                    }
                    let bits = (block_align / channels as u64 * 8) as u32;
                    if bits != u32::from(field(14)) {
                        return None;
                    }
                    let float = match (tag, bits) {
                        (1, 16 | 24 | 32) => false,
                        (3, 32 | 64) => true,
                        _ => return None,
                    };
                    let declared = match (large, size) {
                        (true, 0xffff_ffff) => ds64_data?,
                        _ => size,
                    };
                    // Interrupted recordings keep a header larger than the file.
                    let frames = declared.min(length.saturating_sub(body)) / block_align;
                    return Some(Self {
                        file,
                        sample_rate: sample_rate as f64,
                        channels,
                        bits,
                        float,
                        big_endian: false,
                        chunks: vec![(body, 0, frames)],
                        total: frames,
                        complete: frames * block_align == declared,
                    });
                }
                _ => {}
            }
            let size = match (&chunk[..4], large, size) {
                (b"data", true, 0xffff_ffff) => ds64_data?,
                _ => size,
            };
            offset = body + size + (size & 1);
        }
        None
    }

    /// Decode one whole physical track in chunk order. Keep the source clock
    /// through channel selection, including windows crossing chunk boundaries.
    pub(crate) fn fingerprint(
        &mut self,
        source: align_core::AudioAnalysisSource,
        consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError> {
        if !self.complete {
            return Err(DecodeError::Incomplete("truncated WAVE data chunk".into()));
        }
        let selected = source.selected_channel();
        if selected.is_some_and(|channel| channel >= self.channels) {
            return Err(DecodeError::InvalidPcm);
        }
        let native_float = self.native_f32();
        #[cfg(target_os = "macos")]
        let stride = self.channels * (self.bits / 8) as usize;
        let single = selected.is_some() && !native_float;
        let mut pipe = crate::mono::MonoPipe::new(
            self.sample_rate,
            8000.,
            if single { 1 } else { self.channels },
            if single { Some(0) } else { selected },
            source.mixes_channels(),
            consume,
        )?;
        let mut buffer = PcmBuffer::default();
        for index in 0..self.chunks.len() {
            let (offset, _, count) = self.chunks[index];
            #[cfg(target_os = "macos")]
            if self.chunks.len() > 1 && index % 32 == 0 {
                use std::os::fd::AsRawFd;
                for &(pos, _, frames) in self.chunks.iter().skip(index).take(32) {
                    let hint = libc::radvisory {
                        ra_offset: pos as i64,
                        ra_count: (frames * stride as u64).min(i32::MAX as u64) as i32,
                    };
                    // SAFETY: valid descriptor and read-only advisory structure.
                    // Failure only disables this kernel prefetch hint.
                    unsafe {
                        libc::fcntl(self.file.as_raw_fd(), libc::F_RDADVISE, &hint);
                    }
                }
            }
            self.read_chunk(offset, count, selected, &mut buffer, &mut |s| {
                pipe.push_interleaved(s)
            })?;
        }
        pipe.finish()
    }

    fn converter(&self) -> Option<fn(&[u8]) -> f32> {
        Some(match (self.bits, self.float, self.big_endian) {
            (16, false, false) => scaled::<2, false>,
            (16, false, true) => scaled::<2, true>,
            (24, false, false) => scaled::<3, false>,
            (24, false, true) => scaled::<3, true>,
            (32, false, false) => scaled::<4, false>,
            (32, false, true) => scaled::<4, true>,
            (_, true, true) => |b| float(b, true) as f32,
            (_, true, false) => |b| float(b, false) as f32,
            _ => return None,
        })
    }

    fn read_chunk(
        &mut self,
        offset: u64,
        count: u64,
        selected: Option<usize>,
        buffer: &mut PcmBuffer,
        consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
    ) -> Result<(), DecodeError> {
        let width = (self.bits / 8) as usize;
        let stride = self.channels * width;
        let native_float = self.native_f32();
        let convert = self.converter().ok_or(DecodeError::InvalidPcm)?;
        if buffer.next != Some(offset) {
            self.file.seek(SeekFrom::Start(offset))?;
        }
        let samples = &mut buffer.samples;
        let bytes = &mut buffer.bytes;
        let mut remaining = count;
        while remaining > 0 {
            let n = remaining.min(32768.min((1024 * 1024 / stride) as u64)) as usize;
            if native_float {
                samples.resize(n * self.channels, 0.);
                // SAFETY: initialized, exclusively borrowed, aligned f32 storage.
                // Every native f32 bit pattern is valid, even on a short read.
                let dest = unsafe {
                    std::slice::from_raw_parts_mut(samples.as_mut_ptr().cast::<u8>(), n * stride)
                };
                self.file.read_exact(dest)?;
            } else {
                bytes.resize(n * stride, 0);
                self.file.read_exact(bytes)?;
                samples.clear();
                if self.bits == 24 {
                    // Specialize the packed 3-byte stride once per block.
                    // Explicit selection converts just one word per frame.
                    if self.big_endian {
                        unpack_24::<true>(bytes, self.channels, selected, samples);
                    } else {
                        unpack_24::<false>(bytes, self.channels, selected, samples);
                    }
                } else if let Some(channel) = selected {
                    let at = channel * width;
                    samples.extend(
                        bytes
                            .chunks_exact(stride)
                            .map(|f| convert(&f[at..at + width])),
                    );
                } else {
                    samples.extend(bytes.chunks_exact(width).map(convert));
                }
            }
            consume(samples)?;
            remaining -= n as u64;
        }
        buffer.next = Some(offset + count * stride as u64);
        Ok(())
    }

    fn native_f32(&self) -> bool {
        cfg!(target_endian = "little") && self.float && self.bits == 32 && !self.big_endian
    }

    /// Stream format and duration without decoding.
    pub fn probe(&self) -> crate::backend::ProbeReport {
        crate::backend::ProbeReport {
            duration_seconds: self.total as f64 / self.sample_rate,
            audio_streams: vec![crate::backend::AudioStreamProbe {
                sample_rate: self.sample_rate,
                channels: self.channels,
                bit_depth: Some(self.bits),
                is_float: Some(self.float),
                decoder_delay: 0.0,
                end_seconds: Some(self.total as f64 / self.sample_rate),
            }],
            has_video: false,
            video: None,
        }
    }

    /// Stream presented frames `[start, start + count)` as planar blocks,
    /// converting each sample with `convert` (monomorphized per layout).
    fn read_with<T: Copy, F: Fn(&[u8]) -> T>(
        &mut self,
        start: u64,
        count: Option<u64>,
        convert: F,
        consume: &mut ConsumePlanes<'_, T>,
    ) -> Result<(), DecodeError> {
        let first = start;
        let end = count.map_or(self.total, |n| first.saturating_add(n).min(self.total));
        if first >= end {
            return Ok(());
        }
        let width = (self.bits / 8) as usize;
        let frame_bytes = width * self.channels;
        let index = self
            .chunks
            .partition_point(|&(_, chunk_first, frames)| chunk_first + frames <= first);
        let mut planes: Vec<Vec<T>> = Vec::new();
        let mut bytes = Vec::new();
        let mut frame = first;
        for &(offset, chunk_first, frames) in &self.chunks[index..] {
            if frame >= end {
                break;
            }
            let skip = frame - chunk_first;
            let mut remaining = (frames - skip).min(end - frame) as usize;
            self.file
                .seek(SeekFrom::Start(offset + skip * frame_bytes as u64))?;
            while remaining > 0 {
                let take = remaining.min(BLOCK_FRAMES);
                bytes.resize(take * frame_bytes, 0);
                self.file.read_exact(&mut bytes)?;
                planes.resize_with(self.channels, Vec::new);
                for (channel, plane) in planes.iter_mut().enumerate() {
                    plane.clear();
                    plane.extend(
                        bytes[channel * width..]
                            .chunks(frame_bytes)
                            .map(|frame| convert(&frame[..width])),
                    );
                }
                consume(&mut planes)?;
                remaining -= take;
                frame += take as u64;
            }
        }
        Ok(())
    }

    pub fn read_f32(
        &mut self,
        start: u64,
        count: Option<u64>,
        consume: &mut ConsumePlanes<'_, f32>,
    ) -> Result<(), DecodeError> {
        const SCALE: f32 = 1.0 / 2_147_483_648.0;
        macro_rules! int {
            ($w:literal, $be:literal) => {
                self.read_with(
                    start,
                    count,
                    |s| integer::<$w, $be>(s) as f32 * SCALE,
                    consume,
                )
            };
        }
        match (self.bits, self.float, self.big_endian) {
            (16, false, false) => int!(2, false),
            (16, false, true) => int!(2, true),
            (24, false, false) => int!(3, false),
            (24, false, true) => int!(3, true),
            (32, false, false) => int!(4, false),
            (32, false, true) => int!(4, true),
            (_, true, big_endian) => {
                self.read_with(start, count, |s| float(s, big_endian) as f32, consume)
            }
            _ => Err(DecodeError::InvalidPcm),
        }
    }

    pub fn read_i32(
        &mut self,
        start: u64,
        count: Option<u64>,
        consume: &mut ConsumePlanes<'_, i32>,
    ) -> Result<(), DecodeError> {
        match (self.bits, self.float, self.big_endian) {
            (16, false, false) => self.read_with(start, count, integer::<2, false>, consume),
            (16, false, true) => self.read_with(start, count, integer::<2, true>, consume),
            (24, false, false) => self.read_with(start, count, integer::<3, false>, consume),
            (24, false, true) => self.read_with(start, count, integer::<3, true>, consume),
            (32, false, false) => self.read_with(start, count, integer::<4, false>, consume),
            (32, false, true) => self.read_with(start, count, integer::<4, true>, consume),
            (_, true, big_endian) => self.read_with(
                start,
                count,
                |s| {
                    (float(s, big_endian) * 2_147_483_648.0)
                        .round()
                        .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
                },
                consume,
            ),
            _ => Err(DecodeError::InvalidPcm),
        }
    }
}

/// QuickTime v1/v2 sound descriptions must describe one frame per sample.
fn one_frame_per_sample(entry: &[u8], channels: usize, bits: u32, float: bool) -> bool {
    let body = entry.get(8..).unwrap_or_default();
    let field = |at: usize| {
        body.get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    match body.get(8..10).map(|v| u16::from_be_bytes([v[0], v[1]])) {
        Some(0) => true,
        Some(1) => {
            field(28) == Some(1)
                && field(32) == Some(bits / 8)
                && field(36) == Some(channels as u32 * (bits / 8))
        }
        Some(2) => {
            field(60) == Some(1)
                && field(56) == Some(channels as u32 * (bits / 8))
                && field(52).is_some_and(|flags| {
                    // Packed, interleaved signed integer or floating point. Only
                    // byte order may vary; reject aligned-high, planar, etc.
                    flags & !2 == if float { 9 } else { 12 }
                })
        }
        _ => false,
    }
}

fn unpack_24<const BIG: bool>(
    bytes: &[u8],
    channels: usize,
    selected: Option<usize>,
    out: &mut Vec<f32>,
) {
    let words = bytes.as_chunks::<3>().0;
    if let Some(channel) = selected {
        out.extend(
            words[channel..]
                .iter()
                .step_by(channels)
                .map(|w| scaled::<3, BIG>(w)),
        );
    } else {
        out.extend(words.iter().map(|w| scaled::<3, BIG>(w)));
    }
}

#[inline]
fn scaled<const W: usize, const BIG: bool>(sample: &[u8]) -> f32 {
    integer::<W, BIG>(sample) as f32 * (1.0 / 2_147_483_648.0)
}

/// Signed `W`-byte integer sample widened to the top of an i32.
#[inline(always)]
fn integer<const W: usize, const BIG_ENDIAN: bool>(sample: &[u8]) -> i32 {
    let mut word = [0u8; 4];
    for (index, slot) in word[..W].iter_mut().enumerate() {
        *slot = if BIG_ENDIAN {
            sample[index]
        } else {
            sample[W - 1 - index]
        };
    }
    i32::from_be_bytes(word)
}

#[inline(always)]
fn float(sample: &[u8], big_endian: bool) -> f64 {
    match (sample.len(), big_endian) {
        (4, true) => f32::from_be_bytes(sample.try_into().unwrap_or_default()) as f64,
        (4, false) => f32::from_le_bytes(sample.try_into().unwrap_or_default()) as f64,
        (8, true) => f64::from_be_bytes(sample.try_into().unwrap_or_default()),
        _ => f64::from_le_bytes(sample.try_into().unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_widths_and_byte_orders_widen_to_i32() {
        assert_eq!(integer::<2, false>(&[0x01, 0x80]), i32::MIN + 0x0001_0000);
        assert_eq!(integer::<2, true>(&[0x80, 0x01]), i32::MIN + 0x0001_0000);
        assert_eq!(integer::<3, false>(&[0xff, 0xff, 0x7f]), 0x7fff_ff00);
        assert_eq!(integer::<3, true>(&[0x7f, 0xff, 0xff]), 0x7fff_ff00);
        assert_eq!(integer::<4, false>(&[1, 0, 0, 0x80]), i32::MIN + 1);
        assert_eq!(float(&0.25f32.to_be_bytes(), true), 0.25);
        assert_eq!(float(&(-0.5f64).to_le_bytes(), false), -0.5);
    }

    fn wave(large: bool, extensible: bool, samples: &[i32]) -> Vec<u8> {
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|s| s.to_le_bytes()[1..].to_vec())
            .collect();
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&(if extensible { 0xfffeu16 } else { 1 }).to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&48_000u32.to_le_bytes());
        fmt.extend_from_slice(&(48_000u32 * 6).to_le_bytes());
        fmt.extend_from_slice(&6u16.to_le_bytes());
        fmt.extend_from_slice(&24u16.to_le_bytes());
        if extensible {
            fmt.extend_from_slice(&22u16.to_le_bytes());
            fmt.extend_from_slice(&24u16.to_le_bytes());
            fmt.extend_from_slice(&3u32.to_le_bytes());
            fmt.extend_from_slice(&1u16.to_le_bytes());
            fmt.extend_from_slice(&[0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]);
        }
        let mut out = Vec::new();
        out.extend_from_slice(if large { b"RF64" } else { b"RIFF" });
        out.extend_from_slice(&u32::MAX.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        if large {
            out.extend_from_slice(b"ds64");
            out.extend_from_slice(&28u32.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
            out.extend_from_slice(&(data.len() as u64).to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(&fmt);
        out.extend_from_slice(b"data");
        let size = if large { u32::MAX } else { data.len() as u32 };
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn wave_variants_read_the_same_samples() {
        let dir = tempfile::tempdir().unwrap();
        let samples: Vec<i32> = (0..2_000)
            .map(|i| (i * 40_503 % 16_777_216 - 8_388_608) << 8)
            .collect();
        for (name, large, extensible) in [
            ("riff.wav", false, false),
            ("extensible.wav", false, true),
            ("rf64.wav", true, false),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, wave(large, extensible, &samples)).unwrap();
            let mut track = PcmTrack::open(&path, 0).unwrap_or_else(|| panic!("{name}"));
            assert!(PcmTrack::open(&path, 1).is_none());
            let probe = track.probe();
            assert_eq!(probe.audio_streams[0].channels, 2);
            assert_eq!(probe.audio_streams[0].bit_depth, Some(24));
            assert!((probe.duration_seconds - 1_000.0 / 48_000.0).abs() < 1e-12);
            let mut read = Vec::new();
            track
                .read_i32(100, Some(300), &mut |planes| {
                    read.extend(planes[0].iter().zip(&planes[1]).flat_map(|(l, r)| [*l, *r]));
                    Ok(())
                })
                .unwrap();
            assert_eq!(read, samples[200..800], "{name}");
        }
        // A truncated recording reads what exists instead of failing.
        let path = dir.path().join("truncated.wav");
        let bytes = wave(false, false, &samples);
        std::fs::write(&path, &bytes[..bytes.len() - 600]).unwrap();
        let mut interrupted = PcmTrack::open(&path, 0).unwrap();
        assert_eq!(interrupted.total, 900);
        assert!(matches!(
            interrupted.fingerprint(align_core::AudioAnalysisSource::Automatic, &mut |_| panic!(
                "incomplete data reached fingerprint cache"
            )),
            Err(DecodeError::Incomplete(_))
        ));
    }

    #[test]
    fn edited_or_reconfigured_pcm_tracks_require_a_timeline_decoder() {
        let Some(ffmpeg) = crate::ff::test_ffmpeg() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.mov");
        assert!(
            std::process::Command::new(ffmpeg)
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=duration=1:sample_rate=48000",
                    "-c:a",
                    "pcm_s24le",
                    "-use_editlist",
                    "1"
                ])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(PcmTrack::open(&path, 0).is_some());
        let data = std::fs::read(&path).unwrap();
        for (kind, offset, value) in [
            (b"elst", 16, 32768u32),
            (b"stsc", 20, 2),
            (b"stts", 16, 2),
            (b"stsz", 8, 2),
            (b"stco", 12, 0),
        ] {
            let mut corrupt = data.clone();
            let start = data.windows(4).position(|b| b == kind).unwrap() + offset;
            corrupt[start..start + 4].copy_from_slice(&value.to_be_bytes());
            let path = dir
                .path()
                .join(format!("{}.mov", std::str::from_utf8(kind).unwrap()));
            std::fs::write(&path, corrupt).unwrap();
            assert!(PcmTrack::open(&path, 0).is_none(), "{}", path.display());
        }
    }

    #[test]
    fn interleaved_movie_tracks_keep_independent_clocks_and_channel_state() {
        use crate::MediaBackend;
        use align_core::AudioAnalysisSource;
        let Some(ffmpeg) = crate::ff::test_ffmpeg() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("two.mov");
        assert!(
            std::process::Command::new(ffmpeg)
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "aevalsrc='sin(73*t)*lt(t,1)|cos(293*t)*gte(t,1)':s=48000:d=2",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=845:sample_rate=44100:duration=3",
                    "-map",
                    "0:a",
                    "-map",
                    "1:a",
                    "-c:a",
                    "pcm_s24le",
                    "-use_editlist",
                    "0"
                ])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let sources = [0, 1].map(|index| AudioAnalysisSource::Stream {
            index,
            channel: None,
        });
        let mut combined = [Vec::new(), Vec::new()];
        fingerprint_tracks(&path, &sources, &mut |i, s| {
            combined[i].extend_from_slice(s);
            Ok(())
        })
        .unwrap()
        .unwrap();
        let read = |i, source| {
            let mut samples = Vec::new();
            PcmTrack::open(&path, i)
                .unwrap()
                .fingerprint(source, &mut |s| {
                    samples.extend_from_slice(s);
                    Ok(())
                })
                .unwrap();
            samples
        };
        for i in 0..2 {
            assert_eq!(combined[i], read(i, sources[i]));
        }
        let left = read(0, AudioAnalysisSource::MixedStream(0));
        let right = read(1, AudioAnalysisSource::MixedStream(1));
        let expected: Vec<_> = right
            .iter()
            .enumerate()
            .map(|(i, r)| (left.get(i).unwrap_or(&0.) + r) * 0.5)
            .collect();
        let mut actual = Vec::new();
        crate::portable::PortableBackend
            .decode_mono_8k(&path, AudioAnalysisSource::AllMixed, &mut |s| {
                actual.extend_from_slice(s);
                Ok(())
            })
            .unwrap();
        assert_eq!(actual, expected);
        assert!(matches!(
            fingerprint_tracks(&path, &sources, &mut |_, _| Err(DecodeError::Cancelled)),
            Some(Err(DecodeError::Cancelled))
        ));
    }

    /// Every layout FFmpeg writes must decode to the same samples through
    /// the direct reader and through Symphonia/FFmpeg.
    #[test]
    fn direct_reads_match_reference_decoders() {
        let Some(ffmpeg) = crate::ff::test_ffmpeg() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        for codec in [
            "pcm_s16le",
            "pcm_s16be",
            "pcm_s24le",
            "pcm_s24be",
            "pcm_s32le",
            "pcm_f32le",
            "pcm_f32be",
            "pcm_f64le",
        ] {
            let clip = dir.path().join(format!("{codec}.mov"));
            let ok = std::process::Command::new(&ffmpeg)
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=s=64x36:r=25:d=2",
                ])
                .args([
                    "-f",
                    "lavfi",
                    "-i",
                    "aevalsrc='sin(2*PI*440*t)*0.7|sin(2*PI*660*t)*-0.4':s=48000:d=2",
                ])
                .args(["-c:v", "mpeg4", "-c:a", codec, "-shortest"])
                .arg(&clip)
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                eprintln!("SKIP {codec}");
                continue;
            }
            let mut track =
                PcmTrack::open(&clip, 0).unwrap_or_else(|| panic!("{codec} not direct"));
            assert_eq!(track.channels, 2);
            let mut direct = Vec::new();
            track
                .read_f32(12_345, Some(4_000), &mut |planes| {
                    direct.extend(planes[0].iter().zip(&planes[1]).map(|(l, r)| (*l, *r)));
                    Ok(())
                })
                .unwrap();
            let mut reference = Vec::new();
            let mut pipe = crate::ff::AudioPipe::spawn(
                &clip,
                0,
                Some((12_345.0 / 48_000.0, 4_000.0 / 48_000.0)),
                2,
            )
            .unwrap();
            pipe.pump(&mut |frames| {
                reference.extend(frames.as_chunks::<2>().0.iter().map(|f| (f[0], f[1])));
                Ok(())
            })
            .unwrap();
            assert_eq!(direct.len(), 4_000, "{codec}");
            for (index, (a, b)) in direct.iter().zip(&reference).enumerate() {
                assert!(
                    (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6,
                    "{codec} frame {index}: {a:?} vs {b:?}"
                );
            }
        }
    }
}
