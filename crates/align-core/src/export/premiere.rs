//! FCP 7 XML output for Premiere Pro and Resolve.
//!
//! One sequence per combined island (`– synced`, plus an optional `–
//! replaced` recorder-audio sequence for Premiere), original FCP7 effect
//! payloads passed through verbatim, retime graphs rebuilt only when no
//! preserved XML exists. Frame math mirrors `SequenceRate` exactly
//! (down/nearest/up roundings, subframe offsets, NTSC timebases).
//!
//! Audio placement: Premiere and Resolve start imported sound only on a
//! frame and ignore `<subframeoffset>`, so sound starts on the nearest frame,
//! within half a frame, like other sync tools. The Premiere XML keeps the
//! remainder after a frame below as `<subframeoffset>` for other readers.
//!
//! Media bins: a Premiere project can open with one bin of master clips per
//! camera and recorder, plus a bin of the audio Align prepared. Master clips
//! define every `<file>` before the sequences, whose clipitems then
//! reference them by `masterclipid` and file id.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::model::{
    ExportIsland, ExportItem, ExportTimeline, ExportTransition, TimelineExportFormat, xml_text,
};
use crate::allocator::{TimelineTrackRequest, allocate, source_key_for_url};
use crate::bins::{GENERATED_AUDIO, MediaBins};
use crate::model::{ClipId, MediaKind};
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

/// One clipitem: item instance, timeline side, and source channel.
type ClipKey = (String, MediaKind, usize);

#[derive(Clone, Copy)]
struct TrackEntry<'a> {
    item: &'a ExportItem,
    source_channel: usize,
    /// A channel of a stereo clip: its left and right play on a pair of
    /// tracks Premiere joins into one stereo track.
    stereo: bool,
}

impl<'a> TrackEntry<'a> {
    fn key(&self, media_type: MediaKind) -> ClipKey {
        (
            self.item.instance_id.clone(),
            media_type,
            self.source_channel,
        )
    }
}

struct LinkReference {
    clip_item_id: String,
    media_type: Option<MediaKind>,
    track_index: Option<usize>,
    clip_index: Option<usize>,
    group_index: Option<usize>,
}

// ------------------------------------------------------------ write

/// What a Premiere document contains besides its synchronized sequence.
/// Resolve XML is a bare sequence and ignores both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PremiereOptions {
    /// A second sequence with recorder audio replacing camera audio.
    pub replaced_sequence: bool,
    /// Master clips organized into bins by camera and recorder.
    pub media_bins: bool,
}

pub fn write(
    timeline: &ExportTimeline,
    format: TimelineExportFormat,
    options: PremiereOptions,
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
        let premiere = format == TimelineExportFormat::PremiereXML;
        let mut document = Document {
            rate,
            format,
            defined_files: HashSet::new(),
            master_clips: HashMap::new(),
            stereo_files: if premiere {
                stereo_files(island)
            } else {
                HashSet::new()
            },
        };
        let replaced = if premiere && options.replaced_sequence {
            replaced_island(island)
        } else {
            None
        };
        // Bins come first: their master clips define the files that the
        // sequences then reference.
        let bins = if premiere && options.media_bins {
            let mut islands = vec![(island, true)];
            islands.extend(replaced.as_ref().map(|r| (r, false)));
            document.media_bins(&islands)
        } else {
            String::new()
        };
        let main_xml = document.sequence(
            island,
            if replaced.is_none() {
                timeline.name.clone()
            } else {
                format!("{} – synced", timeline.name)
            },
            "sequence-1",
            "",
            true,
        );
        if premiere {
            xml += "  <project>\n    <name>Align</name>\n    <children>\n";
            xml += &bins;
            xml += &main_xml;
            if let Some(rep) = &replaced {
                xml += &document.sequence(
                    rep,
                    format!("{} – replaced", timeline.name),
                    "sequence-2",
                    "replaced-",
                    false,
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
/// keeping every sequence and resource identifier unique. Documents with
/// media bins each move into a bin named after their sequence, so that bins
/// of the same camera stay apart.
pub fn combine_project_documents(
    documents: &[(String, String)],
    media_bins: bool,
) -> Result<String, String> {
    if documents.is_empty() {
        return Err("no Premiere documents to combine".into());
    }
    let mut xml =
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE xmeml>\n<xmeml version=\"5\">\n  <project>\n    <name>Align</name>\n    <children>\n"
            .to_string();
    for (index, (name, document)) in documents.iter().enumerate() {
        // The project's own children; bins nest deeper-indented ones.
        let open = "\n    <children>\n";
        let start = document
            .find(open)
            .map(|offset| offset + open.len())
            .ok_or("Premiere document has no project children")?;
        let end = document
            .rfind("\n    </children>\n  </project>")
            .filter(|&end| end >= start)
            .map(|end| end + 1)
            .ok_or("Premiere document has unterminated project children")?;
        let children = prefix_document_ids(&document[start..end], &format!("s{}-", index + 1));
        if media_bins {
            xml += &format!(
                "      <bin>\n        <name>{}</name>\n        <children>\n{children}        </children>\n      </bin>\n",
                xml_text::escape(name)
            );
        } else {
            xml += &children;
        }
    }
    xml += "    </children>\n  </project>\n</xmeml>\n";
    Ok(xml)
}

/// Prefix every `id` attribute and every element that refers to one.
fn prefix_document_ids(fragment: &str, prefix: &str) -> String {
    let mut out = fragment.replace(" id=\"", &format!(" id=\"{prefix}"));
    for element in ["linkclipref", "masterclipid"] {
        let open = format!("<{element}>");
        let close = format!("</{element}>");
        let mut cursor = 0;
        while let Some(start) = out[cursor..]
            .find(&open)
            .map(|offset| cursor + offset + open.len())
        {
            out.insert_str(start, prefix);
            cursor = out[start + prefix.len()..]
                .find(&close)
                .map_or(out.len(), |offset| {
                    start + prefix.len() + offset + close.len()
                });
        }
    }
    out
}

/// State shared by every element of one Premiere or Resolve XML document.
struct Document {
    rate: SequenceRate,
    format: TimelineExportFormat,
    /// Resource keys of `<file>` elements already written; later clipitems
    /// reference them by id only.
    defined_files: HashSet<String>,
    /// Master clip ids by the resource key of the file they define.
    master_clips: HashMap<String, String>,
    /// Resource keys of two-channel files every item plays whole, which
    /// Premiere XML plays as stereo clips.
    stereo_files: HashSet<String>,
}

/// Clipitem ids and 1-based track/clip positions of one sequence, which
/// link references between a clip's video and audio items name.
struct Layout {
    ids: HashMap<ClipKey, String>,
    positions: HashMap<ClipKey, (usize, usize)>,
}

impl Document {
    fn sequence(
        &mut self,
        island: &ExportIsland,
        name: String,
        id: &str,
        clip_id_prefix: &str,
        include_embedded_audio: bool,
    ) -> String {
        let rate = self.rate;
        let duration = rate.frames(island.duration, Rounding::Up);
        let video_entries: Vec<TrackEntry> = island
            .clips
            .iter()
            .filter(|i| i.clip.video.is_some())
            .map(|item| TrackEntry {
                item,
                source_channel: 0,
                stereo: false,
            })
            .collect();
        let audio_entries: Vec<TrackEntry> = island
            .clips
            .iter()
            .filter(|i| {
                !i.clip.audio.is_empty()
                    && (include_embedded_audio || i.clip.kind == MediaKind::Audio)
            })
            .flat_map(|item| {
                let stereo = item.selected_audio_source_channel().is_none()
                    && self.stereo_files.contains(&base_resource_key(item));
                item.audio_source_channels().map(move |ch| TrackEntry {
                    item,
                    source_channel: ch,
                    stereo,
                })
            })
            .collect();
        let stereo = audio_entries.iter().any(|entry| entry.stereo);
        // Premiere writes each channel of a stereo track as a track of its
        // own, and joins them where the sequence says it explodes tracks.
        let exploded = if stereo {
            " explodedTracks=\"true\""
        } else {
            ""
        };
        let mut xml = format!("      <sequence id=\"{id}\"{exploded}>\n");
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

        let video_tracks = tracks_for(&video_entries, MediaKind::Video);
        let audio_tracks = tracks_for(&audio_entries, MediaKind::Audio);
        let mut ids = identifiers(&video_tracks, MediaKind::Video, 1, clip_id_prefix);
        ids.extend(identifiers(
            &audio_tracks,
            MediaKind::Audio,
            ids.len() + 1,
            clip_id_prefix,
        ));
        let mut positions = track_positions(&video_tracks, MediaKind::Video);
        positions.extend(track_positions(&audio_tracks, MediaKind::Audio));
        let layout = Layout { ids, positions };

        for track in &video_tracks {
            xml += &self.track(track, MediaKind::Video, &layout);
        }
        let audio_depth = if island.clips.iter().any(|i| i.corrected_audio_url.is_some()) {
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
        if stereo {
            xml += "            <numOutputChannels>2</numOutputChannels>\n            <outputs>\n";
            for channel in 1..=2 {
                xml += &format!(
                    "              <group><index>{channel}</index><numchannels>1</numchannels><downmix>0</downmix><channel><index>{channel}</index></channel></group>\n"
                );
            }
            xml += "            </outputs>\n";
        }
        for track in &audio_tracks {
            xml += &self.track(track, MediaKind::Audio, &layout);
        }
        xml += "          </audio>\n        </media>\n      </sequence>\n";
        xml
    }

    /// One `<track>` of clipitems and transitions.
    fn track(&mut self, track: &[TrackEntry], media_type: MediaKind, layout: &Layout) -> String {
        let stereo = track
            .first()
            .filter(|entry| entry.stereo)
            .map(|entry| entry.source_channel);
        let mut xml = match stereo {
            Some(channel) => format!(
                "            <track premiereTrackType=\"Stereo\" currentExplodedTrackIndex=\"{}\" totalExplodedTrackCount=\"2\">\n",
                channel - 1
            ),
            None => "            <track>\n".to_string(),
        };
        for (index, entry) in track.iter().enumerate() {
            let before = index
                .checked_sub(1)
                .and_then(|previous| transition_between(&track[previous], entry, media_type));
            let after = track
                .get(index + 1)
                .and_then(|next| transition_between(entry, next, media_type));
            let next_start = track
                .get(index + 1)
                .map(|next| self.start_frame(next, media_type));
            xml += &self.clip_item(
                entry,
                media_type,
                &layout.ids[&entry.key(media_type)],
                &links(entry, media_type, self.format, layout),
                before.is_some(),
                after.is_some(),
                next_start,
            );
            if let Some(t) = after {
                xml += &transition_item(t, self.rate);
            }
        }
        let enabled = track
            .first()
            .is_none_or(|e| e.item.is_track_enabled(media_type));
        let locked = track
            .first()
            .is_some_and(|e| e.item.is_track_locked(media_type));
        xml += &format!(
            "              <enabled>{}</enabled><locked>{}</locked>\n",
            tf(enabled),
            tf(locked)
        );
        if let Some(channel) = stereo {
            xml += &format!("              <outputchannelindex>{channel}</outputchannelindex>\n");
        }
        xml += "            </track>\n";
        xml
    }
}

fn base_resource_key(item: &ExportItem) -> String {
    if item.corrected_audio_url.is_none() {
        item.clip.id.0.clone()
    } else {
        format!("{}-corrected-audio", item.clip.id.0)
    }
}

/// Resource keys of the two-channel files, sources and drift sidecars, that
/// no item plays a single channel of. Premiere plays a single channel only
/// of a file described channel by channel.
fn stereo_files(island: &ExportIsland) -> HashSet<String> {
    let mut stereo = HashSet::new();
    let mut single = HashSet::new();
    for item in &island.clips {
        if item
            .clip
            .audio
            .first()
            .is_none_or(|audio| audio.channels != 2)
        {
            continue;
        }
        let keys = [base_resource_key(item), item.clip.id.0.clone()];
        if item.selected_audio_source_channel().is_some() {
            single.extend(keys);
        } else {
            stereo.extend(keys);
        }
    }
    &stereo - &single
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
    media_type: MediaKind,
    first: usize,
    prefix: &str,
) -> HashMap<ClipKey, String> {
    let mut out = HashMap::new();
    for (number, entry) in (first..).zip(tracks.iter().flat_map(|t| t.iter())) {
        out.insert(entry.key(media_type), format!("clipitem-{prefix}{number}"));
    }
    out
}

fn track_positions(
    tracks: &[Vec<TrackEntry>],
    media_type: MediaKind,
) -> HashMap<ClipKey, (usize, usize)> {
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
    media_type: MediaKind,
    format: TimelineExportFormat,
    layout: &Layout,
) -> Vec<LinkReference> {
    let Layout { ids, positions } = layout;
    let video_key = (entry.item.instance_id.clone(), MediaKind::Video, 0);
    let (Some(video_id), Some(video_position)) = (ids.get(&video_key), positions.get(&video_key))
    else {
        // The two channels of a stereo clip without picture link each other.
        if !entry.stereo || format != TimelineExportFormat::PremiereXML {
            return Vec::new();
        }
        return (1..=2)
            .filter_map(|channel| {
                let key = (entry.item.instance_id.clone(), MediaKind::Audio, channel);
                let (track, clip) = *positions.get(&key)?;
                Some(LinkReference {
                    clip_item_id: ids.get(&key)?.clone(),
                    media_type: Some(MediaKind::Audio),
                    track_index: Some(track),
                    clip_index: Some(clip),
                    group_index: Some(1),
                })
            })
            .collect();
    };
    let mut linked_audio: Vec<(String, (usize, usize))> = ids
        .keys()
        .filter(|k| k.0 == entry.item.instance_id && k.1 == MediaKind::Audio)
        .filter_map(|k| Some((ids.get(k)?.clone(), *positions.get(k)?)))
        .collect();
    linked_audio.sort_by(|a, b| {
        let ka = ids
            .iter()
            .find(|(_, v)| **v == a.0)
            .map(|(k, _)| k.2)
            .unwrap_or(0);
        let kb = ids
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
            media_type: Some(MediaKind::Video),
            track_index: Some(video_position.0),
            clip_index: Some(video_position.1),
            group_index: None,
        }];
        out.extend(linked_audio.iter().map(|audio| LinkReference {
            clip_item_id: audio.0.clone(),
            media_type: Some(MediaKind::Audio),
            track_index: Some(audio.1.0),
            clip_index: Some(audio.1.1),
            group_index: Some(1),
        }));
        return out;
    }
    vec![
        LinkReference {
            clip_item_id: video_id.clone(),
            media_type: if media_type == MediaKind::Audio {
                Some(MediaKind::Video)
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

/// Tracks of `entries`. The channels of stereo clips take pairs of
/// adjacent tracks, left then right, each pair holding stereo clips alone.
fn tracks_for<'a>(entries: &[TrackEntry<'a>], media_type: MediaKind) -> Vec<Vec<TrackEntry<'a>>> {
    if !entries.iter().any(|entry| entry.stereo) {
        return lanes_for(entries, media_type);
    }
    let (stereo, single): (Vec<TrackEntry<'a>>, Vec<TrackEntry<'a>>) =
        entries.iter().partition(|entry| entry.stereo);
    let right: HashMap<&str, TrackEntry<'a>> = stereo
        .iter()
        .filter(|entry| entry.source_channel == 2)
        .map(|entry| (entry.item.instance_id.as_str(), *entry))
        .collect();
    let left: Vec<TrackEntry<'a>> = stereo
        .into_iter()
        .filter(|entry| entry.source_channel == 1)
        .collect();
    let source = |track: &[TrackEntry<'a>]| {
        track
            .iter()
            .map(|entry| {
                entry
                    .item
                    .preferred_source_key(media_type)
                    .map(str::to_string)
                    .unwrap_or_else(|| source_key_for_url(&entry.item.clip.url))
            })
            .min()
            .unwrap_or_default()
    };
    // Pairs and single tracks in the order of their sources.
    let mut units: Vec<(String, Vec<Vec<TrackEntry<'a>>>)> = lanes_for(&single, media_type)
        .into_iter()
        .map(|track| (source(&track), vec![track]))
        .collect();
    units.extend(lanes_for(&left, media_type).into_iter().map(|track| {
        let mirrored = track
            .iter()
            .filter_map(|entry| right.get(entry.item.instance_id.as_str()).copied())
            .collect();
        (source(&track), vec![track, mirrored])
    }));
    units.sort_by(|a, b| a.0.cmp(&b.0));
    units.into_iter().flat_map(|(_, tracks)| tracks).collect()
}

/// Tracks of `entries`, packed by the allocator.
fn lanes_for<'a>(entries: &[TrackEntry<'a>], media_type: MediaKind) -> Vec<Vec<TrackEntry<'a>>> {
    let source_key = |entry: &TrackEntry<'a>| -> String {
        let item = entry.item;
        let base = item
            .preferred_source_key(media_type)
            .map(str::to_string)
            .unwrap_or_else(|| source_key_for_url(&item.clip.url));
        if entry.stereo {
            format!("{base}#stereo")
        } else if media_type == MediaKind::Audio {
            format!("{base}#channel-{}", entry.source_channel)
        } else {
            base
        }
    };
    // Clips joined by transitions, channel by channel, stay on one track as
    // a chain; the others are packed.
    let position: HashMap<(&str, usize), usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            (
                (entry.item.instance_id.as_str(), entry.source_channel),
                index,
            )
        })
        .collect();
    let next: Vec<Option<usize>> = entries
        .iter()
        .map(|entry| {
            let t = entry.item.transition_for(media_type)?;
            position
                .get(&(t.right_instance_id.as_str(), entry.source_channel))
                .copied()
        })
        .collect();
    let mut chained = vec![false; entries.len()];
    let mut forced: Vec<Vec<TrackEntry<'a>>> = Vec::new();
    for head in (0..entries.len()).filter(|&i| next[i].is_some() && !next.contains(&Some(i))) {
        let mut chain = vec![head];
        while let Some(following) = next[*chain.last().unwrap()].filter(|f| !chain.contains(f)) {
            chain.push(following);
        }
        for &index in &chain {
            chained[index] = true;
        }
        forced.push(chain.into_iter().map(|index| entries[index]).collect());
    }
    forced.sort_by(|a, b| {
        a[0].item
            .timeline_start(media_type)
            .total_cmp(&b[0].item.timeline_start(media_type))
    });
    let allocatable: Vec<TrackEntry<'a>> = entries
        .iter()
        .zip(&chained)
        .filter(|(_, chained)| !**chained)
        .map(|(entry, _)| *entry)
        .collect();
    // Where an entry plays on the timeline.
    let content = |entry: &TrackEntry<'a>| {
        let item = entry.item;
        let duration = if media_type == MediaKind::Audio
            && item.corrected_audio_url.is_some()
            && !item.is_retimed(media_type)
        {
            item.corrected_selected_duration()
        } else {
            item.selected_timeline_duration(media_type)
        };
        (item.timeline_start(media_type), duration)
    };

    let requests: Vec<TimelineTrackRequest> = allocatable
        .iter()
        .map(|entry| {
            let item = entry.item;
            let (start, duration) = content(entry);
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

impl Document {
    /// The sequence frame a clipitem starts on. Premiere and Resolve start
    /// XML sound only on a frame, so it starts on the nearest one, like its
    /// picture.
    fn start_frame(&self, entry: &TrackEntry, media_type: MediaKind) -> i64 {
        let (rate, item) = (self.rate, entry.item);
        let start = stable_start(item, media_type);
        if media_type == MediaKind::Video {
            return rate.frames(start, Rounding::Nearest);
        }
        // Collapse sub-sample dust (float noise, µs solver jitter) to the
        // adjacent integer frame so near-exact starts encode exactly.
        let sample_rate = item.clip.audio.first().map_or(48_000.0, |a| a.sample_rate);
        let floor = rate.frames(start, Rounding::Down);
        let (adjustment, fraction) = super::model::snap_sub_sample_frac(
            start * rate.fps - floor as f64,
            sample_rate / rate.fps,
        );
        floor + adjustment + i64::from(fraction >= 0.5)
    }

    /// One `<clipitem>`. FCP7 marks an edge shared with a transition as -1.
    /// A clip ending after `next_start`, where the next clip of its track
    /// starts, ends there: Resolve moves the next clip past it instead.
    #[allow(clippy::too_many_arguments)]
    fn clip_item(
        &mut self,
        entry: &TrackEntry,
        media_type: MediaKind,
        id: &str,
        links: &[LinkReference],
        transition_before: bool,
        transition_after: bool,
        next_start: Option<i64>,
    ) -> String {
        let (rate, format) = (self.rate, self.format);
        let item = entry.item;
        let start = self.start_frame(entry, media_type);
        let media = self.media_use(item, media_type);
        let (source_rate, file_duration) = (media.rate, media.duration);
        let corrected = media.corrected;
        let (in_seconds, out_seconds) = (media.in_seconds, media.out_seconds);
        let playback_rate = item.playback_rate(media_type);
        let plays_backward = item.plays_backward(media_type);
        let graph_slope = playback_rate * source_rate.fps / rate.fps;
        let retimed = item.is_retimed(media_type);
        // Premiere counts `<in>` and `<out>` in the sequence timebase whatever
        // the clipitem's `<rate>` says, so a 24 fps camera on a 25 fps
        // sequence lost 4% of its range. Its clipitems use the sequence rate.
        // Resolve honors the clip's own rate and keeps it.
        let conformed = !retimed
            && format == TimelineExportFormat::PremiereXML
            && (source_rate.timebase, source_rate.ntsc) != (rate.timebase, rate.ntsc);
        let item_rate = if retimed || conformed {
            rate
        } else {
            source_rate
        };
        // Resolve plays an imported Time Remap graph as a still frame, and
        // the graph Align writes as recorded, so Resolve XML takes Align's.
        let preserved = format == TimelineExportFormat::PremiereXML;
        let geometry = item.fcp7_retime_geometry(media_type).filter(|_| preserved);
        // The source range moves with the start's rounding, so the sound
        // stays within half a frame of its place; two separate roundings
        // could add up to almost a frame.
        let rounding = if retimed {
            0.0
        } else {
            start as f64 / rate.fps - stable_start(item, media_type)
        };
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
                shifted_frame(item_rate, in_seconds, rounding),
                shifted_frame(item_rate, out_seconds, rounding),
            )
        };
        // How far ahead of its place the sound plays, in sequence frames.
        let ahead = if retimed {
            0.0
        } else {
            (computed_in as f64 / item_rate.fps - in_seconds - rounding) * rate.fps
        };
        // Resolve counts a trimmed WAV's in point from its broadcast start
        // rounded to a frame, halves down, so a WAV starting past the middle
        // of a frame played a frame late; one frame more in the Resolve XML
        // lands on the selected frame.
        let late = i64::from(
            format == TimelineExportFormat::ResolveXML
                && media_type == MediaKind::Audio
                && !retimed
                && computed_in > 0
                && starts_past_the_middle_of_a_frame(item, rate),
        );
        let source_in = geometry.map_or(computed_in, |g| g.0) + late;
        let mut source_out = geometry.map_or(computed_out, |g| g.1) + late;
        let clip_duration = geometry.map_or_else(
            || {
                if retimed {
                    source_out.max((file_duration as f64 / graph_slope).round() as i64)
                } else if conformed {
                    rate.frames(file_duration as f64 / source_rate.fps, Rounding::Nearest)
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
        let mut timeline_duration = rate.frames(selected_tl, Rounding::Nearest);
        if !retimed && geometry.is_none() && (item_rate.fps - rate.fps).abs() < 1e-9 {
            // The source range spans the clip's frames, and the moved range
            // ends with its file.
            let past_end = source_in + timeline_duration - (clip_duration + late);
            if past_end > 0 && past_end < timeline_duration {
                timeline_duration -= past_end;
            }
            source_out = source_in + timeline_duration;
        }
        let mut end = start + timeline_duration;
        // Seams of one source's clips share a track (see the allocator), and
        // rounding to frames can end a clip inside the next.
        let overlap = next_start
            .filter(|_| !transition_after && !retimed)
            .map_or(0, |next| end - next);
        if overlap > 0 && overlap < timeline_duration {
            end -= overlap;
            source_out -= (overlap as f64 * item_rate.fps / rate.fps).round() as i64;
        }
        let name = xml_text::escape(
            &item
                .display_name
                .clone()
                .unwrap_or_else(|| crate::model::file_name(&media.url)),
        );
        let file_id = media.file_id();
        let channel_type = if entry.stereo && media_type == MediaKind::Audio {
            " premiereChannelType=\"stereo\""
        } else {
            ""
        };
        let mut xml = format!("              <clipitem id=\"{id}\"{channel_type}>\n");
        if let Some(master) = self.master_clips.get(&media.resource_key) {
            xml += &format!("                <masterclipid>{master}</masterclipid>\n");
        }
        xml += &format!(
            "                <name>{name}</name>\n                <enabled>{}</enabled>\n                <duration>{clip_duration}</duration>\n",
            tf(item.is_enabled(media_type))
        );
        xml += &item_rate.xml("                ");
        xml += &format!(
            "                <start>{}</start>\n                <end>{}</end>\n                <in>{source_in}</in>\n                <out>{source_out}</out>\n",
            if transition_before { -1 } else { start },
            if transition_after { -1 } else { end }
        );
        if media_type == MediaKind::Audio && format == TimelineExportFormat::PremiereXML {
            let subframes = super::model::subframe_offset_80(ahead.max(0.0));
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
        if self.defined_files.insert(media.resource_key.clone()) {
            let stereo = self.stereo_files.contains(&media.resource_key);
            xml += &file_block(item, media_type, &media, stereo);
        } else {
            xml += &format!("                <file id=\"{file_id}\"/>\n");
        }
        if media_type == MediaKind::Audio {
            xml += &format!(
                "                <sourcetrack><mediatype>audio</mediatype><trackindex>{}</trackindex></sourcetrack>\n",
                entry.source_channel
            );
        }
        if retimed {
            xml += &match item.fcp7_time_remap_xml(media_type).filter(|_| preserved) {
                Some(graph) if plays_backward => {
                    format!(
                        "                {}\n",
                        reverse_out_at_speed(graph, graph_slope)
                    )
                }
                Some(graph) => format!("                {graph}\n"),
                None => TimeRemap {
                    premiere: format == TimelineExportFormat::PremiereXML,
                    media_type,
                    graph_slope,
                    graph_in: source_in,
                    graph_out: source_out,
                    graph_duration: clip_duration,
                    media_in: in_seconds * source_rate.fps,
                    media_out: out_seconds * source_rate.fps,
                    media_duration: file_duration as f64,
                    plays_backward,
                }
                .xml(),
            };
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
}

fn transition_between<'a>(
    left: &TrackEntry<'a>,
    right: &TrackEntry<'a>,
    media_type: MediaKind,
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
        // Rewrite start/end coordinates only: splice
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

/// A generated Time Remap filter: graph frames on the timeline against
/// media frames of the source.
struct TimeRemap {
    /// Premiere reads a reverse graph's speed from its out point.
    premiere: bool,
    media_type: MediaKind,
    graph_slope: f64,
    graph_in: i64,
    graph_out: i64,
    graph_duration: i64,
    media_in: f64,
    media_out: f64,
    media_duration: f64,
    plays_backward: bool,
}

impl TimeRemap {
    fn xml(&self) -> String {
        let &Self {
            premiere,
            media_type,
            graph_slope,
            graph_in,
            graph_out,
            graph_duration,
            media_in,
            media_out,
            media_duration,
            plays_backward,
        } = self;
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
            match (plays_backward, premiere) {
                (true, true) => graph_out as f64 * graph_slope,
                (true, false) => media_in + 1.0,
                (false, _) => media_out,
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
}

/// A reverse Time Remap graph whose out point Premiere reads its speed
/// from: the out keyframe's value is its time at the clip's speed. With the
/// value of the source frame there, Premiere played the clip as a still.
fn reverse_out_at_speed(graph: &str, slope: f64) -> String {
    let rewritten = (|| {
        let flag = graph.find("<speedkfout>TRUE</speedkfout>")?;
        let start = graph[..flag].rfind("<keyframe>")?;
        let end = flag + graph[flag..].find("</keyframe>")?;
        let keyframe = &graph[start..end];
        let when: f64 = keyframe
            .split_once("<when>")?
            .1
            .split_once("</when>")?
            .0
            .trim()
            .parse()
            .ok()?;
        let value = keyframe.find("<value>")? + "<value>".len();
        let value_end = value + keyframe[value..].find("</value>")?;
        Some(format!(
            "{}{}{}",
            &graph[..start + value],
            xml_number(when * slope),
            &graph[start + value_end..]
        ))
    })();
    rewritten.unwrap_or_else(|| graph.to_string())
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

/// The file one side of an item plays: its source or its drift sidecar, with
/// the `<file>` description it is written with once per document.
struct MediaUse {
    /// One per source and drift sidecar.
    resource_key: String,
    url: PathBuf,
    /// The file's own rate, which `duration` counts in.
    rate: SequenceRate,
    duration: i64,
    /// A drift-corrected sidecar.
    corrected: bool,
    /// The selected range in the file's seconds.
    in_seconds: f64,
    out_seconds: f64,
}

impl MediaUse {
    fn file_id(&self) -> String {
        format!("file-{}", xml_text::identifier(&self.resource_key))
    }

    /// Sidecars Align renders are 32-bit float.
    fn is_prepared(&self) -> bool {
        self.corrected
    }
}

/// Where an item's side starts on the timeline, without float noise.
fn stable_start(item: &ExportItem, media_type: MediaKind) -> f64 {
    (item.timeline_start(media_type) * 100_000.0).round() / 100_000.0
}

/// The nearest frame of `seconds` moved by `shift`; halfway between two
/// frames, the one nearer the unmoved time.
fn shifted_frame(rate: SequenceRate, seconds: f64, shift: f64) -> i64 {
    let frames = (seconds + shift) * rate.fps;
    let halfway = (frames - frames.floor() - 0.5).abs() < 1e-6;
    let frame = match (halfway, shift > 0.0) {
        (true, true) => frames.floor(),
        (true, false) => frames.ceil(),
        (false, _) => frames.round(),
    };
    frame as i64
}

/// An audio file whose own broadcast start falls past the middle of a frame
/// of the sequence, by more than half a sample, and short of its end.
fn starts_past_the_middle_of_a_frame(item: &ExportItem, rate: SequenceRate) -> bool {
    let Some(audio) = item.clip.audio.first() else {
        return false;
    };
    let Some(timecode) = audio.source_timecode.as_ref().filter(|tc| !tc.derived) else {
        return false;
    };
    let frames = timecode.as_seconds() * rate.fps;
    let half_sample = 0.5 * rate.fps / audio.sample_rate.max(1.0);
    let fraction = frames - frames.floor();
    item.clip.video.is_none() && fraction - 0.5 > half_sample && 1.0 - fraction > half_sample
}

fn source_rate(item: &ExportItem, sequence: SequenceRate) -> SequenceRate {
    item.clip
        .video
        .as_ref()
        .and_then(|v| v.frame_duration)
        .map(SequenceRate::new)
        .unwrap_or(sequence)
}

impl Document {
    fn media_use(&self, item: &ExportItem, media_type: MediaKind) -> MediaUse {
        let corrected = media_type == MediaKind::Audio && item.corrected_audio_url.is_some();
        let (in_seconds, out_seconds) = if corrected {
            (item.corrected_source_in(), item.corrected_source_out())
        } else {
            (
                item.selected_source_in(media_type),
                item.selected_source_out(media_type),
            )
        };
        let rate = source_rate(item, self.rate);
        let seconds = if corrected {
            item.mapped_duration()
        } else {
            item.source_duration()
        };
        let url = match media_type {
            MediaKind::Audio => item
                .corrected_audio_url
                .clone()
                .unwrap_or_else(|| item.clip.url.clone()),
            MediaKind::Video => item.clip.url.clone(),
        };
        MediaUse {
            resource_key: base_resource_key(item),
            url,
            rate,
            duration: rate.frames(seconds, Rounding::Nearest),
            corrected,
            in_seconds,
            out_seconds,
        }
    }

    /// An item's untouched source file, which it shares with every other
    /// item of its clip that plays the source.
    fn source_use(&self, item: &ExportItem) -> MediaUse {
        let rate = source_rate(item, self.rate);
        MediaUse {
            resource_key: item.clip.id.0.clone(),
            url: item.clip.url.clone(),
            rate,
            duration: rate.frames(item.source_duration(), Rounding::Nearest),
            corrected: false,
            in_seconds: 0.0,
            out_seconds: item.source_duration(),
        }
    }

    /// Bins of master clips: each source in the bin of its camera or
    /// recorder, then every sidecar the sequences play in one bin of
    /// generated audio. A sequence's camera audio is written only when
    /// `include_embedded_audio` is set for its island.
    fn media_bins(&mut self, islands: &[(&ExportIsland, bool)]) -> String {
        let bins = MediaBins::new(
            islands
                .iter()
                .flat_map(|(island, _)| island.clips.iter().map(|item| &item.clip)),
        );
        let mut sources: HashMap<&ClipId, &ExportItem> = HashMap::new();
        let mut prepared = Vec::new();
        let mut prepared_keys = HashSet::new();
        for &(island, include_embedded_audio) in islands {
            for item in &island.clips {
                // The source of a camera file is its item with picture, as
                // its bin lists it.
                let source = sources.entry(&item.clip.id).or_insert(item);
                if source.clip.video.is_none() && item.clip.video.is_some() {
                    *source = item;
                }
                let video = item.clip.video.is_some();
                let audio = !item.clip.audio.is_empty()
                    && (include_embedded_audio || item.clip.kind == MediaKind::Audio);
                for (side, plays) in [(MediaKind::Video, video), (MediaKind::Audio, audio)] {
                    let media = self.media_use(item, side);
                    if plays
                        && media.is_prepared()
                        && prepared_keys.insert(media.resource_key.clone())
                    {
                        prepared.push((item, side, media));
                    }
                }
            }
        }

        let mut xml = String::new();
        for bin in bins.bins() {
            xml += &bin_open(&bin.name);
            for clip in &bin.clips {
                let item = sources[&clip.id];
                let side = if clip.video.is_some() {
                    MediaKind::Video
                } else {
                    MediaKind::Audio
                };
                let media = self.source_use(item);
                xml += &self.master_clip(item, side, &media);
            }
            xml += BIN_CLOSE;
        }
        if !prepared.is_empty() {
            xml += &bin_open(GENERATED_AUDIO);
            for (item, side, media) in &prepared {
                xml += &self.master_clip(item, *side, media);
            }
            xml += BIN_CLOSE;
        }
        xml
    }

    /// A master clip defining `media`'s file: its picture, then one mono
    /// track per audio channel, as Premiere lists a clip's sources.
    fn master_clip(&mut self, item: &ExportItem, side: MediaKind, media: &MediaUse) -> String {
        let id = format!("masterclip-{}", self.master_clips.len() + 1);
        self.master_clips
            .insert(media.resource_key.clone(), id.clone());
        let name = xml_text::escape(&crate::model::file_name(&media.url));
        let mut xml = format!(
            "          <clip id=\"{id}\">\n            <masterclipid>{id}</masterclipid>\n            <ismasterclip>TRUE</ismasterclip>\n            <name>{name}</name>\n            <duration>{}</duration>\n",
            media.duration
        );
        xml += &media.rate.xml("            ");
        xml += "            <media>\n";
        let mut tracks = Vec::new();
        if side == MediaKind::Video {
            tracks.push((MediaKind::Video, 0));
        }
        let channels = item.clip.audio.first().map_or(0, |a| a.channels);
        tracks.extend((1..=channels).map(|channel| (MediaKind::Audio, channel)));
        let mut current = None;
        for (index, (kind, channel)) in tracks.into_iter().enumerate() {
            if current != Some(kind) {
                if let Some(previous) = current {
                    xml += &format!("              </{previous}>\n");
                }
                xml += &format!("              <{kind}>\n");
                current = Some(kind);
            }
            let suffix = match kind {
                MediaKind::Video => "v".to_string(),
                MediaKind::Audio => format!("a{channel}"),
            };
            xml += &format!(
                "                <track>\n                  <clipitem id=\"{id}-{suffix}\">\n                    <masterclipid>{id}</masterclipid>\n                    <name>{name}</name>\n"
            );
            xml += &media.rate.xml("                    ");
            if index == 0 && self.defined_files.insert(media.resource_key.clone()) {
                let stereo = self.stereo_files.contains(&media.resource_key);
                xml += &file_block(item, side, media, stereo);
            } else {
                xml += &format!("                <file id=\"{}\"/>\n", media.file_id());
            }
            if kind == MediaKind::Audio {
                xml += &format!(
                    "                    <sourcetrack><mediatype>audio</mediatype><trackindex>{channel}</trackindex></sourcetrack>\n"
                );
            }
            xml += "                  </clipitem>\n                </track>\n";
        }
        if let Some(kind) = current {
            xml += &format!("              </{kind}>\n");
        }
        xml += "            </media>\n          </clip>\n";
        xml
    }
}

const BIN_CLOSE: &str = "        </children>\n      </bin>\n";

fn bin_open(name: &str) -> String {
    format!(
        "      <bin>\n        <name>{}</name>\n        <children>\n",
        xml_text::escape(name)
    )
}

/// A file's `<file>` element. A stereo file is one two-channel audio, as
/// Premiere writes a file it plays as a stereo clip.
fn file_block(item: &ExportItem, media_type: MediaKind, media: &MediaUse, stereo: bool) -> String {
    let id = media.file_id();
    let (media_url, duration, rate) = (&media.url, media.duration, media.rate);
    let clip = &item.clip;
    let file_name = media_url.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let absolute = crate::model::file_url(media_url);
    let mut xml = format!(
        "                <file id=\"{id}\">\n                  <name>{}</name>\n                  <pathurl>{}</pathurl>\n                  <duration>{duration}</duration>\n",
        xml_text::escape(file_name),
        xml_text::escape(&absolute),
    );
    xml += &rate.xml("                  ");
    let timecode = if media_type == MediaKind::Video {
        clip.video.as_ref().and_then(|v| v.source_timecode.clone())
    } else if clip.video.is_none() {
        // Resolve derives an audio file's start timecode from its BWF time
        // reference itself (at NTSC timeline rates it differs from the
        // sample-exact label) and then offsets `<in>` by the difference
        // between that and this label: seconds of slip that grow with the
        // timecode. Without the label `<in>` counts from the file start.
        // Premiere XML leaves it out too, because people import it into
        // Resolve as well; Premiere reads the BWF timecode itself.
        None
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
    } else if media_type == MediaKind::Video
        && let Some(video) = &clip.video
    {
        let fallback = crate::model::SourceTimecode {
            text: "00:00:00:00".to_string(),
            frame_number: 0,
            frame_duration: video
                .frame_duration
                .unwrap_or(crate::model::MediaTime::new(1, rate.timebase as i32)),
            drop_frame: false,
            derived: false,
        };
        let source_rate = SequenceRate::new(fallback.frame_duration);
        xml += &format!(
            "                  <timecode><string>{}</string><displayformat>NDF</displayformat>\n",
            fallback.text
        );
        xml += &source_rate.xml("                    ");
        xml += "                  </timecode>\n";
    }
    xml += "                  <media>";
    if media_type == MediaKind::Video
        && let Some(video) = &clip.video
    {
        xml += &format!(
            "<video><samplecharacteristics><width>{}</width><height>{}</height></samplecharacteristics></video>",
            video.width, video.height
        );
    }
    if let Some(audio) = clip.audio.first() {
        let depth = if media.is_prepared() {
            32
        } else {
            audio.bit_depth.unwrap_or(24)
        };
        let characteristics = format!(
            "<samplecharacteristics><depth>{depth}</depth><samplerate>{}</samplerate></samplecharacteristics>",
            audio.sample_rate.round() as i64,
        );
        if audio.channels > 1 && !stereo {
            // Premiere plays a clip's `<sourcetrack>` channel only when the
            // file lists that channel as its own mono audio, as Premiere
            // writes multichannel files itself. A single multichannel
            // description leaves every channel after the first silent, also
            // when Premiere opens the Resolve XML; Resolve reads either.
            for channel in 1..=audio.channels {
                xml += &format!(
                    "<audio>{characteristics}<channelcount>1</channelcount><audiochannel><sourcechannel>{channel}</sourcechannel></audiochannel></audio>"
                );
            }
        } else {
            xml += &format!(
                "<audio>{characteristics}<channelcount>{}</channelcount></audio>",
                audio.channels
            );
        }
    }
    xml += "</media>\n                </file>\n";
    xml
}

#[cfg(test)]
mod tests {
    use super::super::otio::tests::fixture_timeline;
    use super::*;

    /// Premiere and Resolve start XML sound on a frame: the nearest one.
    /// Premiere XML keeps the remainder after a frame below as
    /// `<subframeoffset>` for other readers.
    #[test]
    fn audio_starts_on_the_nearest_frame() {
        let at = |start: f64, format| {
            let mut timeline = fixture_timeline();
            let rec = timeline.islands[0]
                .clips
                .iter_mut()
                .find(|i| i.clip.id.0 == "rec")
                .expect("recorder item");
            rec.start = start;
            let xml = write(&timeline, format, PremiereOptions::default());
            let clip = &xml[xml.find("<name>rec.wav</name>").expect("recorder")..];
            clip[..clip.find("</clipitem>").unwrap()].to_string()
        };
        // 12.5 frames at 25 fps starts on frame 13.
        let premiere = at(0.5, TimelineExportFormat::PremiereXML);
        assert!(premiere.contains("<start>13</start>"), "{premiere}");
        assert!(!premiere.contains("subframeoffset"));
        // 2.1 frames starts on frame 2, a tenth of a frame later.
        let premiere = at(0.084, TimelineExportFormat::PremiereXML);
        assert!(premiere.contains("<start>2</start>"), "{premiere}");
        assert!(premiere.contains("<subframeoffset>8</subframeoffset>"));
        let resolve = at(0.084, TimelineExportFormat::ResolveXML);
        assert!(resolve.contains("<start>2</start>"), "{resolve}");
        assert!(!resolve.contains("subframeoffset"));
    }

    /// The source range moves with the start's rounding: a start 0.49 of a
    /// frame past frame 25 with a source in 0.51 past frame 12 plays within
    /// a fiftieth of a frame, where rounding each alone was a frame off.
    #[test]
    fn source_range_moves_with_the_start_rounding() {
        for format in [
            TimelineExportFormat::PremiereXML,
            TimelineExportFormat::ResolveXML,
        ] {
            let mut timeline = fixture_timeline();
            let rec = timeline.islands[0]
                .clips
                .iter_mut()
                .find(|i| i.clip.id.0 == "rec")
                .expect("recorder item");
            rec.start = 25.49 / 25.0;
            rec.source_in = 12.51 / 25.0;
            rec.source_out = rec.source_in + 2.0;
            rec.timeline_duration = 2.0;
            let xml = write(&timeline, format, PremiereOptions::default());
            let clip = &xml[xml.find("<name>rec.wav</name>").expect("recorder")..];
            let clip = &clip[..clip.find("</clipitem>").unwrap()];
            assert!(
                clip.contains("<start>25</start>\n                <end>75</end>\n                <in>12</in>\n                <out>62</out>"),
                "{format:?}: {clip}"
            );
            assert!(!clip.contains("subframeoffset"), "{clip}");
        }
    }

    /// Premiere reads a reverse clip's speed from the out keyframe's value
    /// over its time and froze on the source frame stored there, even in a
    /// graph it wrote itself; the value is the time at the clip's speed.
    #[test]
    fn premiere_reverse_graph_stores_its_speed_at_the_out_point() {
        let graph = "<filter><parameter><keyframe><when>0</when><value>376</value><speedkfin>TRUE</speedkfin></keyframe>\n<keyframe><when>37</when><value>326</value><speedvirtualkf>TRUE</speedvirtualkf><speedkfout>TRUE</speedkfout></keyframe></parameter></filter>";
        let rewritten = reverse_out_at_speed(graph, 2.0);
        assert!(
            rewritten.contains("<when>37</when><value>74</value><speedvirtualkf>"),
            "{rewritten}"
        );
        assert!(rewritten.contains("<when>0</when><value>376</value>"));
        assert_eq!(reverse_out_at_speed("<filter/>", 2.0), "<filter/>");
        let remap = |premiere| {
            TimeRemap {
                premiere,
                media_type: MediaKind::Video,
                graph_slope: 1.0,
                graph_in: 0,
                graph_out: 400,
                graph_duration: 400,
                media_in: 0.0,
                media_out: 400.0,
                media_duration: 400.0,
                plays_backward: true,
            }
            .xml()
        };
        assert!(remap(true).contains("<when>400</when><value>400</value>"));
        assert!(remap(false).contains("<when>400</when><value>1</value>"));
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
        let xml = write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions::default(),
        );
        assert!(!xml.contains("subframeoffset"));
        assert!(xml.contains("<start>232</start>"));
    }

    /// Resolve reads a trimmed WAV whose broadcast start falls past the
    /// middle of a frame a frame early, so its Resolve XML selects a frame
    /// on; up to the middle, and in Premiere, it reads the selected frame.
    #[test]
    fn resolve_reads_a_trimmed_wav_starting_past_mid_frame_a_frame_on() {
        let xml = |format, reference: u64| {
            let mut timeline = fixture_timeline();
            let rec = timeline.islands[0]
                .clips
                .iter_mut()
                .find(|i| i.clip.id.0 == "rec")
                .expect("recorder item");
            rec.source_in = 0.48;
            rec.source_out = 2.48;
            rec.timeline_duration = 2.0;
            rec.clip.audio[0].source_timecode = crate::model::SourceTimecode::from_samples(
                reference,
                48_000.0,
                crate::model::MediaTime::new(1, 25),
                false,
            );
            let xml = write(&timeline, format, PremiereOptions::default());
            let clip = &xml[xml.find("<name>rec.wav</name>").expect("recorder")..];
            clip[..clip.find("</clipitem>").unwrap()].to_string()
        };
        let selected = "<in>12</in>\n                <out>62</out>";
        let frame_on = "<in>13</in>\n                <out>63</out>";
        // Samples past a frame of 1920.
        for (phase, resolve) in [
            (0, selected),
            (200, selected),
            (960, selected),
            (979, frame_on),
            (1536, frame_on),
        ] {
            let reference = 48_000 * 10 + phase;
            let xml_resolve = xml(TimelineExportFormat::ResolveXML, reference);
            assert!(xml_resolve.contains(resolve), "{phase}: {xml_resolve}");
            let premiere = xml(TimelineExportFormat::PremiereXML, reference);
            assert!(premiere.contains(selected), "{phase}: {premiere}");
        }
    }

    /// Takes of one recorder share a track across sub-second seams; when
    /// rounding to frames ends the first inside the next, it ends where the
    /// next starts, or Resolve moves the next a frame later.
    #[test]
    fn a_clip_ends_where_the_next_on_its_track_starts() {
        let mut timeline = fixture_timeline();
        let rec = timeline.islands[0]
            .clips
            .iter_mut()
            .find(|i| i.clip.id.0 == "rec")
            .expect("recorder item");
        rec.start = 0.0;
        rec.source_out = 2.0;
        rec.timeline_duration = 2.0;
        let mut second = rec.clone();
        second.instance_id = "second take".into();
        // 49.5 frames: the second starts on frame 50 and the first, 50
        // frames long, would end there anyway; 49.4 starts on frame 49.
        second.start = 49.4 / 25.0;
        timeline.islands[0].clips.push(second);
        let xml = write(
            &timeline,
            TimelineExportFormat::ResolveXML,
            PremiereOptions::default(),
        );
        let first = &xml[xml.find("<name>rec.wav</name>").expect("first take")..];
        assert!(
            first.contains("<start>0</start>\n                <end>49</end>"),
            "{first}"
        );
        assert!(first.contains("<out>49</out>"), "{first}");
        assert!(
            xml.contains("<start>49</start>\n                <end>99</end>"),
            "{xml}"
        );
    }

    #[test]
    fn premiere_counts_other_rate_sources_in_sequence_frames() {
        // A 24 fps camera on the fixture's 25 fps sequence: Premiere reads
        // `<in>`/`<out>` as sequence frames, Resolve as the clip's own.
        let mut timeline = fixture_timeline();
        let cam = timeline.islands[0]
            .clips
            .iter_mut()
            .find(|i| i.clip.id.0 == "cam")
            .expect("camera item");
        cam.clip.video.as_mut().unwrap().frame_duration = Some(crate::model::MediaTime::new(1, 24));
        let clipitem = |xml: &str| {
            let start = xml.find("<clipitem").unwrap();
            let end = start + xml[start..].find("</clipitem>").unwrap();
            xml[start..end].to_string()
        };
        let premiere = clipitem(&write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions::default(),
        ));
        assert!(premiere.contains("<duration>250</duration>"));
        assert!(premiere.contains("<timebase>25</timebase>"));
        assert!(premiere.contains("<in>0</in>\n                <out>250</out>"));
        // The file keeps its own rate.
        assert!(premiere.contains("<rate><timebase>24</timebase>"));
        let resolve = clipitem(&write(
            &timeline,
            TimelineExportFormat::ResolveXML,
            PremiereOptions::default(),
        ));
        assert!(resolve.contains("<in>0</in>\n                <out>240</out>"));
    }

    #[test]
    fn audio_files_carry_no_timecode_label() {
        // Resolve offsets `<in>` by the gap between this label and the BWF
        // timecode it reads itself, also when it imports the Premiere XML.
        let mut timeline = fixture_timeline();
        let rec = timeline.islands[0]
            .clips
            .iter_mut()
            .find(|i| i.clip.id.0 == "rec")
            .expect("recorder item");
        rec.clip.audio[0].source_timecode = Some(crate::model::SourceTimecode {
            text: "01:26:57:00".into(),
            frame_number: 5_217 * 48_000,
            frame_duration: crate::model::MediaTime::new(1, 48_000),
            drop_frame: false,
            derived: false,
        });
        let resolve = write(
            &timeline,
            TimelineExportFormat::ResolveXML,
            PremiereOptions::default(),
        );
        assert!(!resolve.contains("01:26:57:00"));
        assert!(!resolve.contains("<timebase>48000</timebase>"));
        // Camera files keep their timecode.
        assert!(resolve.contains("<timecode><string>"));
        let premiere = write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions::default(),
        );
        assert!(!premiere.contains("01:26:57:00"));
        assert!(premiere.contains("<timecode><string>"));
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
            let xml = write(&timeline, format, PremiereOptions::default());
            assert!(xml.contains(
                "<sourcetrack><mediatype>audio</mediatype><trackindex>2</trackindex></sourcetrack>"
            ));
        }
    }

    /// Premiere plays channels after the first only when the file lists
    /// each channel as its own mono audio, whichever XML it opens; Resolve
    /// reads that as well.
    #[test]
    fn both_xmls_list_each_channel_of_a_file_as_mono_audio() {
        let mut timeline = fixture_timeline();
        for item in &mut timeline.islands[0].clips {
            if item.clip.video.is_none() {
                item.clip.audio[0].channels = 4;
            }
        }
        let media = |format| {
            let xml = write(&timeline, format, PremiereOptions::default());
            let file = &xml[xml.find("<name>rec.wav</name>").expect("recorder file")..];
            file[file.find("<media>").unwrap()..file.find("</media>").unwrap()].to_string()
        };
        for format in [
            TimelineExportFormat::PremiereXML,
            TimelineExportFormat::ResolveXML,
        ] {
            let described = media(format);
            assert_eq!(described.matches("<audio>").count(), 4, "{described}");
            assert_eq!(
                described.matches("<channelcount>1</channelcount>").count(),
                4
            );
            for channel in 1..=4 {
                assert!(described.contains(&format!("<sourcechannel>{channel}</sourcechannel>")));
            }
        }
    }

    /// A stereo recording is one stereo clip in Premiere, as Premiere writes
    /// it: its file is one two-channel audio, its channels play on a pair
    /// of tracks Premiere joins, and they link as one group. Resolve XML
    /// keeps a mono track per channel, which Resolve reads alike either way.
    #[test]
    fn premiere_plays_a_stereo_recording_as_one_stereo_clip() {
        let mut timeline = fixture_timeline();
        let rec = timeline.islands[0]
            .clips
            .iter_mut()
            .find(|i| i.clip.id.0 == "rec")
            .expect("recorder item");
        rec.clip.audio[0].channels = 2;
        let xml = write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions::default(),
        );
        assert!(xml.contains("<sequence id=\"sequence-1\" explodedTracks=\"true\">"));
        assert!(xml.contains("<numOutputChannels>2</numOutputChannels>"));
        let tracks: Vec<&str> = xml
            .split("\n            <track")
            .skip(1)
            .filter(|track| track.contains("<name>rec.wav</name>"))
            .collect();
        assert_eq!(tracks.len(), 2, "{xml}");
        for (index, track) in tracks.iter().enumerate() {
            assert!(track.starts_with(&format!(
                " premiereTrackType=\"Stereo\" currentExplodedTrackIndex=\"{index}\" totalExplodedTrackCount=\"2\">"
            )), "{track}");
            assert!(track.contains("premiereChannelType=\"stereo\""));
            assert!(track.contains(&format!(
                "<sourcetrack><mediatype>audio</mediatype><trackindex>{}</trackindex></sourcetrack>",
                index + 1
            )));
            assert!(track.contains(&format!(
                "<outputchannelindex>{}</outputchannelindex>",
                index + 1
            )));
            assert_eq!(
                track.matches("<groupindex>1</groupindex>").count(),
                2,
                "{track}"
            );
        }
        let left = tracks[0];
        let id = |track: &str| {
            let start = track.find("<clipitem id=\"").unwrap() + 14;
            track[start..start + track[start..].find('"').unwrap()].to_string()
        };
        for track in &tracks {
            assert!(left.contains(&format!("<linkclipref>{}</linkclipref>", id(track))));
        }
        let file = &left[left.find("<file id=").unwrap()..left.find("</file>").unwrap()];
        assert_eq!(file.matches("<audio>").count(), 1, "{file}");
        assert!(file.contains("<channelcount>2</channelcount>"), "{file}");

        let resolve = write(
            &timeline,
            TimelineExportFormat::ResolveXML,
            PremiereOptions::default(),
        );
        assert!(!resolve.contains("premiereTrackType") && !resolve.contains("explodedTracks"));

        // A single channel plays alone, on a mono track.
        let rec = timeline.islands[0]
            .clips
            .iter_mut()
            .find(|i| i.clip.id.0 == "rec")
            .expect("recorder item");
        rec.audio_source_channel = Some(1);
        let xml = write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions::default(),
        );
        assert!(!xml.contains("premiereTrackType"), "{xml}");
    }

    #[test]
    fn premiere_sequence_structure() {
        let mut timeline = fixture_timeline();
        timeline.islands[0].clips[0].fcp7_labels_xml =
            Some("<labels><label2>Mango</label2></labels>".to_string());
        let xml = write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions::default(),
        );
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
        const BINS: PremiereOptions = PremiereOptions {
            replaced_sequence: false,
            media_bins: true,
        };
        let first = write(&fixture_timeline(), TimelineExportFormat::PremiereXML, BINS);
        let mut second_timeline = fixture_timeline();
        second_timeline.name = "Second cut".into();
        let second = write(&second_timeline, TimelineExportFormat::PremiereXML, BINS);
        let documents = [
            ("First cut".to_string(), first),
            (second_timeline.name.clone(), second),
        ];
        let combined = combine_project_documents(&documents, true).expect("combined project");

        assert_eq!(combined.matches("<sequence id=").count(), 2);
        assert!(combined.contains("<sequence id=\"s1-sequence-1\">"));
        assert!(combined.contains("<sequence id=\"s2-sequence-1\">"));
        assert!(combined.contains("<linkclipref>s1-clipitem-"));
        assert!(combined.contains("<linkclipref>s2-clipitem-"));
        assert!(combined.contains("<masterclipid>s2-masterclip-"));
        // Each sequence keeps its camera bins inside a bin of its own name.
        assert!(combined.contains("<bin>\n        <name>First cut</name>"));
        assert!(combined.contains("<bin>\n        <name>Second cut</name>"));
        assert_references_resolve(&combined);
    }

    /// Every id is defined once, and every reference names a definition
    /// that precedes it, as Premiere resolves them while reading.
    fn assert_references_resolve(xml: &str) {
        let mut defined = HashSet::new();
        let mut rest = xml;
        while let Some(at) = rest.find(" id=\"") {
            let tag_start = rest[..at].rfind('<').expect("tag");
            let value_end = rest[at + 5..].find('"').expect("id") + at + 5;
            let id = &rest[at + 5..value_end];
            let tag = &rest[tag_start + 1..at];
            let reference = tag == "file" && rest[value_end..].starts_with("\"/>");
            if reference {
                assert!(
                    defined.contains(id),
                    "file {id} referenced before its definition"
                );
            } else {
                assert!(defined.insert(id.to_string()), "duplicate id {id}");
            }
            rest = &rest[value_end..];
        }
        for element in ["masterclipid", "linkclipref"] {
            for value in xml.split(&format!("<{element}>")).skip(1) {
                let id = &value[..value.find('<').expect("closing tag")];
                assert!(defined.contains(id), "{element} {id} is not defined");
            }
        }
    }

    /// A sequence without the bin-related details: master clip references
    /// and full file definitions, which bins move into master clips.
    fn sequence_timing(xml: &str) -> String {
        let sequence = &xml[xml.find("<sequence").expect("sequence")..];
        let mut out = String::new();
        let mut lines = sequence.lines();
        while let Some(line) = lines.next() {
            let trimmed = line.trim();
            if trimmed.starts_with("<masterclipid>") {
                continue;
            }
            if trimmed.starts_with("<file id=") && !trimmed.ends_with("/>") {
                out += &format!("{}/>\n", trimmed.trim_end_matches('>'));
                for inner in lines.by_ref() {
                    if inner.trim() == "</file>" {
                        break;
                    }
                }
                continue;
            }
            out += trimmed;
            out += "\n";
        }
        out
    }

    /// A camera file's sound edited on its own, listed before the camera,
    /// leaves the camera's master clip whole: picture, timecode, and sound.
    /// With a sound-only master both editors refused the XML.
    #[test]
    fn media_bins_describe_a_camera_file_with_its_picture() {
        let mut timeline = fixture_timeline();
        let camera = timeline.islands[0]
            .clips
            .iter()
            .find(|i| i.clip.video.is_some())
            .expect("camera item")
            .clone();
        let mut sound = camera.clone();
        sound.instance_id = "camera sound".into();
        sound.clip.video = None;
        timeline.islands[0].clips.insert(0, sound);
        let xml = write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions {
                media_bins: true,
                ..PremiereOptions::default()
            },
        );
        let bin = &xml[..xml.find("<sequence").expect("sequence")];
        let file = format!("<file id=\"file-{}\">", camera.clip.id.0);
        let definition = &bin[bin.find(&file).expect("camera file in its bin")..];
        let definition = &definition[..definition.find("</file>").unwrap()];
        assert!(
            definition.contains("<video><samplecharacteristics>"),
            "{definition}"
        );
        assert!(definition.contains("<timecode>"), "{definition}");
        assert!(bin.contains("-v\">"), "{bin}");
    }

    #[test]
    fn media_bins_define_every_file_before_the_sequences() {
        let mut timeline = fixture_timeline();
        let rec = timeline.islands[0]
            .clips
            .iter_mut()
            .find(|i| i.clip.id.0 == "rec")
            .expect("recorder item");
        rec.start = 2.9 / 25.0;
        *rec = rec.with_corrected_audio(PathBuf::from("/fix/rec-drift.wav"));
        let options = PremiereOptions {
            replaced_sequence: true,
            media_bins: true,
        };
        let xml = write(&timeline, TimelineExportFormat::PremiereXML, options);
        let sequences = xml.find("<sequence").expect("sequence");
        let bins: Vec<&str> = xml[..sequences]
            .split("<bin>\n        <name>")
            .skip(1)
            .map(|bin| &bin[..bin.find('<').expect("name")])
            .collect();
        // One folder holding a camera and a recorder: a bin each, then the
        // drift correction the synchronized sequence plays.
        assert_eq!(bins, ["v – Video", "v – Audio", "Align generated audio"]);
        for path in ["/v/cam.mov", "/v/rec.wav", "/fix/rec-drift.wav"] {
            let pathurl = format!("<pathurl>file://{path}</pathurl>");
            assert_eq!(xml.matches(&pathurl).count(), 1, "{path}");
            assert!(xml.find(&pathurl).expect("defined") < sequences, "{path}");
        }
        assert!(xml[sequences..].contains("<masterclipid>masterclip-"));
        assert_references_resolve(&xml);

        // Bins leave every sequence's placement and references unchanged.
        let plain = write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions {
                media_bins: false,
                ..options
            },
        );
        assert!(!plain.contains("<bin>") && !plain.contains("masterclipid"));
        assert_eq!(sequence_timing(&xml), sequence_timing(&plain));
        assert_references_resolve(&plain);

        // A Resolve XML sequence has no project to hold bins.
        let resolve = write(&timeline, TimelineExportFormat::ResolveXML, options);
        assert!(!resolve.contains("<bin>") && !resolve.contains("masterclipid"));
    }

    #[test]
    fn replaced_sequence_appears_on_request() {
        let timeline = fixture_timeline();
        let xml = write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions {
                replaced_sequence: true,
                ..Default::default()
            },
        );
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
            preserve_editing_tracks: Default::default(),
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
        let xml = write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions::default(),
        );
        assert!(xml.contains("<labels><label2>Grape</label2></labels>"));
        assert!(xml.contains("<labels><label2>Forest</label2></labels>"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_bootstrap_has_no_project_wrapper() {
        let timeline = fixture_timeline();
        let xml = write(
            &timeline,
            TimelineExportFormat::ResolveXML,
            PremiereOptions::default(),
        );
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
            preserve_editing_tracks: Default::default(),
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
        let xml = write(
            &timeline,
            TimelineExportFormat::PremiereXML,
            PremiereOptions::default(),
        );
        // Each side passes through exactly once: no leaks across the link.
        assert_eq!(xml.matches("VideoSideEffect").count(), 1);
        assert_eq!(xml.matches("AudioSideEffect").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
