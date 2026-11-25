//! WAVE chunk reading and Broadcast Wave metadata for rendered audio.
//!
//! [`Chunks`] walks the chunks of RIFF, RF64 and BW64 files for every
//! lenient reader in Align. A render takes its `bext` from
//! [`rendered_broadcast_extension`] before writing: the fixed 602-byte prefix
//! is preserved, `TimeReference` shifted by the trim offset, and a
//! `T=Align <operation>` line appended to CodingHistory (CRLF-normalized).
//! Peak caches, cues and unknown device chunks are deliberately NOT copied
//! (they describe stretched-away samples). Files without `bext` give none.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::model::file_name;

const MAX_BEXT: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error("Cannot read Broadcast Wave metadata from {file}: {source}")]
    Read {
        file: String,
        #[source]
        source: io::Error,
    },
    #[error("Invalid Broadcast Wave metadata: {0}.")]
    Invalid(&'static str),
}

/// One chunk of a WAVE file: its id and where its payload is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub id: [u8; 4],
    /// File offset of the payload, after the 8-byte chunk header.
    pub offset: u64,
    /// Declared payload size. RF64 and BW64 `data` take theirs from `ds64`.
    pub size: u64,
}

/// The chunks of a RIFF, RF64 or BW64 WAVE file in file order.
///
/// The walk is lenient: it ends when fewer than eight bytes remain or a
/// chunk header cannot be read, and leaves validating declared sizes
/// against the file to the reader.
pub struct Chunks<'a> {
    file: &'a mut std::fs::File,
    container: [u8; 4],
    length: u64,
    next: Option<u64>,
    data_size_64: Option<u64>,
}

impl<'a> Chunks<'a> {
    /// `None` when `file` is not a WAVE file.
    pub fn new(file: &'a mut std::fs::File) -> io::Result<Option<Self>> {
        let length = file.metadata()?.len();
        let mut header = [0u8; 12];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut header)?;
        let container = [header[0], header[1], header[2], header[3]];
        if !matches!(&container, b"RIFF" | b"RF64" | b"BW64") || &header[8..12] != b"WAVE" {
            return Ok(None);
        }
        Ok(Some(Self {
            file,
            container,
            length,
            next: Some(12),
            data_size_64: None,
        }))
    }

    /// `RIFF`, `RF64` or `BW64`.
    pub fn container(&self) -> &[u8; 4] {
        &self.container
    }

    /// The file length when the walk started.
    pub fn length(&self) -> u64 {
        self.length
    }

    /// The next chunk, or `None` at the end of the walk.
    pub fn next_chunk(&mut self) -> Option<Chunk> {
        let offset = self.next.take()?;
        if self.length.checked_sub(offset)? < 8 {
            return None;
        }
        let mut header = [0u8; 8];
        self.file.seek(SeekFrom::Start(offset)).ok()?;
        self.file.read_exact(&mut header).ok()?;
        let id = [header[0], header[1], header[2], header[3]];
        let size32 = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let size = if &id == b"data" && size32 == u32::MAX {
            self.data_size_64.unwrap_or(u64::from(size32))
        } else {
            u64::from(size32)
        };
        let chunk = Chunk {
            id,
            offset: offset + 8,
            size,
        };
        if &id == b"ds64"
            && size >= 16
            && let Ok(sizes) = self.read(&chunk, 16)
            && let Ok(data) = sizes[8..16].try_into()
        {
            self.data_size_64 = Some(u64::from_le_bytes(data));
        }
        self.next = chunk
            .offset
            .checked_add(size)
            .and_then(|end| end.checked_add(size & 1));
        Some(chunk)
    }

    /// Up to `limit` bytes of `chunk`'s payload; fails if the file ends first.
    pub fn read(&mut self, chunk: &Chunk, limit: u64) -> io::Result<Vec<u8>> {
        let mut payload =
            vec![0u8; usize::try_from(chunk.size.min(limit)).map_err(io::Error::other)?];
        self.file.seek(SeekFrom::Start(chunk.offset))?;
        self.file.read_exact(&mut payload)?;
        Ok(payload)
    }
}

/// The speaker mask of a `WAVE_FORMAT_EXTENSIBLE` file, so audio rendered
/// from it keeps its channel layout. `None` for other files.
pub fn channel_mask(path: &Path) -> Option<u32> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut chunks = Chunks::new(&mut file).ok()??;
    while let Some(chunk) = chunks.next_chunk() {
        match &chunk.id {
            b"fmt " => {
                if chunk.size < 24 {
                    return None;
                }
                let format = chunks.read(&chunk, 24).ok()?;
                let tag = u16::from_le_bytes([format[0], format[1]]);
                return (tag == 0xFFFE)
                    .then(|| u32::from_le_bytes(format[20..24].try_into().unwrap()));
            }
            b"data" => return None,
            _ => {}
        }
    }
    None
}

/// The `bext` payload for audio rendered from `source`, ready for its
/// header, or `None` when `source` has no Broadcast Wave extension.
pub fn rendered_broadcast_extension(
    source: &Path,
    sample_rate: f64,
    channels: usize,
    bit_depth: u32,
    operation: &str,
    time_reference_offset: i64,
) -> Result<Option<Vec<u8>>, WavError> {
    let Some(extension) = read_bext(source)? else {
        return Ok(None);
    };
    let payload = updated_bext(
        &extension,
        sample_rate,
        channels,
        bit_depth,
        operation,
        time_reference_offset,
    )?;
    if payload.len() > u32::MAX as usize {
        return Err(WavError::Invalid("bext chunk is larger than 4 GiB"));
    }
    Ok(Some(payload))
}

fn read_bext(path: &Path) -> Result<Option<Vec<u8>>, WavError> {
    let read = |source| WavError::Read {
        file: file_name(path),
        source,
    };
    let mut file = std::fs::File::open(path).map_err(read)?;
    let Some(mut chunks) = Chunks::new(&mut file).map_err(read)? else {
        return Ok(None);
    };
    while let Some(chunk) = chunks.next_chunk() {
        if chunk.size > chunks.length() - chunk.offset.min(chunks.length()) {
            return Ok(None);
        }
        if &chunk.id == b"bext" {
            if chunk.size > MAX_BEXT {
                return Err(WavError::Invalid("bext chunk is larger than 16 MiB"));
            }
            return chunks.read(&chunk, MAX_BEXT).map(Some).map_err(read);
        }
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

    #[test]
    fn rendered_extension_shifts_timeref_and_extends_history() {
        let dir = tempfile::tempdir().unwrap();
        let src = bext_fixture(dir.path());
        let payload = rendered_broadcast_extension(&src, 48000.0, 1, 32, "drift correction", 48000)
            .unwrap()
            .expect("bext");
        assert_eq!(&payload[338..346], &(100u64 + 48000).to_le_bytes());
        let history = String::from_utf8_lossy(&payload[602..]);
        assert_eq!(
            history,
            "OLDHISTORY\r\nA=PCM,F=48000,W=32,M=mono,T=Align drift correction\r\n"
        );
    }

    #[test]
    fn chunks_walk_rf64_data_size_from_ds64() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.wav");
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RF64");
        wav.extend_from_slice(&u32::MAX.to_le_bytes());
        wav.extend_from_slice(b"WAVEds64");
        wav.extend_from_slice(&28u32.to_le_bytes());
        wav.extend_from_slice(&0u64.to_le_bytes());
        wav.extend_from_slice(&6u64.to_le_bytes());
        wav.extend_from_slice(&[0u8; 12]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&u32::MAX.to_le_bytes());
        wav.extend_from_slice(&[1u8; 6]);
        wav.extend_from_slice(b"LIST");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(b"INFO");
        std::fs::write(&path, &wav).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let mut chunks = Chunks::new(&mut file).unwrap().unwrap();
        let mut walked = Vec::new();
        while let Some(chunk) = chunks.next_chunk() {
            walked.push((chunk.id, chunk.offset, chunk.size));
        }
        assert_eq!(
            walked,
            [(*b"ds64", 20, 28), (*b"data", 56, 6), (*b"LIST", 70, 4)]
        );
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
    fn missing_bext_gives_no_extension() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.wav");
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&0u32.to_le_bytes());
        let size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&size.to_le_bytes());
        std::fs::write(&plain, &wav).unwrap();
        assert_eq!(
            rendered_broadcast_extension(&plain, 48000.0, 1, 32, "drift correction", 0).unwrap(),
            None
        );
    }
}
