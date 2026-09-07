//! Premiere Pro / Resolve-bootstrap FCP7 XML writer. Port of
//! `PremiereXMLWriter.swift`.
//!
//! One sequence per combined island (`– synced`, plus an optional `–
//! replaced` recorder-audio sequence for Premiere), original FCP7 effect
//! payloads passed through verbatim, retime graphs rebuilt only when no
//! preserved XML exists. Frame math mirrors `SequenceRate` exactly
//! (down/nearest/up roundings, subframe offsets, NTSC timebases).
//!
//! Sample-accurate audio placement: Premiere floors imported audio
//! `start`/`end` to integer frames and ignores `<subframeoffset>`, so
//! `export_prepared` renders placement-pad sidecars (leading silence) for
//! fractional audio starts and records them on the item. Padded items keep
//! the floored integer placement with no `<subframeoffset>`; the content
//! lands exactly. Unpadded items (plain `export`, retimed audio, the
//! `– replaced` sequence) keep the historical `<subframeoffset>` encoding.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::model::{
    ExportIsland, ExportItem, ExportTimeline, ExportTransition, TimelineExportFormat, xml_text,
};
use crate::allocator::{TimelineTrackRequest, allocate, source_key_for_url};
use crate::model::MediaKind;
use crate::piecewise::PiecewiseTimeMapping;

// ------------------------------------------------------------ rate

#[derive(Clone, Copy, Debug)]
struct SequenceRate {
    timebase: i64,
    ntsc: bool,
    fps: f64,
}

impl SequenceRate {
    fn new(frame_duration: crate::model::MediaTime) -> Self {
        let fps = super::model::sequence_fps(frame_duration);
        for (standard, base) in [(23.976, 24), (29.97, 30), (59.94, 60), (119.88, 120)] {
            if (fps - standard).abs() < 0.02 {
                return Self {
                    timebase: base,
                    ntsc: true,
                    fps: base as f64 / 1.001,
                };
            }
        }
        let timebase = fps.round().max(1.0) as i64;
        Self {
            timebase,
            ntsc: false,
            fps,
        }
    }

    fn frames(&self, seconds: f64, rounding: Rounding) -> i64 {
        let value = seconds * self.fps;
        match rounding {
            Rounding::Down => value.floor() as i64,
            Rounding::Nearest => value.round() as i64,
            Rounding::Up => value.ceil() as i64,
        }
    }

    fn xml(&self, indent: &str) -> String {
        format!(
            "{indent}<rate><timebase>{}</timebase><ntsc>{}</ntsc></rate>\n",
            self.timebase,
            if self.ntsc { "TRUE" } else { "FALSE" }
        )
    }
}

#[derive(Clone, Copy)]
enum Rounding {
    Down,
    Nearest,
    Up,
}

// ------------------------------------------------------------ entries

#[derive(Clone, Copy)]
struct TrackEntry<'a> {
    item: &'a ExportItem,
    source_channel: usize,
}

impl<'a> TrackEntry<'a> {
    fn key(&self, media_type: &str) -> (String, String, usize) {
        (
            self.item.instance_id.clone(),
            media_type.to_string(),
            self.source_channel,
        )
    }
}

struct LinkReference {
    clip_item_id: String,
    media_type: Option<&'static str>,
    track_index: Option<usize>,
    clip_index: Option<usize>,
    group_index: Option<usize>,
}

// ------------------------------------------------------------ write

pub fn write(
    timeline: &ExportTimeline,
    format: TimelineExportFormat,
    include_replaced_sequence: bool,
) -> String {
    assert!(!matches!(
        format,
        TimelineExportFormat::ResolveOTIO | TimelineExportFormat::ResolveScript
    ));
    let rate = SequenceRate::new(timeline.frame_duration);
    let mut xml =
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE xmeml>\n<xmeml version=\"5\">\n"
            .to_string();
    if let Some(island) = timeline.islands.first() {
        let mut defined_files = HashSet::new();
        let replaced = if format == TimelineExportFormat::PremiereXML && include_replaced_sequence {
            replaced_island(island)
        } else {
            None
        };
        let main_xml = sequence(
            island,
            if replaced.is_none() {
                timeline.name.clone()
            } else {
                format!("{} – synced", timeline.name)
            },
            "sequence-1",
            "",
            true,
            rate,
            format,
            &mut defined_files,
        );
        if format == TimelineExportFormat::PremiereXML {
            xml += "  <project>\n    <name>Align</name>\n    <children>\n";
            xml += &main_xml;
            if let Some(rep) = &replaced {
                xml += &sequence(
                    rep,
                    format!("{} – replaced", timeline.name),
                    "sequence-2",
                    "replaced-",
                    false,
                    rate,
                    format,
                    &mut defined_files,
                );
            }
            xml += "    </children>\n  </project>\n";
        } else {
            xml += &main_xml;
        }
    }
    xml += "</xmeml>\n";
    xml
}

/// Combine self-contained Premiere project documents into one project while
/// keeping every sequence and resource identifier unique.
pub fn combine_project_documents(documents: &[String]) -> Result<String, String> {
    if documents.is_empty() {
        return Err("no Premiere documents to combine".into());
    }
    let mut xml =
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE xmeml>\n<xmeml version=\"5\">\n  <project>\n    <name>Align</name>\n    <children>\n"
            .to_string();
    for (index, document) in documents.iter().enumerate() {
        let start = document
            .find("    <children>\n")
            .map(|offset| offset + "    <children>\n".len())
            .ok_or("Premiere document has no project children")?;
        let end = document[start..]
            .find("    </children>")
            .map(|offset| start + offset)
            .ok_or("Premiere document has unterminated project children")?;
        xml += &prefix_document_ids(&document[start..end], &format!("s{}-", index + 1));
    }
    xml += "    </children>\n  </project>\n</xmeml>\n";
    Ok(xml)
}

fn prefix_document_ids(fragment: &str, prefix: &str) -> String {
    let mut out = fragment.replace(" id=\"", &format!(" id=\"{prefix}"));
    let open = "<linkclipref>";
    let close = "</linkclipref>";
    let mut cursor = 0;
    while let Some(start) = out[cursor..]
        .find(open)
        .map(|offset| cursor + offset + open.len())
    {
        out.insert_str(start, prefix);
        cursor = out[start + prefix.len()..]
            .find(close)
            .map_or(out.len(), |offset| {
                start + prefix.len() + offset + close.len()
            });
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn sequence(
    island: &ExportIsland,
    name: String,
    id: &str,
    clip_id_prefix: &str,
    include_embedded_audio: bool,
    rate: SequenceRate,
    format: TimelineExportFormat,
    defined_files: &mut HashSet<String>,
) -> String {
    let duration = rate.frames(island.duration, Rounding::Up);
    let mut xml = format!("      <sequence id=\"{id}\">\n");
    xml += &format!("        <name>{}</name>\n", xml_text::escape(&name));
    xml += &format!(
        "        <duration>{duration}</duration>\n{}",
        rate.xml("        ")
    );
    xml += &format!(
        "        <timecode>\n          <string>01:00:00:00</string>\n          <frame>{}</frame>\n          <displayformat>NDF</displayformat>\n{}        </timecode>\n",
        rate.timebase * 3_600,
        rate.xml("          ")
    );
    let width = island
        .clips
        .iter()
        .filter_map(|i| i.clip.video.as_ref().map(|v| v.width))
        .next()
        .unwrap_or(1920);
    let height = island
        .clips
        .iter()
        .filter_map(|i| i.clip.video.as_ref().map(|v| v.height))
        .next()
        .unwrap_or(1080);
    xml += &format!(
        "        <media>\n          <video>\n            <format><samplecharacteristics>\n{}              <width>{width}</width>\n              <height>{height}</height>\n              <pixelaspectratio>square</pixelaspectratio>\n              <fielddominance>none</fielddominance>\n            </samplecharacteristics></format>\n",
        rate.xml("              ")
    );

    let video_entries: Vec<TrackEntry> = island
        .clips
        .iter()
        .filter(|i| i.clip.video.is_some())
        .map(|item| TrackEntry {
            item,
            source_channel: 0,
        })
        .collect();
    let audio_entries: Vec<TrackEntry> = island
        .clips
        .iter()
        .filter(|i| {
            !i.clip.audio.is_empty() && (include_embedded_audio || i.clip.kind == MediaKind::Audio)
        })
        .flat_map(|item| {
            item.audio_source_channels().map(|ch| TrackEntry {
                item,
                source_channel: ch,
            })
        })
        .collect();
    let video_tracks = tracks_for(&video_entries, "video", format, rate);
    let audio_tracks = tracks_for(&audio_entries, "audio", format, rate);
    let video_ids = identifiers(&video_tracks, "video", 1, clip_id_prefix);
    let audio_ids = identifiers(&audio_tracks, "audio", video_ids.len() + 1, clip_id_prefix);
    let video_positions = positions(&video_tracks, "video");
    let audio_positions = positions(&audio_tracks, "audio");

    for track in &video_tracks {
        xml += "            <track>\n";
        for (index, entry) in track.iter().enumerate() {
            let before = if index > 0 {
                transition_between(&track[index - 1], entry, "video")
            } else {
                None
            };
            let after = if index + 1 < track.len() {
                transition_between(entry, &track[index + 1], "video")
            } else {
                None
            };
            xml += &clip_item(
                entry,
                "video",
                format,
                &video_ids[&entry.key("video")],
                &links(
                    entry,
                    "video",
                    format,
                    &video_ids,
                    &audio_ids,
                    &video_positions,
                    &audio_positions,
                ),
                rate,
                defined_files,
                before.map(|_| -1),
                after.map(|_| -1),
            );
            if let Some(t) = after {
                xml += &transition_item(t, rate);
            }
        }
        let enabled = track
            .first()
            .is_none_or(|e| e.item.is_track_enabled("video"));
        let locked = track
            .first()
            .is_some_and(|e| e.item.is_track_locked("video"));
        xml += &format!(
            "              <enabled>{}</enabled><locked>{}</locked>\n            </track>\n",
            tf(enabled),
            tf(locked)
        );
    }
    let use_pads = format == TimelineExportFormat::PremiereXML;
    let audio_depth = if island
        .clips
        .iter()
        .any(|i| i.corrected_audio_url.is_some() || (use_pads && i.placement_pad_url.is_some()))
    {
        32
    } else {
        island
            .clips
            .iter()
            .filter_map(|i| i.clip.audio.first()?.bit_depth)
            .next()
            .unwrap_or(24)
    };
    xml += &format!(
        "          </video>\n          <audio>\n            <format><samplecharacteristics><depth>{audio_depth}</depth><samplerate>48000</samplerate></samplecharacteristics></format>\n"
    );
    for track in &audio_tracks {
        xml += "            <track>\n";
        for (index, entry) in track.iter().enumerate() {
            let before = if index > 0 {
                transition_between(&track[index - 1], entry, "audio")
            } else {
                None
            };
            let after = if index + 1 < track.len() {
                transition_between(entry, &track[index + 1], "audio")
            } else {
                None
            };
            xml += &clip_item(
                entry,
                "audio",
                format,
                &audio_ids[&entry.key("audio")],
                &links(
                    entry,
                    "audio",
                    format,
                    &video_ids,
                    &audio_ids,
                    &video_positions,
                    &audio_positions,
                ),
                rate,
                defined_files,
                before.map(|_| -1),
                after.map(|_| -1),
            );
            if let Some(t) = after {
                xml += &transition_item(t, rate);
            }
        }
        let enabled = track
            .first()
            .is_none_or(|e| e.item.is_track_enabled("audio"));
        let locked = track
            .first()
            .is_some_and(|e| e.item.is_track_locked("audio"));
        xml += &format!(
            "              <enabled>{}</enabled><locked>{}</locked>\n            </track>\n",
            tf(enabled),
            tf(locked)
        );
    }
    xml += "          </audio>\n        </media>\n      </sequence>\n";
    xml
}

fn base_resource_key(item: &ExportItem) -> String {
    if item.corrected_audio_url.is_none() {
        item.clip.id.0.clone()
    } else {
        format!("{}-corrected-audio", item.clip.id.0)
    }
}

fn tf(value: bool) -> &'static str {
    if value { "TRUE" } else { "FALSE" }
}

/// Recorder-audio replacement island (Premiere second sequence): camera
/// audio removed, top overlapping recorder clip trimmed per video clip.
fn replaced_island(island: &ExportIsland) -> Option<ExportIsland> {
    let videos: Vec<&ExportItem> = island
        .clips
        .iter()
        .filter(|i| i.clip.video.is_some())
        .collect();
    let recorders: Vec<&ExportItem> = island
        .clips
        .iter()
        .filter(|i| i.clip.kind == MediaKind::Audio)
        .collect();
    if videos.is_empty() || recorders.is_empty() {
        return None;
    }
    let assignments = allocate(
        &recorders
            .iter()
            .map(|item| {
                TimelineTrackRequest::new(
                    item.instance_id.clone(),
                    item.preferred_audio_source_key
                        .clone()
                        .or(item.preferred_source_key.clone())
                        .unwrap_or_else(|| source_key_for_url(&item.clip.url)),
                    item.start,
                    audio_duration(item),
                )
            })
            .collect::<Vec<_>>(),
    );
    let mut recorder_tracks: HashMap<usize, Vec<&ExportItem>> = HashMap::new();
    for item in recorders {
        recorder_tracks
            .entry(assignments.get(&item.instance_id).copied().unwrap_or(0))
            .or_default()
            .push(item);
    }
    let mut lanes: Vec<usize> = recorder_tracks.keys().copied().collect();
    lanes.sort_unstable();

    let mut replacements = Vec::new();
    for video in &videos {
        let video_end = video.start + video.timeline_duration;
        let mut recorder: Option<&ExportItem> = None;
        for lane in &lanes {
            let mut best = 0.0;
            for item in &recorder_tracks[lane] {
                let item_end = item.start + audio_duration(item);
                let overlap = 0.0f64.max(video_end.min(item_end) - video.start.max(item.start));
                if overlap > best {
                    recorder = Some(item);
                    best = overlap;
                }
            }
            if recorder.is_some() {
                break;
            }
        }
        let Some(recorder) = recorder else {
            continue;
        };
        let overlap_start = video.start.max(recorder.start);
        let overlap_end = video_end.min(recorder.start + audio_duration(recorder));
        if overlap_end <= overlap_start {
            continue;
        }
        let inverse = PiecewiseTimeMapping::new(
            recorder
                .mapping_points
                .iter()
                .map(|p| {
                    crate::piecewise::MapPoint::new(p.source.as_seconds(), p.island.as_seconds())
                })
                .collect(),
        )
        .inverted();
        replacements.push(ExportItem {
            instance_id: video.instance_id.clone(),
            display_name: recorder.display_name.clone(),
            clip: recorder.clip.clone(),
            start: overlap_start,
            source_in: recorder.source_in.max(inverse.value_at(overlap_start)),
            source_out: recorder.source_out.min(inverse.value_at(overlap_end)),
            timeline_duration: overlap_end - overlap_start,
            playback_rate: 1.0,
            plays_backward: false,
            fcp7_time_remap_xml: None,
            fcp7_retime_in: None,
            fcp7_retime_out: None,
            fcp7_retime_duration: None,
            fcp7_filter_xmls: recorder.fcp7_filter_xmls.clone(),
            fcp7_labels_xml: recorder.fcp7_labels_xml.clone(),
            audio_source_channel: recorder.audio_source_channel,
            fcpxml_audio_role: recorder.fcpxml_audio_role.clone(),
            preferred_source_key: recorder.preferred_source_key.clone(),
            preferred_audio_source_key: recorder.preferred_audio_source_key.clone(),
            mapping_rate: recorder.mapping_rate,
            mapping_points: recorder.mapping_points.clone(),
            confidence: video.confidence.min(recorder.confidence),
            corrected_audio_url: recorder.corrected_audio_url.clone(),
            precision_audio_urls: Vec::new(),
            precision_tail_samples: 0,
            // Deliberately not copied: replacement items start at
            // video-overlap positions whose fractional remainder differs
            // from the recorder item the pad was rendered for.
            placement_pad_url: None,
            placement_pad_seconds: 0.0,
            enabled: recorder.enabled,
            track_enabled: recorder.track_enabled,
            track_locked: recorder.track_locked,
            audio_enabled: None,
            audio_track_enabled: None,
            audio_track_locked: None,
            transition_after: None,
            audio_transition_after: None,
            linked_audio_edit: None,
        });
    }
    if replacements.is_empty() {
        return None;
    }
    let mut clips: Vec<ExportItem> = videos.into_iter().cloned().collect();
    clips.extend(replacements);
    Some(ExportIsland {
        id: island.id,
        clips,
        duration: island.duration,
    })
}

fn audio_duration(item: &ExportItem) -> f64 {
    if item.corrected_audio_url.is_none() {
        item.timeline_duration
    } else {
        item.corrected_selected_duration()
    }
}

fn identifiers(
    tracks: &[Vec<TrackEntry>],
    media_type: &str,
    first: usize,
    prefix: &str,
) -> HashMap<(String, String, usize), String> {
    let mut out = HashMap::new();
    for (number, entry) in (first..).zip(tracks.iter().flat_map(|t| t.iter())) {
        out.insert(entry.key(media_type), format!("clipitem-{prefix}{number}"));
    }
    out
}

fn positions(
    tracks: &[Vec<TrackEntry>],
    media_type: &str,
) -> HashMap<(String, String, usize), (usize, usize)> {
    let mut out = HashMap::new();
    for (track_offset, track) in tracks.iter().enumerate() {
        for (clip_offset, entry) in track.iter().enumerate() {
            out.insert(entry.key(media_type), (track_offset + 1, clip_offset + 1));
        }
    }
    out
}

fn links(
    entry: &TrackEntry,
    media_type: &str,
    format: TimelineExportFormat,
    video_ids: &HashMap<(String, String, usize), String>,
    audio_ids: &HashMap<(String, String, usize), String>,
    video_positions: &HashMap<(String, String, usize), (usize, usize)>,
    audio_positions: &HashMap<(String, String, usize), (usize, usize)>,
) -> Vec<LinkReference> {
    let video_key = (entry.item.instance_id.clone(), "video".to_string(), 0);
    let (Some(video_id), Some(video_position)) =
        (video_ids.get(&video_key), video_positions.get(&video_key))
    else {
        return Vec::new();
    };
    let mut linked_audio: Vec<(String, (usize, usize))> = audio_ids
        .keys()
        .filter(|k| k.0 == entry.item.instance_id && k.1 == "audio")
        .filter_map(|k| Some((audio_ids.get(k)?.clone(), *audio_positions.get(k)?)))
        .collect();
    linked_audio.sort_by(|a, b| {
        let ka = audio_ids
            .iter()
            .find(|(_, v)| **v == a.0)
            .map(|(k, _)| k.2)
            .unwrap_or(0);
        let kb = audio_ids
            .iter()
            .find(|(_, v)| **v == b.0)
            .map(|(k, _)| k.2)
            .unwrap_or(0);
        ka.cmp(&kb)
    });
    let Some(first_audio) = linked_audio.first() else {
        return Vec::new();
    };
    if format == TimelineExportFormat::PremiereXML {
        let mut out = vec![LinkReference {
            clip_item_id: video_id.clone(),
            media_type: Some("video"),
            track_index: Some(video_position.0),
            clip_index: Some(video_position.1),
            group_index: None,
        }];
        out.extend(linked_audio.iter().map(|audio| LinkReference {
            clip_item_id: audio.0.clone(),
            media_type: Some("audio"),
            track_index: Some(audio.1.0),
            clip_index: Some(audio.1.1),
            group_index: Some(1),
        }));
        return out;
    }
    vec![
        LinkReference {
            clip_item_id: video_id.clone(),
            media_type: if media_type == "audio" {
                Some("video")
            } else {
                None
            },
            track_index: None,
            clip_index: None,
            group_index: None,
        },
        LinkReference {
            clip_item_id: first_audio.0.clone(),
            media_type: None,
            track_index: None,
            clip_index: None,
            group_index: None,
        },
    ]
}

fn tracks_for<'a>(
    entries: &[TrackEntry<'a>],
    media_type: &str,
    format: TimelineExportFormat,
    rate: SequenceRate,
) -> Vec<Vec<TrackEntry<'a>>> {
    let source_key = |entry: &TrackEntry<'a>| -> String {
        let item = entry.item;
        let base = item
            .preferred_source_key(media_type)
            .map(str::to_string)
            .unwrap_or_else(|| source_key_for_url(&item.clip.url));
        if media_type == "audio" {
            format!("{base}#channel-{}", entry.source_channel)
        } else {
            base
        }
    };
    let mut grouped: HashMap<String, Vec<TrackEntry<'a>>> = HashMap::new();
    for entry in entries {
        grouped.entry(source_key(entry)).or_default().push(*entry);
    }
    let forced_keys: Vec<String> = grouped
        .iter()
        .filter(|(_, v)| {
            v.iter().any(|e| {
                if media_type == "audio" {
                    e.item.transition_for("audio").is_some()
                } else {
                    e.item.transition_for("video").is_some()
                }
            })
        })
        .map(|(k, _)| k.clone())
        .collect();
    let mut forced: Vec<Vec<TrackEntry<'a>>> = Vec::new();
    let mut allocatable: Vec<TrackEntry<'a>> = Vec::new();
    let mut keys: Vec<String> = grouped.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let mut group = grouped.remove(&key).unwrap_or_default();
        group.sort_by(|a, b| {
            a.item
                .timeline_start(media_type)
                .total_cmp(&b.item.timeline_start(media_type))
                .then_with(|| a.item.instance_id.cmp(&b.item.instance_id))
        });
        if forced_keys.contains(&key) {
            forced.push(group);
        } else {
            allocatable.extend(group);
        }
    }
    let requests: Vec<TimelineTrackRequest> = allocatable
        .iter()
        .map(|entry| {
            let item = entry.item;
            let duration = if media_type == "audio"
                && item.corrected_audio_url.is_some()
                && !item.is_retimed(media_type)
            {
                item.corrected_selected_duration()
            } else {
                item.selected_timeline_duration(media_type)
            };
            let (start, duration) = if format == TimelineExportFormat::PremiereXML
                && item.has_placement_pad(media_type)
            {
                let start =
                    rate.frames(item.timeline_start(media_type), Rounding::Down) as f64 / rate.fps;
                let duration = rate.frames(duration + item.placement_pad_seconds, Rounding::Up)
                    as f64
                    / rate.fps;
                (start, duration)
            } else {
                (item.timeline_start(media_type), duration)
            };
            TimelineTrackRequest::new(
                format!("{}#{media_type}#{}", item.instance_id, entry.source_channel),
                source_key(entry),
                start,
                duration,
            )
        })
        .collect();
    let assignments = allocate(&requests);
    let mut by_lane: HashMap<usize, Vec<TrackEntry<'a>>> = HashMap::new();
    for entry in allocatable {
        let id = format!(
            "{}#{media_type}#{}",
            entry.item.instance_id, entry.source_channel
        );
        by_lane
            .entry(assignments.get(&id).copied().unwrap_or(0))
            .or_default()
            .push(entry);
    }
    let mut lanes: Vec<usize> = by_lane.keys().copied().collect();
    lanes.sort_unstable();
    let mut allocated: Vec<Vec<TrackEntry<'a>>> = lanes
        .into_iter()
        .map(|lane| {
            let mut track = by_lane.remove(&lane).unwrap_or_default();
            track.sort_by(|a, b| {
                a.item
                    .timeline_start(media_type)
                    .total_cmp(&b.item.timeline_start(media_type))
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

#[allow(clippy::too_many_arguments)]
fn clip_item(
    entry: &TrackEntry,
    media_type: &str,
    format: TimelineExportFormat,
    id: &str,
    links: &[LinkReference],
    rate: SequenceRate,
    defined_files: &mut HashSet<String>,
    start_override: Option<i64>,
    end_override: Option<i64>,
) -> String {
    let item = entry.item;
    let stable_start = (item.timeline_start(media_type) * 100_000.0).round() / 100_000.0;
    let mut start = rate.frames(
        stable_start,
        if media_type == "video" {
            Rounding::Nearest
        } else {
            Rounding::Down
        },
    );
    // Collapse sub-sample dust (float noise, µs solver jitter) to the
    // adjacent integer frame so near-exact starts encode exactly instead of
    // as floor + a full-frame subframeoffset Premiere would drop.
    let mut start_frac = 0.0;
    if media_type == "audio" {
        let sample_rate = item.clip.audio.first().map_or(48_000.0, |a| a.sample_rate);
        let raw_frac = stable_start * rate.fps - start as f64;
        let (adjustment, effective) =
            super::model::snap_sub_sample_frac(raw_frac, sample_rate / rate.fps);
        start += adjustment;
        start_frac = effective;
    }
    let source_rate = item
        .clip
        .video
        .as_ref()
        .and_then(|v| v.frame_duration)
        .map(SequenceRate::new)
        .unwrap_or(rate);
    let corrected = media_type == "audio" && item.corrected_audio_url.is_some();
    // Pads are a PremiereXML-only essence swap: the Resolve bootstrap keeps
    // referencing drift/original files with the historical subframeoffset
    // encoding, byte-identical to before.
    let padded = item.has_placement_pad(media_type) && format == TimelineExportFormat::PremiereXML;
    let pad_seconds = if padded {
        item.placement_pad_seconds
    } else {
        0.0
    };
    let file_seconds = if corrected {
        item.mapped_duration()
    } else {
        item.source_duration()
    };
    let in_seconds = if corrected {
        item.corrected_source_in()
    } else {
        item.selected_source_in(media_type)
    };
    let out_seconds = if corrected {
        item.corrected_source_out()
    } else {
        item.selected_source_out(media_type)
    };
    let source_rate = if padded { rate } else { source_rate };
    let file_duration = if padded {
        source_rate.frames(out_seconds - in_seconds + pad_seconds, Rounding::Up)
    } else {
        source_rate.frames(file_seconds, Rounding::Nearest)
    };
    let playback_rate = item.playback_rate(media_type);
    let plays_backward = item.plays_backward(media_type);
    let graph_slope = playback_rate * source_rate.fps / rate.fps;
    let retimed = item.is_retimed(media_type);
    let item_rate = if retimed { rate } else { source_rate };
    let geometry = item.fcp7_retime_geometry(media_type);
    let (computed_in, computed_out) = if retimed {
        let media_in = in_seconds * source_rate.fps;
        let media_out = out_seconds * source_rate.fps;
        let (g_in, g_out) = if plays_backward {
            (
                ((file_duration as f64 - media_out) / graph_slope).round() as i64,
                ((file_duration as f64 - media_in) / graph_slope).round() as i64,
            )
        } else {
            (
                (media_in / graph_slope).round() as i64,
                (media_out / graph_slope).round() as i64,
            )
        };
        (g_in, g_out)
    } else {
        (
            source_rate.frames(in_seconds, Rounding::Nearest),
            source_rate.frames(out_seconds, Rounding::Nearest),
        )
    };
    let source_in = if padded {
        0
    } else {
        geometry.map_or(computed_in, |g| g.0)
    };
    let source_out = if padded {
        file_duration
    } else {
        geometry.map_or(computed_out, |g| g.1)
    };
    let clip_duration = geometry.map_or_else(
        || {
            if retimed {
                source_out.max((file_duration as f64 / graph_slope).round() as i64)
            } else {
                file_duration
            }
        },
        |g| g.2,
    );
    let selected_tl = if corrected && !retimed {
        item.corrected_selected_duration()
    } else {
        item.selected_timeline_duration(media_type)
    };
    let timeline_duration = if padded {
        file_duration
    } else {
        rate.frames(selected_tl, Rounding::Nearest)
    };
    let end = start + timeline_duration;
    let media_url: PathBuf = if media_type == "audio" {
        let pad_url = if padded {
            item.placement_pad_url.clone()
        } else {
            None
        };
        pad_url
            .or(item.corrected_audio_url.clone())
            .unwrap_or_else(|| item.clip.url.clone())
    } else {
        item.clip.url.clone()
    };
    let name = xml_text::escape(&item.display_name.clone().unwrap_or_else(|| {
        media_url
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    }));
    let resource_key = if padded {
        // Padded sidecars differ per prepend length: items sharing a source
        // at different fractional offsets must not share a file block.
        let sample_rate = item.clip.audio.first().map_or(48_000.0, |a| a.sample_rate);
        let pad_samples = (pad_seconds * sample_rate).round() as u64;
        format!(
            "{}-placement-pad-{pad_samples}-{}-{}",
            base_resource_key(item),
            in_seconds.to_bits(),
            out_seconds.to_bits()
        )
    } else {
        base_resource_key(item)
    };
    let file_id = format!("file-{}", xml_text::identifier(&resource_key));
    let mut xml = format!("              <clipitem id=\"{id}\">\n");
    xml += &format!(
        "                <name>{name}</name>\n                <enabled>{}</enabled>\n                <duration>{clip_duration}</duration>\n",
        tf(item.is_enabled(media_type))
    );
    xml += &item_rate.xml("                ");
    xml += &format!(
        "                <start>{}</start>\n                <end>{}</end>\n                <in>{source_in}</in>\n                <out>{source_out}</out>\n",
        start_override.unwrap_or(start),
        end_override.unwrap_or(end)
    );
    if media_type == "audio" && !padded {
        let subframes = super::model::subframe_offset_80(start_frac);
        if subframes != 0 {
            xml += &format!("                <subframeoffset>{subframes}</subframeoffset>\n");
        }
    }
    for link in links {
        xml += &format!(
            "                <link><linkclipref>{}</linkclipref>",
            link.clip_item_id
        );
        if let Some(mt) = link.media_type {
            xml += &format!("<mediatype>{mt}</mediatype>");
        }
        if let Some(t) = link.track_index {
            xml += &format!("<trackindex>{t}</trackindex>");
        }
        if let Some(c) = link.clip_index {
            xml += &format!("<clipindex>{c}</clipindex>");
        }
        if let Some(g) = link.group_index {
            xml += &format!("<groupindex>{g}</groupindex>");
        }
        xml += "</link>\n";
    }
    if defined_files.insert(resource_key) {
        xml += &file_block(
            item,
            &media_url,
            media_type,
            &file_id,
            file_duration,
            source_rate,
            padded,
        );
    } else {
        xml += &format!("                <file id=\"{file_id}\"/>\n");
    }
    if media_type == "audio" {
        xml += &format!(
            "                <sourcetrack><mediatype>audio</mediatype><trackindex>{}</trackindex></sourcetrack>\n",
            entry.source_channel
        );
    }
    if retimed {
        xml += &time_remap(
            media_type,
            graph_slope,
            source_in,
            source_out,
            clip_duration,
            in_seconds * source_rate.fps,
            out_seconds * source_rate.fps,
            file_duration as f64,
            plays_backward,
            item.fcp7_time_remap_xml(media_type).map(str::to_string),
        );
    }
    if let Some(labels) = item.fcp7_labels_xml(media_type) {
        xml += &format!("                {labels}\n");
    }
    for filter in item.fcp7_filter_xmls(media_type) {
        xml += &format!("                {filter}\n");
    }
    xml += "              </clipitem>\n";
    xml
}

fn transition_between<'a>(
    left: &TrackEntry<'a>,
    right: &TrackEntry<'a>,
    media_type: &str,
) -> Option<&'a ExportTransition> {
    let t = left.item.transition_for(media_type)?;
    if t.right_instance_id == right.item.instance_id {
        Some(t)
    } else {
        None
    }
}

fn transition_item(t: &ExportTransition, rate: SequenceRate) -> String {
    let start = rate.frames(t.start, Rounding::Nearest);
    let end = rate.frames(t.end, Rounding::Nearest);
    if let Some(source) = &t.fcp7_transition_xml {
        // Mirror Swift's DOM rewrite (start/end coordinates only): splice
        // the first <start>/<end> pair, fall back to synthesis.
        if let Some(rewritten) =
            splice_pair(source, "start", start).and_then(|xml| splice_pair(&xml, "end", end))
        {
            return format!("              {rewritten}\n");
        }
    }
    let mut xml = "              <transitionitem>\n".to_string();
    xml += &format!(
        "                <start>{start}</start><end>{end}</end><alignment>{}</alignment>\n",
        xml_text::escape(&t.alignment)
    );
    xml += &rate.xml("                ");
    xml += &format!("                {}\n", t.fcp7_effect_xml);
    xml += "              </transitionitem>\n";
    xml
}

/// Replace the first `<tag>…</tag>` text with `value`.
fn splice_pair(xml: &str, tag: &str, value: i64) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = xml.find(&open)? + open.len();
    let e = xml[s..].find(&close)? + s;
    Some(format!("{}{value}{}", &xml[..s], &xml[e..]))
}

#[allow(clippy::too_many_arguments)]
fn time_remap(
    media_type: &str,
    graph_slope: f64,
    graph_in: i64,
    graph_out: i64,
    graph_duration: i64,
    media_in: f64,
    media_out: f64,
    media_duration: f64,
    plays_backward: bool,
    preserved_xml: Option<String>,
) -> String {
    if let Some(xml) = preserved_xml {
        return format!("                {xml}\n");
    }
    let mut keyframes: Vec<(i64, f64, &str)> = Vec::new();
    if graph_in > 0 {
        keyframes.push((
            0,
            if plays_backward {
                media_duration + 1.0
            } else {
                0.0
            },
            "speedkfstart",
        ));
    }
    keyframes.push((
        graph_in,
        if plays_backward {
            media_out + 1.0
        } else {
            media_in
        },
        "speedkfin",
    ));
    keyframes.push((
        graph_out,
        if plays_backward {
            media_in + 1.0
        } else {
            media_out
        },
        "speedkfout",
    ));
    if graph_out < graph_duration {
        keyframes.push((
            graph_duration,
            if plays_backward { 1.0 } else { media_duration },
            "speedkfend",
        ));
    }
    let mut xml = "                <filter><effect>\n".to_string();
    xml += "                  <name>Time Remap</name><effectid>timeremap</effectid><effectcategory>motion</effectcategory><effecttype>motion</effecttype>";
    xml += &format!("<mediatype>{media_type}</mediatype>\n");
    xml += "                  <parameter><parameterid>variablespeed</parameterid><name>variablespeed</name><valuemin>0</valuemin><valuemax>1</valuemax><value>0</value></parameter>\n";
    xml += &format!(
        "                  <parameter><parameterid>speed</parameterid><name>speed</name><valuemin>-100000</valuemin><valuemax>100000</valuemax><value>{}</value></parameter>\n",
        xml_number(graph_slope * 100.0)
    );
    xml += &format!(
        "                  <parameter><parameterid>reverse</parameterid><name>reverse</name><value>{}</value></parameter>\n",
        if plays_backward { "TRUE" } else { "FALSE" }
    );
    xml += "                  <parameter><parameterid>frameblending</parameterid><name>frameblending</name><value>FALSE</value></parameter>\n";
    xml += &format!(
        "                  <parameter><parameterid>graphdict</parameterid><name>graphdict</name><valuemin>0</valuemin><valuemax>{}</valuemax><value>0</value>\n",
        xml_number(media_duration)
    );
    for (when, value, flag) in keyframes {
        xml += &format!(
            "                    <keyframe><when>{when}</when><value>{}</value><speedvirtualkf>TRUE</speedvirtualkf><{flag}>TRUE</{flag}></keyframe>\n",
            xml_number(value)
        );
    }
    xml += "                    <interpolation><name>FCPCurve</name></interpolation>\n                  </parameter>\n                </effect></filter>\n";
    xml
}

/// Percent-encode a filesystem path for file:// URLs (unreserved +
/// '/' pass through, mirroring URL.absoluteString for local files).
fn percent_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn xml_number(value: f64) -> String {
    let rounded = value.round();
    if (value - rounded).abs() < 0.000_001 {
        return format!("{}", rounded as i64);
    }
    let mut s = format!("{value:.9}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

fn file_block(
    item: &ExportItem,
    media_url: &Path,
    media_type: &str,
    id: &str,
    duration: i64,
    rate: SequenceRate,
    padded: bool,
) -> String {
    let clip = &item.clip;
    let file_name = media_url.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // file:// URL with percent-encoding (mirrors URL.absoluteString).
    let absolute = format!("file://{}", percent_encode(&media_url.to_string_lossy()));
    let mut xml = format!(
        "                <file id=\"{id}\">\n                  <name>{}</name>\n                  <pathurl>{}</pathurl>\n                  <duration>{duration}</duration>\n",
        xml_text::escape(file_name),
        xml_text::escape(&absolute),
    );
    xml += &rate.xml("                  ");
    let timecode = if media_type == "video" {
        clip.video.as_ref().and_then(|v| v.source_timecode.clone())
    } else {
        clip.audio.first().and_then(|a| a.source_timecode.clone())
    };
    if let Some(tc) = timecode {
        let source_rate = SequenceRate::new(tc.frame_duration);
        xml += &format!(
            "                  <timecode><string>{}</string><displayformat>{}</displayformat>\n",
            tc.text,
            if tc.drop_frame { "DF" } else { "NDF" }
        );
        xml += &source_rate.xml("                    ");
        xml += "                  </timecode>\n";
    } else if media_type == "video" {
        if let Some(video) = &clip.video {
            let fallback = crate::model::SourceTimecode {
                text: "00:00:00:00".to_string(),
                frame_number: 0,
                frame_duration: video
                    .frame_duration
                    .unwrap_or(crate::model::MediaTime::new(1, rate.timebase as i32)),
                drop_frame: false,
            };
            let source_rate = SequenceRate::new(fallback.frame_duration);
            xml += &format!(
                "                  <timecode><string>{}</string><displayformat>NDF</displayformat>\n",
                fallback.text
            );
            xml += &source_rate.xml("                    ");
            xml += "                  </timecode>\n";
        }
    }
    xml += "                  <media>";
    if media_type == "video" {
        if let Some(video) = &clip.video {
            xml += &format!(
                "<video><samplecharacteristics><width>{}</width><height>{}</height></samplecharacteristics></video>",
                video.width, video.height
            );
        }
    }
    if let Some(audio) = clip.audio.first() {
        // Padded sidecars are 32-bit float, like drift sidecars.
        let depth = if item.corrected_audio_url.is_some() || padded {
            32
        } else {
            audio.bit_depth.unwrap_or(24)
        };
        xml += &format!(
            "<audio><samplecharacteristics><depth>{depth}</depth><samplerate>{}</samplerate></samplecharacteristics><channelcount>{}</channelcount></audio>",
            audio.sample_rate.round() as i64,
            audio.channels
        );
    }
    xml += "</media>\n                </file>\n";
    xml
}

#[cfg(test)]
mod tests {
    use super::super::otio::tests::fixture_timeline;
    use super::*;

    #[test]
    fn unpadded_fractional_audio_keeps_subframeoffset() {
        // Historical encoding, preserved for plain `export` (no sidecars):
        // fixture recorder at 0.5 s = 12.5 frames at 25 fps.
        let timeline = fixture_timeline();
        let xml = write(&timeline, TimelineExportFormat::PremiereXML, false);
        assert!(xml.contains("<subframeoffset>40</subframeoffset>"));
    }

    #[test]
    fn near_integer_audio_start_snaps_instead_of_floor_plus_80() {
        // 9.28 s should encode as frame 232, not 231 + subframeoffset 80.
        let mut timeline = fixture_timeline();
        let rec = timeline.islands[0]
            .clips
            .iter_mut()
            .find(|i| i.clip.id.0 == "rec")
            .expect("recorder item");
        rec.start = 9.28;
        let xml = write(&timeline, TimelineExportFormat::PremiereXML, false);
        assert!(!xml.contains("subframeoffset"));
        assert!(xml.contains("<start>232</start>"));
    }

    #[test]
    fn padded_audio_omits_subframeoffset_and_references_sidecar() {
        let mut timeline = fixture_timeline();
        let rec = timeline.islands[0]
            .clips
            .iter_mut()
            .find(|i| i.clip.id.0 == "rec")
            .expect("recorder item");
        // 2.9 frames at 25 fps: floored start 2, 1728-sample prepend.
        rec.start = 2.9 / 25.0;
        let pad = rec
            .clip
            .audio
            .first()
            .and_then(|a| {
                super::super::model::placement_pad_samples(
                    rec.timeline_start("audio"),
                    super::super::model::sequence_fps(timeline.frame_duration),
                    a.sample_rate,
                )
            })
            .expect("pad");
        assert_eq!(pad, 1728);
        let padded = rec
            .clone()
            .with_placement_pad(PathBuf::from("/pad/rec-pad.wav"), pad as f64 / 48_000.0);
        *rec = padded;
        let xml = write(&timeline, TimelineExportFormat::PremiereXML, false);
        assert!(
            !xml.contains("subframeoffset"),
            "padded items carry no subframeoffset"
        );
        assert!(xml.contains("rec-pad.wav"));
        assert!(xml.contains("<start>2</start>\n                <end>303</end>"));
        // File grows by the 0.036 s prepend: 12 s -> 301 frames.
        assert!(xml.matches("<duration>301</duration>").count() >= 2);
        // The padded selection includes the full source tail.
        assert!(xml.contains("<in>0</in>"));
        assert!(xml.contains("<out>301</out>"));
    }

    #[test]
    fn resolve_bootstrap_ignores_pads_byte_identical() {
        // The Resolve bootstrap keeps the historical encoding even when a
        // pad sidecar exists: pads are a PremiereXML-only essence swap.
        let mut timeline = fixture_timeline();
        let rec = timeline.islands[0]
            .clips
            .iter_mut()
            .find(|i| i.clip.id.0 == "rec")
            .expect("recorder item");
        *rec = rec
            .clone()
            .with_placement_pad(PathBuf::from("/pad/rec-pad.wav"), 960.0 / 48_000.0);
        let xml = write(&timeline, TimelineExportFormat::ResolveXML, false);
        assert!(!xml.contains("rec-pad.wav"));
        assert!(xml.contains("<subframeoffset>40</subframeoffset>"));
        assert!(xml.contains("<pathurl>file:///v/rec.wav</pathurl>"));
    }

    #[test]
    fn pads_do_not_share_file_blocks_across_offsets() {
        // Same source at two fractional offsets needs two sidecars: the
        // resource key carries the prepend length.
        let mut timeline = fixture_timeline();
        let mut second = timeline.islands[0]
            .clips
            .iter()
            .find(|i| i.clip.id.0 == "rec")
            .expect("recorder item")
            .clone();
        second.instance_id = "rec-second".into();
        second.start = 0.084;
        second.placement_pad_url = Some(PathBuf::from("/pad/rec-pad-192.wav"));
        second.placement_pad_seconds = 192.0 / 48_000.0;
        timeline.islands[0].clips.push(second);
        let xml = write(&timeline, TimelineExportFormat::PremiereXML, false);
        assert!(xml.contains("file-rec\""), "plain file block keeps its id");
        assert!(
            xml.contains("file-rec-placement-pad-192-"),
            "padded file block gets its own id"
        );
        assert!(xml.contains("/pad/rec-pad-192.wav"));
    }

    #[test]
    fn camera_audio_exports_every_channel() {
        let mut timeline = fixture_timeline();
        for item in &mut timeline.islands[0].clips {
            if item.clip.video.is_some() {
                item.clip.audio[0].channels = 2;
            }
        }
        for format in [
            TimelineExportFormat::PremiereXML,
            TimelineExportFormat::ResolveXML,
        ] {
            let xml = write(&timeline, format, false);
            assert!(xml.contains(
                "<sourcetrack><mediatype>audio</mediatype><trackindex>2</trackindex></sourcetrack>"
            ));
        }
    }

    #[test]
    fn premiere_sequence_structure() {
        let mut timeline = fixture_timeline();
        timeline.islands[0].clips[0].fcp7_labels_xml =
            Some("<labels><label2>Mango</label2></labels>".to_string());
        let xml = write(&timeline, TimelineExportFormat::PremiereXML, false);
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE xmeml>"));
        assert!(xml.contains("<project>"));
        assert!(xml.contains("clipitem-1\"")); // <clipitem id="clipitem-1">
        assert!(xml.contains("<pathurl>file:///v/rec.wav</pathurl>"));
        assert!(xml.contains("<timecode>"));
        assert!(xml.contains("<labels><label2>Mango</label2></labels>"));
        // No replaced sequence without the flag.
        assert!(!xml.contains("sequence-2"));
    }

    #[test]
    fn combined_project_keeps_sequences_and_scopes_identifiers() {
        let first = write(
            &fixture_timeline(),
            TimelineExportFormat::PremiereXML,
            false,
        );
        let mut second_timeline = fixture_timeline();
        second_timeline.name = "Second cut".into();
        let second = write(&second_timeline, TimelineExportFormat::PremiereXML, false);
        let combined = combine_project_documents(&[first, second]).expect("combined project");

        assert_eq!(combined.matches("<sequence id=").count(), 2);
        assert!(combined.contains("<sequence id=\"s1-sequence-1\">"));
        assert!(combined.contains("<sequence id=\"s2-sequence-1\">"));
        assert!(combined.contains("<linkclipref>s1-clipitem-"));
        assert!(combined.contains("<linkclipref>s2-clipitem-"));
        assert!(combined.contains("<name>Second cut</name>"));
    }

    #[test]
    fn replaced_sequence_appears_on_request() {
        let timeline = fixture_timeline();
        let xml = write(&timeline, TimelineExportFormat::PremiereXML, true);
        assert!(xml.contains("sequence-2"));
        assert!(xml.contains("– replaced"));
    }

    #[test]
    fn premiere_labels_import_to_export_round_trip() {
        use crate::model::{
            AudioSummary, Clip, ClipId, ClipPlacement, MappingPoint, MediaKind, MediaTime,
            SyncIsland, SyncProject, SyncResult, VideoSummary,
        };
        use crate::xml::read_timeline;
        use std::path::PathBuf;

        let dir = std::env::temp_dir().join(format!("align-premiere-lab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("labels.xml");
        std::fs::write(
            &path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<xmeml version="4">
<sequence>
<name>Lab</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media>
<video>
<track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="v1">
<name>Cam</name>
<enabled>TRUE</enabled>
<in>0</in><out>100</out><start>0</start><end>100</end>
<labels><label2>Grape</label2></labels>
<file id="fv"><name>Cam</name><pathurl>file:///tmp/lab-cam.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>250</duration></file>
<link><linkclipref>au1</linkclipref></link>
</clipitem>
</track>
</video>
<audio>
<track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="au1">
<name>Cam Audio</name>
<enabled>TRUE</enabled>
<in>0</in><out>100</out><start>0</start><end>100</end>
<labels><label2>Forest</label2></labels>
<file id="fa"><name>Cam</name><pathurl>file:///tmp/lab-cam.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>250</duration></file>
<link><linkclipref>v1</linkclipref></link>
</clipitem>
</track>
</audio>
</media>
</sequence>
</xmeml>"#,
        )
        .unwrap();
        let draft = read_timeline(&path, None).expect("read");
        let cam = Clip {
            id: ClipId::new("cam"),
            url: PathBuf::from("/tmp/lab-cam.mov"),
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
        let imported = draft.resolve(std::slice::from_ref(&cam));
        let edit = imported
            .edits
            .iter()
            .find(|e| e.clip_id == cam.id)
            .expect("cam edit");
        assert_eq!(
            edit.fcp7_labels_xml.as_deref(),
            Some("<labels><label2>Grape</label2></labels>")
        );
        assert_eq!(
            edit.linked_audio_edit
                .as_ref()
                .and_then(|a| a.fcp7_labels_xml.as_deref()),
            Some("<labels><label2>Forest</label2></labels>")
        );

        let result = SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            project: SyncProject {
                clips: vec![cam],
                warnings: Vec::new(),
                imported_timeline: Some(imported),
            },
            islands: vec![SyncIsland {
                id: 0,
                placements: vec![ClipPlacement {
                    clip_id: ClipId::new("cam"),
                    mapping: crate::model::TimeMap {
                        points: vec![
                            MappingPoint {
                                source: MediaTime::seconds(0.0),
                                island: MediaTime::seconds(0.0),
                            },
                            MappingPoint {
                                source: MediaTime::seconds(10.0),
                                island: MediaTime::seconds(10.0),
                            },
                        ],
                    },
                    confidence: 0.9,
                }],
            }],
            unmatched: Vec::new(),
            matches: Vec::new(),
            temporal_policy: crate::model::TemporalPolicy::default(),
        };
        let timeline = ExportTimeline::from_result_with_options(&result, Default::default())
            .expect("timeline");
        let xml = write(&timeline, TimelineExportFormat::PremiereXML, false);
        assert!(xml.contains("<labels><label2>Grape</label2></labels>"));
        assert!(xml.contains("<labels><label2>Forest</label2></labels>"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_bootstrap_has_no_project_wrapper() {
        let timeline = fixture_timeline();
        let xml = write(&timeline, TimelineExportFormat::ResolveXML, false);
        assert!(!xml.contains("<project>"));
        assert!(xml.contains("<sequence id=\"sequence-1\">"));
    }

    #[test]
    fn premiere_other_filters_pass_through_per_side() {
        use crate::model::{
            AudioSummary, Clip, ClipId, ClipPlacement, MappingPoint, MediaKind, MediaTime,
            SyncIsland, SyncProject, SyncResult, VideoSummary,
        };
        use crate::xml::read_timeline;
        use std::path::PathBuf;

        let dir = std::env::temp_dir().join(format!("align-premiere-fx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("filters.xml");
        std::fs::write(
            &path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<xmeml version="4">
<sequence>
<name>Fx</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media>
<video>
<track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="v1">
<name>Cam</name>
<enabled>TRUE</enabled>
<in>0</in><out>100</out><start>0</start><end>100</end>
<filter><effect><name>VideoSideEffect</name><effectid>videoside</effectid></effect></filter>
<file id="fv"><name>Cam</name><pathurl>file:///tmp/fx-cam.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>250</duration></file>
<link><linkclipref>au1</linkclipref></link>
</clipitem>
</track>
</video>
<audio>
<track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="au1">
<name>Cam Audio</name>
<enabled>TRUE</enabled>
<in>0</in><out>100</out><start>0</start><end>100</end>
<filter><effect><name>AudioSideEffect</name><effectid>audioside</effectid></effect></filter>
<file id="fa"><name>Cam</name><pathurl>file:///tmp/fx-cam.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>250</duration></file>
<link><linkclipref>v1</linkclipref></link>
</clipitem>
</track>
</audio>
</media>
</sequence>
</xmeml>"#,
        )
        .unwrap();
        let draft = read_timeline(&path, None).expect("read");
        assert!(
            draft
                .warnings
                .iter()
                .any(|w| w.message.contains("passed through to Premiere XML"))
        );
        let cam = Clip {
            id: ClipId::new("cam"),
            url: PathBuf::from("/tmp/fx-cam.mov"),
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
        let imported = draft.resolve(std::slice::from_ref(&cam));
        let edit = imported
            .edits
            .iter()
            .find(|e| e.clip_id == cam.id)
            .expect("cam edit");
        assert_eq!(edit.fcp7_filter_xmls.len(), 1);
        assert_eq!(
            edit.linked_audio_edit
                .as_ref()
                .expect("linked")
                .fcp7_filter_xmls
                .len(),
            1
        );

        let result = SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            project: SyncProject {
                clips: vec![cam],
                warnings: Vec::new(),
                imported_timeline: Some(imported),
            },
            islands: vec![SyncIsland {
                id: 0,
                placements: vec![ClipPlacement {
                    clip_id: ClipId::new("cam"),
                    mapping: crate::model::TimeMap {
                        points: vec![
                            MappingPoint {
                                source: MediaTime::seconds(0.0),
                                island: MediaTime::seconds(0.0),
                            },
                            MappingPoint {
                                source: MediaTime::seconds(10.0),
                                island: MediaTime::seconds(10.0),
                            },
                        ],
                    },
                    confidence: 0.9,
                }],
            }],
            unmatched: Vec::new(),
            matches: Vec::new(),
            temporal_policy: crate::model::TemporalPolicy::default(),
        };
        let timeline = ExportTimeline::from_result_with_options(&result, Default::default())
            .expect("timeline");
        let xml = write(&timeline, TimelineExportFormat::PremiereXML, false);
        // Each side passes through exactly once: no leaks across the link.
        assert_eq!(xml.matches("VideoSideEffect").count(), 1);
        assert_eq!(xml.matches("AudioSideEffect").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
