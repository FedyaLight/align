//! Process boundary for the bundled AAF writer.
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Serialize)]
pub struct AudioManifest {
    pub version: u32,
    pub name: String,
    pub tracks: Vec<AudioTrack>,
}
#[derive(Debug, Serialize)]
pub struct AudioTrack {
    pub name: String,
    pub sample_rate: u32,
    pub clips: Vec<AudioClip>,
}
#[derive(Debug, Serialize)]
pub struct AudioClip {
    pub path: PathBuf,
    pub start: u64,
    pub source_in: u64,
    pub length: u64,
    pub source_frames: u64,
    pub channels: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum AafError {
    #[error("AAF support module is missing; reinstall the complete Align package")]
    Missing,
    #[error("AAF export cancelled")]
    Cancelled,
    #[error("AAF writer failed: {0}")]
    Writer(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Invoke a manifest already staged by the export job. No shell interpretation.
/// The helper atomically commits the destination only after completing the AAF.
pub fn write_audio(
    manifest: &Path,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<(), AafError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(AafError::Cancelled);
    }
    let executable = crate::ff::resolve_bin("ALIGN_AAF", "align-aaf").ok_or(AafError::Missing)?;
    let mut child = Command::new(executable)
        .arg("write-audio")
        .arg(manifest)
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let status = crate::export::wait_render_process(&mut child, cancel).map_err(|error| {
        if cancel.load(Ordering::Relaxed) {
            AafError::Cancelled
        } else {
            AafError::Writer(error.to_string())
        }
    })?;
    if !status.success() {
        return Err(AafError::Writer(status.to_string()));
    }
    if !destination.is_file() {
        return Err(AafError::Writer("no output file".into()));
    }
    Ok(())
}

/// Assemble already-rendered mono stems. Reject unsupported edit semantics
/// rather than silently dropping them while the AAF implementation expands.
pub fn audio_manifest(
    timeline: &align_core::export::ExportTimeline,
) -> Result<AudioManifest, AafError> {
    use std::collections::BTreeMap;
    let fail = |text: &str| AafError::Writer(text.to_owned());
    let mut groups: BTreeMap<(String, u32, usize), Vec<AudioClip>> = BTreeMap::new();
    for island in &timeline.islands {
        for item in &island.clips {
            if item.clip.video.is_some()
                || item.linked_audio_edit.is_some()
                || item.is_retimed("audio")
                || item.transition_for("audio").is_some()
                || !item.enabled
                || !item.track_enabled
            {
                return Err(fail("AAF edit semantics not implemented for this item"));
            }
            let audio = item
                .clip
                .audio
                .first()
                .ok_or_else(|| fail("AAF item has no audio"))?;
            let rate = audio.sample_rate;
            if !rate.is_finite() || rate < 1.0 || rate > u32::MAX as f64 || rate.fract() != 0.0 {
                return Err(fail("Invalid AAF sample rate"));
            }
            if item.precision_audio_urls.len() != audio.channels || audio.channels == 0 {
                return Err(fail("AAF requires prepared mono stems for every channel"));
            }
            let samples = |seconds: f64| -> Result<u64, AafError> {
                let value = seconds * rate;
                if !value.is_finite() || value < 0.0 || value >= u64::MAX as f64 {
                    return Err(fail("Invalid AAF sample position"));
                }
                Ok(value.round() as u64)
            };
            let start = samples(item.timeline_start("audio"))?;
            let length = samples(item.precision_source_duration())?;
            let frames = length
                .checked_add(item.precision_tail_samples)
                .ok_or_else(|| fail("AAF length overflow"))?;
            if length == 0 || start.checked_add(length).is_none() {
                return Err(fail("Invalid AAF clip length"));
            }
            let source = item
                .preferred_source_key("audio")
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    align_core::allocator::source_key_for_clip(
                        &item.clip.url,
                        item.clip.source_identifier.as_deref(),
                        None,
                    )
                });
            for (channel, path) in item.precision_audio_urls.iter().enumerate() {
                groups
                    .entry((source.clone(), rate as u32, channel))
                    .or_default()
                    .push(AudioClip {
                        path: path.clone(),
                        start,
                        source_in: 0,
                        length,
                        source_frames: frames,
                        channels: 1,
                    });
            }
        }
    }
    let mut tracks = Vec::new();
    for ((source, sample_rate, channel), mut clips) in groups {
        clips.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.path.cmp(&b.path)));
        let mut lanes: Vec<AudioTrack> = Vec::new();
        for clip in clips {
            let lane = lanes.iter().position(|track| {
                track
                    .clips
                    .last()
                    .is_none_or(|last| last.start + last.length <= clip.start)
            });
            let index = lane.unwrap_or_else(|| {
                lanes.push(AudioTrack {
                    name: format!(
                        "{} / channel {} / lane {}",
                        source,
                        channel + 1,
                        lanes.len() + 1
                    ),
                    sample_rate,
                    clips: Vec::new(),
                });
                lanes.len() - 1
            });
            lanes[index].clips.push(clip);
        }
        tracks.extend(lanes);
    }
    if tracks.is_empty() {
        return Err(fail("No AAF audio tracks"));
    }
    Ok(AudioManifest {
        version: 1,
        name: timeline.name.clone(),
        tracks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_preserves_subframe_placement_and_splits_overlaps() {
        use align_core::export::{ExportIsland, ExportItem, ExportTimeline};
        use align_core::{Clip, MediaTime};
        let clip: Clip = serde_json::from_value(serde_json::json!({
            "id":"a", "url":"/recordings/a.wav", "kind":"audio",
            "duration":{"value":1,"timescale":1},
            "audio":[{"sampleRate":48000.0,"channels":2,"bitDepth":24}]
        }))
        .unwrap();
        let mut a = ExportItem::new(clip, 1.0 / 48000.0, 1.0, vec![], 1.0);
        a.precision_audio_urls = vec!["/stems/left.wav".into(), "/stems/right.wav".into()];
        let mut b = a.clone();
        b.start = 0.5;
        b.instance_id = "b".into();
        let timeline = ExportTimeline::new(
            vec![ExportIsland {
                id: 0,
                clips: vec![a, b],
                duration: 2.0,
            }],
            MediaTime::new(1, 25),
            "Test",
        );
        let manifest = audio_manifest(&timeline).unwrap();
        assert_eq!(manifest.tracks.len(), 4);
        assert_eq!(manifest.tracks[0].clips[0].start, 1);
        assert_eq!(manifest.tracks[0].clips[0].length, 48000);
        assert_eq!(manifest.tracks[1].clips[0].start, 24000);
        assert_eq!(
            manifest.tracks[2].clips[0].path,
            PathBuf::from("/stems/right.wav")
        );
    }
    #[test]
    fn cancelled_export_does_not_start_writer() {
        assert!(matches!(
            write_audio(
                Path::new("missing.json"),
                Path::new("missing.aaf"),
                &AtomicBool::new(true)
            ),
            Err(AafError::Cancelled)
        ));
    }
}
