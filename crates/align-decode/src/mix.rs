//! Explicit mixing across every audio stream for waveform analysis.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use align_core::AudioAnalysisSource;

use crate::DecodeError;
use crate::backend::MediaBackend;

const SAMPLE_RATE_16K: f64 = 16_000.0;
const IO_SAMPLES: usize = 32_768;

pub(crate) fn decode_all_streams_mono_8k(
    backend: &dyn MediaBackend,
    path: &Path,
    stream_count: usize,
    consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
) -> Result<(), DecodeError> {
    if stream_count == 0 {
        return Err(DecodeError::NoAudio(path.display().to_string()));
    }
    if stream_count == 1 {
        return backend.decode_mono_8k(path, AudioAnalysisSource::MixedStream(0), consume);
    }

    // Decode streams sequentially so one clip still occupies one media-reader
    // slot. The temporary sum bounds RAM regardless of recording length.
    let mut sum = tempfile::tempfile()?;
    let mut max_samples = 0_u64;
    for index in 0..stream_count {
        let mut cursor = 0_u64;
        let mut bytes = Vec::new();
        backend.decode_mono_8k(
            path,
            AudioAnalysisSource::MixedStream(index),
            &mut |samples| {
                if samples.is_empty() {
                    return Ok(());
                }
                bytes.resize(samples.len() * 4, 0);
                sum.seek(SeekFrom::Start(cursor * 4))?;
                let mut read = 0;
                while read < bytes.len() {
                    match sum.read(&mut bytes[read..])? {
                        0 => break,
                        count => read += count,
                    }
                }
                sum.seek(SeekFrom::Start(cursor * 4))?;
                for (sample_index, sample) in samples.iter().enumerate() {
                    let offset = sample_index * 4;
                    let previous = if offset + 4 <= read {
                        f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
                    } else {
                        0.0
                    };
                    bytes[offset..offset + 4].copy_from_slice(&(previous + sample).to_le_bytes());
                }
                sum.write_all(&bytes)?;
                cursor += samples.len() as u64;
                max_samples = max_samples.max(cursor);
                Ok(())
            },
        )?;
    }

    sum.seek(SeekFrom::Start(0))?;
    let scale = 1.0 / stream_count as f32;
    let mut remaining = max_samples;
    let mut bytes = vec![0_u8; IO_SAMPLES * 4];
    let mut samples = Vec::with_capacity(IO_SAMPLES);
    while remaining > 0 {
        let count = usize::try_from(remaining.min(IO_SAMPLES as u64)).unwrap();
        sum.read_exact(&mut bytes[..count * 4])?;
        samples.clear();
        samples.extend(
            bytes[..count * 4]
                .chunks_exact(4)
                .map(|value| f32::from_le_bytes(value.try_into().unwrap()) * scale),
        );
        consume(&samples)?;
        remaining -= count as u64;
    }
    Ok(())
}

pub(crate) fn decode_all_streams_window_16k(
    backend: &dyn MediaBackend,
    path: &Path,
    stream_count: usize,
    start: f64,
    duration: f64,
) -> Result<(f64, Vec<f32>), DecodeError> {
    if stream_count == 0 {
        return Err(DecodeError::NoAudio(path.display().to_string()));
    }
    if stream_count == 1 {
        return backend.decode_window_16k(
            path,
            start,
            duration,
            AudioAnalysisSource::MixedStream(0),
        );
    }

    let mut windows = Vec::with_capacity(stream_count);
    for index in 0..stream_count {
        windows.push(backend.decode_window_16k(
            path,
            start,
            duration,
            AudioAnalysisSource::MixedStream(index),
        )?);
    }
    let actual_start = windows
        .iter()
        .map(|(window_start, _)| *window_start)
        .fold(f64::INFINITY, f64::min);
    let offsets: Vec<usize> = windows
        .iter()
        .map(|(window_start, _)| {
            ((window_start - actual_start) * SAMPLE_RATE_16K)
                .round()
                .max(0.0) as usize
        })
        .collect();
    let output_len = windows
        .iter()
        .zip(&offsets)
        .map(|((_, samples), offset)| offset + samples.len())
        .max()
        .unwrap_or(0);
    let mut mixed = vec![0.0_f32; output_len];
    let scale = 1.0 / stream_count as f32;
    for ((_, samples), offset) in windows.into_iter().zip(offsets) {
        for (output, sample) in mixed[offset..].iter_mut().zip(samples) {
            *output += sample * scale;
        }
    }
    Ok((actual_start, mixed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendKind, ProbeReport};

    struct FakeBackend;

    impl MediaBackend for FakeBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Portable
        }

        fn inspect(&self, _: &Path) -> Result<ProbeReport, DecodeError> {
            unreachable!()
        }

        fn decode_mono_8k(
            &self,
            _: &Path,
            source: AudioAnalysisSource,
            consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
        ) -> Result<(), DecodeError> {
            let index = source.stream_index();
            consume(if index == 0 {
                &[0.2, 0.4, 0.6]
            } else {
                &[0.4, 0.2]
            })
        }

        fn decode_window_16k(
            &self,
            _: &Path,
            _: f64,
            _: f64,
            source: AudioAnalysisSource,
        ) -> Result<(f64, Vec<f32>), DecodeError> {
            Ok(if source.stream_index() == 0 {
                (1.0, vec![0.2, 0.4, 0.6])
            } else {
                (1.0 + 1.0 / SAMPLE_RATE_16K, vec![0.4, 0.2])
            })
        }

        fn decode_native(
            &self,
            _: &Path,
            _: usize,
            _: Option<(f64, Option<f64>)>,
            _: &mut dyn FnMut(crate::backend::NativeBlock) -> Result<(), DecodeError>,
        ) -> Result<f64, DecodeError> {
            unreachable!()
        }

        fn decode_native_i32(
            &self,
            _: &Path,
            _: usize,
            _: Option<(f64, Option<f64>)>,
            _: &mut dyn FnMut(crate::backend::NativeBlockI32) -> Result<(), DecodeError>,
        ) -> Result<f64, DecodeError> {
            unreachable!()
        }
    }

    #[test]
    fn whole_file_mix_averages_and_zero_pads_streams() {
        let mut actual = Vec::new();
        decode_all_streams_mono_8k(&FakeBackend, Path::new("fake"), 2, &mut |samples| {
            actual.extend_from_slice(samples);
            Ok(())
        })
        .unwrap();
        assert_eq!(actual, vec![0.3, 0.3, 0.3]);
    }

    #[test]
    fn window_mix_aligns_reported_starts() {
        let (start, actual) =
            decode_all_streams_window_16k(&FakeBackend, Path::new("fake"), 2, 1.0, 1.0).unwrap();
        assert_eq!(start, 1.0);
        assert_eq!(actual, vec![0.1, 0.4, 0.4]);
    }

    #[test]
    fn portable_backend_mixes_real_container_streams() {
        let Some(ffmpeg) = crate::ff::ffmpeg_bin() else {
            eprintln!("SKIP: no ffmpeg binary");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("two-streams.mov");
        let status = std::process::Command::new(ffmpeg)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=0.5",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:sample_rate=48000:duration=0.5",
                "-map",
                "0:a",
                "-map",
                "1:a",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&media)
            .status()
            .unwrap();
        if !status.success() {
            eprintln!("SKIP: ffmpeg cannot create multistream MOV");
            return;
        }

        let backend = crate::portable::PortableBackend;
        assert_eq!(backend.inspect(&media).unwrap().audio_streams.len(), 2);
        let decode = |source| {
            let mut samples = Vec::new();
            backend
                .decode_mono_8k(&media, source, &mut |block| {
                    samples.extend_from_slice(block);
                    Ok(())
                })
                .unwrap();
            samples
        };
        let first = decode(AudioAnalysisSource::MixedStream(0));
        let second = decode(AudioAnalysisSource::MixedStream(1));
        let mixed = decode(AudioAnalysisSource::AllMixed);
        assert_eq!(first.len(), second.len());
        assert_eq!(mixed.len(), first.len());
        for ((actual, first), second) in mixed.iter().zip(first).zip(second) {
            assert!((actual - (first + second) * 0.5).abs() < 1e-7);
        }
    }
}
