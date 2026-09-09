//! FFmpeg tier of the portable backend: header inspect via `ffprobe` JSON
//! and audio extraction via an `ffmpeg` stdout pipe for containers Symphonia
//! cannot demux (MTS/MXF/R3D, odd MP4s).
//!
//! Resource discipline: the child is killed on `Drop` (no zombies), stdout
//! is consumed in 64 KiB chunks (a 3-hour take never sits in RAM), and a
//! non-zero exit becomes [`DecodeError::Incomplete`] instead of silent
//! truncation. Decoded bytes are native-rate discrete-channel `f32le` —
//! resampling and mono selection stay in [`crate::mono`] for identical
//! numerics on every path.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use crate::DecodeError;
use crate::backend::{AudioStreamProbe, ProbeReport, VideoProbe};

/// Locate `ffprobe`/`ffmpeg`: explicit env override → executable's dir
/// (bundled sidecar layout) → PATH.
pub fn resolve_bin(env_var: &str, name: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(env_var) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in [dir.join(name), dir.join(format!("{name}.exe"))] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|directory| find_in_directory(&directory, name))
    })
}

fn find_in_directory(directory: &Path, name: &str) -> Option<PathBuf> {
    let plain = directory.join(name);
    if plain.is_file() {
        return Some(plain);
    }
    #[cfg(windows)]
    {
        let executable = directory.join(format!("{name}.exe"));
        if executable.is_file() {
            return Some(executable);
        }
    }
    None
}

pub fn ffprobe_bin() -> Option<PathBuf> {
    resolve_bin("ALIGN_FFPROBE", "ffprobe")
}

pub fn ffmpeg_bin() -> Option<PathBuf> {
    resolve_bin("ALIGN_FFMPEG", "ffmpeg")
}

/// Header-only inspect. No sample data is read (audio-only invariant).
pub fn inspect(path: &Path) -> Result<ProbeReport, DecodeError> {
    let bin =
        ffprobe_bin().ok_or_else(|| DecodeError::FfmpegMissing("ffprobe not found".into()))?;
    let out = Command::new(bin)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .map_err(DecodeError::Io)?;
    if !out.status.success() {
        return Err(DecodeError::Incomplete(format!(
            "ffprobe failed for {}",
            path.display()
        )));
    }
    parse_ffprobe_json(&out.stdout)
        .ok_or_else(|| DecodeError::Incomplete(format!("ffprobe JSON for {}", path.display())))
}

fn num(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Pure parser (unit-tested without a binary on disk).
pub fn parse_ffprobe_json(json: &[u8]) -> Option<ProbeReport> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let streams = v.get("streams")?.as_array()?;
    let mut audio_streams = Vec::new();
    let mut has_video = false;
    for s in streams {
        match s.get("codec_type")?.as_str()? {
            "audio" => {
                let codec = s.get("codec_name").and_then(|c| c.as_str()).unwrap_or("");
                let is_float = if codec.contains("f32") || codec.contains("f64") {
                    Some(true)
                } else if codec.starts_with("pcm_") {
                    Some(false)
                } else {
                    None
                };
                audio_streams.push(AudioStreamProbe {
                    sample_rate: num(s.get("sample_rate")?)?,
                    channels: s.get("channels")?.as_u64()? as usize,
                    bit_depth: s
                        .get("bits_per_sample")
                        .and_then(|b| b.as_u64())
                        .map(|b| b as u32),
                    is_float,
                })
            }
            "video" => {
                // Attached pictures (album art) are not video tracks.
                let attached = s
                    .get("disposition")
                    .and_then(|d| d.get("attached_pic"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                has_video |= attached != 1;
            }
            _ => {}
        }
    }
    if audio_streams.is_empty() {
        return None;
    }
    let duration_seconds = num(v.get("format")?.get("duration")?)
        .or_else(|| {
            streams
                .iter()
                .filter_map(|s| num(s.get("duration")?))
                .fold(None::<f64>, |acc, d| Some(acc.map_or(d, |a: f64| a.max(d))))
        })
        .filter(|d| *d > 0.0)?;
    Some(ProbeReport {
        duration_seconds,
        audio_streams,
        has_video,
        video: None, // filled by inspect_video() below, not the pure parser
    })
}

/// Header inspect + video timing walk. Pure-header fast path when the
/// container has no video track.
pub fn inspect_full(path: &Path) -> Result<ProbeReport, DecodeError> {
    let mut report = inspect(path)?;
    if report.has_video {
        report.video = inspect_video(path);
    }
    Ok(report)
}

struct FfVideoStream {
    width: u32,
    height: u32,
    avg_fps: Option<f64>,
    timecode: Option<String>,
}

/// First video stream's header facts + optional `timecode` tag.
fn parse_video_stream(json: &[u8]) -> Option<FfVideoStream> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let stream = v.get("streams")?.as_array()?.iter().find(|s| {
        s.get("codec_type").and_then(|c| c.as_str()) == Some("video")
            && s.get("disposition")
                .and_then(|d| d.get("attached_pic"))
                .and_then(|a| a.as_u64())
                != Some(1)
    })?;
    Some(FfVideoStream {
        width: stream.get("width")?.as_u64()? as u32,
        height: stream.get("height")?.as_u64()? as u32,
        avg_fps: stream.get("avg_frame_rate").and_then(parse_rational),
        timecode: stream
            .get("tags")
            .and_then(|t| t.get("timecode"))
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .or_else(|| {
                v.get("format")?
                    .get("tags")?
                    .get("timecode")?
                    .as_str()
                    .map(str::to_string)
            }),
    })
}

fn parse_rational(value: &serde_json::Value) -> Option<f64> {
    let text = value.as_str()?;
    let (num, den) = text.split_once('/')?;
    let (num, den): (f64, f64) = (num.parse().ok()?, den.parse().ok()?);
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}

/// Packet order follows decoding, which can differ from presentation
/// order for B-frames. Store timestamps only (8 bytes per frame), then
/// compare adjacent presentation times after sorting. No video is decoded.
struct PacketTiming {
    durations: align_core::RangeAccumulator,
    presentation_times: Vec<f64>,
}

impl Default for PacketTiming {
    fn default() -> Self {
        Self {
            durations: align_core::RangeAccumulator::new(),
            presentation_times: Vec::new(),
        }
    }
}

impl PacketTiming {
    fn observe(&mut self, pts: f64, duration: f64) {
        self.durations.observe(duration);
        if pts.is_finite() {
            self.presentation_times.push(pts);
        }
    }

    fn mode(mut self) -> align_core::VideoFrameRateMode {
        use align_core::{RangeAccumulator, VideoFrameRateMode};
        self.presentation_times.sort_unstable_by(f64::total_cmp);
        let mut deltas = RangeAccumulator::new();
        for pair in self.presentation_times.windows(2) {
            deltas.observe(pair[1] - pair[0]);
        }
        if self.durations.is_variable() || deltas.is_variable() {
            VideoFrameRateMode::Variable
        } else if self.durations.count().max(deltas.count()) >= 2 {
            VideoFrameRateMode::Constant
        } else {
            VideoFrameRateMode::Unknown
        }
    }
}

/// Video timing without decoding frames: packet PTS/duration walk feeding
/// the shared [`align_core::classify`]. Early-exits once variability is
/// proven; constant-rate classification requires the full walk.
pub fn video_timing(path: &Path) -> Result<align_core::VideoTimingInspection, DecodeError> {
    let bin =
        ffprobe_bin().ok_or_else(|| DecodeError::FfmpegMissing("ffprobe not found".into()))?;
    let decoder = bin.canonicalize().ok().and_then(|bin| {
        Some(format!(
            "{}:{}",
            bin.display(),
            align_core::cache::media_revision(&bin).ok()?
        ))
    });
    let identity = |path: &Path| align_core::cache::media_revision(path).ok();
    let before = identity(path);
    let cache = align_core::FingerprintCache::with_backend(None, "portable-timing1");
    if let Some(decoder) = &decoder
        && let Some(timing) = cache.load_video_timing(path, decoder)
    {
        return Ok(timing);
    }
    let timing = video_timing_uncached(path, &bin)?;
    // A scan racing a media rewrite must not publish a cached classification.
    if before.is_some()
        && before == identity(path)
        && let Some(decoder) = &decoder
    {
        cache.save_video_timing(path, decoder, timing);
    }
    Ok(timing)
}

fn video_timing_uncached(
    path: &Path,
    bin: &Path,
) -> Result<align_core::VideoTimingInspection, DecodeError> {
    use align_core::canonical_frame_duration;
    use std::io::BufRead;

    let mut child = Command::new(bin)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,duration_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(DecodeError::Io)?;
    let stdout = child.stdout.take().ok_or(DecodeError::UnsupportedOutput)?;

    // Nominal rate + dimensions from the header call.
    let header_json = Command::new(bin)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-select_streams",
            "v:0",
        ])
        .arg(path)
        .output()
        .map_err(DecodeError::Io)?;
    let header = parse_video_stream(&header_json.stdout);
    let frame_duration = header
        .as_ref()
        .and_then(|h| h.avg_fps)
        .and_then(|fps| canonical_frame_duration(fps, None));

    let mut timing = PacketTiming::default();
    let read_result = (|| {
        for line in std::io::BufReader::new(stdout).lines() {
            let line = line.map_err(DecodeError::Io)?;
            let mut fields = line.split(',');
            let mut next_number = || {
                fields
                    .next()
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(f64::NAN)
            };
            timing.observe(next_number(), next_number());
            if timing.durations.is_variable() {
                break;
            }
        }
        Ok::<(), DecodeError>(())
    })();
    let early = timing.durations.is_variable() || read_result.is_err();
    if early {
        let _ = child.kill();
    }
    let status = child.wait().map_err(DecodeError::Io)?;
    read_result?;
    if !early && !status.success() {
        return Err(DecodeError::Incomplete(format!(
            "ffprobe packet timing failed for {}",
            path.display()
        )));
    }
    Ok(align_core::VideoTimingInspection {
        frame_duration,
        mode: timing.mode(),
    })
}

/// Full video probe: header facts + timing walk + timecode tag.
/// `None` when the container has no usable video stream.
pub fn inspect_video(path: &Path) -> Option<VideoProbe> {
    let header_json = Command::new(ffprobe_bin()?)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-select_streams",
            "v:0",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !header_json.status.success() {
        return None;
    }
    let header = parse_video_stream(&header_json.stdout)?;
    let timing = video_timing(path).ok()?;
    Some(VideoProbe {
        width: header.width,
        height: header.height,
        frame_duration: timing.frame_duration,
        mode: timing.mode,
        source_timecode: timecode_from_header(&header),
    })
}

/// Timecode tag at the header's true rate: `avg_frame_rate` snaps through
/// the canonical broadcast table (29.97 → `1001/30000`, never rounded to
/// `1/30`), then the label validates as NDF/DF. `None` when the rate is
/// unknown or the label is forbidden — never guessed. Pure (no binary).
fn timecode_from_header(header: &FfVideoStream) -> Option<align_core::SourceTimecode> {
    let duration = align_core::canonical_frame_duration(header.avg_fps?, None)?;
    align_core::SourceTimecode::from_label(header.timecode.as_deref()?, duration)
}
pub struct AudioPipe {
    child: Child,
    stdout: ChildStdout,
    channels: usize,
    leftover: Vec<u8>,
}

impl AudioPipe {
    /// Spawn `ffmpeg -i path [-ss start -t dur] -map 0:a:N -vn -f f32le …`.
    /// Seek args go *after* `-i` (accurate, windows are short). Native rate,
    /// discrete channels — no `-ar`, no downmix, ever.
    pub fn spawn(
        path: &Path,
        stream_index: usize,
        window: Option<(f64, f64)>,
        channels: usize,
    ) -> Result<Self, DecodeError> {
        let bin =
            ffmpeg_bin().ok_or_else(|| DecodeError::FfmpegMissing("ffmpeg not found".into()))?;
        let mut cmd = Command::new(bin);
        cmd.args(["-v", "error", "-hide_banner", "-i"]).arg(path);
        if let Some((start, duration)) = window {
            cmd.args([
                "-ss",
                &start.max(0.0).to_string(),
                "-t",
                &duration.to_string(),
            ]);
        }
        cmd.args(["-map", &format!("0:a:{stream_index}"), "-vn"])
            .args([
                "-f",
                "f32le",
                "-acodec",
                "pcm_f32le",
                "-ac",
                &channels.to_string(),
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(DecodeError::Io)?;
        let stdout = child.stdout.take().ok_or(DecodeError::UnsupportedOutput)?;
        Ok(Self {
            child,
            stdout,
            channels,
            leftover: Vec::new(),
        })
    }

    /// Pump stdout through `consume` as interleaved f32 frames until EOF.
    /// Returns `true` when the child exited successfully.
    pub fn pump(
        &mut self,
        consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
    ) -> Result<bool, DecodeError> {
        let mut chunk = vec![0u8; 65536];
        let mut frames = Vec::<f32>::with_capacity(16384);
        loop {
            let n = self.stdout.read(&mut chunk).map_err(DecodeError::Io)?;
            if n == 0 {
                break;
            }
            self.leftover.extend_from_slice(&chunk[..n]);
            let usable = complete_frame_bytes(self.leftover.len(), self.channels)?;
            frames.clear();
            frames.extend(
                self.leftover[..usable]
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            );
            self.leftover.drain(..usable);
            if !frames.is_empty() {
                debug_assert_eq!(frames.len() % self.channels, 0);
                consume(&frames)?;
            }
        }
        let status = self.child.wait().map_err(DecodeError::Io)?;
        if !self.leftover.is_empty() {
            return Err(DecodeError::InvalidPcm);
        }
        Ok(status.success())
    }
}

impl Drop for AudioPipe {
    fn drop(&mut self) {
        // Never leave a zombie transcoder behind (cancel path included).
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn complete_frame_bytes(bytes: usize, channels: usize) -> Result<usize, DecodeError> {
    let frame_bytes = channels
        .checked_mul(4)
        .filter(|&n| n > 0)
        .ok_or(DecodeError::InvalidPcm)?;
    Ok(bytes / frame_bytes * frame_bytes)
}

/// s32le twin of [`AudioPipe`] for bit-exact integer stems.
pub struct AudioPipeI32 {
    child: Child,
    stdout: ChildStdout,
    channels: usize,
    leftover: Vec<u8>,
}

impl AudioPipeI32 {
    pub fn spawn(
        path: &Path,
        stream_index: usize,
        window: Option<(f64, f64)>,
        channels: usize,
    ) -> Result<Self, DecodeError> {
        let bin =
            ffmpeg_bin().ok_or_else(|| DecodeError::FfmpegMissing("ffmpeg not found".into()))?;
        let mut cmd = Command::new(bin);
        cmd.args(["-v", "error", "-hide_banner", "-i"]).arg(path);
        if let Some((start, duration)) = window {
            cmd.args([
                "-ss",
                &start.max(0.0).to_string(),
                "-t",
                &duration.to_string(),
            ]);
        }
        cmd.args(["-map", &format!("0:a:{stream_index}"), "-vn"])
            .args([
                "-f",
                "s32le",
                "-acodec",
                "pcm_s32le",
                "-ac",
                &channels.to_string(),
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(DecodeError::Io)?;
        let stdout = child.stdout.take().ok_or(DecodeError::UnsupportedOutput)?;
        Ok(Self {
            child,
            stdout,
            channels,
            leftover: Vec::new(),
        })
    }

    pub fn pump(
        &mut self,
        consume: &mut dyn FnMut(&[i32]) -> Result<(), DecodeError>,
    ) -> Result<bool, DecodeError> {
        let mut chunk = vec![0u8; 65536];
        let mut frames = Vec::<i32>::with_capacity(16384);
        loop {
            let n = self.stdout.read(&mut chunk).map_err(DecodeError::Io)?;
            if n == 0 {
                break;
            }
            self.leftover.extend_from_slice(&chunk[..n]);
            let usable = complete_frame_bytes(self.leftover.len(), self.channels)?;
            frames.clear();
            frames.extend(
                self.leftover[..usable]
                    .chunks_exact(4)
                    .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            );
            self.leftover.drain(..usable);
            if !frames.is_empty() {
                debug_assert_eq!(frames.len() % self.channels, 0);
                consume(&frames)?;
            }
        }
        let status = self.child.wait().map_err(DecodeError::Io)?;
        if !self.leftover.is_empty() {
            return Err(DecodeError::InvalidPcm);
        }
        Ok(status.success())
    }
}

impl Drop for AudioPipeI32 {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Full-file fallback decode to 8 kHz mono (fingerprint path).
pub fn decode_mono_8k(
    path: &Path,
    stream_index: usize,
    channels: usize,
    explicit_channel: Option<usize>,
    mix_channels: bool,
    consume: &mut dyn FnMut(&[f32]) -> Result<(), DecodeError>,
) -> Result<(), DecodeError> {
    // Native rate unknown here — inspect first (header-only, cheap).
    let probe = inspect(path)?;
    let sample_rate = probe
        .audio_streams
        .get(stream_index)
        .map(|s| s.sample_rate)
        .unwrap_or(0.0);
    if sample_rate <= 0.0 {
        return Err(DecodeError::InvalidPcm);
    }
    let mut pipe = AudioPipe::spawn(path, stream_index, None, channels)?;
    let mut mono = crate::mono::MonoPipe::new(
        sample_rate,
        8000.0,
        channels,
        explicit_channel,
        mix_channels,
        consume,
    )?;
    if !pipe.pump(&mut |frames| mono.push_interleaved(frames))? {
        return Err(DecodeError::Incomplete(format!(
            "ffmpeg decode of {}",
            path.display()
        )));
    }
    mono.finish()
}

/// Window fallback decode to 16 kHz mono (refine path).
pub fn decode_window(
    path: &Path,
    start: f64,
    duration: f64,
    stream_index: usize,
    channels: usize,
    explicit_channel: Option<usize>,
    mix_channels: bool,
) -> Result<(f64, Vec<f32>), DecodeError> {
    let probe = inspect(path)?;
    let sample_rate = probe
        .audio_streams
        .get(stream_index)
        .map(|s| s.sample_rate)
        .unwrap_or(0.0);
    if sample_rate <= 0.0 {
        return Err(DecodeError::InvalidPcm);
    }
    let actual_start = start.max(0.0);
    let mut samples = Vec::new();
    let mut pipe = AudioPipe::spawn(path, stream_index, Some((actual_start, duration)), channels)?;
    let mut mono = crate::mono::MonoPipe::new(
        sample_rate,
        16_000.0,
        channels,
        explicit_channel,
        mix_channels,
        |s: &[f32]| {
            samples.extend_from_slice(s);
            Ok(())
        },
    )?;
    if !pipe.pump(&mut |frames| mono.push_interleaved(frames))? {
        return Err(DecodeError::Incomplete(format!(
            "ffmpeg window of {}",
            path.display()
        )));
    }
    mono.finish()?;
    Ok((actual_start, samples))
}

#[cfg(test)]
mod tests {
    #[test]
    fn pipe_fragments_preserve_complete_multichannel_frames() {
        for channels in [1, 2, 3, 6, 8] {
            let original: Vec<u8> = (0..channels * 4 * 37).map(|n| n as u8).collect();
            for chunk_size in [1, 3, 4, 7, 13, 64, 257] {
                let mut pending = Vec::new();
                let mut reconstructed = Vec::new();
                for chunk in original.chunks(chunk_size) {
                    pending.extend_from_slice(chunk);
                    let usable = super::complete_frame_bytes(pending.len(), channels).unwrap();
                    assert_eq!(usable % (channels * 4), 0);
                    reconstructed.extend(pending.drain(..usable));
                }
                assert!(pending.is_empty());
                assert_eq!(reconstructed, original);
            }
        }
        assert!(super::complete_frame_bytes(4, 0).is_err());
    }

    #[test]
    fn executable_lookup_uses_platform_suffix() {
        let directory =
            std::env::temp_dir().join(format!("align-bin-lookup-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let name = if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        };
        let binary = directory.join(name);
        std::fs::write(&binary, b"fixture").unwrap();
        assert_eq!(
            super::find_in_directory(&directory, "ffprobe"),
            Some(binary)
        );
        assert!(super::find_in_directory(&directory, "missing").is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn packet_timing_sorts_b_frames_but_detects_presentation_gaps() {
        use align_core::VideoFrameRateMode;
        // Actual first packets of the user's CFR MP4, in decode order.
        let pts = [
            0.08, 0.0, 0.04, 0.20, 0.12, 0.16, 0.32, 0.24, 0.28, 0.44, 0.36, 0.40,
        ];
        let mut constant = super::PacketTiming::default();
        let mut gap = super::PacketTiming::default();
        let mut durations = super::PacketTiming::default();
        for p in pts {
            constant.observe(p, 0.04);
            gap.observe(if p >= 0.24 { p + 0.04 } else { p }, 0.04);
            durations.observe(p, if p == 0.24 { 0.08 } else { 0.04 });
        }
        assert_eq!(constant.mode(), VideoFrameRateMode::Constant);
        assert_eq!(gap.mode(), VideoFrameRateMode::Variable);
        assert_eq!(durations.mode(), VideoFrameRateMode::Variable);
        let mut missing = super::PacketTiming::default();
        missing.observe(f64::NAN, f64::NAN);
        assert_eq!(missing.mode(), VideoFrameRateMode::Unknown);
    }

    use super::*;

    const SAMPLE: &[u8] = br#"{
        "streams": [
            {"index": 0, "codec_type": "video", "disposition": {"attached_pic": 0}},
            {"index": 1, "codec_type": "audio", "sample_rate": "48000", "channels": 2},
            {"index": 2, "codec_type": "audio", "sample_rate": 44100, "channels": 6}
        ],
        "format": {"duration": "123.456"}
    }"#;

    #[test]
    fn parses_ffprobe_json() {
        let r = parse_ffprobe_json(SAMPLE).expect("parse");
        assert!((r.duration_seconds - 123.456).abs() < 1e-9);
        assert!(r.has_video);
        assert_eq!(r.audio_streams.len(), 2);
        assert_eq!(r.audio_streams[0].sample_rate, 48000.0);
        assert_eq!(r.audio_streams[1].channels, 6);
    }

    #[test]
    fn timecode_tag_uses_true_rate_and_validates_df() {
        // 29.97 DF: true 1001/30000 rate, drop-compensated elapsed count.
        let json = br#"{"streams": [{"codec_type": "video", "width": 1920,
            "height": 1080, "avg_frame_rate": "30000/1001",
            "tags": {"timecode": "01:00:00;00"}}]}"#;
        let header = parse_video_stream(json).expect("header");
        assert!((header.avg_fps.unwrap() - 29.97).abs() < 0.01);
        let tc = timecode_from_header(&header).expect("tc");
        assert_eq!(tc.text, "01:00:00;00");
        assert!(tc.drop_frame);
        assert_eq!(tc.frame_number, 107_892);
        assert_eq!(tc.frame_duration, align_core::MediaTime::new(1001, 30_000));
        // Skipped DF label at a non-10th minute: forbidden, never guessed.
        let bad = br#"{"streams": [{"codec_type": "video", "width": 1920,
            "height": 1080, "avg_frame_rate": "30000/1001",
            "tags": {"timecode": "00:01:00;00"}}]}"#;
        assert!(timecode_from_header(&parse_video_stream(bad).expect("header")).is_none());
        // 59.94 DF anchor.
        let h59 = br#"{"streams": [{"codec_type": "video", "width": 1920,
            "height": 1080, "avg_frame_rate": "60000/1001",
            "tags": {"timecode": "01:00:00;00"}}]}"#;
        let tc59 = timecode_from_header(&parse_video_stream(h59).expect("header")).expect("tc");
        assert_eq!(tc59.frame_number, 216_000 - 216);
        assert_eq!(
            tc59.frame_duration,
            align_core::MediaTime::new(1001, 60_000)
        );
        // 25 fps NDF stays exact at 1/25.
        let ndf = br#"{"streams": [{"codec_type": "video", "width": 1280,
            "height": 720, "avg_frame_rate": "25/1",
            "disposition": {"attached_pic": 0},
            "tags": {"timecode": "01:02:03:04"}}]}"#;
        let tc25 = timecode_from_header(&parse_video_stream(ndf).expect("header")).expect("tc");
        assert!(!tc25.drop_frame);
        assert_eq!(tc25.frame_number, (62 * 60 + 3) * 25 + 4);
        // Unknown rate: no timecode rather than a guessed one.
        let norate = br#"{"streams": [{"codec_type": "video", "width": 1280,
            "height": 720, "avg_frame_rate": "0/0",
            "tags": {"timecode": "01:02:03:04"}}]}"#;
        assert!(timecode_from_header(&parse_video_stream(norate).expect("header")).is_none());
    }

    #[test]
    fn attached_pic_is_not_video() {
        let json = br#"{"streams": [
            {"codec_type": "video", "disposition": {"attached_pic": 1}},
            {"codec_type": "audio", "sample_rate": "44100", "channels": 2}],
            "format": {"duration": "10.0"}}"#;
        let r = parse_ffprobe_json(json).expect("parse");
        assert!(!r.has_video);
    }

    #[test]
    fn rejects_missing_audio_or_duration() {
        let no_audio = br#"{"streams": [{"codec_type": "video", "disposition": {}}], "format": {"duration": "5"}}"#;
        assert!(parse_ffprobe_json(no_audio).is_none());
        let no_dur = br#"{"streams": [{"codec_type": "audio", "sample_rate": "48000", "channels": 1}], "format": {}}"#;
        assert!(parse_ffprobe_json(no_dur).is_none());
    }

    #[test]
    fn ffmpeg_pipe_roundtrips_wav() {
        if ffmpeg_bin().is_none() {
            eprintln!("SKIP: no ffmpeg binary");
            return;
        }
        let dir = std::env::temp_dir().join(format!("align-ff-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        for i in 0..8000 {
            w.write_sample((2.0 * std::f32::consts::PI * 440.0 * i as f32 / 8000.0).sin())
                .unwrap();
        }
        w.finalize().unwrap();

        // Native rate equals target: pipe → mono passthrough must be exact.
        let mut got = Vec::new();
        let mut pipe = AudioPipe::spawn(&wav, 0, None, 1).expect("spawn");
        assert!(
            pipe.pump(&mut |f| {
                got.extend_from_slice(f);
                Ok(())
            })
            .expect("pump")
        );
        assert_eq!(got.len(), 8000);
        let peak = got.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
        assert!((peak - 1.0).abs() < 0.01, "peak={peak}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
