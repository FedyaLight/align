//! Final Cut Pro 7 XML (xmeml) import, also written by Premiere and Resolve.

use super::*;

pub(super) fn fcp7_rate(doc: &XmlDoc, idx: usize, fallback: MediaTime) -> MediaTime {
    let Some(rate) = doc.children_named(idx, "rate").first().copied() else {
        return fallback;
    };
    let Some(timebase) = doc.child_int(rate, "timebase").filter(|t| *t > 0) else {
        return fallback;
    };
    let ntsc = doc
        .child_text(rate, "ntsc")
        .is_some_and(|t| t.eq_ignore_ascii_case("TRUE"));
    if ntsc {
        MediaTime::new(1_001, (timebase * 1_000) as i32)
    } else {
        MediaTime::new(1, timebase as i32)
    }
}

pub(super) fn summaries_fcp7(
    doc: &XmlDoc,
    path: &Path,
) -> Result<Vec<TimelineSequenceSummary>, ImportError> {
    let sequences = top_sequences(doc);
    if sequences.is_empty() {
        return Err(ImportError::NoSequence(ImportError::filename(path)));
    }
    Ok(sequences
        .iter()
        .enumerate()
        .map(|(index, &seq)| {
            let name = doc
                .child_text(seq, "name")
                .unwrap_or_else(|| format!("Sequence {}", index + 1));
            let mut clip_count = 0;
            for media in ["video", "audio"] {
                // ./media/<video|audio>/track/clipitem
                for media_node in doc.children_named(seq, "media") {
                    for typed in doc.children_named(media_node, media) {
                        for track in doc.children_named(typed, "track") {
                            clip_count += doc.children_named(track, "clipitem").len();
                        }
                    }
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

pub(super) fn top_sequences(doc: &XmlDoc) -> Vec<usize> {
    // //sequence[not(ancestor::sequence)]
    let mut out = Vec::new();
    let mut stack = vec![(doc.root, false)];
    while let Some((idx, under_seq)) = stack.pop() {
        let is_seq = doc.nodes[idx].name == "sequence";
        if is_seq && !under_seq {
            out.push(idx);
        }
        for &c in doc.nodes[idx].children.iter().rev() {
            stack.push((c, under_seq || is_seq));
        }
    }
    out.reverse();
    out
}

pub(super) fn fcp7_files_by_id(doc: &XmlDoc) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    let mut stack = vec![doc.root];
    while let Some(idx) = stack.pop() {
        if doc.nodes[idx].name == "file"
            && doc.child_text(idx, "pathurl").is_some()
            && let Some(id) = doc.attr(idx, "id")
        {
            map.entry(id.to_string()).or_insert(idx);
        }
        for &c in doc.nodes[idx].children.iter().rev() {
            stack.push(c);
        }
    }
    map
}

/// file:// URL → local path (percent-decoded). Non-file URLs are None.
pub(super) fn local_file_path(value: &str) -> Option<PathBuf> {
    let rest = value
        .strip_prefix("file://")
        .or_else(|| value.strip_prefix("FILE://"))?;
    let rest = rest
        .strip_prefix("localhost/")
        .or_else(|| rest.strip_prefix("localhost"))
        .unwrap_or(rest);
    Some(decoded_file_url_path(rest))
}

pub(super) fn decoded_file_url_path(value: &str) -> PathBuf {
    let decoded = percent_decode(value);
    #[cfg(windows)]
    if decoded.as_bytes().first() == Some(&b'/')
        && decoded
            .as_bytes()
            .get(1)
            .is_some_and(u8::is_ascii_alphabetic)
        && decoded.as_bytes().get(2) == Some(&b':')
    {
        return PathBuf::from(&decoded[1..]);
    }
    PathBuf::from(decoded)
}

pub(super) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len() + 1
            && let (Some(h), Some(l)) = (hex_val(bytes.get(i + 1)), hex_val(bytes.get(i + 2)))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(super) fn hex_val(b: Option<&u8>) -> Option<u8> {
    match b.copied()? {
        v @ b'0'..=b'9' => Some(v - b'0'),
        v @ b'a'..=b'f' => Some(v - b'a' + 10),
        v @ b'A'..=b'F' => Some(v - b'A' + 10),
        _ => None,
    }
}

pub(super) struct ConstantRetime {
    pub(super) playback_rate: f64,
    pub(super) plays_backward: bool,
    pub(super) source_in: f64,
    pub(super) source_out: f64,
}

/// The constant speed, in percent, a Time Remap effect states, unless it
/// varies.
fn stated_speed(doc: &XmlDoc, effect: usize) -> Option<f64> {
    let parameter = |id: &str| {
        doc.children_named(effect, "parameter")
            .into_iter()
            .find(|&p| doc.child_text(p, "parameterid").as_deref() == Some(id))
            .and_then(|p| doc.child_text(p, "value"))
    };
    if parameter("variablespeed").as_deref().map(str::trim) != Some("0") {
        return None;
    }
    let speed: f64 = parameter("speed")?.trim().parse().ok()?;
    (speed.is_finite() && speed.abs() > 0.0).then_some(speed.abs())
}

pub(super) fn graph_retime(
    doc: &XmlDoc,
    effect: usize,
    file: usize,
    item_rate: MediaTime,
    graph_in: i64,
    graph_out: i64,
    require_linear: bool,
) -> Option<ConstantRetime> {
    let graph = doc
        .children_named(effect, "parameter")
        .into_iter()
        .find(|&p| doc.child_text(p, "parameterid").as_deref() == Some("graphdict"))?;
    let mut raw: Vec<(f64, f64)> = Vec::new();
    for kf in doc.children_named(graph, "keyframe") {
        let (Some(w), Some(v)) = (
            doc.child_text(kf, "when").and_then(|s| s.parse().ok()),
            doc.child_text(kf, "value").and_then(|s| s.parse().ok()),
        ) else {
            continue;
        };
        raw.push((w, v));
    }
    if raw.len() < 2 {
        return None;
    }
    let (rf0, rv0) = raw[0];
    let (rl0, rv1) = raw[raw.len() - 1];
    if rl0 <= rf0 {
        return None;
    }
    let raw_slope = (rv1 - rv0) / (rl0 - rf0);
    if raw_slope == 0.0 {
        return None;
    }
    let plays_backward = raw_slope < 0.0;
    let shift = if plays_backward { 1.0 } else { 0.0 };
    let points: Vec<(f64, f64)> = raw.iter().map(|&(w, v)| (w, v - shift)).collect();
    let (f0, fv0) = points[0];
    let (l0, lv1) = points[points.len() - 1];
    if l0 <= f0 {
        return None;
    }
    let mut slope = (lv1 - fv0) / (l0 - f0);
    if require_linear {
        let tolerance = 0.05f64.max((lv1 - fv0).abs() * 0.000_001);
        if !points
            .iter()
            .all(|&(w, v)| (v - (fv0 + (w - f0) * slope)).abs() <= tolerance)
        {
            return None;
        }
    }
    let file_rate = fcp7_rate(doc, file, item_rate);
    let item_fps = 1.0 / item_rate.as_seconds();
    let file_fps = 1.0 / file_rate.as_seconds();
    // Premiere states a constant speed, and the ends of its graph can be a
    // frame off it: a 200% reverse runs from 400 to 1, not to 0. A stated
    // speed the graph agrees with wins.
    if let Some(percent) = stated_speed(doc, effect) {
        let stated = percent / 100.0 * file_fps / item_fps;
        if (slope.abs() - stated).abs() <= stated * 0.01 {
            slope = stated.copysign(slope);
        }
    }
    let at = |frame: i64| fv0 + (frame as f64 - f0) * slope;
    let (a, b) = (at(graph_in) / file_fps, at(graph_out) / file_fps);
    let (source_in, source_out) = (a.min(b), a.max(b));
    let playback_rate = slope.abs() * item_fps / file_fps;
    if source_in < 0.0 || source_out <= source_in || !playback_rate.is_finite() {
        return None;
    }
    Some(ConstantRetime {
        playback_rate,
        plays_backward,
        source_in,
        source_out,
    })
}

pub(super) fn adjacent_transition_frame(
    doc: &XmlDoc,
    field: &str,
    items: &[usize],
    index: usize,
) -> Option<i64> {
    let item = *items.get(index)?;
    if doc.nodes[item].name != "transitionitem" {
        return None;
    }
    doc.child_int(item, field).filter(|v| *v >= 0)
}

/// Maximum nesting depth for nested sequences / compound clips / multicam
/// content. Real timelines nest 1–3 deep; the cap only guards against
/// cyclic references (possible through FCPXML idrefs).
pub(super) const MAX_NESTED_DEPTH: usize = 8;

/// Intersect an edit's timeline range with `[lo, hi]` (same time units)
/// and remap source in/out proportionally, honouring playback direction.
/// Returns false when nothing of the edit remains. Shared by nested
/// FCP 7 sequences, multiclips and FCPXML compound/multicam segments so
/// flattened media is never guessed outside its container window.
pub(super) fn clip_edit_to_window(edit: &mut DraftEdit, lo: f64, hi: f64) -> bool {
    let lo = edit.timeline_start.max(lo);
    let hi = edit.timeline_end.min(hi);
    if hi <= lo {
        return false;
    }
    let rate = edit.playback_rate;
    let src_at = |t: f64| {
        if edit.plays_backward {
            edit.source_out - (t - edit.timeline_start) * rate
        } else {
            edit.source_in + (t - edit.timeline_start) * rate
        }
    };
    let (a, b) = (src_at(lo), src_at(hi));
    edit.source_in = a.min(b);
    edit.source_out = a.max(b);
    edit.timeline_start = lo;
    edit.timeline_end = hi;
    true
}

/// Parent placement of one nested container: source window frames plus
/// identity for warnings and id prefixes.
pub(super) struct NestedPlacement {
    pub(super) frames: (i64, i64, i64, i64),
    pub(super) id: String,
    pub(super) name: String,
    pub(super) enabled: bool,
}

/// Shared context for one track's clip items, top-level or nested.
/// Groups the long parameter lists so clippy stays quiet and nesting
/// passes one value instead of ten.
pub(super) struct TrackCtx<'a> {
    pub(super) doc: &'a XmlDoc,
    pub(super) files_by_id: &'a HashMap<String, usize>,
    pub(super) path: &'a Path,
    /// Frame rate of the current level (nested rate when nested).
    pub(super) sequence_rate: MediaTime,
    pub(super) media_type: DraftMediaKind,
    /// 1-based track number for plain leaf edits on this track.
    pub(super) track_number: usize,
    pub(super) track_enabled: bool,
    pub(super) track_locked: bool,
    pub(super) depth: usize,
    pub(super) window: Option<NestedWindow>,
}

/// Affine window mapping nested-sequence (or multiclip-angle) seconds to
/// parent-timeline seconds for one nested placement.
#[derive(Clone, Debug)]
pub(super) struct NestedWindow {
    /// Nested window bounds, nested seconds.
    pub(super) nested_in: f64,
    pub(super) nested_out: f64,
    /// Parent placement start, parent seconds.
    pub(super) parent_start: f64,
    /// Parent seconds per nested second (1.0 without retime/rate change).
    pub(super) speed: f64,
    /// Prefix for flattened edit ids (keeps them unique).
    pub(super) id_prefix: String,
    /// Parent clipitem enabled flag (AND-combined).
    pub(super) parent_enabled: bool,
    /// Drop verbatim remap payloads (parent speed makes them inconsistent).
    pub(super) drop_verbatim: bool,
}

/// Stable per-track clip ids (`id` attr or generated fallback).
pub(super) fn stable_clip_ids(
    doc: &XmlDoc,
    items: &[usize],
    media_type: DraftMediaKind,
    track_number: usize,
) -> HashMap<usize, String> {
    let mut clip_ids: HashMap<usize, String> = HashMap::new();
    let mut next_number = 1;
    for (item_index, &item) in items.iter().enumerate() {
        if doc.nodes[item].name != "clipitem" {
            continue;
        }
        let id = doc
            .attr(item, "id")
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}-{}-{next_number}", media_type.raw(), track_number));
        clip_ids.insert(item_index, id);
        next_number += 1;
    }
    clip_ids
}

pub(super) fn read_fcp7(
    doc: &XmlDoc,
    path: &Path,
    sequence_index: Option<usize>,
) -> Result<TimelineDraft, ImportError> {
    let filename = ImportError::filename(path);
    let sequences = top_sequences(doc);
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
    let sequence = sequences[selected];
    let sequence_rate = fcp7_rate(doc, sequence, MediaTime::new(1, 25));
    let name = doc.child_text(sequence, "name").unwrap_or_else(|| {
        path.file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("timeline")
            .to_string()
    });
    let files_by_id = fcp7_files_by_id(doc);

    let mut edits: Vec<DraftEdit> = Vec::new();
    let mut transitions: Vec<DraftTransition> = Vec::new();
    let mut warnings: Vec<SyncWarning> = Vec::new();
    let warn = warner(path);

    for (media_type, media_name) in [
        (DraftMediaKind::Video, "video"),
        (DraftMediaKind::Audio, "audio"),
    ] {
        let mut tracks: Vec<usize> = Vec::new();
        for media in doc.children_named(sequence, "media") {
            for typed in doc.children_named(media, media_name) {
                tracks.extend(doc.children_named(typed, "track"));
            }
        }

        for (track_offset, &track) in tracks.iter().enumerate() {
            let track_enabled = doc
                .child_text(track, "enabled")
                .is_none_or(|t| !t.eq_ignore_ascii_case("FALSE"));
            let track_locked = doc
                .child_text(track, "locked")
                .is_some_and(|t| t.eq_ignore_ascii_case("TRUE"));
            let items: Vec<usize> = doc.nodes[track]
                .children
                .iter()
                .copied()
                .filter(|&c| {
                    doc.nodes[c].name == "clipitem" || doc.nodes[c].name == "transitionitem"
                })
                .collect();

            // Stable per-track clip ids.
            let clip_ids = stable_clip_ids(doc, &items, media_type, track_offset + 1);

            // Transitions reference neighbours by position.
            for (item_index, &item) in items.iter().enumerate() {
                if doc.nodes[item].name != "transitionitem" {
                    continue;
                }
                let mut bad = || {
                    warn(
                        &mut warnings,
                        "An unsupported transition was flattened to overlapping media.".into(),
                    );
                };
                let (Some(left_id), Some(right_id)) = (
                    clip_ids.get(&item_index.wrapping_sub(1)).cloned(),
                    clip_ids.get(&(item_index + 1)).cloned(),
                ) else {
                    bad();
                    continue;
                };
                let (Some(&left_item), Some(&right_item)) = (
                    items.get(item_index.wrapping_sub(1)),
                    items.get(item_index + 1),
                ) else {
                    bad();
                    continue;
                };
                let (Some(start), Some(end)) =
                    (doc.child_int(item, "start"), doc.child_int(item, "end"))
                else {
                    bad();
                    continue;
                };
                if start < 0 || end <= start {
                    bad();
                    continue;
                }
                let Some(effect) = doc.children_named(item, "effect").first().copied() else {
                    bad();
                    continue;
                };
                let is_transition = doc
                    .child_text(effect, "effecttype")
                    .is_some_and(|t| t.eq_ignore_ascii_case("transition"))
                    && doc
                        .child_text(effect, "mediatype")
                        .is_some_and(|t| t.eq_ignore_ascii_case(media_type.raw()));
                if !is_transition {
                    bad();
                    continue;
                }
                if media_type == DraftMediaKind::Video {
                    let ids = [
                        doc.child_text(effect, "effectid"),
                        doc.child_text(effect, "name"),
                    ]
                    .into_iter()
                    .flatten()
                    .map(|s| s.to_lowercase());
                    if !ids.into_iter().any(|s| s.contains("cross dissolve")) {
                        bad();
                        continue;
                    }
                }
                let alignment = doc
                    .child_text(item, "alignment")
                    .map(|s| s.to_lowercase())
                    .unwrap_or_else(|| "center".to_string());
                let transition_rate = fcp7_rate(doc, item, sequence_rate);
                let left_rate = fcp7_rate(doc, left_item, sequence_rate);
                let right_rate = fcp7_rate(doc, right_item, sequence_rate);
                let start_ratio: f64 = doc
                    .child_text(effect, "startratio")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let end_ratio: f64 = doc
                    .child_text(effect, "endratio")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1.0);
                let reverse = doc
                    .child_text(effect, "reverse")
                    .is_some_and(|t| t.eq_ignore_ascii_case("TRUE"));
                let effect_children: HashSet<String> = doc.nodes[effect]
                    .children
                    .iter()
                    .map(|&c| doc.nodes[c].name.clone())
                    .collect();
                let portable_children: HashSet<String> = [
                    "name",
                    "effectid",
                    "effectcategory",
                    "effecttype",
                    "mediatype",
                    "startratio",
                    "endratio",
                    "reverse",
                ]
                .into_iter()
                .map(str::to_string)
                .collect();
                let half = (end - start) / 2;
                let left_in = doc.child_int(left_item, "in").unwrap_or(0);
                let left_out = doc.child_int(left_item, "out").unwrap_or(0);
                let right_in = doc.child_int(right_item, "in").unwrap_or(0);
                let right_out = doc.child_int(right_item, "out").unwrap_or(0);
                let seq_s = sequence_rate.as_seconds();
                let portable = media_type == DraftMediaKind::Video
                    && alignment == "center"
                    && (end - start) % 2 == 0
                    && (transition_rate.as_seconds() - seq_s).abs() < 0.000_000_001
                    && (left_rate.as_seconds() - seq_s).abs() < 0.000_000_001
                    && (right_rate.as_seconds() - seq_s).abs() < 0.000_000_001
                    && doc.child_int(left_item, "end") == Some(-1)
                    && doc.child_int(right_item, "start") == Some(-1)
                    && left_out - half > left_in
                    && right_in + half < right_out
                    && start_ratio.abs() < 0.000_001
                    && (end_ratio - 1.0).abs() < 0.000_001
                    && !reverse
                    && effect_children.is_subset(&portable_children);
                transitions.push(DraftTransition {
                    kind: if media_type == DraftMediaKind::Video {
                        TimelineTransitionKind::CrossDissolve
                    } else {
                        TimelineTransitionKind::AudioTransition
                    },
                    media_type,
                    left_edit_id: left_id.clone(),
                    right_edit_id: right_id.clone(),
                    start: start as f64 * seq_s,
                    end: end as f64 * seq_s,
                    alignment,
                    effect_xml: doc.verbatim(effect).to_string(),
                    transition_xml: doc.verbatim(item).to_string(),
                    is_otio_portable: portable,
                });
            }

            let ctx = TrackCtx {
                doc,
                files_by_id: &files_by_id,
                path,
                sequence_rate,
                media_type,
                track_number: track_offset + 1,
                track_enabled,
                track_locked,
                depth: 0,
                window: None,
            };
            read_track_items(&ctx, &items, &clip_ids, &mut warnings, &mut edits);
        }
    }

    // Embedded-audio transition support + portability warnings.
    let mut edits_by_id: HashMap<String, &DraftEdit> = HashMap::new();
    for e in &edits {
        edits_by_id
            .entry(format!("{}:{}", e.media_type.raw(), e.id))
            .or_insert(e);
    }
    let video_paths: HashSet<String> = edits
        .iter()
        .filter(|e| e.media_type == DraftMediaKind::Video)
        .map(|e| e.url.to_string_lossy().into_owned())
        .collect();
    let linked_video = |audio: &DraftEdit| -> Option<&DraftEdit> {
        let mut found = None;
        for v in edits
            .iter()
            .filter(|e| e.media_type == DraftMediaKind::Video)
        {
            if v.url.to_string_lossy() == audio.url.to_string_lossy()
                && (audio.linked_edit_ids.contains(&v.id) || v.linked_edit_ids.contains(&audio.id))
            {
                if found.is_some() {
                    return None;
                }
                found = Some(v);
            }
        }
        found
    };
    let unsupported_embedded = |t: &DraftTransition| -> bool {
        if t.media_type != DraftMediaKind::Audio {
            return false;
        }
        let (Some(left), Some(right)) = (
            edits_by_id.get(&format!("audio:{}", t.left_edit_id)),
            edits_by_id.get(&format!("audio:{}", t.right_edit_id)),
        ) else {
            return false;
        };
        if !video_paths.contains(&left.url.to_string_lossy().into_owned())
            && !video_paths.contains(&right.url.to_string_lossy().into_owned())
        {
            return false;
        }
        linked_video(left).is_none() || linked_video(right).is_none()
    };
    let unsupported_count = transitions
        .iter()
        .filter(|t| unsupported_embedded(t))
        .count();
    transitions.retain(|t| !unsupported_embedded(t));
    if unsupported_count > 0 {
        warn(
            &mut warnings,
            "An audio transition on embedded camera audio was flattened because its FCP link identity is missing or ambiguous.".into(),
        );
    }
    let video_transitions: Vec<&DraftTransition> = transitions
        .iter()
        .filter(|t| t.media_type == DraftMediaKind::Video)
        .collect();
    let portable_count = video_transitions
        .iter()
        .filter(|t| {
            t.is_otio_portable
                && edits_by_id
                    .get(&format!("video:{}", t.left_edit_id))
                    .is_some_and(|e| e.playback_rate == 1.0)
                && edits_by_id
                    .get(&format!("video:{}", t.right_edit_id))
                    .is_some_and(|e| e.playback_rate == 1.0)
                && edits_by_id
                    .get(&format!("video:{}", t.left_edit_id))
                    .is_some_and(|e| !e.plays_backward)
                && edits_by_id
                    .get(&format!("video:{}", t.right_edit_id))
                    .is_some_and(|e| !e.plays_backward)
        })
        .count();
    if portable_count < video_transitions.len() {
        warn(
            &mut warnings,
            "A Cross Dissolve is preserved in Premiere XML but flattened in Resolve OTIO because its timing or effect payload is not safely portable.".into(),
        );
    }
    if portable_count > 0 {
        warn(
            &mut warnings,
            "Cross Dissolve is preserved in Premiere XML, Resolve XML, Resolve OTIO, and the Resolve import script; FCPXML does not carry transitions.".into(),
        );
    }
    if transitions
        .iter()
        .any(|t| t.media_type == DraftMediaKind::Audio)
    {
        warn(
            &mut warnings,
            "Audio transitions are preserved in Premiere XML when clip identity is unambiguous; Resolve OTIO and the Resolve import script keep them as crossfades only when portable, and FCPXML does not carry transitions.".into(),
        );
    }
    if edits
        .iter()
        .any(|e| e.plays_backward || (e.playback_rate - 1.0).abs() > RATE_TOLERANCE)
    {
        warn(
            &mut warnings,
            "Speed changes are kept in Premiere XML, Resolve XML, and Resolve OTIO; FCPXML plays these clips at normal speed.".into(),
        );
    }
    if edits.iter().any(|e| e.plays_backward)
        && edits
            .iter()
            .any(|e| e.plays_backward && (e.playback_rate - 1.0).abs() > RATE_TOLERANCE)
    {
        warn(
            &mut warnings,
            "Reverse Time Remap is preserved exactly in Premiere XML; live-verified Resolve playback is frame-exact at 100% reverse speed, while other speeds can differ from Premiere by one frame at the clip boundaries.".into(),
        );
    }
    if edits.is_empty() {
        return Err(ImportError::NoMedia(filename));
    }
    Ok(TimelineDraft {
        source_url: path.to_path_buf(),
        name,
        frame_duration: sequence_rate,
        edits,
        transitions,
        warnings: unique_warnings(warnings),
    })
}

pub(super) fn read_track_items(
    ctx: &TrackCtx,
    items: &[usize],
    clip_ids: &HashMap<usize, String>,
    warnings: &mut Vec<SyncWarning>,
    edits: &mut Vec<DraftEdit>,
) {
    let warn = warner(ctx.path);
    let mut clip_offset = 0usize;
    for (item_offset, &clip_item) in items.iter().enumerate() {
        if ctx.doc.nodes[clip_item].name == "transitionitem" {
            if ctx.depth > 0 {
                warn(
                    &mut *warnings,
                    "Transitions inside nested sequences are flattened to overlapping media without the transition effect.".into(),
                );
            }
            continue;
        }
        if ctx.doc.nodes[clip_item].name != "clipitem" {
            continue;
        }
        clip_offset += 1;
        let file = (|| {
            let inline = ctx.doc.children_named(clip_item, "file").first().copied()?;
            if ctx.doc.child_text(inline, "pathurl").is_some() {
                return Some(inline);
            }
            let id = ctx.doc.attr(inline, "id")?;
            ctx.files_by_id.get(id).copied()
        })();
        let path_text = file.and_then(|f| ctx.doc.child_text(f, "pathurl"));
        let (Some(file), Some(path_text)) = (file, path_text) else {
            if read_nested_clipitem(
                ctx,
                clip_item,
                items,
                item_offset,
                clip_ids,
                &mut *warnings,
                &mut *edits,
            ) {
                continue;
            }
            warn(
                &mut *warnings,
                "A clip item has no readable local file path.".into(),
            );
            continue;
        };
        let Some(media_url) = local_file_path(&path_text) else {
            warn(
                &mut *warnings,
                "A clip item has no readable local file path.".into(),
            );
            continue;
        };
        let item_rate = fcp7_rate(
            ctx.doc,
            clip_item,
            fcp7_rate(ctx.doc, file, ctx.sequence_rate),
        );
        let fps = 1.0 / item_rate.as_seconds();
        let source_in = ctx.doc.child_int(clip_item, "in").unwrap_or(0);
        let source_out = ctx
            .doc
            .child_int(clip_item, "out")
            .or_else(|| ctx.doc.child_int(file, "duration"))
            .or_else(|| ctx.doc.child_int(clip_item, "duration"))
            .unwrap_or(0);
        let raw_start = ctx.doc.child_int(clip_item, "start").unwrap_or(0);
        let start_frames = if raw_start == -1 {
            adjacent_transition_frame(ctx.doc, "start", items, item_offset.wrapping_sub(1))
                .unwrap_or(-1)
        } else {
            raw_start
        };
        let end_frames = if ctx.doc.child_int(clip_item, "end") == Some(-1) {
            adjacent_transition_frame(ctx.doc, "end", items, item_offset + 1).unwrap_or(-1)
        } else {
            ctx.doc
                .child_int(clip_item, "end")
                .unwrap_or_else(|| start_frames + (source_out - source_in).max(0))
        };
        if source_in < 0
            || source_out <= source_in
            || start_frames < 0
            || end_frames <= start_frames
        {
            warn(
                &mut *warnings,
                format!(
                    "Invalid or incomplete timing in {} cannot be preserved.",
                    ctx.doc
                        .child_text(clip_item, "name")
                        .unwrap_or_else(|| "clip item".into())
                ),
            );
            continue;
        }
        let clip_name = || {
            ctx.doc
                .child_text(clip_item, "name")
                .unwrap_or_else(|| "clip item".to_string())
        };
        let source_seconds = (source_out - source_in) as f64 / fps;
        let timeline_seconds = (end_frames - start_frames) as f64 * ctx.sequence_rate.as_seconds();
        let timing_tolerance = item_rate.as_seconds().max(ctx.sequence_rate.as_seconds()) * 2.0;
        // Enabled timeremap filters only.
        let mut remap_effects = Vec::new();
        let mut other_filters = Vec::new();
        for filter in ctx.doc.children_named(clip_item, "filter") {
            if ctx
                .doc
                .child_text(filter, "enabled")
                .is_some_and(|t| t.eq_ignore_ascii_case("FALSE"))
            {
                continue;
            }
            let mut filter_has_remap = false;
            for effect in ctx.doc.children_named(filter, "effect") {
                if ctx.doc.child_text(effect, "effectid").as_deref() == Some("timeremap") {
                    remap_effects.push(effect);
                    filter_has_remap = true;
                }
            }
            if !filter_has_remap {
                other_filters.push(ctx.doc.verbatim(filter).to_string());
            }
        }
        let mut retime = None;
        let mut variable = false;
        if remap_effects.len() == 1 {
            retime = graph_retime(
                ctx.doc,
                remap_effects[0],
                file,
                item_rate,
                source_in,
                source_out,
                true,
            );
            if retime.is_none() {
                retime = graph_retime(
                    ctx.doc,
                    remap_effects[0],
                    file,
                    item_rate,
                    source_in,
                    source_out,
                    false,
                );
                variable = retime.is_some();
            }
        }
        if !remap_effects.is_empty() && retime.is_none() {
            warn(
                &mut *warnings,
                format!(
                    "Variable or malformed retime in {} cannot be preserved safely.",
                    clip_name()
                ),
            );
            continue;
        }
        if variable {
            warn(
                &mut *warnings,
                format!(
                    "Variable speed change in {} is preserved exactly in Premiere XML; Resolve OTIO receives the average constant speed across the clip.",
                    clip_name()
                ),
            );
        }
        if let Some(r) = &retime
            && ((timeline_seconds * r.playback_rate) - (r.source_out - r.source_in)).abs()
                > timing_tolerance
        {
            warn(
                &mut *warnings,
                format!(
                    "Retimed clip {} declares speed {} but its duration does not match the source span; NLEs may clamp this edit differently.",
                    clip_name(),
                    r.playback_rate
                ),
            );
        }
        if retime.is_none() && (source_seconds - timeline_seconds).abs() > timing_tolerance {
            warn(
                &mut *warnings,
                format!(
                    "Retimed clip {} has no readable Time Remap graph.",
                    clip_name()
                ),
            );
            continue;
        }
        if !other_filters.is_empty() {
            warn(
                &mut *warnings,
                format!(
                    "Clip effects in {} are passed through to Premiere XML unchanged; Resolve OTIO drops them.",
                    clip_name()
                ),
            );
        }
        // Verbatim <filter> payload of the timeremap (round-trips to Premiere).
        let remap_filter_xml = if retime.is_some() {
            remap_effects.first().and_then(|&e| {
                ctx.doc.nodes[e]
                    .parent
                    .map(|p| ctx.doc.verbatim(p).to_string())
            })
        } else {
            None
        };
        let mut edit = DraftEdit {
            id: clip_ids.get(&item_offset).cloned().unwrap_or_else(|| {
                format!(
                    "{}-{}-{clip_offset}",
                    ctx.media_type.raw(),
                    ctx.track_number
                )
            }),
            name: ctx.doc.child_text(clip_item, "name"),
            url: media_url,
            media_type: ctx.media_type,
            source_in: retime
                .as_ref()
                .map_or(source_in as f64 / fps, |r| r.source_in),
            source_out: retime
                .as_ref()
                .map_or(source_out as f64 / fps, |r| r.source_out),
            timeline_start: start_frames as f64 * ctx.sequence_rate.as_seconds(),
            timeline_end: end_frames as f64 * ctx.sequence_rate.as_seconds(),
            playback_rate: retime.as_ref().map_or(1.0, |r| r.playback_rate),
            plays_backward: retime.as_ref().is_some_and(|r| r.plays_backward),
            fcp7_time_remap_xml: remap_filter_xml,
            fcp7_filter_xmls: other_filters,
            fcp7_retime_in: retime.as_ref().map(|_| source_in),
            fcp7_retime_out: retime.as_ref().map(|_| source_out),
            fcp7_retime_duration: if retime.is_some() {
                ctx.doc.child_int(clip_item, "duration")
            } else {
                None
            },
            fcp7_labels_xml: ctx
                .doc
                .children_named(clip_item, "labels")
                .first()
                .map(|&labels| ctx.doc.verbatim(labels).to_string()),
            time_scale: 1_000_000,
            include_embedded_audio: true,
            audio_source_channel: if ctx.media_type == DraftMediaKind::Audio {
                ctx.doc
                    .children_named(clip_item, "sourcetrack")
                    .first()
                    .and_then(|&source| ctx.doc.child_int(source, "trackindex"))
                    .filter(|&channel| channel > 0)
                    .map(|channel| channel as usize - 1)
            } else {
                None
            },
            fcpxml_audio_role: None,
            track_index: ctx.track_number,
            enabled: ctx
                .doc
                .child_text(clip_item, "enabled")
                .is_none_or(|t| !t.eq_ignore_ascii_case("FALSE")),
            track_enabled: ctx.track_enabled,
            track_locked: ctx.track_locked,
            linked_edit_ids: ctx
                .doc
                .children_named(clip_item, "link")
                .iter()
                .flat_map(|&l| ctx.doc.children_named(l, "linkclipref"))
                .filter_map(|r| {
                    let t = ctx.doc.nodes[r].text.trim().to_string();
                    if t.is_empty() { None } else { Some(t) }
                })
                .collect(),
        };
        // Nested placement: intersect with the container window,
        // map to parent seconds and rescale the rate.
        if let Some(w) = ctx.window.as_ref() {
            if !clip_edit_to_window(&mut edit, w.nested_in, w.nested_out) {
                continue;
            }
            let map = |t: f64| w.parent_start + (t - w.nested_in) * w.speed;
            edit.timeline_start = map(edit.timeline_start);
            edit.timeline_end = map(edit.timeline_end);
            edit.playback_rate *= w.speed;
            edit.id = format!("{}/{}", w.id_prefix, edit.id);
            edit.linked_edit_ids = edit
                .linked_edit_ids
                .into_iter()
                .map(|l| format!("{}/{}", w.id_prefix, l))
                .collect();
            edit.enabled = edit.enabled && w.parent_enabled;
            if w.drop_verbatim {
                edit.fcp7_time_remap_xml = None;
                edit.fcp7_filter_xmls.clear();
                edit.fcp7_retime_in = None;
                edit.fcp7_retime_out = None;
                edit.fcp7_retime_duration = None;
            }
        }
        edits.push(edit);
    }
}

// Returns true when the clipitem is a nested container (handled).
pub(super) fn read_nested_clipitem(
    ctx: &TrackCtx,
    clip_item: usize,
    items: &[usize],
    item_offset: usize,
    clip_ids: &HashMap<usize, String>,
    warnings: &mut Vec<SyncWarning>,
    edits: &mut Vec<DraftEdit>,
) -> bool {
    let warn = warner(ctx.path);
    let nested_seq = ctx
        .doc
        .children_named(clip_item, "sequence")
        .first()
        .copied();
    let multiclip = ctx
        .doc
        .children_named(clip_item, "multiclip")
        .first()
        .copied();
    if nested_seq.is_none() && multiclip.is_none() {
        return false;
    }
    if ctx.depth >= MAX_NESTED_DEPTH {
        warn(
            &mut *warnings,
            "A nested sequence is nested too deep to expand safely; it is skipped.".into(),
        );
        return true;
    }
    let parent_id = clip_ids
        .get(&item_offset)
        .cloned()
        .unwrap_or_else(|| format!("nested-{}-{item_offset}", ctx.track_number));
    let parent_name = ctx
        .doc
        .child_text(clip_item, "name")
        .unwrap_or_else(|| "clip item".to_string());
    let parent_enabled = ctx
        .doc
        .child_text(clip_item, "enabled")
        .is_none_or(|t| !t.eq_ignore_ascii_case("FALSE"));
    // Parent window timing (in/out in source units, start/end in parent units).
    let window_frames = (|| {
        let raw_in = ctx.doc.child_int(clip_item, "in")?;
        let raw_out = ctx.doc.child_int(clip_item, "out")?;
        let raw_start = match ctx.doc.child_int(clip_item, "start") {
            Some(-1) => {
                adjacent_transition_frame(ctx.doc, "start", items, item_offset.wrapping_sub(1))?
            }
            Some(v) => v,
            None => return None,
        };
        let raw_end = match ctx.doc.child_int(clip_item, "end") {
            Some(-1) => adjacent_transition_frame(ctx.doc, "end", items, item_offset + 1)?,
            Some(v) => v,
            None => return None,
        };
        if raw_in < 0 || raw_out <= raw_in || raw_start < 0 || raw_end <= raw_start {
            return None;
        }
        Some((raw_in, raw_out, raw_start, raw_end))
    })();
    let Some((raw_in, raw_out, raw_start, raw_end)) = window_frames else {
        warn(
            &mut *warnings,
            format!("Invalid timing in nested container {parent_name} cannot be preserved."),
        );
        return true;
    };
    if let Some(nested_seq) = nested_seq {
        let placement = NestedPlacement {
            frames: (raw_in, raw_out, raw_start, raw_end),
            id: parent_id,
            name: parent_name,
            enabled: parent_enabled,
        };
        read_nested_sequence(ctx, nested_seq, placement, &mut *warnings, &mut *edits);
        return true;
    }
    if let Some(mc) = multiclip {
        let placement = NestedPlacement {
            frames: (raw_in, raw_out, raw_start, raw_end),
            id: parent_id,
            name: parent_name,
            enabled: parent_enabled,
        };
        read_multiclip(ctx, mc, placement, &mut *warnings, &mut *edits);
        return true;
    }
    false
}

// Flatten one nested FCP 7 sequence into the parent timeline.
pub(super) fn read_nested_sequence(
    ctx: &TrackCtx,
    nested_seq: usize,
    placement: NestedPlacement,
    warnings: &mut Vec<SyncWarning>,
    edits: &mut Vec<DraftEdit>,
) {
    let warn = warner(ctx.path);
    let NestedPlacement {
        frames: (raw_in, raw_out, raw_start, raw_end),
        id: parent_id,
        name: parent_name,
        enabled: parent_enabled,
    } = placement;
    let nested_rate = fcp7_rate(ctx.doc, nested_seq, ctx.sequence_rate);
    let nf = nested_rate.as_seconds();
    let pf = ctx.sequence_rate.as_seconds();
    let nested_in = raw_in as f64 * nf;
    let nested_out = raw_out as f64 * nf;
    let parent_start = raw_start as f64 * pf;
    let parent_end = raw_end as f64 * pf;
    let window_dur = (nested_out - nested_in).max(f64::EPSILON);
    let parent_dur = (parent_end - parent_start).max(0.0);
    if parent_dur <= 0.0 {
        warn(
            &mut *warnings,
            format!("Invalid timing in nested sequence {parent_name} cannot be preserved."),
        );
        return;
    }
    let speed = parent_dur / window_dur;
    let drop_verbatim = (speed - 1.0).abs() > RATE_TOLERANCE;
    if drop_verbatim {
        warn(
            &mut *warnings,
            format!(
                "A speed change on nested sequence {parent_name} is flattened to a constant {speed:.3}x rate; the nested effect stack is not round-tripped to Premiere."
            ),
        );
    }
    let window = NestedWindow {
        nested_in,
        nested_out,
        parent_start,
        speed,
        id_prefix: parent_id.to_string(),
        parent_enabled,
        drop_verbatim,
    };
    for nested_media in ctx.doc.children_named(nested_seq, "media") {
        for (nested_kind, nested_name) in [
            (DraftMediaKind::Video, "video"),
            (DraftMediaKind::Audio, "audio"),
        ] {
            let mut nested_tracks = Vec::new();
            for typed in ctx.doc.children_named(nested_media, nested_name) {
                // Sequence tracks are `track`; accept aliases defensively.
                for track in ctx.doc.nodes[typed].children.iter().copied() {
                    let name = ctx.doc.nodes[track].name.as_str();
                    if name == "track" || name == "videotrack" || name == "audiotrack" {
                        nested_tracks.push(track);
                    }
                }
            }
            for (nested_j, &nested_track) in nested_tracks.iter().enumerate() {
                let nested_track_enabled = ctx.track_enabled
                    && ctx
                        .doc
                        .child_text(nested_track, "enabled")
                        .is_none_or(|t| !t.eq_ignore_ascii_case("FALSE"));
                let nested_track_locked = ctx.track_locked
                    || ctx
                        .doc
                        .child_text(nested_track, "locked")
                        .is_some_and(|t| t.eq_ignore_ascii_case("TRUE"));
                let nested_items: Vec<usize> = ctx.doc.nodes[nested_track]
                    .children
                    .iter()
                    .copied()
                    .filter(|&c| {
                        ctx.doc.nodes[c].name == "clipitem"
                            || ctx.doc.nodes[c].name == "transitionitem"
                    })
                    .collect();
                let nested_track_number = if nested_kind == ctx.media_type {
                    ctx.track_number + nested_j
                } else {
                    nested_j + 1
                };
                let nested_clip_ids =
                    stable_clip_ids(ctx.doc, &nested_items, nested_kind, nested_track_number);
                let child = TrackCtx {
                    doc: ctx.doc,
                    files_by_id: ctx.files_by_id,
                    path: ctx.path,
                    sequence_rate: nested_rate,
                    media_type: nested_kind,
                    track_number: nested_track_number,
                    track_enabled: nested_track_enabled,
                    track_locked: nested_track_locked,
                    depth: ctx.depth + 1,
                    window: Some(window.clone()),
                };
                read_track_items(
                    &child,
                    &nested_items,
                    &nested_clip_ids,
                    &mut *warnings,
                    &mut *edits,
                );
            }
        }
    }
}

// Flatten one FCP 7 multiclip from its active angles (Apple spec:
// each `angle` carries `activevideoangle`/`activeaudioangle`
// markers; without markers the first angle is used with a warning).
pub(super) fn read_multiclip(
    ctx: &TrackCtx,
    multiclip: usize,
    placement: NestedPlacement,
    warnings: &mut Vec<SyncWarning>,
    edits: &mut Vec<DraftEdit>,
) {
    let warn = warner(ctx.path);
    let NestedPlacement {
        frames: (raw_in, raw_out, raw_start, raw_end),
        id: parent_id,
        name: parent_name,
        enabled: parent_enabled,
    } = placement;
    for kind in [DraftMediaKind::Video, DraftMediaKind::Audio] {
        let marker = match kind {
            DraftMediaKind::Video => "activevideoangle",
            DraftMediaKind::Audio => "activeaudioangle",
        };
        let mut first = None;
        let mut chosen = None;
        for angle in ctx.doc.children_named(multiclip, "angle") {
            if first.is_none() {
                first = Some(angle);
            }
            if !ctx.doc.children_named(angle, marker).is_empty() {
                chosen = Some(angle);
            }
        }
        let marked = chosen.is_some();
        let Some(angle) = chosen.or(first) else {
            warn(
                &mut *warnings,
                format!("Multicam clip {parent_name} has no angles; it is skipped."),
            );
            continue;
        };
        if !marked {
            warn(
                &mut *warnings,
                format!(
                    "Multicam angle choice in {parent_name} is not recorded; the first angle is used."
                ),
            );
        }
        let Some(angle_clip) = ctx.doc.children_named(angle, "clip").first().copied() else {
            warn(
                &mut *warnings,
                format!("Multicam angle in {parent_name} has no media; it is skipped."),
            );
            continue;
        };
        let angle_enabled = parent_enabled
            && ctx
                .doc
                .child_text(angle_clip, "enabled")
                .is_none_or(|t| !t.eq_ignore_ascii_case("FALSE"));
        // Angle tracks (browser clips use videotrack/audiotrack).
        let want = match kind {
            DraftMediaKind::Video => ["videotrack", "track"],
            DraftMediaKind::Audio => ["audiotrack", "track"],
        };
        let mut angle_tracks = Vec::new();
        for angle_media in ctx.doc.children_named(angle_clip, "media") {
            for typed in ctx.doc.children_named(angle_media, kind.raw()) {
                for &track in ctx.doc.nodes[typed].children.iter() {
                    if want.contains(&ctx.doc.nodes[track].name.as_str()) {
                        angle_tracks.push(track);
                    }
                }
            }
        }
        if angle_tracks.is_empty() {
            // Fall back to any track-like child with clipitems.
            for angle_media in ctx.doc.children_named(angle_clip, "media") {
                for typed in ctx.doc.children_named(angle_media, kind.raw()) {
                    for &track in ctx.doc.nodes[typed].children.iter() {
                        let has_clips = ctx.doc.nodes[track]
                            .children
                            .iter()
                            .any(|&c| ctx.doc.nodes[c].name == "clipitem");
                        if has_clips {
                            angle_tracks.push(track);
                        }
                    }
                }
            }
        }
        // Angle rate for the parent window; 1:1 in the common case.
        let angle_rate = fcp7_rate(ctx.doc, angle_clip, ctx.sequence_rate);
        let af = angle_rate.as_seconds();
        let pf = ctx.sequence_rate.as_seconds();
        let nested_in = raw_in as f64 * af;
        let nested_out = raw_out as f64 * af;
        let parent_start = raw_start as f64 * pf;
        let parent_end = raw_end as f64 * pf;
        let window_dur = (nested_out - nested_in).max(f64::EPSILON);
        let parent_dur = (parent_end - parent_start).max(0.0);
        if parent_dur <= 0.0 {
            warn(
                &mut *warnings,
                format!("Invalid timing in multicam clip {parent_name} cannot be preserved."),
            );
            continue;
        }
        let speed = parent_dur / window_dur;
        let drop_verbatim = (speed - 1.0).abs() > RATE_TOLERANCE;
        if drop_verbatim {
            warn(
                &mut *warnings,
                format!(
                    "A speed change on multicam clip {parent_name} is flattened to a constant {speed:.3}x rate."
                ),
            );
        }
        let window = NestedWindow {
            nested_in,
            nested_out,
            parent_start,
            speed,
            id_prefix: format!("{parent_id}/mc"),
            parent_enabled: angle_enabled,
            drop_verbatim,
        };
        for (angle_j, &angle_track) in angle_tracks.iter().enumerate() {
            let angle_items: Vec<usize> = ctx.doc.nodes[angle_track]
                .children
                .iter()
                .copied()
                .filter(|&c| {
                    ctx.doc.nodes[c].name == "clipitem" || ctx.doc.nodes[c].name == "transitionitem"
                })
                .collect();
            if angle_items.is_empty() {
                continue;
            }
            let angle_track_number = ctx.track_number + angle_j;
            let angle_clip_ids = stable_clip_ids(ctx.doc, &angle_items, kind, angle_track_number);
            let child = TrackCtx {
                doc: ctx.doc,
                files_by_id: ctx.files_by_id,
                path: ctx.path,
                sequence_rate: angle_rate,
                media_type: kind,
                track_number: angle_track_number,
                track_enabled: ctx.track_enabled,
                track_locked: ctx.track_locked,
                depth: ctx.depth + 1,
                window: Some(window.clone()),
            };
            read_track_items(
                &child,
                &angle_items,
                &angle_clip_ids,
                &mut *warnings,
                &mut *edits,
            );
        }
    }
}

pub(super) fn unique_warnings(warnings: Vec<SyncWarning>) -> Vec<SyncWarning> {
    let mut seen = HashSet::new();
    warnings
        .into_iter()
        .filter(|w| seen.insert(w.message.clone()))
        .collect()
}
