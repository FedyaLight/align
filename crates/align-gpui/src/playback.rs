use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use align_core::MappingPoint;

#[derive(Clone, Debug)]
pub struct PlaybackItem {
    pub url: PathBuf,
    pub lane_id: String,
    /// Position in the exported timeline, in seconds.
    pub start: f64,
    pub duration: f64,
    pub source_in: f64,
    pub source_out: f64,
    /// Piecewise source-time to exported island-time mapping.
    pub mapping_points: Vec<MappingPoint>,
}

/// Decoded, mono preview PCM. Audio is normalized to 48 kHz by FFmpeg before
/// it reaches the output callback, keeping that callback allocation-free.
#[derive(Clone, Debug)]
pub struct DecodedItem {
    pub item: PlaybackItem,
    pub samples: Arc<[f32]>,
}

#[derive(Debug)]
pub struct PlaybackClock {
    pub position: f64,
    pub playing: bool,
    pub updated_at: Instant,
}

impl PlaybackClock {
    pub fn new(position: f64) -> Self {
        Self {
            position: position.max(0.0),
            playing: false,
            updated_at: Instant::now(),
        }
    }

    pub fn position_at(&self, now: Instant) -> f64 {
        if self.playing {
            self.position + now.saturating_duration_since(self.updated_at).as_secs_f64()
        } else {
            self.position
        }
    }
}

/// Shared state read by the realtime output callback. The callback only uses
/// `try_lock`, so UI operations never make the audio device wait.
pub type SharedPlayback = Arc<Mutex<PlaybackState>>;

pub struct PlaybackState {
    pub clock: PlaybackClock,
    pub items: Vec<DecodedItem>,
    pub muted: HashSet<String>,
    pub soloed: HashSet<String>,
    pub end: f64,
}

impl PlaybackState {
    pub fn sample_at(&self, timeline_seconds: f64) -> f32 {
        if !timeline_seconds.is_finite() {
            return 0.0;
        }
        let mut active_lane: Option<&str> = None;
        let mut multiple_active_lanes = false;
        for decoded in &self.items {
            let item = &decoded.item;
            if item.source_time_at(timeline_seconds).is_none() {
                continue;
            }
            match active_lane {
                None => active_lane = Some(&item.lane_id),
                Some(lane_id) if lane_id != item.lane_id => {
                    multiple_active_lanes = true;
                    break;
                }
                Some(_) => {}
            }
        }
        let single_active_lane = active_lane.is_some() && !multiple_active_lanes;
        let mut mixed = 0.0;
        let mut contributors = 0usize;
        for decoded in &self.items {
            let item = &decoded.item;
            if !single_active_lane && !item.is_audible(&self.muted, &self.soloed) {
                continue;
            }
            let Some(source_seconds) = item.source_time_at(timeline_seconds) else {
                continue;
            };
            let index = ((source_seconds - item.source_in).max(0.0) * 48_000.0).floor() as usize;
            if let Some(sample) = decoded.samples.get(index) {
                mixed += sample;
                contributors += 1;
            }
        }
        if contributors > 1 {
            mixed / (contributors as f32).sqrt()
        } else {
            mixed
        }
    }

    pub fn position_at(&self, now: Instant) -> f64 {
        self.clock.position_at(now).min(self.end)
    }
}

/// Decode the selected timeline sources off the UI/audio threads. Each source
/// is reduced to 48 kHz mono preview PCM; the export itself is untouched.
pub fn decode_items(items: Vec<PlaybackItem>) -> Result<Vec<DecodedItem>, String> {
    let expected_bytes = items.iter().try_fold(0u64, |total, item| {
        let duration = (item.source_out - item.source_in).max(0.0);
        let bytes = (duration * 48_000.0 * 4.0).ceil() as u64;
        total.checked_add(bytes)
    });
    if expected_bytes.is_none_or(|bytes| bytes > 512 * 1024 * 1024) {
        return Err("Timeline preview audio exceeds the 512 MB memory limit.".into());
    }
    items
        .into_iter()
        .filter_map(|item| match decode_item(item) {
            Ok(Some(item)) => Some(Ok(item)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn decode_item(item: PlaybackItem) -> Result<Option<DecodedItem>, String> {
    use std::process::Stdio;

    let ffmpeg = align_decode::ff::ffmpeg_bin()
        .ok_or_else(|| "FFmpeg is required for timeline audio playback.".to_owned())?;
    let duration = item.source_out - item.source_in;
    if !duration.is_finite() || duration <= 0.0 {
        return Ok(None);
    }
    let mut command = align_decode::process::command(ffmpeg);
    let output = command
        .args(["-v", "error", "-hide_banner", "-i"])
        .arg(&item.url)
        .args(["-ss"])
        .arg(item.source_in.to_string())
        .args(["-t"])
        .arg(duration.to_string())
        .args([
            "-map", "0:a:0?", "-vn", "-ac", "1", "-ar", "48000", "-f", "f32le", "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("Could not decode {}: {error}", item.url.display()))?;
    if !output.status.success() {
        return Err(format!(
            "Could not decode timeline audio from {}.",
            item.url.display()
        ));
    }
    if output.stdout.len() % 4 != 0 {
        return Err(format!(
            "FFmpeg returned incomplete PCM for {}.",
            item.url.display()
        ));
    }
    let samples: Vec<f32> = output
        .stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect();
    if samples.is_empty() {
        return Ok(None);
    }
    Ok(Some(DecodedItem {
        item,
        samples: samples.into(),
    }))
}

pub fn output_stream(shared: SharedPlayback) -> Result<cpal::Stream, String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| "No audio output device is available.".to_owned())?;
    let supported = device
        .default_output_config()
        .map_err(|error| format!("Could not configure audio output: {error}"))?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    match sample_format {
        cpal::SampleFormat::I8 => build_output::<i8>(&device, config, shared),
        cpal::SampleFormat::I16 => build_output::<i16>(&device, config, shared),
        cpal::SampleFormat::I24 => build_output::<cpal::I24>(&device, config, shared),
        cpal::SampleFormat::I32 => build_output::<i32>(&device, config, shared),
        cpal::SampleFormat::I64 => build_output::<i64>(&device, config, shared),
        cpal::SampleFormat::U8 => build_output::<u8>(&device, config, shared),
        cpal::SampleFormat::U16 => build_output::<u16>(&device, config, shared),
        cpal::SampleFormat::U24 => build_output::<cpal::U24>(&device, config, shared),
        cpal::SampleFormat::U32 => build_output::<u32>(&device, config, shared),
        cpal::SampleFormat::U64 => build_output::<u64>(&device, config, shared),
        cpal::SampleFormat::F32 => build_output::<f32>(&device, config, shared),
        cpal::SampleFormat::F64 => build_output::<f64>(&device, config, shared),
        format => Err(format!(
            "The selected audio device uses unsupported sample format {format:?}."
        )),
    }
}

fn build_output<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: SharedPlayback,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    use cpal::traits::StreamTrait;
    let channels = usize::from(config.channels);
    let sample_rate = f64::from(config.sample_rate);
    let callback_state = shared.clone();
    let stream = device
        .build_output_stream(
            config,
            move |output: &mut [T], _| {
                let Ok(mut state) = callback_state.try_lock() else {
                    output.fill(T::from_sample(0.0));
                    return;
                };
                if !state.clock.playing {
                    output.fill(T::from_sample(0.0));
                    return;
                }
                let mut position = state.clock.position;
                for frame in output.chunks_mut(channels) {
                    let sample = state.sample_at(position).clamp(-1.0, 1.0);
                    frame.fill(T::from_sample(sample));
                    if state.clock.playing {
                        position += 1.0 / sample_rate;
                    }
                }
                if state.clock.playing {
                    state.clock.position = position.min(state.end);
                    state.clock.updated_at = Instant::now();
                    if position >= state.end {
                        state.clock.playing = false;
                    }
                }
            },
            |error| eprintln!("Timeline audio output error: {error}"),
            None,
        )
        .map_err(|error| format!("Could not start audio output: {error}"))?;
    stream
        .play()
        .map_err(|error| format!("Could not start audio output: {error}"))?;
    Ok(stream)
}

impl PlaybackItem {
    fn source_time_at(&self, timeline_seconds: f64) -> Option<f64> {
        let end = self.start + self.duration;
        if !timeline_seconds.is_finite() || timeline_seconds < self.start || timeline_seconds >= end
        {
            return None;
        }
        let points = &self.mapping_points;
        let mapped = if points.len() >= 2 {
            let pair = points.windows(2).find(|pair| {
                let left = pair[0].island.as_seconds();
                let right = pair[1].island.as_seconds();
                timeline_seconds >= left.min(right) && timeline_seconds <= left.max(right)
            })?;
            let island_a = pair[0].island.as_seconds();
            let island_b = pair[1].island.as_seconds();
            let source_a = pair[0].source.as_seconds();
            let source_b = pair[1].source.as_seconds();
            if (island_b - island_a).abs() < f64::EPSILON {
                return None;
            }
            source_a + (timeline_seconds - island_a) * (source_b - source_a) / (island_b - island_a)
        } else {
            self.source_in
                + (timeline_seconds - self.start) / self.duration
                    * (self.source_out - self.source_in)
        };
        (mapped >= self.source_in && mapped < self.source_out).then_some(mapped)
    }

    fn is_audible(&self, muted: &HashSet<String>, soloed: &HashSet<String>) -> bool {
        !muted.contains(&self.lane_id) && (soloed.is_empty() || soloed.contains(&self.lane_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use align_core::{MappingPoint, MediaTime};
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn item() -> PlaybackItem {
        PlaybackItem {
            url: PathBuf::from("clip.wav"),
            lane_id: "lane-1".into(),
            start: 5.0,
            duration: 4.0,
            source_in: 10.0,
            source_out: 14.0,
            mapping_points: vec![
                MappingPoint {
                    source: MediaTime::seconds(10.0),
                    island: MediaTime::seconds(5.0),
                },
                MappingPoint {
                    source: MediaTime::seconds(11.25),
                    island: MediaTime::seconds(6.0),
                },
                MappingPoint {
                    source: MediaTime::seconds(12.5),
                    island: MediaTime::seconds(7.0),
                },
                MappingPoint {
                    source: MediaTime::seconds(14.0),
                    island: MediaTime::seconds(9.0),
                },
            ],
        }
    }

    #[test]
    fn timeline_position_maps_piecewise_to_source_time() {
        let clip = item();

        assert_eq!(clip.source_time_at(5.0), Some(10.0));
        assert_eq!(clip.source_time_at(6.0), Some(11.25));
        assert_eq!(clip.source_time_at(7.0), Some(12.5));
        assert_eq!(clip.source_time_at(9.0), None);
    }

    #[test]
    fn camera_video_lane_is_audible_and_obeys_mute_solo() {
        let clip = item();
        let muted = HashSet::new();
        let soloed = HashSet::new();
        assert!(clip.is_audible(&muted, &soloed));

        let muted = HashSet::from(["lane-1".to_owned()]);
        assert!(!clip.is_audible(&muted, &soloed));

        let muted = HashSet::new();
        let soloed = HashSet::from(["other-lane".to_owned()]);
        assert!(!clip.is_audible(&muted, &soloed));
    }

    #[test]
    fn playback_samples_follow_piecewise_export_drift_mapping() {
        let clip = item();
        let mut samples = vec![0.0; 14 * 48_000];
        samples[((11.25 - 10.0) * 48_000.0) as usize] = 0.75;
        let state = PlaybackState {
            clock: PlaybackClock::new(0.0),
            items: vec![DecodedItem {
                item: clip,
                samples: samples.into(),
            }],
            muted: HashSet::new(),
            soloed: HashSet::new(),
            end: 20.0,
        };

        assert_eq!(state.sample_at(5.8), 0.0);
        assert_eq!(state.sample_at(6.0), 0.75);
    }

    #[test]
    fn mute_and_solo_filter_audio_lanes_but_preserve_camera_audio() {
        let mut recorder = item();
        recorder.lane_id = "recorder".into();
        let mut camera = item();
        camera.lane_id = "camera".into();
        let state = PlaybackState {
            clock: PlaybackClock::new(0.0),
            items: vec![
                DecodedItem {
                    item: recorder,
                    samples: vec![0.25; 20 * 48_000].into(),
                },
                DecodedItem {
                    item: camera,
                    samples: vec![0.5; 20 * 48_000].into(),
                },
            ],
            muted: HashSet::from(["recorder".to_owned()]),
            soloed: HashSet::new(),
            end: 20.0,
        };

        assert_eq!(state.sample_at(5.0), 0.5);
    }

    #[test]
    fn only_active_lane_remains_audible_when_muted_or_excluded_by_solo() {
        let mut first = item();
        first.lane_id = "only-lane".into();
        let mut second = first.clone();
        second.start = 6.0;
        second.duration = 2.0;
        second.source_in = 11.0;
        second.source_out = 13.0;
        second.mapping_points = vec![
            MappingPoint {
                source: MediaTime::seconds(11.0),
                island: MediaTime::seconds(6.0),
            },
            MappingPoint {
                source: MediaTime::seconds(13.0),
                island: MediaTime::seconds(8.0),
            },
        ];
        let state = PlaybackState {
            clock: PlaybackClock::new(0.0),
            items: vec![
                DecodedItem {
                    item: first,
                    samples: vec![0.25; 20 * 48_000].into(),
                },
                DecodedItem {
                    item: second,
                    samples: vec![0.25; 20 * 48_000].into(),
                },
            ],
            muted: HashSet::from(["only-lane".to_owned()]),
            soloed: HashSet::from(["another-lane".to_owned()]),
            end: 20.0,
        };

        assert!(state.sample_at(6.5) > 0.0);
    }
}
