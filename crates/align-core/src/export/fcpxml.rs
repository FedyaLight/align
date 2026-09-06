//! Final Cut Pro FCPXML writer. Port of `FinalCutProXMLWriter.swift`:
//! resources (sequence format + per-media assets), a multicam resource
//! grouping angles by source, a synced multi-track project and a multicam
//! project. Times are reduced `MediaTime` rationals (`0s`, `100/25s`).

use std::collections::HashMap;

use super::model::{ExportIsland, ExportItem, ExportTimeline, xml_text};
use crate::allocator::{TimelineTrackRequest, allocate, source_key_for_url};
use crate::model::MediaTime;

/// A single selected source channel remains mono even when its asset is stereo.
fn audio_channel_end(item: &ExportItem) -> String {
    match item.selected_audio_source_channel() {
        Some(channel) => format!(
            "><audio-channel-source srcCh=\"{}\" role=\"{}\"/></asset-clip>",
            channel + 1,
            xml_text::escape(item.fcpxml_audio_role().unwrap_or("dialogue")),
        ),
        None => "/>".into(),
    }
}

// Keep frame-aligned values exact instead of rounding them below a frame
// boundary at fractional rates; retain subframe audio positions otherwise.
fn timeline_time(seconds: f64, frame: MediaTime) -> String {
    let frames = (seconds / frame.as_seconds()).round();
    let exact = frames * frame.as_seconds();
    let time = if (seconds - exact).abs() <= 0.000001 {
        MediaTime::new(frames as i64 * frame.value, frame.timescale)
    } else {
        MediaTime::microseconds(seconds)
    };
    xml_text::fcpxml_time(time)
}

pub fn write(timeline: &ExportTimeline, include_multicam_clip: bool) -> String {
    write_with_storylines(timeline, include_multicam_clip, false)
}

pub fn write_with_storylines(
    timeline: &ExportTimeline,
    include_multicam_clip: bool,
    group_storylines: bool,
) -> String {
    let island = timeline.islands.first().cloned().unwrap_or(ExportIsland {
        id: 0,
        clips: Vec::new(),
        duration: 0.0,
    });
    write_island(timeline, &island, include_multicam_clip, group_storylines)
}

fn write_island(
    timeline: &ExportTimeline,
    island: &ExportIsland,
    include_multicam: bool,
    group_storylines: bool,
) -> String {
    let frame_duration = timeline.frame_duration;
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
    let format_id = "r_fmt_0";

    let mut xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n".to_string();
    xml += "<!DOCTYPE fcpxml>\n<fcpxml version=\"1.10\">\n  <resources>\n";
    xml += &format!(
        "    <format id=\"{format_id}\" name=\"FFVideoFormat{width}x{height}p\" frameDuration=\"{}\" width=\"{width}\" height=\"{height}\"/>\n",
        xml_text::fcpxml_time(frame_duration)
    );

    #[derive(Hash, PartialEq, Eq, Clone)]
    struct AssetDescriptor {
        url: String,
        audio_only: bool,
    }
    let mut asset_map: HashMap<AssetDescriptor, String> = HashMap::new();
    let mut asset_index = 1;
    for item in &island.clips {
        let media_url = item
            .corrected_audio_url
            .clone()
            .unwrap_or_else(|| item.clip.url.clone());
        let desc = AssetDescriptor {
            url: media_url.to_string_lossy().into_owned(),
            audio_only: item.clip.video.is_none(),
        };
        if asset_map.contains_key(&desc) {
            continue;
        }
        let asset_id = format!("r_asset_{asset_index}");
        asset_index += 1;
        let duration =
            xml_text::fcpxml_time(MediaTime::microseconds(item.clip.duration.as_seconds()));
        let start = item
            .clip
            .source_timecode()
            .map(|tc| {
                let frames = (tc.as_seconds() * frame_duration.timescale as f64
                    / frame_duration.value as f64)
                    .round() as i64
                    * frame_duration.value;
                MediaTime::new(frames, frame_duration.timescale)
            })
            .map(xml_text::fcpxml_time)
            .unwrap_or_else(|| xml_text::fcpxml_time(MediaTime::new(0, frame_duration.timescale)));
        let name = xml_text::escape(media_url.file_name().and_then(|n| n.to_str()).unwrap_or(""));
        let src = xml_text::escape(&format!("file://{}", media_url.to_string_lossy()));
        if desc.audio_only || item.clip.video.is_none() {
            let channels = item.clip.audio.first().map_or(2, |a| a.channels);
            let rate = item
                .clip
                .audio
                .first()
                .map_or(48000, |a| a.sample_rate.round() as i64);
            xml += &format!(
                "    <asset id=\"{asset_id}\" name=\"{name}\" start=\"{start}\" duration=\"{duration}\" hasAudio=\"1\" audioSources=\"1\" audioChannels=\"{channels}\" audioRate=\"{rate}\">\n"
            );
        } else {
            let channels = item.clip.audio.first().map_or(2, |a| a.channels);
            let rate = item
                .clip
                .audio
                .first()
                .map_or(48000, |a| a.sample_rate.round() as i64);
            let has_audio = !item.clip.audio.is_empty();
            xml += &format!(
                "    <asset id=\"{asset_id}\" name=\"{name}\" start=\"{start}\" duration=\"{duration}\" format=\"{format_id}\" hasVideo=\"1\" hasAudio=\"{}\"",
                if has_audio { 1 } else { 0 }
            );
            if has_audio {
                xml += &format!(
                    " audioSources=\"1\" audioChannels=\"{channels}\" audioRate=\"{rate}\""
                );
            }
            xml += ">\n";
        }
        xml += &format!("      <media-rep kind=\"original-media\" src=\"{src}\"/>\n    </asset>\n");
        asset_map.insert(desc, asset_id);
    }

    let multicam_id = "r_multicam";
    if include_multicam {
        let mut angle_groups: HashMap<String, Vec<&ExportItem>> = HashMap::new();
        for item in &island.clips {
            angle_groups
                .entry(
                    item.preferred_source_key
                        .clone()
                        .unwrap_or_else(|| source_key_for_url(&item.clip.url)),
                )
                .or_default()
                .push(item);
        }
        let mut keys: Vec<String> = angle_groups.keys().cloned().collect();
        keys.sort_by(|a, b| {
            let ga_video = angle_groups[a].iter().any(|i| i.clip.video.is_some());
            let gb_video = angle_groups[b].iter().any(|i| i.clip.video.is_some());
            if ga_video != gb_video {
                return gb_video.cmp(&ga_video);
            }
            a.cmp(b)
        });
        xml += &format!(
            "    <media id=\"{multicam_id}\" name=\"{} – Multicam\">\n      <multicam format=\"{format_id}\">\n",
            xml_text::escape(&timeline.name)
        );
        for (angle_index, key) in keys.iter().enumerate() {
            let mut items = angle_groups[key].clone();
            items.sort_by(|a, b| a.start.total_cmp(&b.start));
            let angle_name = xml_text::escape(&angle_name(key, &items));
            xml += &format!(
                "        <mc-angle name=\"{angle_name}\" angleID=\"a{}\">\n",
                angle_index + 1
            );
            for item in items {
                let media_url = item
                    .corrected_audio_url
                    .clone()
                    .unwrap_or_else(|| item.clip.url.clone());
                let desc = AssetDescriptor {
                    url: media_url.to_string_lossy().into_owned(),
                    audio_only: item.clip.video.is_none(),
                };
                let Some(asset_id) = asset_map.get(&desc) else {
                    continue;
                };
                let clip_name = xml_text::escape(&item.display_name.clone().unwrap_or_else(|| {
                    media_url
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string()
                }));
                let audio_role = item.fcpxml_audio_role().map_or_else(String::new, |role| {
                    format!(" audioRole=\"{}\"", xml_text::escape(role))
                });
                let audio_channel_end = audio_channel_end(item);
                xml += &format!(
                    "          <asset-clip name=\"{clip_name}\" ref=\"{asset_id}\" offset=\"{}\" duration=\"{}\" start=\"{}\" enabled=\"{}\"{audio_role}{audio_channel_end}\n",
                    timeline_time(item.start, frame_duration),
                    timeline_time(item.timeline_duration, frame_duration),
                    timeline_time(item.source_in, frame_duration),
                    u8::from(item.is_enabled(if item.clip.video.is_some() {
                        "video"
                    } else {
                        "audio"
                    })),
                );
            }
            xml += "        </mc-angle>\n";
        }
        xml += "      </multicam>\n    </media>\n";
    }
    xml += "  </resources>\n";

    let total_duration =
        xml_text::fcpxml_time(MediaTime::microseconds(1.0f64.max(island.duration)));
    let sequence_start = island
        .clips
        .iter()
        .filter_map(|i| i.clip.source_timecode())
        .next()
        .map(|tc| {
            let frames = (tc.as_seconds() * frame_duration.timescale as f64
                / frame_duration.value as f64)
                .round() as i64
                * frame_duration.value;
            xml_text::fcpxml_time(MediaTime::new(frames, frame_duration.timescale))
        })
        .unwrap_or_else(|| xml_text::fcpxml_time(MediaTime::new(0, frame_duration.timescale)));

    xml += "  <library>\n";
    xml += &format!(
        "    <event name=\"{}\">\n",
        xml_text::escape(&timeline.name)
    );
    xml += &format!(
        "      <project name=\"{} – synced\">\n",
        xml_text::escape(&timeline.name)
    );
    xml += &format!(
        "        <sequence format=\"{format_id}\" duration=\"{total_duration}\" tcStart=\"{sequence_start}\" tcFormat=\"NDF\">\n          <spine>\n            <gap offset=\"0s\" duration=\"{total_duration}\" name=\"Master\">\n"
    );

    let video_items: Vec<&ExportItem> = island
        .clips
        .iter()
        .filter(|i| i.clip.video.is_some())
        .collect();
    let audio_items: Vec<&ExportItem> = island
        .clips
        .iter()
        .filter(|i| !i.clip.audio.is_empty())
        .collect();
    let video_assignments = allocate(
        &video_items
            .iter()
            .map(|item| {
                TimelineTrackRequest::new(
                    item.instance_id.clone(),
                    item.preferred_source_key
                        .clone()
                        .unwrap_or_else(|| source_key_for_url(&item.clip.url)),
                    item.start,
                    item.timeline_duration,
                )
            })
            .collect::<Vec<_>>(),
    );
    let audio_assignments = allocate(
        &audio_items
            .iter()
            .map(|item| {
                TimelineTrackRequest::new(
                    item.instance_id.clone(),
                    item.preferred_audio_source_key
                        .clone()
                        .or(item.preferred_source_key.clone())
                        .unwrap_or_else(|| source_key_for_url(&item.clip.url)),
                    item.start,
                    if item.corrected_audio_url.is_none() {
                        item.timeline_duration
                    } else {
                        item.corrected_selected_duration()
                    },
                )
            })
            .collect::<Vec<_>>(),
    );

    let mut stories = std::collections::BTreeMap::<i64, String>::new();
    for item in video_items {
        let media_url = item
            .corrected_audio_url
            .clone()
            .unwrap_or_else(|| item.clip.url.clone());
        let desc = AssetDescriptor {
            url: media_url.to_string_lossy().into_owned(),
            audio_only: false,
        };
        let Some(asset_id) = asset_map.get(&desc) else {
            continue;
        };
        let lane = video_assignments
            .get(&item.instance_id)
            .copied()
            .unwrap_or(0)
            + 1;
        let clip_name = xml_text::escape(&item.display_name.clone().unwrap_or_else(|| {
            media_url
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string()
        }));
        let target = if group_storylines {
            stories.entry(lane as i64).or_default()
        } else {
            &mut xml
        };
        let lane_attribute = if group_storylines {
            String::new()
        } else {
            format!(" lane=\"{lane}\"")
        };
        *target += &format!(
            "              <asset-clip ref=\"{asset_id}\" {lane_attribute} offset=\"{}\" duration=\"{}\" start=\"{}\" name=\"{clip_name}\" srcEnable=\"video\" enabled=\"{}\"/>\n",
            timeline_time(item.start, frame_duration),
            timeline_time(item.timeline_duration, frame_duration),
            timeline_time(item.source_in, frame_duration),
            u8::from(item.is_enabled("video")),
        );
    }
    for item in audio_items {
        let media_url = item
            .corrected_audio_url
            .clone()
            .unwrap_or_else(|| item.clip.url.clone());
        let desc = AssetDescriptor {
            url: media_url.to_string_lossy().into_owned(),
            audio_only: item.clip.video.is_none(),
        };
        let Some(asset_id) = asset_map.get(&desc) else {
            continue;
        };
        let lane = -((audio_assignments
            .get(&item.instance_id)
            .copied()
            .unwrap_or(0)
            + 1) as i64);
        let clip_name = xml_text::escape(&item.display_name.clone().unwrap_or_else(|| {
            media_url
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string()
        }));
        let duration = if item.corrected_audio_url.is_none() {
            item.timeline_duration
        } else {
            item.corrected_selected_duration()
        };
        let audio_role = item.fcpxml_audio_role().map_or_else(String::new, |role| {
            format!(" audioRole=\"{}\"", xml_text::escape(role))
        });
        let audio_channel_end = audio_channel_end(item);
        let target = if group_storylines {
            stories.entry(lane).or_default()
        } else {
            &mut xml
        };
        let lane_attribute = if group_storylines {
            String::new()
        } else {
            format!(" lane=\"{lane}\"")
        };
        *target += &format!(
            "              <asset-clip ref=\"{asset_id}\" {lane_attribute} offset=\"{}\" duration=\"{}\" start=\"{}\" name=\"{clip_name}\" srcEnable=\"audio\" enabled=\"{}\"{audio_role}{audio_channel_end}\n",
            timeline_time(item.start, frame_duration),
            timeline_time(duration, frame_duration),
            timeline_time(item.source_in, frame_duration),
            u8::from(item.is_enabled("audio")),
        );
    }
    for (lane, clips) in stories {
        xml += &format!(
            "              <spine lane=\"{lane}\" offset=\"0s\">\n{clips}              </spine>\n"
        );
    }
    xml += "            </gap>\n          </spine>\n        </sequence>\n      </project>\n";

    if include_multicam {
        xml += &format!(
            "      <project name=\"{} – multicam\">\n",
            xml_text::escape(&timeline.name)
        );
        xml += &format!(
            "        <sequence format=\"{format_id}\" duration=\"{total_duration}\" tcStart=\"{sequence_start}\" tcFormat=\"NDF\">\n          <spine>\n            <mc-clip name=\"{} – Multicam\" ref=\"{multicam_id}\" offset=\"0s\" duration=\"{total_duration}\" tcStart=\"{sequence_start}\" format=\"{format_id}\"/>\n          </spine>\n        </sequence>\n      </project>\n",
            xml_text::escape(&timeline.name)
        );
    }
    xml += "    </event>\n  </library>\n</fcpxml>\n";
    xml
}

fn angle_name(source_key: &str, items: &[&ExportItem]) -> String {
    if let Some(first) = items.first() {
        let folder = first
            .clip
            .url
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if !folder.is_empty() && folder != "/" && folder != "." {
            return folder.to_string();
        }
        return first
            .clip
            .url
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or(source_key)
            .to_string();
    }
    source_key.to_string()
}

#[cfg(test)]
mod tests {
    use super::super::otio::tests::fixture_timeline;
    use super::*;
    use crate::xml::{read_timeline, timeline_sequence_summaries};

    #[test]
    fn fractional_frame_boundary_is_not_rounded_down() {
        let frame = MediaTime::new(1001, 30000);
        assert_eq!(timeline_time(50.0 * frame.as_seconds(), frame), "1001/600s");
        assert_eq!(timeline_time(0.012345, frame), "2469/200000s");
    }

    #[test]
    fn storylines_preserve_track_and_source_ranges() {
        let timeline = fixture_timeline();
        let directory =
            std::env::temp_dir().join(format!("align-storyline-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let plain = directory.join("plain.fcpxml");
        let grouped = directory.join("grouped.fcpxml");
        std::fs::write(&plain, write(&timeline, false)).unwrap();
        let xml = write_with_storylines(&timeline, false, true);
        assert!(xml.contains("<spine lane="));
        std::fs::write(&grouped, xml).unwrap();
        let plain = read_timeline(&plain, None).unwrap();
        let grouped = read_timeline(&grouped, None).unwrap();
        let signature = |draft: crate::xml::TimelineDraft| {
            let mut rows = draft
                .edits
                .into_iter()
                .map(|edit| {
                    format!(
                        "{} {:?} {} {} {} {} {} {:?}",
                        edit.url.display(),
                        edit.media_type,
                        edit.track_index,
                        edit.timeline_start,
                        edit.timeline_end,
                        edit.source_in,
                        edit.source_out,
                        edit.audio_source_channel
                    )
                })
                .collect::<Vec<_>>();
            rows.sort();
            rows
        };
        assert_eq!(signature(plain), signature(grouped));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn fcpxml_roundtrips_through_importer() {
        let timeline = fixture_timeline();
        let xml = write(&timeline, true);
        assert!(xml.contains("<fcpxml version=\"1.10\">"));
        assert!(xml.contains("r_multicam"));
        assert!(xml.contains("– synced"));
        assert!(xml.contains("srcEnable=\"video\""));
        assert!(xml.contains("srcEnable=\"audio\""));
        // The output re-parses as a timeline (self-hosting check).
        let dir = std::env::temp_dir().join(format!("align-fcpxml-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.fcpxml");
        std::fs::write(&path, &xml).unwrap();
        let summaries = timeline_sequence_summaries(&path).expect("summaries");
        assert_eq!(summaries.len(), 2);
        let draft = read_timeline(&path, Some(0)).expect("read");
        assert_eq!(draft.edits.len(), 3);
        assert_eq!(
            draft
                .edits
                .iter()
                .filter(|edit| edit.media_type == crate::xml::DraftMediaKind::Video)
                .count(),
            1
        );
        assert_eq!(
            draft
                .edits
                .iter()
                .filter(|edit| edit.media_type == crate::xml::DraftMediaKind::Audio)
                .count(),
            2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fcpxml_preserves_disabled_clip_state() {
        let mut timeline = fixture_timeline();
        timeline.islands[0].clips[0].enabled = false;
        let xml = write(&timeline, false);
        assert!(xml.contains("enabled=\"0\""));
    }

    #[test]
    fn fcpxml_preserves_audio_role() {
        let mut timeline = fixture_timeline();
        timeline.islands[0].clips[0].fcpxml_audio_role = Some("dialogue.interview".to_string());
        let xml = write(&timeline, false);
        assert!(xml.contains("audioRole=\"dialogue.interview\""));
    }

    #[test]
    fn fcpxml_audio_role_import_to_export_round_trip() {
        use crate::model::{
            AudioSummary, Clip, ClipId, ClipPlacement, MappingPoint, MediaKind, MediaTime,
            SyncIsland, SyncProject, SyncResult, VideoSummary,
        };
        use std::path::PathBuf;

        let dir = std::env::temp_dir().join(format!("align-fcpxml-role-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roles.fcpxml");
        std::fs::write(
            &path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<fcpxml version="1.10">
<resources>
<format id="r1" frameDuration="1/25s"/>
<asset id="a1" hasVideo="1" hasAudio="1"><media-rep src="file:///tmp/role-cam.mov"/></asset>
<asset id="a2" hasVideo="0" hasAudio="1"><media-rep src="file:///tmp/role-rec.wav"/></asset>
</resources>
<library><event><project name="Proj">
<sequence format="r1" name="Cut">
<spine>
<asset-clip ref="a1" name="Cam" offset="0s" duration="100/25s" start="0s" lane="1" audioRole="dialogue"/>
<asset-clip ref="a2" name="Rec" offset="0s" duration="100/25s" start="0s" lane="-1" audioRole="effects"/>
</spine>
</sequence>
</project></event></library>
</fcpxml>"#,
        )
        .unwrap();
        let draft = read_timeline(&path, None).expect("read");
        let cam = Clip {
            id: ClipId::new("cam"),
            url: PathBuf::from("/tmp/role-cam.mov"),
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
            url: PathBuf::from("/tmp/role-rec.wav"),
            kind: MediaKind::Audio,
            duration: MediaTime::seconds(10.0),
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
        let imported = draft.resolve(&[cam.clone(), rec.clone()]);
        let cam_edit = imported
            .edits
            .iter()
            .find(|e| e.clip_id == cam.id)
            .expect("cam edit");
        assert_eq!(
            cam_edit
                .linked_audio_edit
                .as_ref()
                .and_then(|a| a.fcpxml_audio_role.as_deref()),
            Some("dialogue")
        );
        let rec_edit = imported
            .edits
            .iter()
            .find(|e| e.clip_id == rec.id)
            .expect("rec edit");
        assert_eq!(rec_edit.fcpxml_audio_role.as_deref(), Some("effects"));

        let placement = |id: &str| ClipPlacement {
            clip_id: ClipId::new(id),
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
        };
        let result = SyncResult {
            search_overrides: Default::default(),
            stopped: false,
            stages: Vec::new(),
            selected_stage: None,
            search_accuracy: Default::default(),
            project: SyncProject {
                clips: vec![cam, rec],
                warnings: Vec::new(),
                imported_timeline: Some(imported),
            },
            islands: vec![SyncIsland {
                id: 0,
                placements: vec![placement("cam"), placement("rec")],
            }],
            unmatched: Vec::new(),
            matches: Vec::new(),
            temporal_policy: crate::model::TemporalPolicy::default(),
        };
        let timeline = ExportTimeline::from_result_with_options(&result, Default::default())
            .expect("timeline");
        let xml = write(&timeline, false);
        assert!(xml.contains("audioRole=\"dialogue\""));
        assert!(xml.contains("audioRole=\"effects\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_template_intact() {
        let script = super::super::script::write();
        assert!(script.starts_with("#!/usr/bin/env python3"));
        assert!(script.contains("AppendToTimeline"));
        assert!(script.contains("sample-accurate recorder clips"));
    }
}
