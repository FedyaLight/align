//! Broadcast Wave metadata preservation for corrected audio and stems.
//!
//! After rendering, the source file's `bext` is transplanted into the
//! render: fixed 602-byte prefix preserved, `TimeReference` shifted by the
//! trim offset, and a `T=Align <operation>` line appended to CodingHistory
//! (CRLF-normalized). Peak caches, cues and unknown device chunks are
//! deliberately NOT copied (they describe stretched-away samples).
//! Files without `bext` pass through untouched.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::model::file_name;
use crate::staged::StagedFile;

const MAX_BEXT: u64 = 16 * 1024 * 1024;
const COPY_BUF: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error("Cannot read Broadcast Wave metadata from {file}: {source}")]
    Read {
        file: String,
        #[source]
        source: io::Error,
    },
    #[error("Cannot write Broadcast Wave metadata: {0}")]
    Write(#[source] io::Error),
    #[error("Invalid Broadcast Wave metadata: {0}.")]
    Invalid(&'static str),
}

/// The speaker mask of a `WAVE_FORMAT_EXTENSIBLE` file, so audio rendered
/// from it keeps its channel layout. `None` for other files.
pub fn channel_mask(path: &Path) -> Option<u32> {
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut header = [0u8; 12];
    file.read_exact(&mut header).ok()?;
    if !matches!(&header[0..4], b"RIFF" | b"RF64" | b"BW64") || &header[8..12] != b"WAVE" {
        return None;
    }
    let mut offset = 12u64;
    while size.saturating_sub(offset) >= 8 {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut chunk = [0u8; 8];
        file.read_exact(&mut chunk).ok()?;
        let length = u64::from(u32::from_le_bytes(chunk[4..8].try_into().unwrap()));
        if &chunk[0..4] == b"fmt " {
            if length < 24 {
                return None;
            }
            let mut format = [0u8; 24];
            file.read_exact(&mut format).ok()?;
            let tag = u16::from_le_bytes([format[0], format[1]]);
            return (tag == 0xFFFE).then(|| u32::from_le_bytes(format[20..24].try_into().unwrap()));
        }
        if &chunk[0..4] == b"data" {
            return None;
        }
        offset += 8 + length + (length & 1);
    }
    None
}

/// Copy `bext` from `source` into already-rendered `target`.
pub fn preserve_broadcast_extension(
    source: &Path,
    target: &Path,
    sample_rate: f64,
    channels: usize,
    bit_depth: u32,
    operation: &str,
    time_reference_offset: i64,
) -> Result<(), WavError> {
    let Some(extension) = read_bext(source)? else {
        return Ok(());
    };
    let payload = updated_bext(
        &extension,
        sample_rate,
        channels,
        bit_depth,
        operation,
        time_reference_offset,
    )?;
    insert_bext(&payload, target)
}

fn read_bext(path: &Path) -> Result<Option<Vec<u8>>, WavError> {
    let read = |source| WavError::Read {
        file: file_name(path),
        source,
    };
    let mut file = std::fs::File::open(path).map_err(read)?;
    let size = file.metadata().map_err(read)?.len();
    let mut header = [0u8; 12];
    file.read_exact(&mut header).map_err(read)?;
    if !matches!(&header[0..4], b"RIFF" | b"RF64" | b"BW64") || &header[8..12] != b"WAVE" {
        return Ok(None);
    }
    let mut offset = 12u64;
    let mut data_size_64 = None;
    while offset <= size && size - offset >= 8 {
        file.seek(SeekFrom::Start(offset)).map_err(read)?;
        let mut chunk_header = [0u8; 8];
        file.read_exact(&mut chunk_header).map_err(read)?;
        let id = &chunk_header[0..4];
        let size32 = u32::from_le_bytes(chunk_header[4..8].try_into().unwrap());
        let chunk_size = if id == b"data" && size32 == u32::MAX {
            data_size_64.unwrap_or(u64::from(size32))
        } else {
            u64::from(size32)
        };
        let payload = offset + 8;
        if payload > size || chunk_size > size - payload {
            return Ok(None);
        }
        if id == b"ds64" && chunk_size >= 16 {
            file.seek(SeekFrom::Start(payload)).map_err(read)?;
            let mut buf = [0u8; 16];
            file.read_exact(&mut buf).map_err(read)?;
            data_size_64 = Some(u64::from_le_bytes(buf[8..16].try_into().unwrap()));
        } else if id == b"bext" {
            if chunk_size > MAX_BEXT {
                return Err(WavError::Invalid("bext chunk is larger than 16 MiB"));
            }
            file.seek(SeekFrom::Start(payload)).map_err(read)?;
            let mut buf = vec![0u8; chunk_size as usize];
            file.read_exact(&mut buf).map_err(read)?;
            return Ok(Some(buf));
        }
        offset = payload + chunk_size + (chunk_size & 1);
    }
    Ok(None)
}

fn updated_bext(
    source: &[u8],
    sample_rate: f64,
    channels: usize,
    bit_depth: u32,
    operation: &str,
    time_reference_offset: i64,
) -> Result<Vec<u8>, WavError> {
    if source.len() < 602 {
        return Ok(source.to_vec());
    }
    let mut result = source[..602].to_vec();
    if time_reference_offset != 0 {
        let original = u64::from(u32::from_le_bytes(result[338..342].try_into().unwrap()))
            | (u64::from(u32::from_le_bytes(result[342..346].try_into().unwrap())) << 32);
        // Never wrap a timestamp into a plausible but unrelated value.
        let updated = original
            .checked_add_signed(time_reference_offset)
            .ok_or(WavError::Invalid("TimeReference is out of range"))?;
        result[338..342].copy_from_slice(&(updated as u32).to_le_bytes());
        result[342..346].copy_from_slice(&((updated >> 32) as u32).to_le_bytes());
    }
    let mut history = source[602..].to_vec();
    while history.last() == Some(&0) {
        history.pop();
    }
    if !history.is_empty() && !history.ends_with(&[13, 10]) {
        history.extend_from_slice(&[13, 10]);
    }
    let mode = match channels {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{n}-channel"),
    };
    let rate = sample_rate.round() as i64;
    history.extend_from_slice(
        format!("A=PCM,F={rate},W={bit_depth},M={mode},T=Align {operation}\r\n").as_bytes(),
    );
    result.extend_from_slice(&history);
    Ok(result)
}

/// Rewrites `target` with `payload` as its first chunk after any `ds64`.
/// The audio is streamed into a staged copy that replaces `target` only when
/// complete.
fn insert_bext(payload: &[u8], target: &Path) -> Result<(), WavError> {
    if payload.len() > u32::MAX as usize {
        return Err(WavError::Invalid("bext chunk is larger than 4 GiB"));
    }
    let mut input = std::fs::File::open(target).map_err(WavError::Write)?;
    let original_size = input.metadata().map_err(WavError::Write)?.len();
    let mut header = [0u8; 12];
    input.read_exact(&mut header).map_err(WavError::Write)?;
    if !matches!(&header[0..4], b"RIFF" | b"RF64" | b"BW64") || &header[8..12] != b"WAVE" {
        return Err(WavError::Invalid("rendered audio is not a WAVE file"));
    }
    let container = header[0..4].to_vec();

    let rewritten = StagedFile::new(target).map_err(WavError::Write)?;
    {
        let mut output = std::fs::File::create(rewritten.path()).map_err(WavError::Write)?;
        output.write_all(&header).map_err(WavError::Write)?;
        let mut input_offset = 12u64;
        let mut ds64_data_offset = None;
        if container != b"RIFF" {
            input
                .seek(SeekFrom::Start(input_offset))
                .map_err(WavError::Write)?;
            let mut ds64_header = [0u8; 8];
            input
                .read_exact(&mut ds64_header)
                .map_err(WavError::Write)?;
            if &ds64_header[0..4] != b"ds64" {
                return Err(WavError::Invalid("RF64 audio has no ds64 chunk"));
            }
            let ds64_size = u64::from(u32::from_le_bytes(ds64_header[4..8].try_into().unwrap()));
            let chunk_size = 8 + ds64_size + (ds64_size & 1);
            if chunk_size > original_size - input_offset {
                return Err(WavError::Invalid("ds64 chunk is truncated"));
            }
            ds64_data_offset = Some(20u64);
            input
                .seek(SeekFrom::Start(input_offset))
                .map_err(WavError::Write)?;
            copy_bytes(chunk_size, &mut input, &mut output)?;
            input_offset += chunk_size;
        }
        output.write_all(b"bext").map_err(WavError::Write)?;
        output
            .write_all(&(payload.len() as u32).to_le_bytes())
            .map_err(WavError::Write)?;
        output.write_all(payload).map_err(WavError::Write)?;
        if payload.len() % 2 == 1 {
            output.write_all(&[0]).map_err(WavError::Write)?;
        }
        input
            .seek(SeekFrom::Start(input_offset))
            .map_err(WavError::Write)?;
        copy_bytes(original_size - input_offset, &mut input, &mut output)?;
        let rewritten_size = output.stream_position().map_err(WavError::Write)?;
        if container == b"RIFF" {
            if rewritten_size < 8 || rewritten_size - 8 > u64::from(u32::MAX) {
                return Err(WavError::Invalid("RIFF audio would exceed 4 GiB"));
            }
            output.seek(SeekFrom::Start(4)).map_err(WavError::Write)?;
            output
                .write_all(&((rewritten_size - 8) as u32).to_le_bytes())
                .map_err(WavError::Write)?;
        } else if let Some(off) = ds64_data_offset {
            output.seek(SeekFrom::Start(off)).map_err(WavError::Write)?;
            output
                .write_all(&(rewritten_size - 8).to_le_bytes())
                .map_err(WavError::Write)?;
        }
        output.flush().map_err(WavError::Write)?;
    }
    // Both handles are closed here: Windows cannot replace an open file.
    drop(input);
    rewritten.commit().map_err(WavError::Write)
}

fn copy_bytes(
    mut count: u64,
    input: &mut std::fs::File,
    output: &mut std::fs::File,
) -> Result<(), WavError> {
    let mut buf = vec![0u8; COPY_BUF];
    while count > 0 {
        let n = (count.min(COPY_BUF as u64)) as usize;
        input.read_exact(&mut buf[..n]).map_err(WavError::Write)?;
        output.write_all(&buf[..n]).map_err(WavError::Write)?;
        count -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bext_fixture(dir: &std::path::Path) -> std::path::PathBuf {
        // RIFF + fmt + bext(602 + history) + data.
        let path = dir.join("src.wav");
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&48000u32.to_le_bytes());
        wav.extend_from_slice(&(48000u32 * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&32u16.to_le_bytes());
        let mut bext = vec![0u8; 616];
        bext[320..330].copy_from_slice(b"2024-05-06");
        bext[330..338].copy_from_slice(b"10:00:00");
        bext[338..346].copy_from_slice(&100u64.to_le_bytes());
        bext[602..614].copy_from_slice(b"OLDHISTORY\r\n");
        wav.extend_from_slice(b"bext");
        wav.extend_from_slice(&616u32.to_le_bytes());
        wav.extend_from_slice(&bext);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&192000u32.to_le_bytes());
        wav.extend_from_slice(&vec![0u8; 192000]);
        let size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&size.to_le_bytes());
        std::fs::write(&path, &wav).unwrap();
        path
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("align-wav-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn transplant_updates_timeref_and_history() {
        let dir = tmp("transplant");
        let src = bext_fixture(&dir);
        let dst = dir.join("render.wav");
        std::fs::write(&dst, b"RIFF____WAVEfmt data").unwrap();
        // Minimal valid target: fmt + empty data.
        let mut target: Vec<u8> = Vec::new();
        target.extend_from_slice(b"RIFF");
        target.extend_from_slice(&[0u8; 4]);
        target.extend_from_slice(b"WAVE");
        target.extend_from_slice(b"fmt ");
        target.extend_from_slice(&16u32.to_le_bytes());
        target.extend_from_slice(&[0u8; 16]);
        target.extend_from_slice(b"data");
        target.extend_from_slice(&0u32.to_le_bytes());
        let size = (target.len() - 8) as u32;
        target[4..8].copy_from_slice(&size.to_le_bytes());
        std::fs::write(&dst, &target).unwrap();

        preserve_broadcast_extension(&src, &dst, 48000.0, 1, 32, "drift correction", 48000)
            .expect("preserve");
        let meta = crate::meta::read_bwf(&dst).expect("bwf back");
        // TimeReference shifted by one second.
        assert_eq!(meta.time_reference, Some(100 + 48000));
        let rendered = std::fs::read(&dst).unwrap();
        let at = search_bext(&dst);
        let end = (at + 700).min(rendered.len());
        let history = String::from_utf8_lossy(&rendered[at + 602..end]);
        assert!(history.contains("OLDHISTORY"));
        assert!(history.contains("T=Align drift correction"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn search_bext(path: &std::path::Path) -> usize {
        let bytes = std::fs::read(path).unwrap();
        bytes.windows(4).position(|w| w == b"bext").unwrap() + 8
    }

    #[test]
    fn timestamp_shift_rejects_underflow_and_overflow() {
        let mut bext = vec![0u8; 602];
        bext[338..346].copy_from_slice(&100u64.to_le_bytes());
        let shifted = updated_bext(&bext, 48000.0, 1, 32, "pad", -100).unwrap();
        assert_eq!(&shifted[338..346], &0u64.to_le_bytes());
        assert!(matches!(
            updated_bext(&bext, 48000.0, 1, 32, "pad", -101),
            Err(WavError::Invalid(_))
        ));
        bext[338..346].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            updated_bext(&bext, 48000.0, 1, 32, "trim", 1),
            Err(WavError::Invalid(_))
        ));
    }

    #[test]
    fn rf64_and_bw64_transplant_preserve_audio_and_update_large_sizes() {
        let dir = tmp("rf64-transplant");
        let src = bext_fixture(&dir);
        let original = std::fs::read(&src).unwrap();
        for container in [b"RF64", b"BW64"] {
            let dst = dir.join(format!("{}.wav", String::from_utf8_lossy(container)));
            let audio = [1u8, 2, 3, 4, 5, 6, 7, 8];
            let mut target = Vec::new();
            target.extend_from_slice(container);
            target.extend_from_slice(&u32::MAX.to_le_bytes());
            target.extend_from_slice(b"WAVEds64");
            target.extend_from_slice(&28u32.to_le_bytes());
            target.extend_from_slice(&80u64.to_le_bytes());
            target.extend_from_slice(&8u64.to_le_bytes());
            target.extend_from_slice(&2u64.to_le_bytes());
            target.extend_from_slice(&0u32.to_le_bytes());
            target.extend_from_slice(&original[12..36]);
            target.extend_from_slice(b"data");
            target.extend_from_slice(&u32::MAX.to_le_bytes());
            target.extend_from_slice(&audio);
            std::fs::write(&dst, &target).unwrap();
            preserve_broadcast_extension(&src, &dst, 48000.0, 1, 32, "drift correction", 0)
                .unwrap();
            let got = std::fs::read(&dst).unwrap();
            assert_eq!(&got[..4], container);
            assert_eq!(&got[4..8], &u32::MAX.to_le_bytes());
            assert_eq!(&got[12..16], b"ds64");
            assert_eq!(
                u64::from_le_bytes(got[20..28].try_into().unwrap()),
                (got.len() - 8) as u64
            );
            assert_eq!(&got[28..48], &target[28..48]);
            assert_eq!(&got[got.len() - audio.len()..], &audio);
            assert_eq!(
                crate::meta::read_bwf(&dst).unwrap().time_reference,
                Some(100)
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_bext_passes_through() {
        let dir = tmp("passthrough");
        let plain = dir.join("plain.wav");
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&0u32.to_le_bytes());
        let size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&size.to_le_bytes());
        std::fs::write(&plain, &wav).unwrap();
        let dst = dir.join("out.wav");
        std::fs::copy(&plain, &dst).unwrap();
        preserve_broadcast_extension(&plain, &dst, 48000.0, 1, 32, "drift correction", 0)
            .expect("passthrough");
        assert_eq!(std::fs::read(&dst).unwrap(), wav);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
