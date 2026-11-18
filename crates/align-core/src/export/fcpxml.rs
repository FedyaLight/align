//! Final Cut Pro FCPXML writer: resources (sequence format + per-media assets), a multicam resource
//! grouping angles by source, a synced multi-track project and a multicam
//! project. With media bins, the event also lists every asset as a browser
//! clip tagged with its camera or recorder, and a keyword collection per
//! tag. Times are reduced `MediaTime` rationals (`0s`, `100/25s`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::model::{ExportIsland, ExportItem, ExportTimeline, xml_text};
use crate::allocator::{TimelineTrackRequest, allocate, source_key_for_url};
use crate::bins::{GENERATED_AUDIO, MediaBins};
use crate::model::{Clip, MediaKind, MediaTime, TIME_TOLERANCE};

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
fn grid_time(seconds: f64, frame: MediaTime) -> MediaTime {
    let frames = (seconds / frame.as_seconds()).round();
    let exact = frames * frame.as_seconds();
    if (seconds - exact).abs() <= TIME_TOLERANCE {
        MediaTime::new(frames as i64 * frame.value, frame.timescale)
    } else {
        MediaTime::microseconds(seconds)
    }
}

fn timeline_time(seconds: f64, frame: MediaTime) -> String {
    xml_text::fcpxml_time(grid_time(seconds, frame))
}

/// Whole frames covering `seconds`: Final Cut Pro rounds a sequence or gap
/// that ends inside a frame up to the next frame, with a warning.
fn frames_covering(seconds: f64, frame: MediaTime) -> String {
    let frames = (seconds / frame.as_seconds() - TIME_TOLERANCE)
        .ceil()
        .max(1.0);
    xml_text::fcpxml_time(MediaTime::new(frames as i64 * frame.value, frame.timescale))
}

fn sum(a: MediaTime, b: MediaTime) -> MediaTime {
    let (ta, tb) = (i64::from(a.timescale), i64::from(b.timescale));
    let (mut x, mut y) = (ta, tb);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    let lcm = ta / x * tb;
    match i32::try_from(lcm) {
        Ok(timescale) => MediaTime::new(a.value * (lcm / ta) + b.value * (lcm / tb), timescale),
        Err(_) => MediaTime::microseconds(a.as_seconds() + b.as_seconds()),
    }
}

/// The time from `from` to `to`.
fn difference(to: MediaTime, from: MediaTime) -> MediaTime {
    sum(to, MediaTime::new(-from.value, from.timescale))
}

/// Gaps between the clips of a storyline or multicam angle. Final Cut Pro
/// places the clips at their offsets, but Resolve plays a storyline's clips
/// back to back, so every space between them is an explicit gap.
fn with_gaps(mut clips: Vec<(MediaTime, MediaTime, String)>, indent: &str) -> String {
    clips.sort_by(|a, b| a.0.as_seconds().total_cmp(&b.0.as_seconds()));
    let mut xml = String::new();
    let mut cursor = MediaTime::new(0, 1);
    for (start, end, clip) in clips {
        let space = difference(start, cursor);
        if space.as_seconds() > TIME_TOLERANCE {
            xml += &format!(
                "{indent}<gap offset=\"{}\" duration=\"{}\"/>\n",
                xml_text::fcpxml_time(cursor),
                xml_text::fcpxml_time(space)
            );
        }
        xml += &clip;
        if end.as_seconds() > cursor.as_seconds() {
            cursor = end;
        }
    }
    xml
}

/// A video source position on its file's own frames. Final Cut Pro moves a
/// picture's start to a frame of its file, flooring it; the nearest frame
/// keeps the picture closer to its sound.
fn picture_seconds(seconds: f64, clip: &Clip) -> f64 {
    match clip.video.as_ref().and_then(|video| video.frame_duration) {
        Some(frame) if frame.value > 0 => {
            (seconds / frame.as_seconds()).round() * frame.as_seconds()
        }
        _ => seconds,
    }
}

/// Where Final Cut Pro starts a file: exactly at its own timecode, which FCP
/// reads from the file itself. Timecode Align derived from LTC or camera XML
/// is not in the file, so FCP starts such a file at zero.
fn media_start(clip: &Clip) -> MediaTime {
    clip.source_timecode()
        .filter(|tc| !tc.derived)
        .map_or(MediaTime::new(0, 1), |tc| {
            MediaTime::new(
                tc.frame_number * tc.frame_duration.value,
                tc.frame_duration.timescale,
            )
        })
}

/// A source position `seconds` into a file starting at `origin`. FCPXML
/// source times count from the file's timecode, not from zero.
fn source_time(origin: MediaTime, seconds: f64, clip: &Clip, frame: MediaTime) -> String {
    let grid = clip
        .video
        .as_ref()
        .and_then(|video| video.frame_duration)
        .unwrap_or(frame);
    xml_text::fcpxml_time(sum(origin, grid_time(seconds, grid)))
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
    /// Browser clips of every source with a keyword collection per camera
    /// and recorder, and one for the audio Align prepared.
    pub media_bins: bool,
}

impl FcpxmlOptions {
    /// The project timeline alone.
    pub const TIMELINE: Self = Self {
        timeline: true,
        multicam: false,
        group_storylines: false,
        media_bins: false,
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
    /// The sequence's frame size.
    size: (u32, u32),
    /// Each asset's id and media start.
    assets: HashMap<AssetKey, (String, MediaTime)>,
    /// Video formats besides the sequence's: frame duration and size by id.
    formats: Vec<(String, MediaTime, (u32, u32))>,
    /// Each asset as a clip of the event's browser, in resource order.
    browser_clips: Vec<BrowserClip>,
}

/// An asset's whole extent, as the event's browser shows it.
struct BrowserClip {
    key: AssetKey,
    /// Escaped file name.
    name: String,
    id: String,
    /// The asset's format, for a video asset.
    format: Option<String>,
    start: String,
    duration: String,
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

/// `name` set to the item's FCPXML audio role, or nothing without one.
/// Asset clips carry it as `audioRole`, the audio inside a clip as `role`.
fn audio_role_attribute(item: &ExportItem, name: &str) -> String {
    item.fcpxml_audio_role().map_or_else(String::new, |role| {
        format!(" {name}=\"{}\"", xml_text::escape(role))
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
            // A stem starts at its source's timecode plus the selected start,
            // in whole samples, as its broadcast extension says.
            let rate = item.clip.audio.first().map_or(48_000.0, |a| a.sample_rate);
            let start = MediaTime::new(
                (item.precision_source_start() * rate).round() as i64,
                rate.round() as i32,
            );
            let origin = sum(media_start(&item.clip), start);
            for audio in &mut item.clip.audio {
                if let Some(tc) = &mut audio.source_timecode
                    && !tc.derived
                {
                    tc.frame_number = origin.value;
                    tc.frame_duration = MediaTime::new(1, origin.timescale);
                }
            }
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
    sources: &ExportIsland,
    options: FcpxmlOptions,
) -> String {
    let island = &with_precision_stems(sources);
    let frame = timeline.frame_duration;
    let dimension = |side: fn(&crate::model::VideoSummary) -> u32, fallback: u32| {
        island
            .clips
            .iter()
            .find_map(|i| i.clip.video.as_ref().map(side))
            .unwrap_or(fallback)
    };
    let (width, height) = (dimension(|v| v.width, 1920), dimension(|v| v.height, 1080));

    let mut library = Library {
        timeline,
        frame,
        size: (width, height),
        assets: HashMap::new(),
        formats: Vec::new(),
        browser_clips: Vec::new(),
    };
    let mut assets = library.assets_xml(island);
    let bins = options
        .media_bins
        .then(|| MediaBins::new(sources.clips.iter().map(|item| &item.clip)));
    if let Some(bins) = &bins {
        for clip in bins.bins().iter().flat_map(|bin| &bin.clips) {
            assets += &library.asset_xml(clip, &clip.url);
        }
    }

    // Final Cut Pro names its standard formats itself; an unknown name only
    // earns a warning, so formats carry their parameters alone.
    let mut xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n".to_string();
    xml += "<!DOCTYPE fcpxml>\n<fcpxml version=\"1.10\">\n  <resources>\n";
    for (id, duration, (width, height)) in
        std::iter::once((FORMAT_ID.to_string(), frame, (width, height)))
            .chain(library.formats.iter().cloned())
    {
        xml += &format!(
            "    <format id=\"{id}\" frameDuration=\"{}\" width=\"{width}\" height=\"{height}\"/>\n",
            xml_text::fcpxml_time(duration)
        );
    }
    xml += &assets;
    if options.multicam {
        xml += &library.multicam_xml(island);
    }
    xml += "  </resources>\n";

    let total_duration = frames_covering(island.duration, frame);
    let first_timecode = island.clips.iter().find_map(|i| i.clip.source_timecode());
    let sequence_start = frame_start(first_timecode, frame);
    // Drop-frame labels exist only at 29.97 and 59.94.
    let drop_frame = first_timecode.is_some_and(|tc| tc.drop_frame)
        && frame.value == 1001
        && matches!(frame.timescale, 30_000 | 60_000);
    let tc_format = if drop_frame { "DF" } else { "NDF" };
    let sequence = format!(
        "        <sequence format=\"{FORMAT_ID}\" duration=\"{total_duration}\" tcStart=\"{sequence_start}\" tcFormat=\"{tc_format}\">\n"
    );
    let name = xml_text::escape(&timeline.name);
    xml += "  <library>\n";
    xml += &format!("    <event name=\"{name}\">\n");
    if let Some(bins) = &bins {
        xml += &library.browser_xml(bins);
    }
    if options.timeline {
        xml += &format!("      <project name=\"{name} – synced\">\n{sequence}");
        xml += &format!(
            "          <spine>\n            <gap offset=\"{sequence_start}\" duration=\"{total_duration}\" name=\"Master\">\n"
        );
        xml += &library.synced_clips_xml(island, options.group_storylines);
        xml += "            </gap>\n          </spine>\n        </sequence>\n      </project>\n";
    }
    if options.multicam {
        xml += &format!("      <project name=\"{name} – multicam\">\n{sequence}");
        xml += &format!(
            "          <spine>\n            <mc-clip name=\"{name} – Multicam\" ref=\"{MULTICAM_ID}\" offset=\"{sequence_start}\" duration=\"{total_duration}\"/>\n          </spine>\n        </sequence>\n      </project>\n"
        );
    }
    xml += "    </event>\n  </library>\n</fcpxml>\n";
    xml
}

impl Library<'_> {
    /// The file an item plays, its asset id and the file's start.
    fn asset(&self, item: &ExportItem, audio_only: bool) -> Option<(PathBuf, &str, MediaTime)> {
        let media_url = media_url(item);
        let key = AssetKey {
            url: media_url.to_string_lossy().into_owned(),
            audio_only,
        };
        let (id, start) = self.assets.get(&key)?;
        Some((media_url, id, *start))
    }

    /// The format of a video file: the sequence's when frame duration and
    /// size match, else one of its own.
    fn format_id(&mut self, video: &crate::model::VideoSummary) -> String {
        let duration = video.frame_duration.unwrap_or(self.frame);
        let size = (video.width, video.height);
        if (duration, size) == (self.frame, self.size) {
            return FORMAT_ID.to_string();
        }
        if let Some((id, ..)) = self
            .formats
            .iter()
            .find(|(_, d, s)| (*d, *s) == (duration, size))
        {
            return id.clone();
        }
        let id = format!("r_fmt_{}", self.formats.len() + 1);
        self.formats.push((id.clone(), duration, size));
        id
    }

    /// One `<asset>` per distinct media file and side, in item order.
    fn assets_xml(&mut self, island: &ExportIsland) -> String {
        island
            .clips
            .iter()
            .map(|item| self.asset_xml(&item.clip, &media_url(item)))
            .collect()
    }

    /// The `<asset>` of `clip` played from `media_url`, unless one exists.
    fn asset_xml(&mut self, clip: &Clip, media_url: &Path) -> String {
        let audio_only = clip.video.is_none();
        let key = AssetKey {
            url: media_url.to_string_lossy().into_owned(),
            audio_only,
        };
        if self.assets.contains_key(&key) {
            return String::new();
        }
        let id = format!("r_asset_{}", self.assets.len() + 1);
        let duration = xml_text::fcpxml_time(MediaTime::microseconds(clip.duration.as_seconds()));
        let origin = media_start(clip);
        let start = xml_text::fcpxml_time(origin);
        let name = xml_text::escape(media_url.file_name().and_then(|n| n.to_str()).unwrap_or(""));
        let src = xml_text::escape(&crate::model::file_url(media_url));
        let audio = clip.audio.first();
        let channels = audio.map_or(2, |a| a.channels);
        let rate = audio.map_or(48000, |a| a.sample_rate.round() as i64);
        let audio_attributes =
            format!(" audioSources=\"1\" audioChannels=\"{channels}\" audioRate=\"{rate}\"");
        let mut xml = format!(
            "    <asset id=\"{id}\" name=\"{name}\" start=\"{start}\" duration=\"{duration}\""
        );
        let format = clip.video.as_ref().map(|video| self.format_id(video));
        if let Some(format) = &format {
            let has_audio = audio.is_some();
            xml += &format!(
                " format=\"{format}\" hasVideo=\"1\" hasAudio=\"{}\"",
                u8::from(has_audio)
            );
            if has_audio {
                xml += &audio_attributes;
            }
        } else {
            xml += &format!(" hasAudio=\"1\"{audio_attributes}");
        }
        xml +=
            &format!(">\n      <media-rep kind=\"original-media\" src=\"{src}\"/>\n    </asset>\n");
        self.browser_clips.push(BrowserClip {
            key: key.clone(),
            name,
            id: id.clone(),
            format,
            start,
            duration,
        });
        self.assets.insert(key, (id, origin));
        xml
    }

    /// Browser clips of the event: each source tagged with its bin's
    /// keyword, every other asset with the prepared-audio keyword, and a
    /// keyword collection per keyword.
    fn browser_xml(&self, bins: &MediaBins) -> String {
        let source_keys: HashMap<AssetKey, &str> = bins
            .bins()
            .iter()
            .flat_map(|bin| {
                bin.clips.iter().map(|clip| {
                    let key = AssetKey {
                        url: clip.url.to_string_lossy().into_owned(),
                        audio_only: clip.video.is_none(),
                    };
                    (key, bin.name.as_str())
                })
            })
            .collect();
        let source_urls: HashSet<&str> = source_keys.keys().map(|k| k.url.as_str()).collect();
        let mut keywords: Vec<&str> = bins.bins().iter().map(|bin| bin.name.as_str()).collect();
        let mut xml = String::new();
        for clip in &self.browser_clips {
            let keyword = match source_keys.get(&clip.key) {
                Some(bin) => *bin,
                None if !source_urls.contains(clip.key.url.as_str()) => GENERATED_AUDIO,
                // The other side of a source, which is not its own browser clip.
                None => continue,
            };
            if keyword == GENERATED_AUDIO && !keywords.contains(&GENERATED_AUDIO) {
                keywords.push(GENERATED_AUDIO);
            }
            let BrowserClip {
                name,
                id,
                start,
                duration,
                ..
            } = clip;
            let format = clip
                .format
                .as_ref()
                .map_or_else(String::new, |format| format!(" format=\"{format}\""));
            xml += &format!(
                "      <asset-clip name=\"{name}\" ref=\"{id}\" start=\"{start}\" duration=\"{duration}\"{format}>\n        <keyword start=\"{start}\" duration=\"{duration}\" value=\"{}\"/>\n      </asset-clip>\n",
                keyword_value(keyword)
            );
        }
        for keyword in keywords {
            xml += &format!(
                "      <keyword-collection name=\"{}\"/>\n",
                keyword_value(keyword)
            );
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
            let mut clips = Vec::new();
            for item in items {
                let side = if item.clip.video.is_some() {
                    MediaKind::Video
                } else {
                    MediaKind::Audio
                };
                let Some((media_url, asset_id, origin)) =
                    self.asset(item, side == MediaKind::Audio)
                else {
                    continue;
                };
                let source_in = match side {
                    MediaKind::Video => picture_seconds(item.source_in, &item.clip),
                    MediaKind::Audio => item.source_in,
                };
                let start = grid_time(item.start, self.frame);
                let duration = grid_time(item.timeline_duration, self.frame);
                clips.push((
                    start,
                    sum(start, duration),
                    format!(
                        "          <asset-clip name=\"{}\" ref=\"{asset_id}\" offset=\"{}\" duration=\"{}\" start=\"{}\" enabled=\"{}\"{}{}\n",
                        clip_name(item, &media_url),
                        xml_text::fcpxml_time(start),
                        xml_text::fcpxml_time(duration),
                        source_time(origin, source_in, &item.clip, self.frame),
                        u8::from(item.is_enabled(side)),
                        audio_role_attribute(item, "audioRole"),
                        audio_channel_end(item),
                    ),
                ));
            }
            xml += &with_gaps(clips, "          ");
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
        let mut stories = std::collections::BTreeMap::<i64, Vec<_>>::new();
        let lane_attribute = |lane: i64| {
            if group_storylines {
                String::new()
            } else {
                format!(" lane=\"{lane}\"")
            }
        };
        let mut place = |lane: i64, start: f64, duration: f64, clip: String| {
            if group_storylines {
                let start = grid_time(start, frame);
                let end = sum(start, grid_time(duration, frame));
                stories.entry(lane).or_default().push((start, end, clip));
            } else {
                spine += &clip;
            }
        };
        for item in video_items {
            let Some((media_url, asset_id, origin)) = self.asset(item, false) else {
                continue;
            };
            let lane = video_assignments
                .get(&item.instance_id)
                .copied()
                .unwrap_or(0) as i64
                + 1;
            // The clip's own time runs from the source in point, where the
            // video starts its file at that position past the file's start,
            // on a frame of the file.
            let source_start = timeline_time(item.source_in, frame);
            let media_start = source_time(
                origin,
                picture_seconds(item.source_in, &item.clip),
                &item.clip,
                frame,
            );
            let source_duration = timeline_time(item.timeline_duration, frame);
            place(
                lane,
                item.start,
                item.timeline_duration,
                format!(
                    "              <clip{} offset=\"{}\" duration=\"{}\" start=\"{}\" name=\"{}\" enabled=\"{}\"><video ref=\"{asset_id}\" offset=\"{source_start}\" start=\"{media_start}\" duration=\"{source_duration}\"/></clip>\n",
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
            let Some((media_url, asset_id, origin)) = self.asset(item, item.clip.video.is_none())
            else {
                continue;
            };
            let lane = -((audio_assignments
                .get(&item.instance_id)
                .copied()
                .unwrap_or(0)
                + 1) as i64);
            let duration = audio_duration(item);
            let source_start = timeline_time(item.source_in, frame);
            let media_start = source_time(origin, item.source_in, &item.clip, frame);
            let source_duration = timeline_time(duration, frame);
            let channel_attribute = item
                .selected_audio_source_channel()
                .map_or_else(String::new, |channel| format!(" srcCh=\"{}\"", channel + 1));
            place(
                lane,
                item.start,
                duration,
                format!(
                    "              <clip{} offset=\"{}\" duration=\"{}\" start=\"{}\" name=\"{}\" enabled=\"{}\"><audio ref=\"{asset_id}\" offset=\"{source_start}\" start=\"{media_start}\" duration=\"{source_duration}\"{channel_attribute}{}/></clip>\n",
                    lane_attribute(lane),
                    timeline_time(item.start, frame),
                    timeline_time(duration, frame),
                    timeline_time(item.source_in, frame),
                    clip_name(item, &media_url),
                    u8::from(item.is_enabled(MediaKind::Audio)),
                    audio_role_attribute(item, "role"),
                ),
            );
        }
        let mut stories = stories.into_iter().collect::<Vec<_>>();
        stories.sort_by_key(|(lane, _)| (*lane >= 0, lane.unsigned_abs()));
        for (lane, clips) in stories {
            spine += &format!(
                "              <spine lane=\"{lane}\" offset=\"0s\">\n{}              </spine>\n",
                with_gaps(clips, "              ")
            );
        }
        spine
    }
}

/// A keyword attribute value. Final Cut Pro splits keyword values at commas.
fn keyword_value(name: &str) -> String {
    xml_text::escape(&name.replace(',', ";"))
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

    /// The fixture with timecode 11:26:57:00 on the camera and a recorder
    /// whose broadcast extension starts 200 samples later.
    fn timecoded_timeline(recorder_derived: bool) -> ExportTimeline {
        use crate::model::SourceTimecode;
        let mut result = super::super::otio::tests::fixture_result();
        let camera = SourceTimecode::from_label("11:26:57:00", MediaTime::new(1, 25)).unwrap();
        result.project.clips[0]
            .video
            .as_mut()
            .unwrap()
            .source_timecode = Some(camera);
        let recorder = SourceTimecode::from_samples(
            41_217 * 48_000 + 200,
            48_000.0,
            MediaTime::new(1, 25),
            false,
        )
        .unwrap();
        result.project.clips[1].audio[0].source_timecode = Some(SourceTimecode {
            derived: recorder_derived,
            ..recorder
        });
        ExportTimeline::from_result(&result, true).unwrap()
    }

    /// Final Cut Pro reads each file's start from the file and drops clips
    /// whose source range lies before it, so source times count from the
    /// file's exact timecode and the spine starts at the sequence's.
    #[test]
    fn source_times_count_from_each_files_timecode() {
        let timeline = timecoded_timeline(false);
        let xml = write(&timeline, FcpxmlOptions::default());
        let recorder_start = xml_text::fcpxml_time(MediaTime::new(41_217 * 48_000 + 200, 48_000));
        let asset = |name: &str| {
            let line = xml
                .lines()
                .find(|line| line.contains("<asset ") && line.contains(name))
                .unwrap();
            line.to_string()
        };
        assert!(asset("cam.mov").contains(" start=\"41217s\""));
        assert!(asset("rec.wav").contains(&format!(" start=\"{recorder_start}\"")));
        assert!(xml.contains("<video ref=\"r_asset_1\" offset=\"0s\" start=\"41217s\""));
        assert!(xml.contains(&format!(
            "<audio ref=\"r_asset_2\" offset=\"0s\" start=\"{recorder_start}\""
        )));
        assert!(xml.contains("<gap offset=\"41217s\""));
        assert!(xml.contains("offset=\"41217s\" duration=\"313/25s\"/>"));
        let angle = xml
            .lines()
            .find(|line| line.contains("<asset-clip ") && line.contains("rec.wav"))
            .unwrap();
        assert!(
            angle.contains(&format!(" start=\"{recorder_start}\"")),
            "{angle}"
        );

        // Align reads its own document back at the same places.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timecode.fcpxml");
        std::fs::write(&path, write(&timeline, FcpxmlOptions::TIMELINE)).unwrap();
        let draft = read_timeline(&path, None).unwrap();
        for edit in &draft.edits {
            let expected = if edit.url.ends_with("rec.wav") {
                0.5
            } else {
                0.0
            };
            assert!(edit.source_in.abs() < 1e-6, "{edit:?}");
            assert!((edit.timeline_start - expected).abs() < 1e-6, "{edit:?}");
        }
    }

    /// Timecode Align derived from LTC is not in the file, so Final Cut Pro
    /// starts that file at zero.
    #[test]
    fn derived_timecode_leaves_the_file_starting_at_zero() {
        let xml = write(&timecoded_timeline(true), FcpxmlOptions::TIMELINE);
        let asset = xml
            .lines()
            .find(|line| line.contains("<asset ") && line.contains("rec.wav"))
            .unwrap();
        assert!(asset.contains(" start=\"0s\""), "{asset}");
        assert!(xml.contains("<audio ref=\"r_asset_2\" offset=\"0s\" start=\"0s\""));
    }

    /// Resolve plays a storyline's clips back to back, so the space before
    /// a clip is an explicit gap, in storylines and multicam angles alike.
    #[test]
    fn storylines_and_angles_spell_out_their_gaps() {
        let mut timeline = fixture_timeline();
        // A recorder in its own folder is its own angle.
        timeline.islands[0].clips[1].clip.url = PathBuf::from("/r/rec.wav");
        let xml = write(
            &timeline,
            FcpxmlOptions {
                group_storylines: true,
                ..FcpxmlOptions::default()
            },
        );
        // The recorder starts 0.5 s in, after a gap, in its storyline and
        // in its angle.
        assert_eq!(
            xml.matches("<gap offset=\"0s\" duration=\"1/2s\"/>")
                .count(),
            2,
            "{xml}"
        );
    }

    /// Final Cut Pro moves a picture's source start to a frame of its file:
    /// 0.2 s into a 24 fps camera is between frames 4 and 5, so Align writes
    /// the nearest, frame 5.
    #[test]
    fn picture_starts_on_a_frame_of_its_file() {
        let mut timeline = fixture_timeline();
        let camera = &mut timeline.islands[0].clips[0];
        camera.clip.video.as_mut().unwrap().frame_duration = Some(MediaTime::new(1, 24));
        camera.source_in = 0.2;
        let xml = write(&timeline, FcpxmlOptions::default());
        assert!(
            xml.contains("<video ref=\"r_asset_1\" offset=\"1/5s\" start=\"5/24s\""),
            "{xml}"
        );
        let angle = xml
            .lines()
            .find(|line| line.contains("<asset-clip ") && line.contains("cam.mov"))
            .unwrap();
        assert!(angle.contains(" start=\"5/24s\""), "{angle}");
        // Its sound keeps the exact position.
        assert!(xml.contains("<audio ref=\"r_asset_1\" offset=\"1/5s\" start=\"1/5s\""));
    }

    /// A drop-frame camera labels the sequence's timecode drop-frame, or
    /// Resolve shows its start as a different, non-drop label.
    #[test]
    fn drop_frame_timecode_labels_the_sequence() {
        use crate::model::SourceTimecode;
        let ntsc = MediaTime::new(1001, 30_000);
        let mut timeline = fixture_timeline();
        timeline.frame_duration = ntsc;
        let camera = timeline.islands[0].clips[0].clip.video.as_mut().unwrap();
        camera.frame_duration = Some(ntsc);
        camera.source_timecode = SourceTimecode::from_label("11:26:57;00", ntsc);
        let xml = write(&timeline, FcpxmlOptions::TIMELINE);
        assert!(xml.contains("tcFormat=\"DF\""), "{xml}");
        timeline.islands[0].clips[0]
            .clip
            .video
            .as_mut()
            .unwrap()
            .source_timecode = SourceTimecode::from_label("11:26:57:00", ntsc);
        assert!(write(&timeline, FcpxmlOptions::TIMELINE).contains("tcFormat=\"NDF\""));
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
        let xml = write(
            &timeline,
            FcpxmlOptions {
                media_bins: true,
                ..FcpxmlOptions::default()
            },
        );
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
                media_bins: false,
            },
        );
        assert!(!multicam_only.contains("– synced"));
        assert!(multicam_only.contains("r_multicam"));
        assert!(multicam_only.contains("– multicam"));
    }

    #[test]
    fn media_bins_tag_each_asset_as_a_browser_clip() {
        let mut timeline = fixture_timeline();
        let rec = timeline.islands[0]
            .clips
            .iter_mut()
            .find(|i| i.clip.id.0 == "rec")
            .expect("recorder item");
        *rec = rec.with_corrected_audio(PathBuf::from("/fix/rec – drift.wav"));
        let xml = write(
            &timeline,
            FcpxmlOptions {
                media_bins: true,
                ..FcpxmlOptions::default()
            },
        );
        let event = &xml[xml.find("<event").expect("event")..];
        let browser = &event[..event.find("<project").expect("project")];
        // Every asset, including the original recorder file the project
        // does not play, appears once with its bin's keyword.
        let assets = xml.matches("<asset id=").count();
        assert_eq!(assets, 3);
        assert_eq!(browser.matches("<asset-clip ").count(), assets);
        for (file, keyword) in [
            ("cam.mov", "v – Video"),
            ("rec.wav", "v – Audio"),
            ("rec – drift.wav", "Align generated audio"),
        ] {
            let clip = &browser[browser.find(&format!("name=\"{file}\"")).expect(file)..];
            let clip = &clip[..clip.find("</asset-clip>").expect("clip end")];
            assert!(clip.contains(&format!("value=\"{keyword}\"/>")), "{file}");
            assert!(browser.contains(&format!("<keyword-collection name=\"{keyword}\"/>")));
        }
        let plain = write(&timeline, FcpxmlOptions::default());
        assert!(!plain.contains("<keyword") && !plain.contains("rec.wav"));
    }

    #[test]
    fn multicam_clip_uses_only_attributes_the_dtd_declares() {
        let xml = write(&fixture_timeline(), FcpxmlOptions::default());
        let clip = &xml[xml.find("<mc-clip ").expect("mc-clip")..];
        let clip = &clip[..clip.find("/>").expect("end")];
        assert!(
            !clip.contains("tcStart=") && !clip.contains("format="),
            "{clip}"
        );
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

    /// Final Cut Pro rejects a whole document whose `clip` carries
    /// `audioRole`: a clip's audio takes the role, as `role`.
    #[test]
    fn audio_roles_sit_where_the_dtd_declares_them() {
        let mut timeline = fixture_timeline();
        for item in &mut timeline.islands[0].clips {
            item.fcpxml_audio_role = Some("dialogue.interview".to_string());
        }
        let xml = write(&timeline, FcpxmlOptions::default());
        for line in xml.lines().filter(|line| line.contains("<clip ")) {
            assert!(!line.contains("audioRole"), "{line}");
            assert!(
                !line.contains("<audio ") || line.contains(" role=\"dialogue.interview\""),
                "{line}"
            );
        }
        let angle = xml
            .lines()
            .find(|line| line.contains("<asset-clip ") && line.contains("rec.wav"))
            .expect("recorder angle");
        assert!(
            angle.contains("audioRole=\"dialogue.interview\""),
            "{angle}"
        );
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
        assert!(xml.contains(" role=\"dialogue\""));
        assert!(xml.contains(" role=\"effects\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_template_intact() {
        let script = super::super::script::write();
        assert!(script.starts_with("#!/usr/bin/env python3"));
        assert!(script.contains("AppendToTimeline"));
        assert!(script.contains("sample-accurate recorder clips"));
        assert!(script.contains("mediaBins") && script.contains("generatedAudio"));
    }
}
