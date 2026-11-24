//! Cancellable process boundary for the bundled AAF reader and writer.
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserved_start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub path: PathBuf,
    pub start: u64,
    pub source_in: u64,
    pub length: u64,
    pub source_frames: u64,
    pub channels: u16,
    /// The channel of a camera file linked as it is, without a mono stem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<usize>,
    /// The camera file's stream description for its media link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
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
    #[serde(default)]
    pub sample_rate: u32,
    #[serde(default)]
    pub media_kind: ImportedKind,
    pub edit_rate: Option<ImportedRate>,
    pub time_rate: Option<ImportedRate>,
    pub clips: Vec<ImportedClip>,
}
#[derive(Debug, Deserialize)]
pub struct ImportedClip {
    pub path: PathBuf,
    pub start: u64,
    pub source_in: u64,
    pub length: u64,
    pub channel: Option<usize>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ImportedKind {
    #[default]
    Sound,
    Picture,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportedRate {
    pub numerator: u32,
    pub denominator: u32,
}

impl ImportedTrack {
    fn clock(&self) -> (u32, u32) {
        self.time_rate
            .as_ref()
            .or(self.edit_rate.as_ref())
            .map_or((self.sample_rate, 1), |rate| {
                (rate.numerator, rate.denominator)
            })
    }
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
        let mut frame_duration = None;
        let mut aaf_track_numbers = Vec::new();
        let (mut picture_number, mut sound_number) = (0, 0);
        for (track_index, track) in sequence.tracks.into_iter().enumerate() {
            let track_number = match track.media_kind {
                ImportedKind::Picture => &mut picture_number,
                ImportedKind::Sound => &mut sound_number,
            };
            *track_number += 1;
            aaf_track_numbers.push(*track_number);
            let (numerator, denominator) = track.clock();
            let rate = numerator as f64 / denominator as f64;
            if track.media_kind == ImportedKind::Picture {
                let picture_duration = track.edit_rate.as_ref().map_or(
                    MediaTime::new(denominator as i64, numerator as i32),
                    |rate| MediaTime::new(rate.denominator as i64, rate.numerator as i32),
                );
                frame_duration.get_or_insert(picture_duration);
            }
            for (clip_index, clip) in track.clips.into_iter().enumerate() {
                edits.push(DraftEdit {
                    id: format!("aaf-{index}-{track_index}-{clip_index}"),
                    name: clip
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned()),
                    url: clip.path,
                    media_type: match track.media_kind {
                        ImportedKind::Sound => DraftMediaKind::Audio,
                        ImportedKind::Picture => DraftMediaKind::Video,
                    },
                    source_in: clip.source_in as f64 / rate,
                    source_out: (clip.source_in + clip.length) as f64 / rate,
                    timeline_start: clip.start as f64 / rate,
                    timeline_end: (clip.start + clip.length) as f64 / rate,
                    retime: Default::default(),

                    fcp7: Default::default(),

                    time_scale: numerator as i32,
                    include_embedded_audio: false,
                    audio_source_channel: clip.channel,
                    fcpxml_audio_role: None,
                    track_index: track_index + 1,
                    flags: Default::default(),

                    linked_edit_ids: Default::default(),
                });
            }
        }
        if edits.is_empty() {
            return Err(AafError::Writer(
                "AAF sequence contains no media clips".into(),
            ));
        }
        Ok(TimelineDraft {
            aaf_track_numbers,
            source_url: path.to_path_buf(),
            name: sequence.name,
            // Audio AAF has no picture edit rate. Use the application's default
            // picture rate while retaining each edit's native sample clock.
            frame_duration: frame_duration.unwrap_or(MediaTime::new(1, 25)),
            edits,
            transitions: Vec::new(),
            warnings: Vec::new(),
        })
    }
}

/// Protocol version of the bridge's `read-timeline` response.
const READ_TIMELINE_VERSION: u32 = 3;

/// AAF responses already read, by file, revision, and extraction folder:
/// listing a project's sequences and reading each of them parsed the whole
/// project, and extracted its embedded sound, every time.
static READ: std::sync::LazyLock<std::sync::Mutex<ReadTimelines>> =
    std::sync::LazyLock::new(Default::default);
type ReadTimelines = std::collections::HashMap<(PathBuf, String, PathBuf), Vec<u8>>;
/// Responses kept at most; a long session starts over past it.
const READ_LIMIT: usize = 64;
const MAX_AAF_RESPONSE: usize = 64 << 20;
const MAX_AAF_CACHE_BYTES: usize = 64 << 20;

pub fn read_timeline(path: &Path, cancel: &AtomicBool) -> Result<ImportedAudio, AafError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(AafError::Cancelled);
    }
    let directory = crate::media_assets::extraction_directory()?;
    let key = align_core::cache::media_revision(path)
        .ok()
        .map(|revision| (path.to_path_buf(), revision, directory.clone()));
    let lock = || {
        READ.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let cached = key.as_ref().and_then(|key| lock().get(key).cloned());
    if let Some(json) = cached
        && let Ok(result) = parse_timeline(&json)
        // Extracted sound lives on while the folder does; read again if not.
        // Media the project only refers to may be missing, and is relinked
        // later.
        && result
            .sequences
            .iter()
            .flat_map(|sequence| &sequence.tracks)
            .flat_map(|track| &track.clips)
            .filter(|clip| clip.path.starts_with(&directory))
            .all(|clip| clip.path.exists())
    {
        return Ok(result);
    }
    let json = run_read_timeline(path, &directory, cancel)?;
    let result = parse_timeline(&json)?;
    // A project that changed while the module read it may have been read
    // in either state; its response is not kept.
    let unchanged = |key: &(PathBuf, String, PathBuf)| {
        align_core::cache::media_revision(path).is_ok_and(|revision| revision == key.1)
    };
    if let Some(key) = key.filter(unchanged) {
        let mut read = lock();
        let cached_bytes: usize = read.values().map(Vec::len).sum();
        if read.len() >= READ_LIMIT || cached_bytes + json.len() > MAX_AAF_CACHE_BYTES {
            read.clear();
        }
        read.insert(key, json);
    }
    Ok(result)
}

fn parse_timeline(json: &[u8]) -> Result<ImportedAudio, AafError> {
    let result: ImportedAudio =
        serde_json::from_slice(json).map_err(|e| AafError::Writer(e.to_string()))?;
    if result.version != READ_TIMELINE_VERSION {
        return Err(AafError::Writer(
            "AAF module protocol version mismatch".into(),
        ));
    }
    validate_import(&result)?;
    Ok(result)
}

/// The bridge's `read-timeline` response for `path`, extracting embedded
/// sound into `directory`.
fn run_read_timeline(
    path: &Path,
    directory: &Path,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, AafError> {
    let executable = crate::ff::resolve_bin("ALIGN_AAF", "align-aaf").ok_or(AafError::Missing)?;
    // A file avoids stdout pipe deadlocks on large compositions while retaining
    // cancellable process supervision and automatic temporary-file cleanup.
    let mut output = tempfile::tempfile()?;
    let mut errors = tempfile::tempfile()?;
    let mut child = crate::process::command(executable)
        .arg("read-timeline")
        .arg(path)
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(errors.try_clone()?))
        .spawn()?;
    let status = crate::process::wait_cancellable(&mut child, cancel)?;
    use std::io::{Read, Seek};
    if !status.success() {
        errors.rewind()?;
        let mut details = Vec::new();
        errors.take(8192).read_to_end(&mut details)?;
        let details = String::from_utf8_lossy(&details);
        let details = details.trim();
        return Err(AafError::Writer(if details.is_empty() {
            status.to_string()
        } else {
            details.to_owned()
        }));
    }
    output.rewind()?;
    let mut json = Vec::new();
    output
        .take(MAX_AAF_RESPONSE as u64 + 1)
        .read_to_end(&mut json)?;
    if json.len() > MAX_AAF_RESPONSE {
        return Err(AafError::Writer(
            "AAF timeline response is too large".into(),
        ));
    }
    Ok(json)
}

fn validate_import(result: &ImportedAudio) -> Result<(), AafError> {
    if ![1, 2, 3].contains(&result.version) || result.sequences.is_empty() {
        return Err(AafError::Writer("Unsupported or empty AAF response".into()));
    }
    for sequence in &result.sequences {
        for track in &sequence.tracks {
            let (numerator, denominator) = track.clock();
            let valid_end = |start: u64, length: u64| {
                start
                    .checked_add(length)
                    .and_then(|end| end.checked_mul(denominator as u64))
                    .is_some_and(|end| end < (1u64 << 52))
            };
            if numerator == 0
                || numerator > i32::MAX as u32
                || denominator == 0
                || (result.version >= 2 && track.edit_rate.is_none())
                || (result.version == 3 && track.time_rate.is_none())
                || track.edit_rate.as_ref().is_some_and(|rate| {
                    rate.numerator == 0 || rate.numerator > i32::MAX as u32 || rate.denominator == 0
                })
                || track.clips.iter().any(|clip| {
                    clip.length == 0
                        || match track.media_kind {
                            ImportedKind::Sound => {
                                clip.channel.is_none_or(|channel| channel == usize::MAX)
                            }
                            ImportedKind::Picture => clip.channel.is_some(),
                        }
                        || !valid_end(clip.start, clip.length)
                        || !valid_end(clip.source_in, clip.length)
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

impl From<crate::process::WaitError> for AafError {
    fn from(error: crate::process::WaitError) -> Self {
        match error {
            crate::process::WaitError::Cancelled => Self::Cancelled,
            crate::process::WaitError::Io(error) => Self::Io(error),
        }
    }
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
    let mut child = crate::process::command(executable)
        .arg("write-audio")
        .arg(manifest)
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let status = crate::process::wait_cancellable(&mut child, cancel)?;
    if !status.success() {
        return Err(AafError::Writer(status.to_string()));
    }
    if !destination.is_file() {
        return Err(AafError::Writer("no output file".into()));
    }
    Ok(())
}

/// Write an AAF copy whose external media locators use the supplied paths.
pub fn repair_paths(
    source: &Path,
    destination: &Path,
    replacements: &[(PathBuf, PathBuf)],
    cancel: &AtomicBool,
) -> Result<usize, AafError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(AafError::Cancelled);
    }
    let canonical_source = source.canonicalize().ok();
    let canonical_destination = destination.canonicalize().ok();
    let same_file = source == destination
        || (canonical_source.is_some() && canonical_source == canonical_destination);
    if same_file {
        return Err(AafError::Writer(
            "path repair destination must be a copy".into(),
        ));
    }
    let executable = crate::ff::resolve_bin("ALIGN_AAF", "align-aaf").ok_or(AafError::Missing)?;
    let mut manifest = tempfile::NamedTempFile::new()?;
    let document = serde_json::json!({
        "version": 1,
        "replacements": replacements.iter().map(|(from, to)| serde_json::json!({
            "from": from,
            "to": to,
        })).collect::<Vec<_>>(),
    });
    serde_json::to_writer(manifest.as_file_mut(), &document)
        .map_err(|error| AafError::Writer(error.to_string()))?;
    let mut output = tempfile::tempfile()?;
    let mut child = crate::process::command(executable)
        .arg("repair-paths")
        .arg(manifest.path())
        .arg(source)
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::inherit())
        .spawn()?;
    let status = crate::process::wait_cancellable(&mut child, cancel)?;
    if !status.success() {
        return Err(AafError::Writer(status.to_string()));
    }
    use std::io::{Seek, SeekFrom};
    output.seek(SeekFrom::Start(0))?;
    #[derive(Deserialize)]
    struct Response {
        version: u32,
        replacements: usize,
    }
    let response: Response =
        serde_json::from_reader(output).map_err(|error| AafError::Writer(error.to_string()))?;
    if response.version != 1 || !destination.is_file() {
        return Err(AafError::Writer("invalid AAF path repair response".into()));
    }
    Ok(response.replacements)
}

/// Assemble already-rendered mono stems. Reject unsupported edit semantics
/// rather than silently dropping them while the AAF implementation expands.
pub fn audio_manifest(
    timeline: &align_core::export::ExportTimeline,
) -> Result<AudioManifest, AafError> {
    audio_manifest_linking(timeline, &std::collections::HashSet::new())
}

/// The audio manifest, linking the sound of `cameras` without stems as it
/// is: an item of one of them without prepared stems refers to the file's
/// own channels.
/// Lanes of clips in start order (`order` breaks ties), each clip joining
/// the first lane, of a clip `joins` accepts, whose last clip has ended. A
/// clip the camera clock moved is packed at its assembled start first, so
/// every other clip keeps its lane in the ordinary assembly; then it takes
/// that lane if free at its own start, else the first free lane, else a new
/// last one.
fn pack_lanes<T>(
    mut clips: Vec<(T, Option<u64>)>,
    span: impl Fn(&T) -> (u64, u64),
    order: impl Fn(&T, &T) -> std::cmp::Ordering,
    joins: impl Fn(&T, &T) -> bool,
) -> Vec<Vec<T>> {
    let at = |(clip, assembled): &(T, Option<u64>)| assembled.unwrap_or(span(clip).0);
    clips.sort_by(|a, b| at(a).cmp(&at(b)).then_with(|| order(&a.0, &b.0)));
    let mut assembled: Vec<Vec<(T, Option<u64>)>> = Vec::new();
    for entry in clips {
        let start = at(&entry);
        let lane = assembled.iter().position(|lane| {
            let last = lane.last().expect("lanes are never empty");
            joins(&lane[0].0, &entry.0) && at(last) + span(&last.0).1 <= start
        });
        match lane {
            Some(index) => assembled[index].push(entry),
            None => assembled.push(vec![entry]),
        }
    }
    let mut lanes: Vec<Vec<T>> = Vec::with_capacity(assembled.len());
    let mut moved = Vec::new();
    for (index, lane) in assembled.into_iter().enumerate() {
        let mut kept = Vec::with_capacity(lane.len());
        for (clip, assembled) in lane {
            match assembled {
                Some(_) => moved.push((index, clip)),
                None => kept.push(clip),
            }
        }
        lanes.push(kept);
    }
    moved.sort_by(|a, b| {
        span(&a.1)
            .0
            .cmp(&span(&b.1).0)
            .then_with(|| order(&a.1, &b.1))
    });
    for (lane, clip) in moved {
        let (start, length) = span(&clip);
        let free = |lane: &Vec<T>| {
            lane.first().is_none_or(|first| joins(first, &clip))
                && lane.iter().all(|other| {
                    let (other_start, other_length) = span(other);
                    other_start + other_length <= start || start + length <= other_start
                })
        };
        let index = if free(&lanes[lane]) {
            lane
        } else if let Some(index) = lanes.iter().position(free) {
            index
        } else {
            lanes.push(Vec::new());
            lanes.len() - 1
        };
        let lane = &mut lanes[index];
        let at = lane.partition_point(|other| {
            span(other).0 < start
                || (span(other).0 == start && order(other, &clip) != std::cmp::Ordering::Greater)
        });
        lane.insert(at, clip);
    }
    lanes.retain(|lane| !lane.is_empty());
    lanes
}

fn audio_manifest_linking(
    timeline: &align_core::export::ExportTimeline,
    cameras: &std::collections::HashSet<PathBuf>,
) -> Result<AudioManifest, AafError> {
    use std::collections::BTreeMap;
    let fail = |text: &str| AafError::Writer(text.to_owned());
    type Placed = (AudioClip, Option<u64>);
    let mut groups: BTreeMap<(String, u32, usize), Vec<Placed>> = BTreeMap::new();
    for island in &timeline.islands {
        let reserve_tail = island.reserved_end().is_some();
        for item in &island.clips {
            if item.clip.video.is_some()
                || item.linked_audio_edit.is_some()
                || item.is_retimed(align_core::MediaKind::Audio)
                || item.transition_for(align_core::MediaKind::Audio).is_some()
                || !item.flags.enabled
                || !item.flags.track_enabled
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
            let stems = &item.precision_audio_urls;
            let direct = stems.is_empty() && cameras.contains(&item.clip.url);
            if !direct && (stems.len() != audio.channels || audio.channels == 0) {
                return Err(fail("AAF requires prepared mono stems for every channel"));
            }
            let samples = |seconds: f64| -> Result<u64, AafError> {
                let value = seconds * rate;
                if !value.is_finite() || value < 0.0 || value >= u64::MAX as f64 {
                    return Err(fail("Invalid AAF sample position"));
                }
                Ok(value.round() as u64)
            };
            let start = samples(item.timeline_start(align_core::MediaKind::Audio))?;
            let assembled = item
                .assembled_start(align_core::MediaKind::Audio)
                .map(samples)
                .transpose()?;
            let length = samples(item.precision_source_duration())?;
            let (source_in, frames) = if direct {
                (
                    samples(item.precision_source_start())?,
                    samples(item.clip.duration.as_seconds())?,
                )
            } else {
                (0, length)
            };
            if length == 0 || start.checked_add(length).is_none() {
                return Err(fail("Invalid AAF clip length"));
            }
            let source = item
                .preferred_source_key(align_core::MediaKind::Audio)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    align_core::allocator::source_key_for_clip(
                        &item.clip.url,
                        item.clip.source_identifier.as_deref(),
                        None,
                    )
                });
            for channel in 0..audio.channels {
                if item
                    .selected_audio_source_channel()
                    .is_some_and(|selected| selected != channel)
                {
                    continue;
                }
                let clip = match stems.get(channel) {
                    None => AudioClip {
                        reserved_start: assembled.filter(|_| reserve_tail),
                        display_name: item.camera_clock.as_ref().and(item.display_name.clone()),
                        path: item.clip.url.clone(),
                        start,
                        source_in,
                        length,
                        source_frames: frames,
                        channels: audio.channels as u16,
                        channel: Some(channel),
                        metadata: None,
                    },
                    Some(stem) => AudioClip {
                        reserved_start: assembled.filter(|_| reserve_tail),
                        display_name: item.camera_clock.as_ref().and(item.display_name.clone()),
                        path: stem.clone(),
                        start,
                        source_in,
                        length,
                        source_frames: frames,
                        channels: 1,
                        channel: None,
                        metadata: None,
                    },
                };
                groups
                    .entry((source.clone(), rate as u32, channel))
                    .or_default()
                    .push((clip, assembled));
            }
        }
    }
    let mut tracks = Vec::new();
    for ((source, sample_rate, channel), clips) in groups {
        let lanes = pack_lanes(
            clips,
            |clip| (clip.start, clip.length),
            |a, b| a.path.cmp(&b.path),
            |_, _| true,
        );
        tracks.extend(
            lanes
                .into_iter()
                .enumerate()
                .map(|(index, clips)| AudioTrack {
                    name: format!("{} / channel {} / lane {}", source, channel + 1, index + 1),
                    sample_rate,
                    clips,
                }),
        );
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

/// QuickTime/MP4 metadata is collected here. For `.mxf` the AAF helper
/// reads the OP-Atom header for the package UMIDs, tracks, and descriptor.
fn picture_metadata(path: &Path) -> Option<serde_json::Value> {
    let is_mxf = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mxf"));
    if is_mxf {
        return Some(serde_json::json!({}));
    }
    crate::isobmff::ama_metadata(path)
}

/// Assemble an AAF manifest with an optional composition rate. Picture is
/// counted in composition frames, the timeline's unless overridden; each
/// file keeps its own rate.
pub fn timeline_manifest_with_frame_rate(
    timeline: &align_core::export::ExportTimeline,
    frame_duration: Option<align_core::MediaTime>,
    cancel: &AtomicBool,
) -> Result<serde_json::Value, AafError> {
    use serde_json::json;
    let fail = |message: &str| AafError::Writer(message.into());
    let composition_edit_rate = frame_duration
        .map(|frame| {
            if frame.value <= 0 || frame.timescale <= 0 {
                return Err(fail("Invalid AAF frame-rate override"));
            }
            Ok(json!({"numerator":frame.timescale,"denominator":frame.value}))
        })
        .transpose()?;
    // Camera sound without stems links the camera file itself.
    let cameras: std::collections::HashSet<PathBuf> = timeline
        .islands
        .iter()
        .flat_map(|island| &island.clips)
        .filter(|item| item.clip.video.is_some() && item.precision_audio_urls.is_empty())
        .map(|item| item.clip.url.clone())
        .collect();
    let mut audio = timeline.clone();
    for island in &mut audio.islands {
        island.clips.retain(|item| {
            !item.clip.audio.is_empty() && item.is_enabled(align_core::MediaKind::Audio)
        });
        for item in &mut island.clips {
            item.clip.video = None;
            item.clip.kind = align_core::MediaKind::Audio;
        }
    }
    let mut tracks = if audio.islands.iter().all(|island| island.clips.is_empty()) {
        Vec::new()
    } else {
        audio_manifest_linking(&audio, &cameras)?.tracks
    };
    let reserve_tail = timeline
        .islands
        .iter()
        .any(|island| island.reserved_end().is_some());
    let mut pictures = Vec::new();
    let mut metadata = std::collections::HashMap::new();
    for item in timeline.islands.iter().flat_map(|island| &island.clips) {
        let Some(video) = &item.clip.video else {
            continue;
        };
        if item.is_retimed(align_core::MediaKind::Video)
            || item.transition_for(align_core::MediaKind::Video).is_some()
            || !item.is_enabled(align_core::MediaKind::Video)
            || !item.flags.track_enabled
        {
            return Err(fail("AAF picture edit semantics not implemented"));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(AafError::Cancelled);
        }
        let clock = video
            .frame_duration
            .ok_or_else(|| fail("AAF picture requires a known frame rate"))?;
        if clock.value <= 0 || clock.timescale <= 0 {
            return Err(fail("Invalid AAF picture rate"));
        }
        let frame = |seconds: f64| -> Result<u64, AafError> {
            let value = seconds / clock.as_seconds();
            if !value.is_finite() || value < 0.0 || value >= (1u64 << 52) as f64 {
                return Err(fail("Invalid AAF picture frame range"));
            }
            Ok(value.round() as u64)
        };
        let start = frame(item.timeline_start(align_core::MediaKind::Video))?;
        let source_in = frame(item.selected_source_in(align_core::MediaKind::Video))?;
        let length = frame(item.selected_timeline_duration(align_core::MediaKind::Video))?;
        if length == 0 {
            return Err(fail("Empty AAF picture edit"));
        }
        if !metadata.contains_key(&item.clip.url)
            && let Some(value) = picture_metadata(&item.clip.url)
        {
            metadata.insert(item.clip.url.clone(), value);
        }
        if !metadata.contains_key(&item.clip.url) {
            let probe = crate::ff::ffprobe_bin()
                .ok_or_else(|| fail("AAF picture export requires ffprobe"))?;
            let mut output = tempfile::tempfile()?;
            let mut child = crate::process::command(probe)
                .args([
                    "-v",
                    "error",
                    "-show_format",
                    "-show_streams",
                    "-of",
                    "json",
                ])
                .arg(&item.clip.url)
                .stdin(Stdio::null())
                .stdout(Stdio::from(output.try_clone()?))
                .stderr(Stdio::inherit())
                .spawn()?;
            let status = crate::process::wait_cancellable(&mut child, cancel)?;
            if !status.success() {
                return Err(fail("AAF picture media inspection failed"));
            }
            use std::io::{Seek, SeekFrom};
            output.seek(SeekFrom::Start(0))?;
            let value: serde_json::Value = serde_json::from_reader(output)
                .map_err(|error| AafError::Writer(error.to_string()))?;
            metadata.insert(item.clip.url.clone(), value);
        }
        // The file's start timecode in picture frames, so editors that
        // locate media by timecode find the referenced frames.
        let timecode = video.source_timecode.as_ref().map(|tc| {
            let start =
                tc.frame_number as f64 * tc.frame_duration.as_seconds() / clock.as_seconds();
            json!({"start":start.round().max(0.0) as u64,
                "fps":(1.0 / clock.as_seconds()).round() as u64,"drop":tc.drop_frame})
        });
        let assembled = item
            .assembled_start(align_core::MediaKind::Video)
            .map(frame)
            .transpose()?;
        pictures.push((
            json!({"name":item.preferred_source_key(align_core::MediaKind::Video).unwrap_or("Video"),
            "edit_rate":{"numerator":clock.timescale,"denominator":clock.value},
            "clips":[{"path":item.clip.url,"start":start,"source_in":source_in,"length":length,
                "reserved_start":assembled.filter(|_| reserve_tail),"display_name":item.camera_clock.as_ref().and(item.display_name.as_ref()), "timecode":timecode,"metadata":metadata[&item.clip.url]}]}),
            assembled,
        ));
    }
    let span = |track: &serde_json::Value| {
        let clip = &track["clips"][0];
        (
            clip["start"].as_u64().unwrap(),
            clip["length"].as_u64().unwrap(),
        )
    };
    let lanes: Vec<serde_json::Value> = pack_lanes(
        pictures,
        span,
        |_, _| std::cmp::Ordering::Equal,
        |first, track| first["name"] == track["name"] && first["edit_rate"] == track["edit_rate"],
    )
    .into_iter()
    .map(|mut lane| {
        let clips: Vec<serde_json::Value> = lane
            .iter_mut()
            .map(|track| track["clips"][0].take())
            .collect();
        let mut track = lane.swap_remove(0);
        track["clips"] = serde_json::Value::Array(clips);
        track
    })
    .collect();
    // Linked camera sound shares its picture's media description.
    for clip in tracks.iter_mut().flat_map(|track| &mut track.clips) {
        if clip.channel.is_some() {
            clip.metadata = Some(
                metadata
                    .get(&clip.path)
                    .cloned()
                    .ok_or_else(|| fail("AAF camera sound has no picture link"))?,
            );
        }
    }
    // Editors count picture and sound in the composition's frames, so the
    // composition runs at the timeline's rate unless overridden.
    let composition_edit_rate = composition_edit_rate.or_else(|| {
        let frame = timeline.frame_duration;
        (frame.value > 0).then(|| json!({"numerator":frame.timescale,"denominator":frame.value}))
    });
    // Its timecode counts drop frames, as the first source's does, where
    // drop-frame labels exist: at 29.97 and 59.94.
    let drop_frame = composition_edit_rate.as_ref().is_some_and(|rate| {
        rate["denominator"] == 1001 && (rate["numerator"] == 30_000 || rate["numerator"] == 60_000)
    }) && timeline
        .islands
        .iter()
        .flat_map(|island| &island.clips)
        .find_map(|item| item.clip.source_timecode())
        .is_some_and(|timecode| timecode.drop_frame);
    Ok(
        json!({"version":2,"name":timeline.name,"tracks":tracks,"picture_tracks":lanes,
        "composition_edit_rate":composition_edit_rate,"composition_drop_frame":drop_frame}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pictures of cameras A and B share lanes. Moving A's C3 by camera
    /// clock into the time B0 plays must not move B0 off its lane.
    #[test]
    fn a_clip_moved_by_camera_clock_keeps_every_other_lane() {
        let lanes = |moved: Option<(u64, u64)>| {
            let mut clips = vec![
                (("C1", 100), None),
                (("C2", 400), None),
                (("B0", 700), None),
                (("C4", 900), None),
                (("B1", 1_142), None),
            ];
            if let Some((start, assembled)) = moved {
                clips.push((("C3", start), Some(assembled)));
            } else {
                clips.push((("C3", 961), None));
            }
            pack_lanes(
                clips,
                |&(_, start)| (start, 60),
                |a, b| a.0.cmp(b.0),
                |_, _| true,
            )
            .into_iter()
            .map(|lane| lane.into_iter().map(|(name, _)| name).collect::<Vec<_>>())
            .collect::<Vec<_>>()
        };
        let ordinary = lanes(None);
        assert_eq!(ordinary, [vec!["C1", "C2", "B0", "C4", "C3", "B1"]]);
        // Where its lane is taken, it opens a new last lane.
        assert_eq!(
            lanes(Some((690, 961))),
            [vec!["C1", "C2", "B0", "C4", "B1"], vec!["C3"]]
        );
        // Where its lane is free, it stays on it.
        assert_eq!(
            lanes(Some((550, 961))),
            [vec!["C1", "C2", "C3", "B0", "C4", "B1"]]
        );
    }

    #[test]
    fn picture_clock_and_separate_sound_survive_resolution() {
        let imported: ImportedAudio = serde_json::from_value(serde_json::json!({
            "version":2,"sequences":[{"name":"Camera edit","tracks":[
                {"name":"V1","media_kind":"picture","edit_rate":{"numerator":30000,"denominator":1001},
                 "clips":[{"path":"/camera.mov","start":7,"source_in":11,"length":101,"channel":null}]},
                {"name":"A1","media_kind":"sound","edit_rate":{"numerator":48000,"denominator":1},
                 "clips":[{"path":"/camera.mov","start":11211,"source_in":17618,"length":161762,"channel":1}]}
            ]}]
        })).unwrap();
        let draft = imported.into_draft(Path::new("camera.aaf"), None).unwrap();
        assert_eq!(
            draft.frame_duration,
            align_core::MediaTime::new(1001, 30000)
        );
        let clip: align_core::Clip = serde_json::from_value(serde_json::json!({
            "id":"camera","url":"/camera.mov","kind":"video","duration":{"value":10,"timescale":1},
            "audio":[{"sampleRate":48000.0,"channels":2}],
            "video":{"width":1920,"height":1080,"frameDuration":{"value":1001,"timescale":30000}}
        }))
        .unwrap();
        let resolved = draft.resolve(&[clip]);
        assert_eq!(resolved.edits.len(), 2);
        let video = &resolved.edits[0];
        assert_eq!(video.source_in, align_core::MediaTime::new(11011, 30000));
        assert_eq!(
            video.timeline_start,
            align_core::MediaTime::new(7007, 30000)
        );
        assert_eq!(video.source_out, align_core::MediaTime::new(112112, 30000));
        assert_eq!(video.audio_enabled, Some(false));
        assert!(video.linked_audio_edit.is_none());
        assert_eq!(resolved.edits[1].audio_source_channel, Some(1));
    }

    #[test]
    fn exact_tick_clock_preserves_boundaries_and_picture_frame_rate() {
        let imported: ImportedAudio = serde_json::from_value(serde_json::json!({
            "version":3,"sequences":[{"name":"Mixed clocks","tracks":[
                {"name":"V1","media_kind":"picture",
                 "edit_rate":{"numerator":30000,"denominator":1001},
                 "time_rate":{"numerator":240000,"denominator":1},
                 "clips":[{"path":"/camera.mov","start":8008,"source_in":249605,"length":24024}]}
            ]}]
        }))
        .unwrap();
        let draft = imported.into_draft(Path::new("mixed.aaf"), None).unwrap();
        assert_eq!(
            draft.frame_duration,
            align_core::MediaTime::new(1001, 30000)
        );
        let edit = &draft.edits[0];
        assert_eq!(edit.time_scale, 240000);
        assert!((edit.timeline_start - 1001.0 / 30000.0).abs() < 1e-12);
        assert!((edit.source_in - 49921.0 / 48000.0).abs() < 1e-12);
        assert!((edit.source_out - 273629.0 / 240000.0).abs() < 1e-12);
    }

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
                premiere::write(
                    &timeline,
                    TimelineExportFormat::PremiereXML,
                    premiere::PremiereOptions {
                        media_bins: true,
                        ..Default::default()
                    },
                ),
            ),
            (
                "fcpxml",
                fcpxml::write(&timeline, fcpxml::FcpxmlOptions::TIMELINE),
            ),
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
            serde_json::from_slice(&otio::data(&timeline, true).unwrap()).unwrap();
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
        let overridden = timeline_manifest_with_frame_rate(
            &timeline,
            Some(MediaTime::new(1001, 30000)),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            overridden["composition_edit_rate"],
            serde_json::json!({"numerator":30000,"denominator":1001})
        );
        assert_eq!(overridden["composition_drop_frame"], false);
        // A drop-frame source labels a 29.97 composition drop-frame; 25 has
        // no drop-frame labels.
        let mut dropped = timeline.clone();
        for item in &mut dropped.islands[0].clips {
            item.clip.audio[0].source_timecode = align_core::model::SourceTimecode::from_samples(
                48_000 * 60,
                48_000.0,
                MediaTime::new(1001, 30000),
                true,
            );
        }
        let manifest = |frame| {
            timeline_manifest_with_frame_rate(&dropped, frame, &AtomicBool::new(false)).unwrap()
        };
        assert_eq!(
            manifest(Some(MediaTime::new(1001, 30000)))["composition_drop_frame"],
            true
        );
        assert_eq!(manifest(None)["composition_drop_frame"], false);
    }
    /// A camera's sound without stems links the camera file's channels.
    #[test]
    fn camera_sound_without_stems_links_the_camera_file() {
        use align_core::export::{ExportIsland, ExportItem, ExportTimeline};
        use align_core::{Clip, MediaTime};
        let clip: Clip = serde_json::from_value(serde_json::json!({
            "id":"cam", "url":"/cameras/a.mov", "kind":"audio",
            "duration":{"value":10,"timescale":1},
            "audio":[{"sampleRate":48000.0,"channels":2,"bitDepth":16}]
        }))
        .unwrap();
        let mut item = ExportItem::new(clip, 0.04, 1.0, vec![], 1.0);
        item.source_in = 0.5;
        item.source_out = 2.5;
        item.timeline_duration = 2.0;
        let timeline = ExportTimeline::new(
            vec![ExportIsland {
                id: 0,
                clips: vec![item],
                duration: 2.04,
            }],
            MediaTime::new(1, 25),
            "Camera",
        );
        assert!(audio_manifest(&timeline).is_err());
        let cameras = [PathBuf::from("/cameras/a.mov")].into_iter().collect();
        let manifest = audio_manifest_linking(&timeline, &cameras).unwrap();
        let clips: Vec<_> = manifest
            .tracks
            .iter()
            .map(|track| &track.clips[0])
            .collect();
        assert_eq!(
            clips
                .iter()
                .map(|clip| (clip.channel, clip.start, clip.source_in, clip.length))
                .collect::<Vec<_>>(),
            vec![(Some(0), 1920, 24000, 96000), (Some(1), 1920, 24000, 96000)]
        );
        assert!(
            clips
                .iter()
                .all(|clip| clip.path == Path::new("/cameras/a.mov"))
        );
        assert_eq!(clips[0].source_frames, 480_000);
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
            read_timeline(Path::new("missing.aaf"), &AtomicBool::new(true)),
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
        assert_eq!(imported.sequences[0].tracks[0].clips[0].channel, Some(1));
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
        imported.version = 3;
        assert!(validate_import(&imported).is_err());
        imported.version = 1;
        imported.sequences.clear();
        assert!(validate_import(&imported).is_err());
    }
}
