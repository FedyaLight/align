//! Final Cut Pro FCPXML import: assets, story lanes, compounds, and multicams.

use super::*;

pub(super) fn fcpxml_time(text: Option<&str>) -> f64 {
    let mut s = text.unwrap_or("").trim().to_string();
    if s.is_empty() {
        return 0.0;
    }
    if s.ends_with('s') {
        s.pop();
    }
    if let Some((num, den)) = s.split_once('/')
        && let (Ok(n), Ok(d)) = (num.parse::<f64>(), den.parse::<f64>())
        && d != 0.0
    {
        return n / d;
    }
    s.parse().unwrap_or(0.0)
}

pub(super) fn fcpxml_frame_duration(text: Option<&str>) -> MediaTime {
    let mut s = text.unwrap_or("").trim().to_string();
    if s.is_empty() {
        return MediaTime::new(1, 25);
    }
    if s.ends_with('s') {
        s.pop();
    }
    if let Some((num, den)) = s.split_once('/')
        && let (Ok(n), Ok(d)) = (num.parse::<i64>(), den.parse::<i32>())
        && d != 0
    {
        return MediaTime::new(n, d);
    }
    if let Ok(seconds) = s.parse::<f64>()
        && seconds > 0.0
    {
        let fps = (1.0 / seconds).round().max(1.0) as i32;
        return MediaTime::new(1, fps);
    }
    MediaTime::new(1, 25)
}

pub(super) fn summaries_fcpxml(doc: &XmlDoc) -> Result<Vec<TimelineSequenceSummary>, ImportError> {
    let sequences: Vec<usize> = doc
        .descendants_named(doc.root, "sequence")
        .into_iter()
        .filter(|&s| !inside_media(doc, s))
        .collect();
    if sequences.is_empty() {
        return Err(ImportError::NoSequence(String::new()));
    }
    Ok(sequences
        .iter()
        .enumerate()
        .map(|(index, &seq)| {
            let project_name = doc.nodes[seq]
                .parent
                .and_then(|p| doc.attr(p, "name").map(str::to_string));
            let seq_name = doc.attr(seq, "name").map(str::to_string);
            let name = project_name
                .or(seq_name)
                .unwrap_or_else(|| format!("Sequence {}", index + 1));
            let mut clip_count = 0;
            let mut stack = vec![seq];
            while let Some(n) = stack.pop() {
                for &c in doc.nodes[n].children.iter().rev() {
                    let cn = &doc.nodes[c].name;
                    if cn == "asset-clip" || (cn == "clip" && !inside_clip(doc, c)) {
                        clip_count += 1;
                    }
                    stack.push(c);
                }
            }
            TimelineSequenceSummary {
                index,
                name,
                clip_count,
            }
        })
        .collect())
}

/// True when nested inside another `clip` (self excluded).
pub(super) fn inside_clip(doc: &XmlDoc, mut idx: usize) -> bool {
    while let Some(p) = doc.nodes[idx].parent {
        if doc.nodes[p].name == "clip" {
            return true;
        }
        idx = p;
    }
    false
}

/// True for compound-definition sequences (inside a `media` resource),
/// which are flattened through `ref-clip`, never picked as timelines.
pub(super) fn inside_media(doc: &XmlDoc, mut idx: usize) -> bool {
    while let Some(p) = doc.nodes[idx].parent {
        if doc.nodes[p].name == "media" {
            return true;
        }
        idx = p;
    }
    false
}

/// An FCPXML media location: a file URL, or a URL relative to the folder of
/// the document (`base`), which FCPXML allows. Other URL schemes are kept.
pub(super) fn fcpxml_url(src: &str, base: &Path) -> PathBuf {
    if let Some(rest) = src.strip_prefix("file://") {
        let rest = rest
            .strip_prefix("localhost/")
            .or_else(|| rest.strip_prefix("localhost"))
            .unwrap_or(rest);
        decoded_file_url_path(rest)
    } else if src.contains("://") {
        PathBuf::from(src)
    } else {
        relative_to(base, &percent_decode(src))
    }
}

/// `relative` resolved lexically against `base` (`.` and `..` folded).
pub(super) fn relative_to(base: &Path, relative: &str) -> PathBuf {
    use std::path::Component;
    let relative = Path::new(relative);
    if relative.is_absolute() {
        return relative.to_path_buf();
    }
    let mut resolved = base.to_path_buf();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

pub(super) fn read_fcpxml(
    doc: &XmlDoc,
    path: &Path,
    sequence_index: Option<usize>,
    prefer_proxies: bool,
) -> Result<TimelineDraft, ImportError> {
    let filename = ImportError::filename(path);
    let sequences: Vec<usize> = doc
        .descendants_named(doc.root, "sequence")
        .into_iter()
        .filter(|&s| !inside_media(doc, s))
        .collect();
    if sequences.is_empty() {
        return Err(ImportError::NoSequence(filename));
    }
    if sequences.len() > 1 && sequence_index.is_none() {
        return Err(ImportError::MultipleSequences(filename, sequences.len()));
    }
    let selected = sequence_index.unwrap_or(0);
    if selected >= sequences.len() {
        return Err(ImportError::InvalidSequence(filename, selected));
    }
    let seq = sequences[selected];

    let mut formats: HashMap<String, MediaTime> = HashMap::new();
    let mut stack = vec![doc.root];
    while let Some(n) = stack.pop() {
        if doc.nodes[n].name == "resources" {
            for f in doc.children_named(n, "format") {
                if let Some(id) = doc.attr(f, "id") {
                    formats.insert(
                        id.to_string(),
                        fcpxml_frame_duration(doc.attr(f, "frameDuration")),
                    );
                }
            }
        }
        for &c in doc.nodes[n].children.iter().rev() {
            stack.push(c);
        }
    }
    let frame_duration = doc
        .attr(seq, "format")
        .and_then(|id| formats.get(id).copied())
        .unwrap_or(MediaTime::new(1, 25));

    // FCPXML media URLs may be relative to the document's folder.
    let base = path.parent().unwrap_or_else(|| Path::new(""));
    let mut assets: HashMap<String, AssetInfo> = HashMap::new();
    let mut stack = vec![doc.root];
    while let Some(n) = stack.pop() {
        if doc.nodes[n].name == "resources" {
            // Direct + nested asset elements under resources.
            let mut inner = vec![n];
            while let Some(m) = inner.pop() {
                for &c in doc.nodes[m].children.iter().rev() {
                    if doc.nodes[c].name == "asset" {
                        let representations = doc.children_named(c, "media-rep");
                        let proxy = prefer_proxies
                            .then(|| {
                                representations
                                    .iter()
                                    .filter(|&&mr| doc.attr(mr, "kind") == Some("proxy-media"))
                                    .find_map(|&mr| doc.attr(mr, "src"))
                            })
                            .flatten();
                        let src = proxy
                            .or_else(|| {
                                representations
                                    .iter()
                                    .filter(|&&mr| doc.attr(mr, "kind") == Some("original-media"))
                                    .find_map(|&mr| doc.attr(mr, "src"))
                            })
                            .or_else(|| doc.attr(c, "src"))
                            .or_else(|| representations.iter().find_map(|&mr| doc.attr(mr, "src")));
                        let (Some(id), Some(src)) = (doc.attr(c, "id"), src) else {
                            continue;
                        };
                        assets.insert(
                            id.to_string(),
                            AssetInfo {
                                url: fcpxml_url(src, base),
                                start: fcpxml_time(doc.attr(c, "start")),
                                // Final Cut Pro leaves `hasVideo` out of a
                                // sound file's asset; picture has a format.
                                has_video: match doc.attr(c, "hasVideo") {
                                    Some(value) => value != "0",
                                    None => {
                                        doc.attr(c, "format").is_some()
                                            || doc.attr(c, "hasAudio").is_none()
                                    }
                                },
                                has_audio: doc.attr(c, "hasAudio") != Some("0"),
                            },
                        );
                    }
                    inner.push(c);
                }
            }
        }
        for &c in doc.nodes[n].children.iter().rev() {
            stack.push(c);
        }
    }

    let project_name = doc.nodes[seq]
        .parent
        .and_then(|p| doc.attr(p, "name").map(str::to_string));
    let name = project_name
        .or_else(|| doc.attr(seq, "name").map(str::to_string))
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("timeline")
                .to_string()
        });
    let mut edits: Vec<DraftEdit> = Vec::new();
    let mut next_number = 1;
    let mut warnings: Vec<SyncWarning> = Vec::new();

    // Compound (`sequence`) and multicam resources by media id.
    let mut compounds: HashMap<String, usize> = HashMap::new();
    let mut multicams: HashMap<String, usize> = HashMap::new();
    {
        let mut stack = vec![doc.root];
        while let Some(n) = stack.pop() {
            if doc.nodes[n].name == "media"
                && let Some(id) = doc.attr(n, "id").map(str::to_string)
            {
                if !doc.children_named(n, "sequence").is_empty() {
                    compounds.entry(id.clone()).or_insert(n);
                }
                if !doc.children_named(n, "multicam").is_empty() {
                    multicams.entry(id).or_insert(n);
                }
            }
            for &c in doc.nodes[n].children.iter().rev() {
                stack.push(c);
            }
        }
    }

    // Story elements under the sequence (containers recurse instead of
    // being descended into blindly, so nested media is placed exactly once).
    let refs = FcpxmlRefs {
        doc,
        path,
        assets: &assets,
        compounds: &compounds,
        multicams: &multicams,
    };
    let root = StoryPlacement {
        shift: -sequence_origin(doc, seq),
        enabled_and: true,
        lane_shift: 0,
        depth: 0,
    };
    read_fcpxml_story(
        &refs,
        &[seq],
        root,
        &mut Vec::new(),
        &mut warnings,
        &mut edits,
        &mut next_number,
    );
    Ok(TimelineDraft {
        source_url: path.to_path_buf(),
        name,
        frame_duration,
        edits,
        transitions: Vec::new(),
        warnings: unique_warnings(warnings),
    })
}

/// Where a sequence's spine starts. Final Cut Pro places it at the
/// sequence's timecode start; Align's earlier FCPXML placed it at zero.
fn sequence_origin(doc: &XmlDoc, sequence: usize) -> f64 {
    let tc_start = fcpxml_time(doc.attr(sequence, "tcStart"));
    let first = doc
        .children_named(sequence, "spine")
        .first()
        .and_then(|&spine| {
            doc.nodes[spine]
                .children
                .iter()
                .map(|&item| fcpxml_time(doc.attr(item, "offset")))
                .reduce(f64::min)
        });
    if first.is_some_and(|first| first >= tc_start - TIME_TOLERANCE) {
        tc_start
    } else {
        0.0
    }
}

pub(super) struct AssetInfo {
    pub(super) url: PathBuf,
    /// The file's own start: source times count from it.
    pub(super) start: f64,
    pub(super) has_video: bool,
    pub(super) has_audio: bool,
}

impl AssetInfo {
    /// Seconds into the file at FCPXML source time `time`. Align's earlier
    /// FCPXML counted source times from zero whatever the file's start, so
    /// a time before the start is taken as counted from zero.
    fn source_seconds(&self, time: f64) -> f64 {
        if time >= self.start - TIME_TOLERANCE {
            time - self.start
        } else {
            time
        }
    }
}

/// Shared FCPXML lookup tables for the story walker.
pub(super) struct FcpxmlRefs<'a> {
    pub(super) doc: &'a XmlDoc,
    pub(super) path: &'a Path,
    pub(super) assets: &'a HashMap<String, AssetInfo>,
    pub(super) compounds: &'a HashMap<String, usize>,
    pub(super) multicams: &'a HashMap<String, usize>,
}

/// Placement accumulated while descending FCPXML containers.
#[derive(Clone, Copy)]
pub(super) struct StoryPlacement {
    pub(super) shift: f64,
    pub(super) enabled_and: bool,
    pub(super) lane_shift: i64,
    pub(super) depth: usize,
}

pub(super) fn emit_fcpxml_asset_clip(
    refs: &FcpxmlRefs,
    clip: usize,
    place: StoryPlacement,
    next_number: &mut usize,
    edits: &mut Vec<DraftEdit>,
) {
    let (Some(id), Some(asset)) = (
        refs.doc.attr(clip, "ref"),
        refs.doc.attr(clip, "ref").and_then(|r| refs.assets.get(r)),
    ) else {
        return;
    };
    let _ = id;
    let offset = fcpxml_time(refs.doc.attr(clip, "offset"));
    let duration = fcpxml_time(refs.doc.attr(clip, "duration"));
    // Without a start the clip begins at the file's start.
    let start = refs
        .doc
        .attr(clip, "start")
        .map_or(0.0, |start| asset.source_seconds(fcpxml_time(Some(start))));
    let lane: i64 = refs
        .doc
        .attr(clip, "lane")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let enabled = refs.doc.attr(clip, "enabled") != Some("0") && place.enabled_and;
    let source_enable = refs.doc.attr(clip, "srcEnable").unwrap_or("all");
    let video_enabled = source_enable == "all" || source_enable == "video";
    let audio_enabled = source_enable == "all" || source_enable == "audio";
    let audio_role = refs.doc.attr(clip, "audioRole").map(str::to_string);
    let clip_name = refs
        .doc
        .attr(clip, "name")
        .map(str::to_string)
        .unwrap_or_else(|| {
            asset
                .url
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("clip")
                .to_string()
        });
    let n = *next_number;
    let video_id = format!("edit-{n}");
    let audio_id = format!("edit-{}", n + 1);
    *next_number = n + 2;
    if asset.has_video && video_enabled {
        edits.push(DraftEdit {
            id: video_id.clone(),
            name: Some(clip_name.clone()),
            url: asset.url.clone(),
            media_type: DraftMediaKind::Video,
            source_in: start,
            source_out: start + duration,
            timeline_start: offset + place.shift,
            timeline_end: offset + place.shift + duration,
            retime: Default::default(),

            fcp7: Default::default(),

            time_scale: 1_000_000,
            include_embedded_audio: true,
            audio_source_channel: None,
            fcpxml_audio_role: None,
            track_index: (lane + place.lane_shift).max(0) as usize,
            flags: crate::model::TrackFlags {
                enabled,
                ..Default::default()
            },

            linked_edit_ids: if asset.has_audio && audio_enabled {
                [audio_id.clone()].into_iter().collect()
            } else {
                HashSet::new()
            },
        });
    }
    if asset.has_audio && audio_enabled {
        edits.push(DraftEdit {
            id: audio_id.clone(),
            name: Some(clip_name.clone()),
            url: asset.url.clone(),
            media_type: DraftMediaKind::Audio,
            source_in: start,
            source_out: start + duration,
            timeline_start: offset + place.shift,
            timeline_end: offset + place.shift + duration,
            retime: Default::default(),

            fcp7: Default::default(),

            time_scale: 1_000_000,
            include_embedded_audio: true,
            audio_source_channel: {
                let components = refs.doc.children_named(clip, "audio-channel-source");
                if components.len() == 1 {
                    refs.doc
                        .attr(components[0], "srcCh")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .and_then(|channel| channel.checked_sub(1))
                } else {
                    None
                }
            },
            fcpxml_audio_role: audio_role.clone(),
            track_index: (lane + place.lane_shift).abs().max(0) as usize,
            flags: crate::model::TrackFlags {
                enabled,
                ..Default::default()
            },

            linked_edit_ids: if asset.has_video && video_enabled {
                [video_id.clone()].into_iter().collect()
            } else {
                HashSet::new()
            },
        });
    }
}

pub(super) fn emit_fcpxml_std_clip(
    refs: &FcpxmlRefs,
    clip: usize,
    place: StoryPlacement,
    next_number: &mut usize,
    edits: &mut Vec<DraftEdit>,
) {
    let offset = fcpxml_time(refs.doc.attr(clip, "offset"));
    let duration = fcpxml_time(refs.doc.attr(clip, "duration"));
    let start = fcpxml_time(refs.doc.attr(clip, "start"));
    let lane: i64 = refs
        .doc
        .attr(clip, "lane")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let enabled = refs.doc.attr(clip, "enabled") != Some("0") && place.enabled_and;
    let clip_name = refs.doc.attr(clip, "name").map(str::to_string);
    let mut media = Vec::new();
    {
        let mut stack = vec![clip];
        while let Some(n) = stack.pop() {
            for &c in refs.doc.nodes[n].children.iter().rev() {
                match refs.doc.nodes[c].name.as_str() {
                    "video" => media.push((c, DraftMediaKind::Video)),
                    "audio" => media.push((c, DraftMediaKind::Audio)),
                    _ => {}
                }
                stack.push(c);
            }
        }
    }
    // A clip's picture carries the file's sound only when the clip holds that
    // sound too; Final Cut Pro writes a muted camera's sound as its own clip.
    let holds_sound = |reference: Option<&str>| {
        media.iter().any(|&(element, kind)| {
            kind == DraftMediaKind::Audio && refs.doc.attr(element, "ref") == reference
        })
    };
    for &(element, media_type) in &media {
        let Some(asset) = refs
            .doc
            .attr(element, "ref")
            .and_then(|r| refs.assets.get(r))
        else {
            continue;
        };
        // The element sits at `offset` in the clip's own time, which the
        // clip shows from its `start`, and plays its file from `start`.
        let element_offset = fcpxml_time(refs.doc.attr(element, "offset"));
        let element_end = refs
            .doc
            .attr(element, "duration")
            .map_or(f64::INFINITY, |value| {
                element_offset + fcpxml_time(Some(value))
            });
        let local_in = start.max(element_offset);
        let local_out = (start + duration).min(element_end);
        if local_out <= local_in {
            continue;
        }
        let media_in = refs
            .doc
            .attr(element, "start")
            .map_or(0.0, |value| asset.source_seconds(fcpxml_time(Some(value))))
            + (local_in - element_offset);
        let timeline_start = offset + place.shift + (local_in - start);
        let edit_id = format!("edit-{next_number}");
        *next_number += 1;
        let audio = media_type == DraftMediaKind::Audio;
        edits.push(DraftEdit {
            id: edit_id,
            name: clip_name.clone().or_else(|| {
                asset
                    .url
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
            }),
            url: asset.url.clone(),
            media_type,
            source_in: media_in,
            source_out: media_in + (local_out - local_in),
            timeline_start,
            timeline_end: timeline_start + (local_out - local_in),
            retime: Default::default(),

            fcp7: Default::default(),

            time_scale: 1_000_000,
            include_embedded_audio: audio || holds_sound(refs.doc.attr(element, "ref")),
            audio_source_channel: if audio {
                refs.doc
                    .attr(element, "srcCh")
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .and_then(|channel| channel.checked_sub(1))
            } else {
                None
            },
            fcpxml_audio_role: if audio {
                refs.doc
                    .attr(element, "role")
                    .or_else(|| refs.doc.attr(clip, "audioRole"))
                    .map(str::to_string)
            } else {
                None
            },
            track_index: if audio {
                (lane + place.lane_shift).unsigned_abs() as usize
            } else {
                (lane + place.lane_shift).max(0) as usize
            },
            flags: crate::model::TrackFlags {
                enabled,
                ..Default::default()
            },

            linked_edit_ids: HashSet::new(),
        });
    }
}

// Recursive story walker: flat clips emit, compound/multicam/sync
// containers recurse, everything else is descended into. Timing is
// absolute seconds throughout; `place.shift` translates container-local
// offsets into the parent timeline.
pub(super) fn read_fcpxml_story(
    refs: &FcpxmlRefs,
    nodes: &[usize],
    place: StoryPlacement,
    visited: &mut Vec<String>,
    warnings: &mut Vec<SyncWarning>,
    edits: &mut Vec<DraftEdit>,
    next_number: &mut usize,
) {
    let warn = warner(refs.path);
    for &node in nodes {
        match refs.doc.nodes[node].name.as_str() {
            "asset-clip" => {
                emit_fcpxml_asset_clip(refs, node, place, &mut *next_number, &mut *edits)
            }
            "clip" => emit_fcpxml_std_clip(refs, node, place, &mut *next_number, &mut *edits),
            "ref-clip" => {
                let name = refs
                    .doc
                    .attr(node, "name")
                    .map(str::to_string)
                    .unwrap_or_else(|| "compound clip".to_string());
                let Some(media_id) = refs.doc.attr(node, "ref") else {
                    warn(
                        &mut *warnings,
                        format!("Compound clip {name} has no media reference; it is skipped."),
                    );
                    continue;
                };
                let Some(&seq_node) = refs.compounds.get(media_id) else {
                    warn(
                        &mut *warnings,
                        format!("Compound clip {name} cannot be resolved; it is skipped."),
                    );
                    continue;
                };
                if visited.iter().any(|v| v == media_id) {
                    warn(
                        &mut *warnings,
                        format!("Cyclic compound reference in {name}; it is skipped."),
                    );
                    continue;
                }
                if place.depth >= MAX_NESTED_DEPTH {
                    warn(
                        &mut *warnings,
                        format!("Compound clip {name} is nested too deep; it is skipped."),
                    );
                    continue;
                }
                let offset = fcpxml_time(refs.doc.attr(node, "offset")) + place.shift;
                let duration = fcpxml_time(refs.doc.attr(node, "duration"));
                let enabled = refs.doc.attr(node, "enabled") != Some("0") && place.enabled_and;
                let lane = refs
                    .doc
                    .attr(node, "lane")
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0)
                    + place.lane_shift;
                // Compound media wraps one sequence; its spine holds the
                // story elements, and the clip shows it from its start.
                let inner = refs
                    .doc
                    .children_named(seq_node, "sequence")
                    .first()
                    .copied();
                let start = refs.doc.attr(node, "start").map_or_else(
                    || inner.map_or(0.0, |s| sequence_origin(refs.doc, s)),
                    |start| fcpxml_time(Some(start)),
                );
                let spine =
                    inner.and_then(|s| refs.doc.children_named(s, "spine").first().copied());
                let Some(spine) = spine else {
                    warn(
                        &mut *warnings,
                        format!("Compound clip {name} has no timeline; it is skipped."),
                    );
                    continue;
                };
                let kids: Vec<usize> = refs.doc.nodes[spine].children.clone();
                visited.push(media_id.to_string());
                let base = edits.len();
                let child = StoryPlacement {
                    shift: offset - start,
                    enabled_and: enabled,
                    lane_shift: lane,
                    depth: place.depth + 1,
                };
                read_fcpxml_story(
                    refs,
                    &kids,
                    child,
                    &mut *visited,
                    &mut *warnings,
                    &mut *edits,
                    &mut *next_number,
                );
                visited.pop();
                // Keep only the referenced subrange.
                let mut tail: Vec<DraftEdit> = edits.drain(base..).collect();
                let mut kept = Vec::new();
                for mut e in tail.drain(..) {
                    if clip_edit_to_window(&mut e, offset, offset + duration) {
                        kept.push(e);
                    }
                }
                edits.extend(kept);
            }
            "sync-clip" => {
                let name = refs
                    .doc
                    .attr(node, "name")
                    .map(str::to_string)
                    .unwrap_or_else(|| "synchronized clip".to_string());
                let offset = fcpxml_time(refs.doc.attr(node, "offset")) + place.shift;
                let enabled = refs.doc.attr(node, "enabled") != Some("0") && place.enabled_and;
                let lane = refs
                    .doc
                    .attr(node, "lane")
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0)
                    + place.lane_shift;
                warn(
                    &mut *warnings,
                    format!("Synchronized clip {name} is flattened to its contained media."),
                );
                // Children offsets are relative to the sync clip start.
                let kids: Vec<usize> = refs.doc.nodes[node].children.clone();
                let base = edits.len();
                let child = StoryPlacement {
                    shift: offset,
                    enabled_and: enabled,
                    lane_shift: lane,
                    depth: place.depth + 1,
                };
                read_fcpxml_story(
                    refs,
                    &kids,
                    child,
                    &mut *visited,
                    &mut *warnings,
                    &mut *edits,
                    &mut *next_number,
                );
                let sync_duration = fcpxml_time(refs.doc.attr(node, "duration"));
                let mut tail: Vec<DraftEdit> = edits.drain(base..).collect();
                let mut kept = Vec::new();
                for mut e in tail.drain(..) {
                    if clip_edit_to_window(&mut e, offset, offset + sync_duration) {
                        kept.push(e);
                    }
                }
                edits.extend(kept);
            }
            "mc-clip" => {
                read_fcpxml_multicam(
                    refs,
                    node,
                    place,
                    &mut *visited,
                    &mut *warnings,
                    &mut *edits,
                    &mut *next_number,
                );
            }
            "audition" => {
                let picks: Vec<usize> = refs.doc.nodes[node]
                    .children
                    .iter()
                    .copied()
                    .filter(|&c| {
                        matches!(
                            refs.doc.nodes[c].name.as_str(),
                            "asset-clip" | "clip" | "ref-clip" | "sync-clip" | "mc-clip"
                        )
                    })
                    .collect();
                let Some(&first) = picks.first() else {
                    continue;
                };
                if picks.len() > 1 {
                    warn(
                        &mut *warnings,
                        "Audition alternatives beyond the active pick are ignored.".into(),
                    );
                }
                read_fcpxml_story(
                    refs,
                    &[first],
                    place,
                    &mut *visited,
                    &mut *warnings,
                    &mut *edits,
                    &mut *next_number,
                );
            }
            // Clips connected to a gap sit in the gap's own time.
            "gap" => {
                let child = StoryPlacement {
                    shift: place.shift + fcpxml_time(refs.doc.attr(node, "offset"))
                        - fcpxml_time(refs.doc.attr(node, "start")),
                    ..place
                };
                let kids = refs.doc.nodes[node].children.clone();
                read_fcpxml_story(
                    refs,
                    &kids,
                    child,
                    &mut *visited,
                    &mut *warnings,
                    &mut *edits,
                    &mut *next_number,
                );
            }
            "spine" => {
                let offset = fcpxml_time(refs.doc.attr(node, "offset"));
                let lane = refs
                    .doc
                    .attr(node, "lane")
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap_or(0);
                let child = StoryPlacement {
                    shift: place.shift + offset,
                    lane_shift: place.lane_shift + lane,
                    depth: place.depth + 1,
                    ..place
                };
                let kids = refs.doc.nodes[node].children.clone();
                read_fcpxml_story(
                    refs,
                    &kids,
                    child,
                    &mut *visited,
                    &mut *warnings,
                    &mut *edits,
                    &mut *next_number,
                );
            }
            _ => {
                let kids: Vec<usize> = refs.doc.nodes[node].children.clone();
                read_fcpxml_story(
                    refs,
                    &kids,
                    place,
                    &mut *visited,
                    &mut *warnings,
                    &mut *edits,
                    &mut *next_number,
                );
            }
        }
    }
}

// Flatten one FCPXML multicam clip: each mc-source segment takes its
// angle's media (Apple: angleID + srcEnable audio/video/all/none).
// Angles share one timeline, so the angle-time cursor advances with
// the segment position; segments are clipped to their own spans.
pub(super) fn read_fcpxml_multicam(
    refs: &FcpxmlRefs,
    node: usize,
    place: StoryPlacement,
    visited: &mut Vec<String>,
    warnings: &mut Vec<SyncWarning>,
    edits: &mut Vec<DraftEdit>,
    next_number: &mut usize,
) {
    let warn = warner(refs.path);
    let name = refs
        .doc
        .attr(node, "name")
        .map(str::to_string)
        .unwrap_or_else(|| "multicam clip".to_string());
    let Some(media_id) = refs.doc.attr(node, "ref") else {
        warn(
            &mut *warnings,
            format!("Multicam clip {name} has no media reference; it is skipped."),
        );
        return;
    };
    let Some(&mc_node) = refs.multicams.get(media_id) else {
        warn(
            &mut *warnings,
            format!("Multicam clip {name} cannot be resolved; it is skipped."),
        );
        return;
    };
    if place.depth >= MAX_NESTED_DEPTH {
        warn(
            &mut *warnings,
            format!("Multicam clip {name} is nested too deep; it is skipped."),
        );
        return;
    }
    let offset = fcpxml_time(refs.doc.attr(node, "offset")) + place.shift;
    let duration = fcpxml_time(refs.doc.attr(node, "duration"));
    let multicam = refs
        .doc
        .children_named(mc_node, "multicam")
        .first()
        .copied();
    // Without a start the clip shows its angles from the multicam's start.
    let start = refs.doc.attr(node, "start").map_or_else(
        || multicam.map_or(0.0, |m| fcpxml_time(refs.doc.attr(m, "tcStart"))),
        |start| fcpxml_time(Some(start)),
    );
    let enabled = refs.doc.attr(node, "enabled") != Some("0") && place.enabled_and;
    let lane = refs
        .doc
        .attr(node, "lane")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
        + place.lane_shift;
    let mc_end = offset + duration;
    let mut angles: HashMap<String, usize> = HashMap::new();
    if let Some(mcam) = multicam {
        for angle in refs.doc.children_named(mcam, "mc-angle") {
            if let Some(id) = refs.doc.attr(angle, "angleID") {
                angles.entry(id.to_string()).or_insert(angle);
            }
        }
    }
    if angles.is_empty() {
        warn(
            &mut *warnings,
            format!("Multicam clip {name} has no angles; it is skipped."),
        );
        return;
    }
    // (angle, srcEnable, segment start, segment end), absolute seconds.
    // Bare mc-source spans the whole clip; timed ones tile it.
    let mut segments: Vec<(String, String, f64, f64)> = Vec::new();
    for src in refs.doc.children_named(node, "mc-source") {
        let Some(angle_id) = refs.doc.attr(src, "angleID") else {
            warn(
                &mut *warnings,
                format!("A multicam segment in {name} has no angle; it is skipped."),
            );
            continue;
        };
        let enable = refs.doc.attr(src, "srcEnable").unwrap_or("all").to_string();
        if enable == "none" {
            continue;
        }
        let seg_lo = match refs.doc.attr(src, "offset") {
            Some(_) => fcpxml_time(refs.doc.attr(src, "offset")),
            None => offset,
        };
        let seg_hi = seg_lo
            + match refs.doc.attr(src, "duration") {
                Some(_) => fcpxml_time(refs.doc.attr(src, "duration")),
                None => mc_end - seg_lo,
            };
        segments.push((angle_id.to_string(), enable, seg_lo, seg_hi));
    }
    if segments.is_empty() {
        // Legacy 1.1 angle attributes, or a bare clip: whole span.
        let va = refs.doc.attr(node, "videoAngleID");
        let aa = refs.doc.attr(node, "audioAngleID");
        match (va, aa) {
            (Some(v), Some(a)) if v != a => {
                segments.push((v.to_string(), "video".to_string(), offset, mc_end));
                segments.push((a.to_string(), "audio".to_string(), offset, mc_end));
            }
            (Some(v), _) => segments.push((v.to_string(), "all".to_string(), offset, mc_end)),
            (_, Some(a)) => segments.push((a.to_string(), "all".to_string(), offset, mc_end)),
            (None, None) => {
                if let Some(first) = angles.keys().next().cloned() {
                    warn(
                        &mut *warnings,
                        format!(
                            "Multicam clip {name} has no angle selection; the first angle is used."
                        ),
                    );
                    segments.push((first, "all".to_string(), offset, mc_end));
                }
            }
        }
    }
    for (angle_id, enable, seg_lo, seg_hi) in segments {
        let Some(&angle_node) = angles.get(&angle_id) else {
            warn(
                &mut *warnings,
                format!(
                    "Multicam angle {angle_id} in {name} cannot be resolved; the segment is skipped."
                ),
            );
            continue;
        };
        if seg_hi <= seg_lo || seg_lo >= mc_end || seg_hi <= offset {
            warn(
                &mut *warnings,
                format!("A multicam segment in {name} lies outside its clip; it is skipped."),
            );
            continue;
        }
        let angle_cursor = start + (seg_lo - offset);
        let kids: Vec<usize> = refs.doc.nodes[angle_node].children.clone();
        let base = edits.len();
        let child = StoryPlacement {
            shift: seg_lo - angle_cursor,
            enabled_and: enabled,
            lane_shift: lane,
            depth: place.depth + 1,
        };
        read_fcpxml_story(
            refs,
            &kids,
            child,
            &mut *visited,
            &mut *warnings,
            &mut *edits,
            &mut *next_number,
        );
        let mut tail: Vec<DraftEdit> = edits.drain(base..).collect();
        let mut kept = Vec::new();
        for mut e in tail.drain(..) {
            let kind_ok = matches!(
                (enable.as_str(), e.media_type),
                ("all", _) | ("audio", DraftMediaKind::Audio) | ("video", DraftMediaKind::Video)
            );
            if kind_ok && clip_edit_to_window(&mut e, seg_lo, seg_hi) {
                kept.push(e);
            }
        }
        edits.extend(kept);
    }
}
