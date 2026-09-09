//! OpenTimelineIO output for Resolve.
//!
//! Frame-quantized portable timeline (the sample-accurate path is the
//! sibling precision importer script): video tracks with camera A/V link
//! groups, per-channel mono audio tracks, gaps, portable Cross Dissolves
//! as `SMPTE_Dissolve`, and `LinearTimeWarp` for constant retimes
//! (negative scalar for reverse). Serialized pretty-printed with sorted
//! keys and unescaped slashes.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde_json::{Value, json};

use super::model::{ExportItem, ExportTimeline, ExportTransition};
use crate::allocator::{TimelineTrackRequest, allocate, source_key_for_url};

struct Entry<'a> {
    item: &'a ExportItem,
    source_channel: usize,
}

pub fn data(timeline: &ExportTimeline) -> Result<Vec<u8>, String> {
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
        "Video",
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
        "Audio",
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
            "Video",
            rate,
            &link_groups,
        ));
    }
    for (i, track) in audio_tracks.iter().enumerate() {
        children.push(track_json(
            track,
            &format!("Audio {}", i + 1),
            "Audio",
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
        "metadata": { "Resolve_OTIO": { "Resolve OTIO Meta Version": "1.0" } },
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

fn track_json(
    entries: &[Entry],
    name: &str,
    kind: &str,
    rate: f64,
    link_groups: &HashMap<&str, usize>,
) -> Value {
    let mut cursor = 0.0;
    let mut children = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let before = if index > 0 {
            transition_between(&entries[index - 1], entry)
        } else {
            None
        };
        let after = if index + 1 < entries.len() {
            transition_between(entry, &entries[index + 1])
        } else {
            None
        };
        let item_start = timeline_start(entry, kind);
        if before.is_none() && item_start > cursor + 0.000_001 {
            children.push(gap(item_start - cursor, rate));
        }
        let trim_start = before.map_or(0.0, |t| (t.end - t.start) / 2.0);
        let trim_end = after.map_or(0.0, |t| (t.end - t.start) / 2.0);
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
    let media = kind.to_lowercase();
    let mut resolve_meta = BTreeMap::from([("Locked".to_string(), json!(false))]);
    if kind == "Audio" {
        resolve_meta.insert("Audio Type".to_string(), json!("Mono"));
        resolve_meta.insert("SoloOn".to_string(), json!(false));
    }
    resolve_meta.insert(
        "Locked".to_string(),
        json!(
            entries
                .first()
                .is_some_and(|e| e.item.is_track_locked(&media))
        ),
    );
    json!({
        "OTIO_SCHEMA": "Track.1",
        "metadata": { "Resolve_OTIO": resolve_meta },
        "name": name,
        "source_range": null,
        "effects": [],
        "markers": [],
        "enabled": entries.first().is_none_or(|e| e.item.is_track_enabled(&media)),
        "kind": kind,
        "children": children,
    })
}

#[allow(clippy::too_many_arguments)]
fn clip_json(
    entry: &Entry,
    kind: &str,
    rate: f64,
    link_groups: &HashMap<&str, usize>,
    trim_start: f64,
    trim_end: f64,
) -> Value {
    let item = entry.item;
    let media = kind.to_lowercase();
    let precision_url = if kind == "Audio" {
        item.precision_audio_url(entry.source_channel).cloned()
    } else {
        None
    };
    let media_url: PathBuf = if kind == "Audio" {
        precision_url
            .clone()
            .or(item.corrected_audio_url.clone())
            .unwrap_or_else(|| item.clip.url.clone())
    } else {
        item.clip.url.clone()
    };
    let base = media_start(item, kind);
    let untrimmed_in = if precision_url.is_some() {
        0.0
    } else if kind == "Audio" && item.corrected_audio_url.is_some() {
        item.corrected_source_in()
    } else {
        item.selected_source_in(&media)
    };
    let duration = (timeline_duration(entry, kind) - trim_start - trim_end).max(0.0);
    let available = if precision_url.is_some() {
        item.precision_selected_duration()
    } else if kind == "Audio" && item.corrected_audio_url.is_some() {
        item.mapped_duration()
    } else {
        item.source_duration()
    };
    let playback_rate = item.playback_rate(&media);
    let plays_backward = item.plays_backward(&media);
    let source_in = untrimmed_in + trim_start * playback_rate;

    let mut resolve_meta = BTreeMap::new();
    if let Some(group) = link_groups.get(item.instance_id.as_str()) {
        resolve_meta.insert("Link Group ID".to_string(), json!(*group));
    }
    if kind == "Audio" {
        resolve_meta.insert(
            "Channels".to_string(),
            json!([{
                "Source Channel ID": if precision_url.is_none() {
                    entry.source_channel as i64 - 1
                } else {
                    0
                },
                "Source Track ID": 0,
            }]),
        );
    }
    let mut metadata = serde_json::Map::new();
    if !resolve_meta.is_empty() {
        metadata.insert("Resolve_OTIO".to_string(), json!(resolve_meta));
    }
    if let Some(remap) = item.fcp7_time_remap_xml(&media) {
        metadata.insert(
            "Align".to_string(),
            json!({
                "FCP7 Time Remap XML": remap,
                "Source In Seconds": item.selected_source_in(&media),
                "Source Out Seconds": item.selected_source_out(&media),
                "Plays Backward": plays_backward,
            }),
        );
    }
    let effects: Vec<Value> = if item.is_retimed(&media) {
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
        "enabled": item.is_enabled(&media),
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

/// Portable dissolve between consecutive entries (video `transitionAfter`
/// field).
fn transition_between<'a>(left: &'a Entry, right: &'a Entry) -> Option<&'a ExportTransition> {
    let t = left.item.transition_after.as_ref()?;
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

fn timeline_duration(entry: &Entry, kind: &str) -> f64 {
    let media = kind.to_lowercase();
    let item = entry.item;
    if item.is_retimed(&media) {
        return item.selected_timeline_duration(&media);
    }
    if kind == "Audio" && !item.precision_audio_urls.is_empty() {
        return item.precision_selected_duration();
    }
    if kind == "Audio" && item.corrected_audio_url.is_some() {
        return item.corrected_selected_duration();
    }
    item.selected_timeline_duration(&media)
}

fn timeline_start(entry: &Entry, kind: &str) -> f64 {
    entry.item.timeline_start(&kind.to_lowercase())
}

fn media_start(item: &ExportItem, kind: &str) -> f64 {
    if item.corrected_audio_url.is_some() {
        return 0.0;
    }
    if let Some(tc) = item.clip.source_timecode() {
        return tc.as_seconds();
    }
    if kind == "Audio"
        && item.clip.kind == crate::model::MediaKind::Audio
        && item.clip.recorded_at_source
            == Some(crate::model::RecordingTimestampSource::EmbeddedMetadata)
    {
        if let Some(date) = item.clip.recorded_at {
            // Seconds since UTC midnight.
            return (date % 86_400) as f64;
        }
    }
    0.0
}

fn tracks<'a>(entries: Vec<Entry<'a>>, kind: &str) -> Vec<Vec<Entry<'a>>> {
    // Keep a source's video dissolve on one track.
    let mut forced: Vec<Vec<Entry>> = Vec::new();
    let mut allocatable: Vec<Entry> = Vec::new();
    if kind == "Video" {
        let mut groups: HashMap<String, Vec<Entry>> = HashMap::new();
        for entry in entries {
            let key = entry
                .item
                .preferred_source_key("video")
                .map(str::to_string)
                .unwrap_or_else(|| source_key_for_url(&entry.item.clip.url));
            groups.entry(key).or_default().push(entry);
        }
        let mut forced_keys: Vec<String> = groups
            .iter()
            .filter(|(_, v)| {
                let ts: Vec<_> = v
                    .iter()
                    .filter_map(|e| e.item.transition_for("video"))
                    .collect();
                !ts.is_empty() && ts.iter().all(|t| t.is_otio_portable)
            })
            .map(|(k, _)| k.clone())
            .collect();
        forced_keys.sort();
        let mut keys: Vec<String> = groups.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let mut group = groups.remove(&key).unwrap_or_default();
            group.sort_by(|a, b| {
                timeline_start(a, kind)
                    .total_cmp(&timeline_start(b, kind))
                    .then_with(|| a.item.instance_id.cmp(&b.item.instance_id))
            });
            if forced_keys.contains(&key) {
                forced.push(group);
            } else {
                allocatable.extend(group);
            }
        }
    } else {
        allocatable = entries;
    }
    let media = kind.to_lowercase();
    let requests: Vec<TimelineTrackRequest> = allocatable
        .iter()
        .map(|entry| {
            let item = entry.item;
            let base_key = item
                .preferred_source_key(&media)
                .map(str::to_string)
                .unwrap_or_else(|| source_key_for_url(&item.clip.url));
            let source_key = if kind == "Audio" {
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
        })
        .collect();
    let assignments = allocate(&requests);
    let mut by_lane: HashMap<usize, Vec<Entry>> = HashMap::new();
    for entry in allocatable {
        let id = entry_id(entry.item, kind, entry.source_channel);
        by_lane
            .entry(assignments.get(&id).copied().unwrap_or(0))
            .or_default()
            .push(entry);
    }
    let mut lanes: Vec<usize> = by_lane.keys().copied().collect();
    lanes.sort_unstable();
    let mut allocated: Vec<Vec<Entry>> = lanes
        .into_iter()
        .map(|lane| {
            let mut track = by_lane.remove(&lane).unwrap_or_default();
            track.sort_by(|a, b| {
                timeline_start(a, kind)
                    .total_cmp(&timeline_start(b, kind))
                    .then_with(|| {
                        let fa = a.item.clip.url.file_name();
                        let fb = b.item.clip.url.file_name();
                        fa.cmp(&fb)
                    })
                    .then_with(|| a.source_channel.cmp(&b.source_channel))
            });
            track
        })
        .collect();
    forced.append(&mut allocated);
    forced
}

fn entry_id(item: &ExportItem, kind: &str, channel: usize) -> String {
    format!("{}#{kind}#{channel}", item.instance_id)
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
        let v: Value = serde_json::from_slice(&data(&timeline).unwrap()).unwrap();
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
            let v: Value = serde_json::from_slice(&data(&timeline).unwrap()).unwrap();
            assert_eq!(v["global_start_time"]["value"], expected);
            let exported_rate = v["global_start_time"]["rate"].as_f64().unwrap();
            assert!((exported_rate - 1.0 / duration.as_seconds()).abs() < 1e-12);
        }
    }

    #[test]
    fn otio_structure() {
        let timeline = fixture_timeline();
        let bytes = data(&timeline).expect("otio");
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
