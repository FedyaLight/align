//! Final Cut Pro FCPXML writer: resources (sequence format + per-media assets), a multicam resource
//! grouping angles by source, a synced multi-track project and a multicam
//! project. Times are reduced `MediaTime` rationals (`0s`, `100/25s`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::model::{ExportIsland, ExportItem, ExportTimeline, xml_text};
use crate::allocator::{TimelineTrackRequest, allocate, source_key_for_url};
use crate::model::{MediaKind, MediaTime, TIME_TOLERANCE};

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
    let time = if (seconds - exact).abs() <= TIME_TOLERANCE {
        MediaTime::new(frames as i64 * frame.value, frame.timescale)
    } else {
        MediaTime::microseconds(seconds)
    };
    xml_text::fcpxml_time(time)
}

/// What a Final Cut Pro document contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FcpxmlOptions {
    /// The synchronized project timeline.
    pub timeline: bool,
    /// A multicam clip of the synchronized angles.
    pub multicam: bool,
    /// Group each camera's connected clips into one storyline.
    pub group_storylines: bool,
}

impl FcpxmlOptions {
    /// The project timeline alone.
    pub const TIMELINE: Self = Self {
        timeline: true,
        multicam: false,
        group_storylines: false,
    };
}

impl Default for FcpxmlOptions {
    fn default() -> Self {
        Self {
            multicam: true,
            ..Self::TIMELINE
        }
    }
}

pub fn write(timeline: &ExportTimeline, options: FcpxmlOptions) -> String {
    let island = timeline.islands.first().cloned().unwrap_or(ExportIsland {
        id: 0,
        clips: Vec::new(),
        duration: 0.0,
    });
    write_island(timeline, &island, options)
}

/// Combine self-contained FCPXML documents into one library. Each source
/// keeps its own event while resource identifiers are scoped per sequence.
pub fn combine_documents(documents: &[String]) -> Result<String, String> {
    if documents.is_empty() {
        return Err("no FCPXML documents to combine".into());
    }
    let mut resources = String::new();
    let mut events = String::new();
    for (index, document) in documents.iter().enumerate() {
        let prefix = format!("s{}-", index + 1);
        resources += &prefix_references(
            document_section(document, "  <resources>\n", "  </resources>")?,
            &prefix,
        );
        events += &prefix_references(
            document_section(document, "  <library>\n", "  </library>")?,
            &prefix,
        );
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE fcpxml>\n<fcpxml version=\"1.10\">\n  <resources>\n{resources}  </resources>\n  <library>\n{events}  </library>\n</fcpxml>\n"
    ))
}

fn document_section<'a>(document: &'a str, open: &str, close: &str) -> Result<&'a str, String> {
    let start = document
        .find(open)
        .map(|offset| offset + open.len())
        .ok_or_else(|| format!("FCPXML document has no {open}"))?;
    let end = document[start..]
        .find(close)
        .map(|offset| start + offset)
        .ok_or_else(|| format!("FCPXML document has no {close}"))?;
    Ok(&document[start..end])
}

fn prefix_references(fragment: &str, prefix: &str) -> String {
    [" id=\"", " ref=\"", " format=\"", " angleID=\""]
        .into_iter()
        .fold(fragment.to_string(), |text, attribute| {
            text.replace(attribute, &format!("{attribute}{prefix}"))
        })
}

const FORMAT_ID: &str = "r_fmt_0";
const MULTICAM_ID: &str = "r_multicam";

/// One FCPXML asset per media file and side: a camera file's video asset
/// and an audio-only item of the same file are distinct resources.
#[derive(Hash, PartialEq, Eq, Clone)]
struct AssetKey {
    url: String,
    audio_only: bool,
}

/// Resources shared by the projects of one document.
struct Library<'a> {
    timeline: &'a ExportTimeline,
    frame: MediaTime,
    assets: HashMap<AssetKey, String>,
}

/// The file an item plays: its drift sidecar, else its source.
fn media_url(item: &ExportItem) -> PathBuf {
    item.corrected_audio_url
        .clone()
        .unwrap_or_else(|| item.clip.url.clone())
}

fn clip_name(item: &ExportItem, media_url: &Path) -> String {
    xml_text::escape(&item.display_name.clone().unwrap_or_else(|| {
        media_url
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    }))
}

fn audio_role_attribute(item: &ExportItem) -> String {
    item.fcpxml_audio_role().map_or_else(String::new, |role| {
        format!(" audioRole=\"{}\"", xml_text::escape(role))
    })
}

/// A source timecode as a start time on the frame grid; zero without one.
fn frame_start(timecode: Option<&crate::model::SourceTimecode>, frame: MediaTime) -> String {
    let frames = timecode.map_or(0, |tc| {
        (tc.as_seconds() * frame.timescale as f64 / frame.value as f64).round() as i64 * frame.value
    });
    xml_text::fcpxml_time(MediaTime::new(frames, frame.timescale))
}

/// Prepared precision stems contain exactly the selected source range, so
/// recorder items reference their stem from its start.
fn with_precision_stems(island: &ExportIsland) -> ExportIsland {
    let mut prepared = island.clone();
    for item in &mut prepared.clips {
        if item.clip.video.is_none()
            && let Some(channel) = item.selected_audio_source_channel()
            && let Some(path) = item.precision_audio_url(channel + 1).cloned()
        {
            let duration = item.corrected_selected_duration();
            item.clip.url = path;
            item.clip.duration = MediaTime::microseconds(duration);
            for audio in &mut item.clip.audio {
                audio.channels = 1;
            }
            item.corrected_audio_url = None;
            item.source_in = 0.0;
            item.source_out = duration;
            item.audio_source_channel = Some(0);
            item.precision_audio_urls.clear();
        }
    }
    prepared
}

fn write_island(
    timeline: &ExportTimeline,
    island: &ExportIsland,
    options: FcpxmlOptions,
) -> String {
    let island = &with_precision_stems(island);
    let frame = timeline.frame_duration;
    let dimension = |side: fn(&crate::model::VideoSummary) -> u32, fallback: u32| {
        island
            .clips
            .iter()
            .find_map(|i| i.clip.video.as_ref().map(side))
            .unwrap_or(fallback)
    };
    let (width, height) = (dimension(|v| v.width, 1920), dimension(|v| v.height, 1080));

    let mut xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n".to_string();
    xml += "<!DOCTYPE fcpxml>\n<fcpxml version=\"1.10\">\n  <resources>\n";
    xml += &format!(
        "    <format id=\"{FORMAT_ID}\" name=\"FFVideoFormat{width}x{height}p\" frameDuration=\"{}\" width=\"{width}\" height=\"{height}\"/>\n",
        xml_text::fcpxml_time(frame)
    );
    let mut library = Library {
        timeline,
        frame,
        assets: HashMap::new(),
    };
    xml += &library.assets_xml(island);
    if options.multicam {
        xml += &library.multicam_xml(island);
    }
    xml += "  </resources>\n";

    let total_duration =
        xml_text::fcpxml_time(MediaTime::microseconds(1.0f64.max(island.duration)));
    let sequence_start = frame_start(
        island.clips.iter().find_map(|i| i.clip.source_timecode()),
        frame,
    );
    let sequence = format!(
        "        <sequence format=\"{FORMAT_ID}\" duration=\"{total_duration}\" tcStart=\"{sequence_start}\" tcFormat=\"NDF\">\n"
    );
    let name = xml_text::escape(&timeline.name);
    xml += "  <library>\n";
    xml += &format!("    <event name=\"{name}\">\n");
    if options.timeline {
        xml += &format!("      <project name=\"{name} – synced\">\n{sequence}");
        xml += &format!(
            "          <spine>\n            <gap offset=\"0s\" duration=\"{total_duration}\" name=\"Master\">\n"
        );
        xml += &library.synced_clips_xml(island, options.group_storylines);
        xml += "            </gap>\n          </spine>\n        </sequence>\n      </project>\n";
    }
    if options.multicam {
        xml += &format!("      <project name=\"{name} – multicam\">\n{sequence}");
        xml += &format!(
            "          <spine>\n            <mc-clip name=\"{name} – Multicam\" ref=\"{MULTICAM_ID}\" offset=\"0s\" duration=\"{total_duration}\" tcStart=\"{sequence_start}\" format=\"{FORMAT_ID}\"/>\n          </spine>\n        </sequence>\n      </project>\n"
        );
    }
    xml += "    </event>\n  </library>\n</fcpxml>\n";
    xml
}

impl Library<'_> {
    fn asset(&self, item: &ExportItem, audio_only: bool) -> Option<(PathBuf, &str)> {
        let media_url = media_url(item);
        let key = AssetKey {
            url: media_url.to_string_lossy().into_owned(),
            audio_only,
        };
        let id = self.assets.get(&key)?;
        Some((media_url, id))
    }

    /// One `<asset>` per distinct media file and side, in item order.
    fn assets_xml(&mut self, island: &ExportIsland) -> String {
        let mut xml = String::new();
        for item in &island.clips {
            let media_url = media_url(item);
            let audio_only = item.clip.video.is_none();
            let key = AssetKey {
                url: media_url.to_string_lossy().into_owned(),
                audio_only,
            };
            if self.assets.contains_key(&key) {
                continue;
            }
            let asset_id = format!("r_asset_{}", self.assets.len() + 1);
            let duration =
                xml_text::fcpxml_time(MediaTime::microseconds(item.clip.duration.as_seconds()));
            let start = frame_start(item.clip.source_timecode(), self.frame);
            let name =
                xml_text::escape(media_url.file_name().and_then(|n| n.to_str()).unwrap_or(""));
            let src = xml_text::escape(&crate::model::file_url(&media_url));
            let audio = item.clip.audio.first();
            let channels = audio.map_or(2, |a| a.channels);
            let rate = audio.map_or(48000, |a| a.sample_rate.round() as i64);
            let audio_attributes =
                format!(" audioSources=\"1\" audioChannels=\"{channels}\" audioRate=\"{rate}\"");
            xml += &format!(
                "    <asset id=\"{asset_id}\" name=\"{name}\" start=\"{start}\" duration=\"{duration}\""
            );
            if audio_only {
                xml += &format!(" hasAudio=\"1\"{audio_attributes}");
            } else {
                let has_audio = audio.is_some();
                xml += &format!(
                    " format=\"{FORMAT_ID}\" hasVideo=\"1\" hasAudio=\"{}\"",
                    u8::from(has_audio)
                );
                if has_audio {
                    xml += &audio_attributes;
                }
            }
            xml += &format!(
                ">\n      <media-rep kind=\"original-media\" src=\"{src}\"/>\n    </asset>\n"
            );
            self.assets.insert(key, asset_id);
        }
        xml
    }

    /// The multicam resource: one angle per source, cameras first.
    fn multicam_xml(&self, island: &ExportIsland) -> String {
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
            gb_video.cmp(&ga_video).then_with(|| a.cmp(b))
        });
        let mut xml = format!(
            "    <media id=\"{MULTICAM_ID}\" name=\"{} – Multicam\">\n      <multicam format=\"{FORMAT_ID}\">\n",
            xml_text::escape(&self.timeline.name)
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
                let side = if item.clip.video.is_some() {
                    MediaKind::Video
                } else {
                    MediaKind::Audio
                };
                let Some((media_url, asset_id)) = self.asset(item, side == MediaKind::Audio) else {
                    continue;
                };
                xml += &format!(
                    "          <asset-clip name=\"{}\" ref=\"{asset_id}\" offset=\"{}\" duration=\"{}\" start=\"{}\" enabled=\"{}\"{}{}\n",
                    clip_name(item, &media_url),
                    timeline_time(item.start, self.frame),
                    timeline_time(item.timeline_duration, self.frame),
                    timeline_time(item.source_in, self.frame),
                    u8::from(item.is_enabled(side)),
                    audio_role_attribute(item),
                    audio_channel_end(item),
                );
            }
            xml += "        </mc-angle>\n";
        }
        xml += "      </multicam>\n    </media>\n";
        xml
    }

    /// The synced project's connected clips: video above the spine, audio
    /// below, on lanes allocated per source. With storylines grouped, each
    /// lane becomes its own storyline.
    fn synced_clips_xml(&self, island: &ExportIsland, group_storylines: bool) -> String {
        let frame = self.frame;
        let video_items: Vec<&ExportItem> = island
            .clips
            .iter()
            .filter(|i| i.clip.video.is_some())
            .collect();
        let audio_items: Vec<&ExportItem> = island
            .clips
            .iter()
            .filter(|i| {
                // Disabled camera audio without a linked edit has nothing to place.
                let muted_camera_audio = i.clip.video.is_some()
                    && i.audio_enabled == Some(false)
                    && i.linked_audio_edit.is_none();
                !i.clip.audio.is_empty() && !muted_camera_audio
            })
            .collect();
        let audio_duration = |item: &ExportItem| {
            if item.corrected_audio_url.is_none() {
                item.timeline_duration
            } else {
                item.corrected_selected_duration()
            }
        };
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
                        audio_duration(item),
                    )
                })
                .collect::<Vec<_>>(),
        );

        let mut spine = String::new();
        let mut stories = std::collections::BTreeMap::<i64, String>::new();
        let lane_attribute = |lane: i64| {
            if group_storylines {
                String::new()
            } else {
                format!(" lane=\"{lane}\"")
            }
        };
        let mut place = |lane: i64, clip: String| {
            if group_storylines {
                *stories.entry(lane).or_default() += &clip;
            } else {
                spine += &clip;
            }
        };
        for item in video_items {
            let Some((media_url, asset_id)) = self.asset(item, false) else {
                continue;
            };
            let lane = video_assignments
                .get(&item.instance_id)
                .copied()
                .unwrap_or(0) as i64
                + 1;
            let source_start = timeline_time(item.source_in, frame);
            let source_duration = timeline_time(item.timeline_duration, frame);
            place(
                lane,
                format!(
                    "              <clip {} offset=\"{}\" duration=\"{}\" start=\"{}\" name=\"{}\" enabled=\"{}\"><video ref=\"{asset_id}\" offset=\"{source_start}\" start=\"{source_start}\" duration=\"{source_duration}\"/></clip>\n",
                    lane_attribute(lane),
                    timeline_time(item.start, frame),
                    timeline_time(item.timeline_duration, frame),
                    timeline_time(item.source_in, frame),
                    clip_name(item, &media_url),
                    u8::from(item.is_enabled(MediaKind::Video)),
                ),
            );
        }
        for item in audio_items {
            let Some((media_url, asset_id)) = self.asset(item, item.clip.video.is_none()) else {
                continue;
            };
            let lane = -((audio_assignments
                .get(&item.instance_id)
                .copied()
                .unwrap_or(0)
                + 1) as i64);
            let duration = audio_duration(item);
            let source_start = timeline_time(item.source_in, frame);
            let source_duration = timeline_time(duration, frame);
            let channel_attribute = item
                .selected_audio_source_channel()
                .map_or_else(String::new, |channel| format!(" srcCh=\"{}\"", channel + 1));
            place(
                lane,
                format!(
                    "              <clip {} offset=\"{}\" duration=\"{}\" start=\"{}\" name=\"{}\" enabled=\"{}\"{}><audio ref=\"{asset_id}\" offset=\"{source_start}\" start=\"{source_start}\" duration=\"{source_duration}\"{channel_attribute}/></clip>\n",
                    lane_attribute(lane),
                    timeline_time(item.start, frame),
                    timeline_time(duration, frame),
                    timeline_time(item.source_in, frame),
                    clip_name(item, &media_url),
                    u8::from(item.is_enabled(MediaKind::Audio)),
                    audio_role_attribute(item),
                ),
            );
        }
        let mut stories = stories.into_iter().collect::<Vec<_>>();
        stories.sort_by_key(|(lane, _)| (*lane >= 0, lane.unsigned_abs()));
        for (lane, clips) in stories {
            spine += &format!(
                "              <spine lane=\"{lane}\" offset=\"0s\">\n{clips}              </spine>\n"
            );
        }
        spine
    }
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
        std::fs::write(&plain, write(&timeline, FcpxmlOptions::TIMELINE)).unwrap();
        let xml = write(
            &timeline,
            FcpxmlOptions {
                group_storylines: true,
                ..FcpxmlOptions::TIMELINE
            },
        );
        assert!(xml.contains("<spine lane="));
        assert!(xml.find("spine lane=\"-1\"").unwrap() < xml.find("spine lane=\"-2\"").unwrap());
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
        let xml = write(&timeline, FcpxmlOptions::default());
        assert!(xml.contains("<fcpxml version=\"1.10\">"));
        assert!(xml.contains("r_multicam"));
        assert!(xml.contains("– synced"));
        assert!(xml.contains("<video ref="));
        assert!(xml.contains("<audio ref="));
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
    fn timeline_and_multicam_projects_can_be_selected_independently() {
        let timeline = fixture_timeline();
        let timeline_only = write(&timeline, FcpxmlOptions::TIMELINE);
        assert!(timeline_only.contains("– synced"));
        assert!(!timeline_only.contains("r_multicam"));

        let multicam_only = write(
            &timeline,
            FcpxmlOptions {
                timeline: false,
                multicam: true,
                group_storylines: false,
            },
        );
        assert!(!multicam_only.contains("– synced"));
        assert!(multicam_only.contains("r_multicam"));
        assert!(multicam_only.contains("– multicam"));
    }

    #[test]
    fn combined_fcpxml_keeps_each_sequence_and_scopes_resources() {
        let first = write(&fixture_timeline(), FcpxmlOptions::TIMELINE);
        let mut second_timeline = fixture_timeline();
        second_timeline.name = "Second cut".into();
        let second = write(&second_timeline, FcpxmlOptions::TIMELINE);
        let combined = combine_documents(&[first, second]).expect("combined FCPXML");
        assert!(combined.contains("id=\"s1-r_fmt_0\""));
        assert!(combined.contains("id=\"s2-r_fmt_0\""));
        assert!(combined.contains("ref=\"s1-r_asset_"));
        assert!(combined.contains("ref=\"s2-r_asset_"));

        let dir = std::env::temp_dir().join(format!("align-fcpxml-many-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("many.fcpxml");
        std::fs::write(&path, combined).unwrap();
        let summaries = timeline_sequence_summaries(&path).expect("summaries");
        assert_eq!(summaries.len(), 2);
        assert!(
            summaries
                .iter()
                .any(|summary| summary.name.contains("Second cut"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn video_only_edit_does_not_invent_disabled_camera_audio() {
        let mut timeline = fixture_timeline();
        for item in &mut timeline.islands[0].clips {
            if item.clip.video.is_some() {
                item.audio_enabled = Some(false);
            }
        }
        let xml = write(&timeline, FcpxmlOptions::TIMELINE);
        assert_eq!(xml.matches("<video ref=").count(), 1);
        assert_eq!(xml.matches("<audio ref=").count(), 1);
        timeline.islands[0].clips[0].enabled = false;
        let xml = write(&timeline, FcpxmlOptions::TIMELINE);
        assert!(xml.contains("enabled=\"0\""));
        assert_eq!(xml.matches("<video ref=").count(), 1);
    }

    #[test]
    fn fcpxml_preserves_disabled_clip_state() {
        let mut timeline = fixture_timeline();
        timeline.islands[0].clips[0].enabled = false;
        let xml = write(&timeline, FcpxmlOptions::TIMELINE);
        assert!(xml.contains("enabled=\"0\""));
    }

    #[test]
    fn fcpxml_preserves_audio_role() {
        let mut timeline = fixture_timeline();
        timeline.islands[0].clips[0].fcpxml_audio_role = Some("dialogue.interview".to_string());
        let xml = write(&timeline, FcpxmlOptions::TIMELINE);
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
            preserve_editing_tracks: Default::default(),
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
        let xml = write(&timeline, FcpxmlOptions::TIMELINE);
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
