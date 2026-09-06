//! Cancellable process boundary for the bundled AAF reader and writer.
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Deserialize)]
pub struct ImportedAudio {
    pub version: u32,
    pub sequences: Vec<ImportedSequence>,
}
#[derive(Debug, Deserialize)]
pub struct ImportedSequence {
    pub name: String,
    pub tracks: Vec<ImportedTrack>,
}
#[derive(Debug, Deserialize)]
pub struct ImportedTrack {
    pub name: String,
    pub sample_rate: u32,
    pub clips: Vec<ImportedClip>,
}
#[derive(Debug, Deserialize)]
pub struct ImportedClip {
    pub path: PathBuf,
    pub start: u64,
    pub source_in: u64,
    pub length: u64,
    pub channel: usize,
}

impl ImportedAudio {
    pub fn summaries(&self) -> Vec<align_core::TimelineSequenceSummary> {
        self.sequences
            .iter()
            .enumerate()
            .map(|(index, sequence)| align_core::TimelineSequenceSummary {
                index,
                name: sequence.name.clone(),
                clip_count: sequence.tracks.iter().map(|track| track.clips.len()).sum(),
            })
            .collect()
    }

    pub fn into_draft(
        self,
        path: &Path,
        sequence_index: Option<usize>,
    ) -> Result<align_core::xml::TimelineDraft, AafError> {
        use align_core::{
            MediaTime,
            xml::{DraftEdit, DraftMediaKind, TimelineDraft},
        };
        validate_import(&self)?;
        if sequence_index.is_none() && self.sequences.len() != 1 {
            return Err(AafError::Writer(format!(
                "{} contains {} sequences. Choose one explicitly",
                path.display(),
                self.sequences.len()
            )));
        }
        let index = sequence_index.unwrap_or(0);
        let sequence = self.sequences.into_iter().nth(index).ok_or_else(|| {
            AafError::Writer(format!(
                "Sequence {} does not exist in {}",
                index + 1,
                path.display()
            ))
        })?;
        let mut edits = Vec::new();
        for (track_index, track) in sequence.tracks.into_iter().enumerate() {
            let rate = track.sample_rate as f64;
            for (clip_index, clip) in track.clips.into_iter().enumerate() {
                edits.push(DraftEdit {
                    id: format!("aaf-{index}-{track_index}-{clip_index}"),
                    name: clip
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned()),
                    url: clip.path,
                    media_type: DraftMediaKind::Audio,
                    source_in: clip.source_in as f64 / rate,
                    source_out: (clip.source_in + clip.length) as f64 / rate,
                    timeline_start: clip.start as f64 / rate,
                    timeline_end: (clip.start + clip.length) as f64 / rate,
                    playback_rate: 1.0,
                    plays_backward: false,
                    fcp7_time_remap_xml: None,
                    fcp7_filter_xmls: Vec::new(),
                    fcp7_retime_in: None,
                    fcp7_retime_out: None,
                    fcp7_retime_duration: None,
                    fcp7_labels_xml: None,
                    time_scale: track.sample_rate as i32,
                    audio_source_channel: Some(clip.channel),
                    fcpxml_audio_role: None,
                    track_index: track_index + 1,
                    enabled: true,
                    track_enabled: true,
                    track_locked: false,
                    linked_edit_ids: Default::default(),
                });
            }
        }
        if edits.is_empty() {
            return Err(AafError::Writer(
                "AAF sequence contains no audio clips".into(),
            ));
        }
        Ok(TimelineDraft {
            source_url: path.to_path_buf(),
            name: sequence.name,
            // Audio AAF has no picture edit rate. Use the application's default
            // picture rate while retaining each edit's native sample clock.
            frame_duration: MediaTime::new(1, 25),
            edits,
            transitions: Vec::new(),
            warnings: Vec::new(),
        })
    }
}

pub fn read_audio(path: &Path, cancel: &AtomicBool) -> Result<ImportedAudio, AafError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(AafError::Cancelled);
    }
    let executable = crate::ff::resolve_bin("ALIGN_AAF", "align-aaf").ok_or(AafError::Missing)?;
    // A file avoids stdout pipe deadlocks on large compositions while retaining
    // cancellable process supervision and automatic temporary-file cleanup.
    let mut output = tempfile::tempfile()?;
    let mut child = Command::new(executable)
        .arg("read-audio")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
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
    use std::io::{Seek, SeekFrom};
    output.seek(SeekFrom::Start(0))?;
    let result: ImportedAudio =
        serde_json::from_reader(output).map_err(|e| AafError::Writer(e.to_string()))?;
    validate_import(&result)?;
    Ok(result)
}

fn validate_import(result: &ImportedAudio) -> Result<(), AafError> {
    if result.version != 1 || result.sequences.is_empty() {
        return Err(AafError::Writer("Unsupported or empty AAF response".into()));
    }
    for sequence in &result.sequences {
        for track in &sequence.tracks {
            if track.sample_rate == 0
                || track.sample_rate > i32::MAX as u32
                || track.clips.iter().any(|clip| {
                    clip.length == 0
                        || clip.channel == usize::MAX
                        || clip
                            .start
                            .checked_add(clip.length)
                            .is_none_or(|end| end > (1u64 << 53) - 1)
                        || clip
                            .source_in
                            .checked_add(clip.length)
                            .is_none_or(|end| end > (1u64 << 53) - 1)
                })
            {
                return Err(AafError::Writer("Invalid AAF sample range".into()));
            }
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum AafError {
    #[error("AAF support module is missing; reinstall the complete Align package")]
    Missing,
    #[error("AAF operation cancelled")]
    Cancelled,
    #[error("AAF support module failed: {0}")]
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
                if item
                    .selected_audio_source_channel()
                    .is_some_and(|selected| selected != channel)
                {
                    continue;
                }
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

    fn stereo_import() -> ImportedAudio {
        serde_json::from_value(serde_json::json!({
            "version":1, "sequences":[{"name":"Stereo edit", "tracks":
                (0..2).map(|channel| serde_json::json!({
                    "name":format!("Channel {channel}"), "sample_rate":48000,
                    "clips":[{"path":"/media/stereo.wav", "start":96001,
                        "source_in":24001, "length":48001, "channel":channel}]
                })).collect::<Vec<_>>()
            }]
        }))
        .unwrap()
    }

    #[test]
    fn imported_stereo_edits_remain_discrete_through_resolve_and_export() {
        use align_core::{
            Clip, MediaTime, SyncResult,
            export::{ExportTimeline, TimelineExportFormat, fcpxml, otio, premiere},
        };
        let clip: Clip = serde_json::from_value(serde_json::json!({
            "id":"stereo", "url":"/media/stereo.wav", "kind":"audio",
            "duration":{"value":10,"timescale":1},
            "audio":[{"sampleRate":48000.0,"channels":2,"bitDepth":24}]
        }))
        .unwrap();
        let draft = stereo_import()
            .into_draft(Path::new("edit.aaf"), None)
            .unwrap();
        draft
            .validate_source_channels(std::slice::from_ref(&clip))
            .unwrap();
        let imported = draft.resolve(std::slice::from_ref(&clip));
        assert_eq!(
            imported.edits.len(),
            2,
            "coincident source channels must not collapse"
        );
        for (channel, edit) in imported.edits.iter().enumerate() {
            assert_eq!(edit.audio_source_channel, Some(channel));
            assert_eq!(edit.source_in, MediaTime::new(24001, 48000));
            assert_eq!(edit.source_out, MediaTime::new(72002, 48000));
            assert_eq!(edit.timeline_start, MediaTime::new(96001, 48000));
            assert_eq!(edit.timeline_end, MediaTime::new(144002, 48000));
        }
        let result: SyncResult = serde_json::from_value(serde_json::json!({
            "project":{"clips":[clip], "warnings":[], "importedTimeline": imported},
            "islands":[], "unmatched":["stereo"], "matches":[]
        }))
        .unwrap();
        let mut timeline = ExportTimeline::from_result(&result, true).unwrap();
        assert_eq!(timeline.islands[0].clips.len(), 2);
        assert_eq!(
            timeline.islands[0].clips[1]
                .audio_source_channels()
                .collect::<Vec<_>>(),
            vec![2]
        );
        let temp = tempfile::tempdir().unwrap();
        for (extension, xml) in [
            (
                "xml",
                premiere::write(&timeline, TimelineExportFormat::PremiereXML, false),
            ),
            ("fcpxml", fcpxml::write(&timeline, false)),
        ] {
            let path = temp.path().join(format!("roundtrip.{extension}"));
            std::fs::write(&path, xml).unwrap();
            let restored = align_core::read_timeline(&path, None).unwrap();
            assert_eq!(restored.edits.len(), 2, "{extension}");
            let mut channels: Vec<_> = restored
                .edits
                .iter()
                .map(|edit| edit.audio_source_channel)
                .collect();
            channels.sort();
            assert_eq!(channels, vec![Some(0), Some(1)], "{extension}");
        }
        let otio: serde_json::Value =
            serde_json::from_slice(&otio::data(&timeline).unwrap()).unwrap();
        let mut channels: Vec<_> = otio["tracks"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|track| track["kind"] == "Audio")
            .flat_map(|track| track["children"].as_array().unwrap())
            .filter_map(|clip| {
                clip["metadata"]["Resolve_OTIO"]["Channels"][0]["Source Channel ID"].as_i64()
            })
            .collect();
        channels.sort();
        assert_eq!(channels, vec![0, 1]);
        for item in &mut timeline.islands[0].clips {
            item.precision_audio_urls = vec!["/stems/left.wav".into(), "/stems/right.wav".into()];
        }
        let manifest = audio_manifest(&timeline).unwrap();
        assert_eq!(manifest.tracks.len(), 2);
        let mut paths: Vec<_> = manifest
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .map(|clip| clip.path.clone())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/stems/left.wav"),
                PathBuf::from("/stems/right.wav")
            ]
        );
        let mut invalid_clip = result.project.clips[0].clone();
        invalid_clip.audio[0].channels = 1;
        assert!(draft.validate_source_channels(&[invalid_clip]).is_err());
    }

    #[test]
    fn aaf_sequence_selection_is_explicit() {
        let mut imported = stereo_import();
        imported.sequences.push(ImportedSequence {
            name: "Empty".into(),
            tracks: vec![],
        });
        let summaries = imported.summaries();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].clip_count, 2);
        assert_eq!(summaries[1].name, "Empty");
        assert!(imported.into_draft(Path::new("edit.aaf"), None).is_err());
        assert!(
            stereo_import()
                .into_draft(Path::new("edit.aaf"), Some(1))
                .is_err()
        );
        assert!(
            stereo_import()
                .into_draft(Path::new("edit.aaf"), Some(0))
                .is_ok()
        );
    }
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

    #[test]
    fn cancelled_import_does_not_start_reader() {
        assert!(matches!(
            read_audio(Path::new("missing.aaf"), &AtomicBool::new(true)),
            Err(AafError::Cancelled)
        ));
    }

    #[test]
    fn reader_validates_sample_ranges_and_preserves_channels() {
        let mut imported: ImportedAudio = serde_json::from_str(
            r#"{
            "version":1,"sequences":[{"name":"Edit","tracks":[{
                "name":"Audio","sample_rate":48000,"clips":[{
                    "path":"/media/stereo.wav","start":96000,"source_in":24000,
                    "length":672000,"channel":1
                }]
            }]}]
        }"#,
        )
        .unwrap();
        validate_import(&imported).unwrap();
        assert_eq!(imported.sequences[0].tracks[0].clips[0].channel, 1);
        imported.sequences[0].tracks[0].clips[0].start = u64::MAX;
        assert!(validate_import(&imported).is_err());
        imported.sequences[0].tracks[0].clips[0].start = 0;
        imported.sequences[0].tracks[0].clips[0].source_in = u64::MAX;
        assert!(validate_import(&imported).is_err());
        imported.sequences[0].tracks[0].clips[0].source_in = 0;
        imported.sequences[0].tracks[0].clips[0].length = 0;
        assert!(validate_import(&imported).is_err());
        imported.sequences[0].tracks[0].clips[0].length = 1;
        imported.sequences[0].tracks[0].sample_rate = 0;
        assert!(validate_import(&imported).is_err());
        imported.sequences[0].tracks[0].sample_rate = 48000;
        imported.version = 2;
        assert!(validate_import(&imported).is_err());
        imported.version = 1;
        imported.sequences.clear();
        assert!(validate_import(&imported).is_err());
    }
}
