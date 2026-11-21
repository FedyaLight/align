//! OpenTimelineIO output for Resolve.
//!
//! Frame-quantized portable timeline (the sibling import script places its
//! recordings between frames): video tracks with camera A/V link groups,
//! per-channel mono audio tracks of the original files, gaps, portable
//! Cross Dissolves as `SMPTE_Dissolve`, and `LinearTimeWarp` for constant
//! retimes (negative scalar for reverse). Serialized pretty-printed with
//! sorted keys and unescaped slashes. `Align` metadata tells the import
//! script which channels form one recording, and optionally lists the media
//! bins it creates.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde_json::{Value, json};

use super::model::{ExportIsland, ExportItem, ExportTimeline, ExportTransition};
use crate::allocator::{TimelineTrackRequest, source_key_for_url};
use crate::bins::{GENERATED_AUDIO, MediaBins};
use crate::model::{MediaKind, TIME_TOLERANCE};

#[derive(Clone, Copy)]
struct Entry<'a> {
    item: &'a ExportItem,
    source_channel: usize,
}

/// The OTIO document. With `media_bins`, its metadata lists each camera and
/// recorder bin's source files for the import script, which files them into
/// media pool folders.
pub fn data(timeline: &ExportTimeline, media_bins: bool) -> Result<Vec<u8>, String> {
    let rate = 1.0 / timeline.frame_duration.as_seconds();
    let island = timeline.islands.first().ok_or("no islands")?;
    let video_tracks = tracks(
        island
            .clips
            .iter()
            .filter(|i| i.clip.video.is_some())
            .map(|item| Entry {
                item,
                source_channel: 0,
            })
            .collect(),
        MediaKind::Video,
    );
    let audio_tracks = tracks(
        island
            .clips
            .iter()
            .filter(|i| !i.clip.audio.is_empty())
            .flat_map(|item| {
                item.audio_source_channels().map(|ch| Entry {
                    item,
                    source_channel: ch,
                })
            })
            .collect(),
        MediaKind::Audio,
    );
    let resolve_video: Vec<Vec<Entry>> = if video_tracks.is_empty() {
        vec![Vec::new()]
    } else {
        video_tracks
    };
    let mut link_groups: HashMap<&str, usize> = HashMap::new();
    for (index, entry) in resolve_video.iter().flat_map(|t| t.iter()).enumerate() {
        link_groups
            .entry(entry.item.instance_id.as_str())
            .or_insert(index + 1);
    }
    let mut children = Vec::new();
    for (i, track) in resolve_video.iter().enumerate() {
        children.push(track_json(
            track,
            &format!("Video {}", i + 1),
            MediaKind::Video,
            rate,
            &link_groups,
        ));
    }
    for (i, track) in audio_tracks.iter().enumerate() {
        children.push(track_json(
            track,
            &format!("Audio {}", i + 1),
            MediaKind::Audio,
            rate,
            &link_groups,
        ));
    }
    let root = json!({
        "OTIO_SCHEMA": "Timeline.1",
        "name": timeline.name,
        // Match the XML writer's 01:00:00:00 NDF label. At fractional FPS,
        // one timecode hour is nominal_fps * 3600 frames, not 3600 seconds.
        "global_start_time": {
            "OTIO_SCHEMA": "RationalTime.1",
            "rate": rate,
            "value": rate.round() * 3_600.0,
        },
        "metadata": metadata(island, media_bins),
        "tracks": {
            "OTIO_SCHEMA": "Stack.1",
            "metadata": {},
            "name": "",
            "source_range": null,
            "effects": [],
            "markers": [],
            "enabled": true,
            "children": children,
        },
    });
    serde_json::to_vec_pretty(&root).map_err(|e| e.to_string())
}

/// Resolve metadata, plus with `media_bins` the source files of each bin
/// and the audio files Align generated for the timeline.
fn metadata(island: &ExportIsland, media_bins: bool) -> Value {
    let mut metadata = json!({ "Resolve_OTIO": { "Resolve OTIO Meta Version": "1.0" } });
    if media_bins {
        let bins = MediaBins::new(island.clips.iter().map(|item| &item.clip));
        let mut generated: Vec<String> = Vec::new();
        for url in island
            .clips
            .iter()
            .filter_map(|item| item.corrected_audio_url.as_ref())
        {
            let path = url.to_string_lossy().into_owned();
            if !generated.contains(&path) {
                generated.push(path);
            }
        }
        metadata["Align"] = json!({
            "mediaBins": bins
                .bins()
                .iter()
                .map(|bin| json!({
                    "name": bin.name,
                    "media": bin
                        .clips
                        .iter()
                        .map(|clip| clip.url.to_string_lossy())
                        .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
            "generatedAudio": { "name": GENERATED_AUDIO, "media": generated },
        });
    }
    metadata
}

fn track_json(
    entries: &[Entry],
    name: &str,
    kind: MediaKind,
    rate: f64,
    link_groups: &HashMap<&str, usize>,
) -> Value {
    let mut cursor = 0.0;
    let mut children = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let before = if index > 0 {
            transition_between(&entries[index - 1], entry, kind)
        } else {
            None
        };
        let after = if index + 1 < entries.len() {
            transition_between(entry, &entries[index + 1], kind)
        } else {
            None
        };
        let item_start = timeline_start(entry, kind);
        if before.is_none() && item_start > cursor + TIME_TOLERANCE {
            children.push(gap(item_start - cursor, rate));
        }
        let trim_start = before.map_or(0.0, |t| (t.end - t.start) / 2.0);
        // A clip runs into the next at a seam of one recording's parts; it
        // ends where the next starts, or a track's clips play back to back
        // and the next would start that much later.
        let duration = timeline_duration(entry, kind);
        let seam = entries
            .get(index + 1)
            .filter(|_| after.is_none())
            .map_or(0.0, |next| {
                item_start + duration - timeline_start(next, kind)
            });
        let trim_end = match after {
            Some(t) => (t.end - t.start) / 2.0,
            None if seam > TIME_TOLERANCE && seam < duration => seam,
            None => 0.0,
        };
        children.push(clip_json(
            entry,
            kind,
            rate,
            link_groups,
            trim_start,
            trim_end,
        ));
        if let Some(t) = after {
            children.push(transition_json(t, rate));
        }
        cursor = cursor.max(item_start + timeline_duration(entry, kind) - trim_end);
    }
    let mut resolve_meta = BTreeMap::from([("Locked".to_string(), json!(false))]);
    if kind == MediaKind::Audio {
        resolve_meta.insert("Audio Type".to_string(), json!("Mono"));
        resolve_meta.insert("SoloOn".to_string(), json!(false));
    }
    resolve_meta.insert(
        "Locked".to_string(),
        json!(
            entries
                .first()
                .is_some_and(|e| e.item.is_track_locked(kind))
        ),
    );
    json!({
        "OTIO_SCHEMA": "Track.1",
        "metadata": { "Resolve_OTIO": resolve_meta },
        "name": name,
        "source_range": null,
        "effects": [],
        "markers": [],
        "enabled": entries.first().is_none_or(|e| e.item.is_track_enabled(kind)),
        "kind": otio_kind(kind),
        "children": children,
    })
}

fn clip_json(
    entry: &Entry,
    kind: MediaKind,
    rate: f64,
    link_groups: &HashMap<&str, usize>,
    trim_start: f64,
    trim_end: f64,
) -> Value {
    let item = entry.item;
    let corrected = kind == MediaKind::Audio && item.corrected_audio_url.is_some();
    let media_url: PathBuf = match &item.corrected_audio_url {
        Some(url) if corrected => url.clone(),
        _ => item.clip.url.clone(),
    };
    let base = media_start(item, kind);
    let untrimmed_in = if corrected {
        item.corrected_source_in()
    } else {
        item.selected_source_in(kind)
    };
    let duration = (timeline_duration(entry, kind) - trim_start - trim_end).max(0.0);
    let available = if corrected {
        item.mapped_duration()
    } else {
        item.source_duration()
    };
    let playback_rate = item.playback_rate(kind);
    let plays_backward = item.plays_backward(kind);
    let source_in = untrimmed_in + trim_start * playback_rate;

    let mut resolve_meta = BTreeMap::new();
    if let Some(group) = link_groups.get(item.instance_id.as_str()) {
        resolve_meta.insert("Link Group ID".to_string(), json!(*group));
    }
    if kind == MediaKind::Audio {
        resolve_meta.insert(
            "Channels".to_string(),
            json!([{
                "Source Channel ID": entry.source_channel as i64 - 1,
                "Source Track ID": 0,
            }]),
        );
    }
    let mut metadata = serde_json::Map::new();
    if !resolve_meta.is_empty() {
        metadata.insert("Resolve_OTIO".to_string(), json!(resolve_meta));
    }
    if kind == MediaKind::Audio {
        // The import script places a recording once, all its channels on a
        // track of their format, or its one selected channel.
        metadata.insert(
            "Align".to_string(),
            json!({
                "Item": item.instance_id,
                "Channel": entry.source_channel,
                "Channels": item.clip.audio.first().map_or(1, |audio| audio.channels),
                "Selected Channel": item.selected_audio_source_channel().is_some(),
            }),
        );
    }
    if let Some(remap) = item.fcp7_time_remap_xml(kind) {
        let align = metadata.entry("Align").or_insert_with(|| json!({}));
        align["FCP7 Time Remap XML"] = json!(remap);
        align["Source In Seconds"] = json!(item.selected_source_in(kind));
        align["Source Out Seconds"] = json!(item.selected_source_out(kind));
        align["Plays Backward"] = json!(plays_backward);
    }
    if let Some(color) = item.fcp7_labels_xml(kind).and_then(resolve_clip_color) {
        metadata.entry("Align").or_insert_with(|| json!({}))["Resolve Clip Color"] = json!(color);
    }
    let effects: Vec<Value> = if item.is_retimed(kind) {
        vec![json!({
            "OTIO_SCHEMA": "LinearTimeWarp.1",
            "metadata": {},
            "name": "",
            "effect_name": "LinearTimeWarp",
            "enabled": true,
            "time_scalar": if plays_backward { -playback_rate } else { playback_rate },
        })]
    } else {
        Vec::new()
    };
    let file_name = media_url
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    json!({
        "OTIO_SCHEMA": "Clip.2",
        "metadata": metadata,
        "name": item.display_name.clone().unwrap_or(file_name.clone()),
        "source_range": time_range(base + source_in, duration, rate),
        "effects": effects,
        "markers": [],
        "enabled": item.is_enabled(kind),
        "media_references": {
            "DEFAULT_MEDIA": {
                "OTIO_SCHEMA": "ExternalReference.1",
                "metadata": {},
                "name": file_name,
                "available_range": time_range(base, available, rate),
                "available_image_bounds": null,
                "target_url": media_url.to_string_lossy(),
            },
        },
        "active_media_reference_key": "DEFAULT_MEDIA",
    })
}

/// The Resolve clip color closest to an FCP 7 / Premiere color label.
/// Resolve imports neither the label nor a clip color from XML or OTIO, so
/// the import script applies this one; `None` leaves the clip uncolored.
fn resolve_clip_color(labels_xml: &str) -> Option<&'static str> {
    let label = labels_xml
        .split_once("<label2>")?
        .1
        .split_once("</label2>")?
        .0
        .trim()
        .to_ascii_lowercase();
    Some(match label.as_str() {
        "orange" | "mango" | "red" => "Orange",
        "apricot" => "Apricot",
        "yellow" => "Yellow",
        "lime" => "Lime",
        "olive" => "Olive",
        "green" | "forest" => "Green",
        "teal" | "caribbean" => "Teal",
        "navy" => "Navy",
        "blue" | "iris" | "cerulean" => "Blue",
        "purple" => "Purple",
        "violet" => "Violet",
        "pink" | "rose" | "magenta" | "lavender" => "Pink",
        "tan" => "Tan",
        "beige" => "Beige",
        "brown" => "Brown",
        "chocolate" => "Chocolate",
        _ => return None,
    })
}

fn gap(duration: f64, rate: f64) -> Value {
    json!({
        "OTIO_SCHEMA": "Gap.1",
        "metadata": {},
        "name": "",
        "source_range": time_range(0.0, duration, rate),
        "effects": [],
        "markers": [],
        "enabled": true,
    })
}

/// Portable dissolve or crossfade between consecutive entries of a track
/// of `kind`.
fn transition_between<'a>(
    left: &'a Entry,
    right: &'a Entry,
    kind: MediaKind,
) -> Option<&'a ExportTransition> {
    let t = left.item.transition_for(kind)?;
    if t.is_otio_portable && t.right_instance_id == right.item.instance_id {
        Some(t)
    } else {
        None
    }
}

fn transition_json(t: &ExportTransition, rate: f64) -> Value {
    let half = (t.end - t.start) / 2.0;
    json!({
        "OTIO_SCHEMA": "Transition.1",
        "metadata": { "Align": {
            "FCP7 Effect XML": t.fcp7_effect_xml,
            "FCP7 Transition XML": t.fcp7_transition_xml.clone().unwrap_or_default(),
        } },
        "name": "Cross Dissolve",
        "transition_type": "SMPTE_Dissolve",
        "in_offset": rational_time(half, rate),
        "out_offset": rational_time(half, rate),
    })
}

fn time_range(start: f64, duration: f64, rate: f64) -> Value {
    json!({
        "OTIO_SCHEMA": "TimeRange.1",
        "duration": rational_time(duration, rate),
        "start_time": rational_time(start, rate),
    })
}

fn rational_time(seconds: f64, rate: f64) -> Value {
    json!({
        "OTIO_SCHEMA": "RationalTime.1",
        "rate": rate,
        "value": seconds * rate,
    })
}

fn timeline_duration(entry: &Entry, kind: MediaKind) -> f64 {
    let item = entry.item;
    if item.is_retimed(kind) {
        return item.selected_timeline_duration(kind);
    }
    if kind == MediaKind::Audio && item.corrected_audio_url.is_some() {
        return item.corrected_selected_duration();
    }
    item.selected_timeline_duration(kind)
}

fn timeline_start(entry: &Entry, kind: MediaKind) -> f64 {
    entry.item.timeline_start(kind)
}

fn media_start(item: &ExportItem, kind: MediaKind) -> f64 {
    if item.corrected_audio_url.is_some() {
        return 0.0;
    }
    if let Some(tc) = item.clip.source_timecode() {
        return tc.as_seconds();
    }
    if kind == MediaKind::Audio
        && item.clip.kind == MediaKind::Audio
        && item.clip.recorded_at_source
            == Some(crate::model::RecordingTimestampSource::EmbeddedMetadata)
        && let Some(date) = item.clip.recorded_at
    {
        // Seconds since UTC midnight.
        return (date % 86_400) as f64;
    }
    0.0
}

fn tracks<'a>(entries: Vec<Entry<'a>>, kind: MediaKind) -> Vec<Vec<Entry<'a>>> {
    super::lanes::allocate_entries(
        &entries,
        kind,
        super::lanes::Format::Otio,
        |entry| (entry.item, entry.source_channel),
        |entry| {
            let item = entry.item;
            let base_key = item
                .preferred_source_key(kind)
                .map(str::to_string)
                .unwrap_or_else(|| source_key_for_url(&item.clip.url));
            let source_key = if kind == MediaKind::Audio {
                format!("{base_key}#channel-{}", entry.source_channel)
            } else {
                base_key
            };
            TimelineTrackRequest::new(
                entry_id(item, kind, entry.source_channel),
                source_key,
                timeline_start(entry, kind),
                timeline_duration(entry, kind),
            )
        },
    )
}

fn entry_id(item: &ExportItem, kind: MediaKind, channel: usize) -> String {
    format!("{}#{}#{channel}", item.instance_id, otio_kind(kind))
}

/// OTIO's track kind for one side of the timeline.
fn otio_kind(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Audio => "Audio",
        MediaKind::Video => "Video",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{
        AudioSummary, Clip, ClipId, ClipPlacement, MappingPoint, MediaKind, MediaTime, SyncIsland,
        SyncProject, SyncResult, VideoSummary,
    };
    use std::path::PathBuf;

    pub fn fixture_result() -> SyncResult {
        let video = Clip {
            id: ClipId::new("cam"),
            url: PathBuf::from("/v/cam.mov"),
            kind: MediaKind::Video,
            duration: MediaTime::seconds(10.0),
            audio: vec![AudioSummary {
                sample_rate: 48000.0,
                channels: 1,
                bit_depth: Some(16),
                is_float: Some(false),
                source_timecode: None,
            }],
            video: Some(VideoSummary {
                width: 1920,
                height: 1080,
                frame_duration: Some(MediaTime::new(1, 25)),
                source_timecode: None,
                frame_rate_mode: None,
            }),
            recorded_at: None,
            recorded_at_source: None,
            source_identifier: None,
            media_span: None,
        };
        let rec = Clip {
            id: ClipId::new("rec"),
            url: PathBuf::from("/v/rec.wav"),
            kind: MediaKind::Audio,
            duration: MediaTime::seconds(12.0),
            audio: vec![AudioSummary {
                sample_rate: 48000.0,
                channels: 1,
                bit_depth: Some(24),
                is_float: Some(false),
                source_timecode: None,
            }],
            video: None,
            recorded_at: None,
            recorded_at_source: None,
            source_identifier: None,
            media_span: None,
        };
        let placement = |id: &str, off: f64, dur: f64| ClipPlacement {
            clip_id: ClipId::new(id),
            mapping: crate::model::TimeMap {
                points: vec![
                    MappingPoint {
                        source: MediaTime::seconds(0.0),
                        island: MediaTime::seconds(off),
                    },
                    MappingPoint {
                        source: MediaTime::seconds(dur),
                        island: MediaTime::seconds(off + dur),
                    },
                ],
            },
            confidence: 0.9,
        };
        SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            preserve_editing_tracks: Default::default(),
            project: SyncProject {
                clips: vec![video, rec],
                warnings: Vec::new(),
                imported_timeline: None,
            },
            islands: vec![SyncIsland {
                id: 0,
                placements: vec![placement("cam", 0.0, 10.0), placement("rec", 0.5, 12.0)],
            }],
            unmatched: Vec::new(),
            estimated_placements: Vec::new(),
            matches: Vec::new(),
            temporal_policy: crate::model::TemporalPolicy::default(),
        }
    }

    pub fn fixture_timeline() -> ExportTimeline {
        ExportTimeline::from_result(&fixture_result(), true).expect("timeline")
    }

    #[test]
    fn camera_audio_exports_every_channel() {
        let mut result = fixture_result();
        result.project.clips[0].audio[0].channels = 2;
        let timeline = ExportTimeline::from_result(&result, true).unwrap();
        let v: Value = serde_json::from_slice(&data(&timeline, false).unwrap()).unwrap();
        let channels: Vec<i64> = v["tracks"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["kind"] == "Audio")
            .flat_map(|t| t["children"].as_array().unwrap())
            .filter(|c| c["media_references"]["DEFAULT_MEDIA"]["name"] == "cam.mov")
            .map(|c| {
                c["metadata"]["Resolve_OTIO"]["Channels"][0]["Source Channel ID"]
                    .as_i64()
                    .unwrap()
            })
            .collect();
        assert_eq!(channels, vec![0, 1]);
    }

    /// A camera's dissolve stays on its picture, and a recorder's crossfade
    /// is written on its sound: each track takes the transitions of its own
    /// kind. Resolve's import script stopped at a dissolve on camera sound.
    #[test]
    fn each_track_takes_the_transitions_of_its_kind() {
        use crate::export::model::{ExportTransition, ExportTransitionKind};
        let mut timeline = fixture_timeline();
        let clips = &mut timeline.islands[0].clips;
        let transition = |kind, right: &str, start: f64| ExportTransition {
            kind,
            right_instance_id: right.to_string(),
            start,
            end: start + 0.32,
            alignment: "center".into(),
            fcp7_effect_xml: String::new(),
            fcp7_transition_xml: None,
            is_otio_portable: true,
        };
        for index in 0..2 {
            let mut second = clips[index].clone();
            second.instance_id = format!("{} second", clips[index].instance_id);
            second.start += 4.0;
            second.source_in += 4.0;
            second.source_out = second.source_out.max(second.source_in + 1.0);
            let kind = if second.clip.video.is_some() {
                ExportTransitionKind::CrossDissolve
            } else {
                ExportTransitionKind::AudioTransition
            };
            let after = transition(kind, &second.instance_id, second.start - 0.16);
            if second.clip.video.is_some() {
                clips[index].transition_after = Some(after);
            } else {
                clips[index].audio_transition_after = Some(after);
            }
            clips[index].timeline_duration = 4.0;
            clips[index].source_out = clips[index].source_in + 4.0;
            clips.push(second);
        }
        let v: Value = serde_json::from_slice(&data(&timeline, false).unwrap()).unwrap();
        let transitions = |kind: &str, file: &str| {
            v["tracks"]["children"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|t| t["kind"] == kind)
                .filter(|t| {
                    t["children"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|c| c["media_references"]["DEFAULT_MEDIA"]["name"] == file)
                })
                .flat_map(|t| t["children"].as_array().unwrap())
                .filter(|c| c["OTIO_SCHEMA"] == "Transition.1")
                .count()
        };
        assert_eq!(transitions("Video", "cam.mov"), 1);
        assert_eq!(transitions("Audio", "cam.mov"), 0);
        assert_eq!(transitions("Audio", "rec.wav"), 1);
    }

    /// Two parts of a recording that overlap at their seam share a track;
    /// the first ends where the second starts, so the second plays at its
    /// own place rather than after the whole first.
    #[test]
    fn a_seam_ends_the_first_part_where_the_second_starts() {
        let mut timeline = fixture_timeline();
        let clips = &mut timeline.islands[0].clips;
        let rec = clips
            .iter()
            .position(|item| item.clip.video.is_none())
            .unwrap();
        let mut second = clips[rec].clone();
        second.instance_id = "second part".into();
        second.start = clips[rec].start + clips[rec].timeline_duration - 0.2;
        clips.push(second.clone());
        let v: Value = serde_json::from_slice(&data(&timeline, false).unwrap()).unwrap();
        let track = v["tracks"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| {
                t["kind"] == "Audio"
                    && t["children"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|c| c["OTIO_SCHEMA"] == "Clip.2")
                        .count()
                        == 2
            })
            .expect("both parts on one track");
        let mut cursor = 0.0;
        let mut starts = Vec::new();
        for child in track["children"].as_array().unwrap() {
            if child["OTIO_SCHEMA"] == "Clip.2" {
                starts.push(cursor);
            }
            cursor += child["source_range"]["duration"]["value"].as_f64().unwrap() / 25.0;
        }
        assert!((starts[1] - second.start).abs() < 1e-6, "{starts:?}");
    }

    #[test]
    fn sequence_start_is_one_hour_ndf_at_fractional_rates() {
        for (duration, expected) in [
            (MediaTime::new(1001, 24_000), 86_400.0),
            (MediaTime::new(1001, 30_000), 108_000.0),
            (MediaTime::new(1001, 60_000), 216_000.0),
            (MediaTime::new(1, 25), 90_000.0),
        ] {
            let mut timeline = fixture_timeline();
            timeline.frame_duration = duration;
            let v: Value = serde_json::from_slice(&data(&timeline, false).unwrap()).unwrap();
            assert_eq!(v["global_start_time"]["value"], expected);
            let exported_rate = v["global_start_time"]["rate"].as_f64().unwrap();
            assert!((exported_rate - 1.0 / duration.as_seconds()).abs() < 1e-12);
        }
    }

    #[test]
    fn media_bins_are_listed_for_the_import_script() {
        let timeline = fixture_timeline();
        let v: Value = serde_json::from_slice(&data(&timeline, true).unwrap()).unwrap();
        assert_eq!(
            v["metadata"]["Align"],
            json!({
                "mediaBins": [
                    { "name": "v – Video", "media": ["/v/cam.mov"] },
                    { "name": "v – Audio", "media": ["/v/rec.wav"] },
                ],
                "generatedAudio": { "name": "Align generated audio", "media": [] },
            })
        );
        assert_eq!(
            v["metadata"]["Resolve_OTIO"]["Resolve OTIO Meta Version"],
            "1.0"
        );
        let plain: Value = serde_json::from_slice(&data(&timeline, false).unwrap()).unwrap();
        assert!(plain["metadata"].get("Align").is_none());
    }

    #[test]
    fn otio_structure() {
        let timeline = fixture_timeline();
        let bytes = data(&timeline, false).expect("otio");
        let v: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(v["OTIO_SCHEMA"], "Timeline.1");
        assert_eq!(v["global_start_time"]["value"], 3_600.0 * 25.0);
        let tracks = v["tracks"]["children"].as_array().unwrap();
        assert_eq!(tracks.len(), 3); // 1 video + 2 audio (cam embedded + rec)
        assert_eq!(tracks[0]["kind"], "Video");
        assert_eq!(tracks[0]["name"], "Video 1");
        let clips: Vec<&Value> = tracks[0]["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["OTIO_SCHEMA"] == "Clip.2")
            .collect();
        assert_eq!(clips.len(), 1);
        assert!(clips[0]["metadata"]["Resolve_OTIO"]["Link Group ID"].is_number());
        // Recorder audio references the wav with full range.
        let audio_clips: Vec<&Value> = tracks[2]["children"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["OTIO_SCHEMA"] == "Clip.2")
            .collect();
        assert_eq!(audio_clips.len(), 1);
        assert_eq!(
            audio_clips[0]["media_references"]["DEFAULT_MEDIA"]["name"],
            "rec.wav"
        );
    }
}
