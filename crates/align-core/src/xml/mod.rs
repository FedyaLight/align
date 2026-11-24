//! FCP 7 XML and FCPXML timeline import.
//!
//! A minimal arena DOM over quick-xml streaming: elements keep children,
//! trimmed text, attributes, parent links and **verbatim byte spans** —
//! effect/filter/transition payloads round-trip byte-identically into
//! Premiere exports. External entities are never loaded. Targeted traversal
//! helpers handle the supported import structures.
//!
//! Known limitation: files
//! with a `DOCTYPE` internal subset (`[...]`) misparse in streaming mode.
//! Real FCP7/FCPXML exports carry none; such files error instead of
//! producing a plausible-but-wrong timeline.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesCData, BytesText, Event};
use quick_xml::{Reader, Writer};

mod fcp7;
mod fcpxml;
#[cfg(test)]
mod tests;

use fcp7::*;
use fcpxml::*;

use crate::model::{
    Clip, ImportedTimeline, MediaKind, MediaTime, RATE_TOLERANCE, SyncWarning, TIME_TOLERANCE,
    TimelineEdit, TimelineLinkedAudioEdit, TimelineSequenceSummary, TimelineTransition,
    TimelineTransitionKind,
};

// ------------------------------------------------------------ DOM

#[derive(Clone, Debug, Default)]
struct XmlNode {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<usize>,
    parent: Option<usize>,
    text: String,
    start: usize,
    end: usize,
}

#[derive(Clone, Debug)]
pub struct XmlDoc {
    xml: String,
    nodes: Vec<XmlNode>,
    root: usize,
}

impl XmlDoc {
    pub fn parse(text: &str) -> Result<Self, ImportError> {
        // Premiere writes its XML with a byte order mark. The reader skips it
        // but counts positions after it, and verbatim spans index `text`.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut reader = Reader::from_str(text);
        reader.config_mut().trim_text(false);
        let mut doc = XmlDoc {
            xml: text.to_string(),
            nodes: Vec::new(),
            root: usize::MAX,
        };
        let mut stack: Vec<usize> = Vec::new();
        let mut buf = Vec::new();
        loop {
            let pos = reader.buffer_position() as usize;
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let idx = doc.nodes.len();
                    doc.nodes.push(XmlNode {
                        name: Self::node_name(&e.local_name()),
                        attrs: Self::attr_list(&e, &reader),
                        parent: stack.last().copied(),
                        start: pos,
                        end: pos,
                        ..Default::default()
                    });
                    if let Some(&parent) = stack.last() {
                        doc.nodes[parent].children.push(idx);
                    } else {
                        doc.root = idx;
                    }
                    stack.push(idx);
                }
                Ok(Event::Empty(e)) => {
                    let end = reader.buffer_position() as usize;
                    let idx = doc.nodes.len();
                    doc.nodes.push(XmlNode {
                        name: Self::node_name(&e.local_name()),
                        attrs: Self::attr_list(&e, &reader),
                        parent: stack.last().copied(),
                        start: pos,
                        end,
                        ..Default::default()
                    });
                    if let Some(&parent) = stack.last() {
                        doc.nodes[parent].children.push(idx);
                    } else {
                        doc.root = idx;
                    }
                }
                Ok(Event::End(_)) => {
                    if let Some(idx) = stack.pop() {
                        doc.nodes[idx].end = reader.buffer_position() as usize;
                    }
                }
                Ok(Event::Text(e)) => {
                    if let Ok(t) = e.unescape()
                        && let Some(&top) = stack.last()
                    {
                        doc.nodes[top].text.push_str(&t);
                    }
                }
                Ok(Event::CData(e)) => {
                    if let Ok(t) = std::str::from_utf8(e.as_ref())
                        && let Some(&top) = stack.last()
                    {
                        doc.nodes[top].text.push_str(t);
                    }
                }
                Ok(Event::Eof) => break,
                // Named by the caller, which knows the file.
                Err(error) => return Err(ImportError::unreadable("", error)),
                _ => {}
            }
            buf.clear();
        }
        if doc.root == usize::MAX || doc.nodes.is_empty() {
            return Err(ImportError::NoSequence(String::new()));
        }
        Ok(doc)
    }

    fn node_name(local: &quick_xml::name::LocalName<'_>) -> String {
        String::from_utf8_lossy(local.as_ref()).into_owned()
    }

    fn attr_list(
        e: &quick_xml::events::BytesStart<'_>,
        reader: &Reader<&[u8]>,
    ) -> Vec<(String, String)> {
        e.attributes()
            .flatten()
            .filter_map(|a| {
                let v = a
                    .decode_and_unescape_value(reader.decoder())
                    .ok()?
                    .into_owned();
                Some((String::from_utf8_lossy(a.key.as_ref()).into_owned(), v))
            })
            .collect()
    }

    pub fn root_name(&self) -> &str {
        &self.nodes[self.root].name
    }

    pub fn children_named(&self, idx: usize, name: &str) -> Vec<usize> {
        self.nodes[idx]
            .children
            .iter()
            .copied()
            .filter(|&c| self.nodes[c].name == name)
            .collect()
    }

    pub fn descendants_named(&self, idx: usize, name: &str) -> Vec<usize> {
        // Iterative pre-order DFS with reversed pushes, so pops (and the
        // collected matches) follow document order like XPath.
        let mut out = Vec::new();
        let mut stack: Vec<usize> = self.nodes[idx].children.iter().rev().copied().collect();
        while let Some(n) = stack.pop() {
            if self.nodes[n].name == name {
                out.push(n);
            }
            for &c in self.nodes[n].children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// First direct child text, trimmed.
    pub fn child_text(&self, idx: usize, name: &str) -> Option<String> {
        let t = self
            .children_named(idx, name)
            .first()
            .map(|&c| self.nodes[c].text.trim().to_string())?;
        if t.is_empty() { None } else { Some(t) }
    }

    pub fn child_int(&self, idx: usize, name: &str) -> Option<i64> {
        self.child_text(idx, name)?.parse().ok()
    }

    pub fn attr(&self, idx: usize, name: &str) -> Option<&str> {
        self.nodes[idx]
            .attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Verbatim source slice of an element (effect/filter round-trip).
    pub fn verbatim(&self, idx: usize) -> &str {
        let n = &self.nodes[idx];
        self.xml.get(n.start..n.end).unwrap_or("")
    }
}

// ------------------------------------------------------------ draft

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftMediaKind {
    Video,
    Audio,
}

impl DraftMediaKind {
    fn as_media_kind(&self) -> MediaKind {
        match self {
            Self::Video => MediaKind::Video,
            Self::Audio => MediaKind::Audio,
        }
    }
    fn raw(&self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DraftEdit {
    pub id: String,
    pub name: Option<String>,
    pub url: PathBuf,
    pub media_type: DraftMediaKind,
    pub source_in: f64,
    pub source_out: f64,
    pub timeline_start: f64,
    pub timeline_end: f64,
    pub retime: crate::model::RetimeSpec,

    pub fcp7: crate::model::Fcp7Passthrough,

    /// Resolution for imported edit times; AAF uses the track sample rate.
    pub time_scale: i32,
    pub include_embedded_audio: bool,
    pub audio_source_channel: Option<usize>,
    pub fcpxml_audio_role: Option<String>,
    pub track_index: usize,
    pub flags: crate::model::TrackFlags,

    pub linked_edit_ids: HashSet<String>,
}

impl DraftEdit {
    fn media_time(&self, seconds: f64) -> MediaTime {
        MediaTime::new(
            (seconds * self.time_scale as f64).round() as i64,
            self.time_scale,
        )
    }
}

#[derive(Clone, Debug)]
pub struct DraftTransition {
    pub kind: TimelineTransitionKind,
    pub media_type: DraftMediaKind,
    pub left_edit_id: String,
    pub right_edit_id: String,
    pub start: f64,
    pub end: f64,
    pub alignment: String,
    pub effect_xml: String,
    pub transition_xml: String,
    pub is_otio_portable: bool,
}

#[derive(Clone, Debug)]
pub struct TimelineDraft {
    pub aaf_track_numbers: Vec<usize>,
    pub source_url: PathBuf,
    pub name: String,
    pub frame_duration: MediaTime,
    pub edits: Vec<DraftEdit>,
    pub transitions: Vec<DraftTransition>,
    pub warnings: Vec<SyncWarning>,
}

impl TimelineDraft {
    /// Validate explicit physical channel routing after relinking and inspection.
    pub fn validate_source_channels(&self, clips: &[Clip]) -> Result<(), String> {
        let by_path: HashMap<&Path, &Clip> = clips.iter().map(|c| (c.url.as_path(), c)).collect();
        for edit in &self.edits {
            if let Some(channel) = edit.audio_source_channel
                && let Some(clip) = by_path.get(edit.url.as_path())
                && clip
                    .audio
                    .first()
                    .is_none_or(|audio| channel >= audio.channels)
            {
                return Err(format!(
                    "Audio channel {} is unavailable in {}",
                    channel.saturating_add(1),
                    edit.url.display()
                ));
            }
        }
        Ok(())
    }

    pub fn media_urls(&self) -> Vec<PathBuf> {
        let mut urls: Vec<PathBuf> = self.edits.iter().map(|e| e.url.clone()).collect();
        urls.sort();
        urls.dedup();
        urls
    }

    /// Missing-file relink by unique filename, with conservative longest
    /// matching directory-suffix disambiguation. `manual` holds exact
    /// `filename = path` picks (applied first); `redirects` holds saved
    /// old-prefix → new-location mappings (applied second). Both beat the
    /// pool guess, and neither fires without an existing file.
    pub fn relinking_missing_media(
        &self,
        candidates: &[PathBuf],
        redirects: &[crate::redirect::PathRedirection],
        manual: &[(String, PathBuf)],
    ) -> Self {
        self.relinking_missing_media_with_replacements(candidates, redirects, manual)
            .0
    }

    /// Relink missing media and return the exact source-path replacements.
    /// The mapping is suitable for writing the same fixes back to a copy of
    /// the imported project.
    pub fn relinking_missing_media_with_replacements(
        &self,
        candidates: &[PathBuf],
        redirects: &[crate::redirect::PathRedirection],
        manual: &[(String, PathBuf)],
    ) -> (Self, Vec<(PathBuf, PathBuf)>) {
        let available: Vec<&PathBuf> = candidates.iter().filter(|p| p.is_file()).collect();
        let mut by_filename: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for p in available {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                by_filename
                    .entry(name.to_lowercase())
                    .or_default()
                    .push((*p).clone());
            }
        }
        let mut replacements: HashMap<PathBuf, PathBuf> = HashMap::new();
        let mut relink_warnings = Vec::new();
        for missing in self.media_urls() {
            if missing.is_file() {
                continue;
            }
            let display = missing
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if let Some(choice) = manual
                .iter()
                .find(|(name, _)| name.to_lowercase() == display.to_lowercase())
                .map(|(_, path)| path)
            {
                if choice.is_file() {
                    replacements.insert(missing.clone(), choice.clone());
                    relink_warnings.push(SyncWarning {
                        url: self.source_url.clone(),
                        message: format!("Relinked {display} to {} (manual).", choice.display()),
                    });
                } else {
                    relink_warnings.push(SyncWarning {
                        url: self.source_url.clone(),
                        message: format!(
                            "Manual relink target for {display} does not exist: {}.",
                            choice.display()
                        ),
                    });
                }
                continue;
            }
            if let Some(rewritten) = crate::redirect::rewrite(&missing, redirects)
                && rewritten.is_file()
            {
                replacements.insert(missing.clone(), rewritten.clone());
                relink_warnings.push(SyncWarning {
                    url: self.source_url.clone(),
                    message: format!(
                        "Relinked {display} to {} via saved redirection.",
                        rewritten.display()
                    ),
                });
                continue;
            }
            let filename = missing
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            let mut matches: Vec<PathBuf> = by_filename.get(&filename).cloned().unwrap_or_default();
            matches.sort();
            matches.dedup();
            if matches.len() == 1 {
                replacements.insert(missing.clone(), matches[0].clone());
                relink_warnings.push(SyncWarning {
                    url: self.source_url.clone(),
                    message: format!("Relinked {display} to {}.", matches[0].display()),
                });
            } else if matches.len() > 1 {
                let scores: Vec<usize> = matches
                    .iter()
                    .map(|candidate| matching_parent_suffix(&missing, candidate))
                    .collect();
                let best = scores.iter().copied().max().unwrap_or(0);
                let winners: Vec<&PathBuf> = matches
                    .iter()
                    .zip(scores)
                    .filter_map(|(candidate, score)| {
                        (best > 0 && score == best).then_some(candidate)
                    })
                    .collect();
                if winners.len() == 1 {
                    replacements.insert(missing.clone(), (*winners[0]).clone());
                    relink_warnings.push(SyncWarning {
                        url: self.source_url.clone(),
                        message: format!(
                            "Relinked {display} by matching {best} parent folder{} to {}.",
                            if best == 1 { "" } else { "s" },
                            winners[0].display()
                        ),
                    });
                } else {
                    relink_warnings.push(SyncWarning {
                        url: self.source_url.clone(),
                        message: format!(
                            "Could not relink {display}: {} files with that name were provided.",
                            matches.len()
                        ),
                    });
                }
            }
        }
        if replacements.is_empty() && relink_warnings.is_empty() {
            return (self.clone(), Vec::new());
        }
        let edits = self
            .edits
            .iter()
            .map(|edit| {
                let mut edit = edit.clone();
                if let Some(replacement) = replacements.get(&edit.url) {
                    edit.url = replacement.clone();
                }
                edit
            })
            .collect();
        let draft = Self {
            aaf_track_numbers: self.aaf_track_numbers.clone(),
            source_url: self.source_url.clone(),
            name: self.name.clone(),
            frame_duration: self.frame_duration,
            edits,
            transitions: self.transitions.clone(),
            warnings: [self.warnings.clone(), relink_warnings].concat(),
        };
        let mut replacements: Vec<_> = replacements.into_iter().collect();
        replacements.sort();
        (draft, replacements)
    }

    /// Resolve draft edits against inspected clips.
    pub fn resolve(&self, clips: &[Clip]) -> ImportedTimeline {
        let clips_by_path: HashMap<String, &Clip> = clips
            .iter()
            .map(|c| (c.url.to_string_lossy().into_owned(), c))
            .collect();
        let mut seen_audio: HashSet<String> = HashSet::new();
        let audio_edits: Vec<&DraftEdit> = self
            .edits
            .iter()
            .filter(|e| e.media_type == DraftMediaKind::Audio)
            .collect();
        let mut embedded_audio_ids = HashSet::new();
        for video in self
            .edits
            .iter()
            .filter(|e| e.media_type == DraftMediaKind::Video && e.include_embedded_audio)
        {
            let explicit: Vec<_> = audio_edits
                .iter()
                .filter(|audio| {
                    audio.url == video.url
                        && (video.linked_edit_ids.contains(&audio.id)
                            || audio.linked_edit_ids.contains(&video.id))
                })
                .collect();
            if explicit.is_empty() {
                if let Some(audio) =
                    closest_embedded_audio(video, &audio_edits, self.frame_duration.as_seconds())
                {
                    embedded_audio_ids.insert(audio.id.as_str());
                }
            } else {
                embedded_audio_ids.extend(explicit.into_iter().map(|audio| audio.id.as_str()));
            }
        }
        let mut transitions_by_left: HashMap<String, &DraftTransition> = HashMap::new();
        for t in &self.transitions {
            transitions_by_left
                .entry(format!("{}:{}", t.media_type.raw(), t.left_edit_id))
                .or_insert(t);
        }
        let mut edits_by_id: HashMap<String, &DraftEdit> = HashMap::new();
        for e in &self.edits {
            edits_by_id
                .entry(format!("{}:{}", e.media_type.raw(), e.id))
                .or_insert(e);
        }
        let resolved_transition = |t: &DraftTransition| {
            let portable = t.is_otio_portable
                && edits_by_id
                    .get(&format!("{}:{}", t.media_type.raw(), t.left_edit_id))
                    .is_some_and(|e| e.retime.playback_rate == 1.0)
                && edits_by_id
                    .get(&format!("{}:{}", t.media_type.raw(), t.right_edit_id))
                    .is_some_and(|e| e.retime.playback_rate == 1.0)
                && edits_by_id
                    .get(&format!("{}:{}", t.media_type.raw(), t.left_edit_id))
                    .is_some_and(|e| !e.retime.plays_backward)
                && edits_by_id
                    .get(&format!("{}:{}", t.media_type.raw(), t.right_edit_id))
                    .is_some_and(|e| !e.retime.plays_backward);
            TimelineTransition {
                kind: t.kind,
                right_edit_id: t.right_edit_id.clone(),
                start: MediaTime::microseconds(t.start),
                end: MediaTime::microseconds(t.end),
                alignment: t.alignment.clone(),
                fcp7_effect_xml: t.effect_xml.clone(),
                fcp7_transition_xml: Some(t.transition_xml.clone()),
                is_otio_portable: portable,
            }
        };

        let mut resolved = Vec::new();
        for edit in &self.edits {
            if edit.media_type == DraftMediaKind::Audio
                && embedded_audio_ids.contains(edit.id.as_str())
            {
                continue;
            }
            let Some(clip) = clips_by_path.get(&edit.url.to_string_lossy().into_owned()) else {
                continue;
            };
            let kind_ok = (clip.kind == MediaKind::Video
                && edit.media_type == DraftMediaKind::Video)
                || (!clip.audio.is_empty() && edit.media_type == DraftMediaKind::Audio);
            if !kind_ok {
                continue;
            }
            if edit.media_type == DraftMediaKind::Audio {
                let key = format!(
                    "{}:{}:{}:{}:{}:{:?}",
                    clip.id.0,
                    edit.source_in,
                    edit.source_out,
                    edit.timeline_start,
                    edit.timeline_end,
                    edit.audio_source_channel
                        .map(|channel| (channel, edit.track_index))
                );
                if !seen_audio.insert(key) {
                    continue;
                }
            }
            let embedded =
                if edit.media_type == DraftMediaKind::Video && edit.include_embedded_audio {
                    closest_embedded_audio(edit, &audio_edits, self.frame_duration.as_seconds())
                } else {
                    None
                };
            let transition =
                transitions_by_left.get(&format!("{}:{}", edit.media_type.raw(), edit.id));
            let linked_audio_edit = embedded.map(|audio| TimelineLinkedAudioEdit {
                id: audio.id.clone(),
                source_in: audio.media_time(audio.source_in),
                source_out: audio.media_time(audio.source_out),
                timeline_start: audio.media_time(audio.timeline_start),
                timeline_end: audio.media_time(audio.timeline_end),
                retime: audio.retime,

                fcp7: audio.fcp7.clone(),

                audio_source_channel: audio.audio_source_channel,
                fcpxml_audio_role: audio.fcpxml_audio_role.clone(),
                track_index: audio.track_index,
                flags: audio.flags,

                transition_after: transitions_by_left
                    .get(&format!("audio:{}", audio.id))
                    .map(|t| resolved_transition(t)),
            });
            resolved.push(TimelineEdit {
                id: edit.id.clone(),
                name: edit.name.clone(),
                clip_id: clip.id.clone(),
                media_type: edit.media_type.as_media_kind(),
                source_in: edit.media_time(edit.source_in),
                source_out: edit.media_time(edit.source_out),
                timeline_start: edit.media_time(edit.timeline_start),
                timeline_end: edit.media_time(edit.timeline_end),
                retime: edit.retime,

                fcp7: edit.fcp7.clone(),

                audio_source_channel: edit.audio_source_channel,
                fcpxml_audio_role: edit.fcpxml_audio_role.clone(),
                track_index: edit.track_index,
                audio_track_index: embedded.map(|a| a.track_index),
                flags: edit.flags,

                audio_enabled: if edit.include_embedded_audio {
                    embedded.map(|a| a.flags.enabled)
                } else {
                    Some(false)
                },
                audio_track_enabled: embedded.map(|a| a.flags.track_enabled),
                audio_track_locked: embedded.map(|a| a.flags.track_locked),
                transition_after: if edit.media_type == DraftMediaKind::Video {
                    transition.map(|t| resolved_transition(t))
                } else {
                    None
                },
                audio_transition_after: if edit.media_type == DraftMediaKind::Audio {
                    transition.map(|t| resolved_transition(t))
                } else {
                    None
                },
                linked_audio_edit,
            });
        }
        ImportedTimeline {
            aaf_track_numbers: self.aaf_track_numbers.clone(),
            name: self.name.clone(),
            frame_duration: self.frame_duration,
            edits: resolved,
        }
    }

    /// Warnings for timeline media that could not be opened (deduped).
    /// Extensions in `omit` (for example, timeline
    /// photos) are skipped silently instead of warning.
    pub fn unresolved_warnings(&self, clips: &[Clip], omit: &[String]) -> Vec<SyncWarning> {
        let paths: HashSet<String> = clips
            .iter()
            .map(|c| c.url.to_string_lossy().into_owned())
            .collect();
        let mut out = Vec::new();
        for edit in &self.edits {
            if paths.contains(&edit.url.to_string_lossy().into_owned()) {
                continue;
            }
            let omitted = edit
                .url
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    omit.iter()
                        .any(|skipped| skipped.eq_ignore_ascii_case(extension))
                });
            if omitted {
                continue;
            }
            let warning = SyncWarning {
                url: self.source_url.clone(),
                message: format!("Timeline media could not be opened: {}", edit.url.display()),
            };
            if !out
                .iter()
                .any(|w: &SyncWarning| w.message == warning.message)
            {
                out.push(warning);
            }
        }
        out
    }
}

fn matching_parent_suffix(left: &Path, right: &Path) -> usize {
    use std::path::Component;
    let parents = |path: &Path| -> Vec<String> {
        path.parent()
            .into_iter()
            .flat_map(Path::components)
            .filter_map(|component| match component {
                Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let left = parents(left);
    let right = parents(right);
    left.iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
        .count()
}

fn closest_embedded_audio<'a>(
    video: &DraftEdit,
    candidates: &[&'a DraftEdit],
    frame_seconds: f64,
) -> Option<&'a DraftEdit> {
    let tolerance = 0.1f64.max(frame_seconds * 2.0);
    let same_media: Vec<&&DraftEdit> = candidates.iter().filter(|c| c.url == video.url).collect();
    let explicit: Vec<&&DraftEdit> = same_media
        .iter()
        .filter(|c| video.linked_edit_ids.contains(&c.id) || c.linked_edit_ids.contains(&video.id))
        .copied()
        .collect();
    if explicit.len() == 1 {
        return Some(explicit[0]);
    }
    if explicit.len() > 1 {
        return None;
    }
    same_media
        .into_iter()
        .filter(|c| {
            (c.source_in - video.source_in).abs() < TIME_TOLERANCE
                && (c.source_out - video.source_out).abs() < TIME_TOLERANCE
                && (c.timeline_start - video.timeline_start).abs() <= tolerance
                && (c.timeline_end - video.timeline_end).abs() <= tolerance
        })
        .min_by(|a, b| {
            let ea = (a.timeline_start - video.timeline_start).abs()
                + (a.timeline_end - video.timeline_end).abs();
            let eb = (b.timeline_start - video.timeline_start).abs()
                + (b.timeline_end - video.timeline_end).abs();
            ea.total_cmp(&eb)
        })
        .copied()
}

// ------------------------------------------------------------ errors

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ImportError {
    #[error("No FCP 7 XML sequence was found in {0}.")]
    NoSequence(String),
    #[error("{0} contains {1} sequences. Choose one explicitly.")]
    MultipleSequences(String, usize),
    #[error("Sequence {} does not exist in {}.", .1 + 1, .0)]
    InvalidSequence(String, usize),
    #[error("No readable media clip items were found in {0}.")]
    NoMedia(String),
    #[error("Cannot read timeline {file}: {reason}.")]
    Unreadable { file: String, reason: String },
}

impl ImportError {
    /// The same error naming `name` instead of the document read, e.g. an
    /// FCPXML bundle instead of the `Info.fcpxml` inside it.
    pub fn naming(self, name: &str) -> Self {
        let name = name.to_string();
        match self {
            Self::NoSequence(_) => Self::NoSequence(name),
            Self::MultipleSequences(_, count) => Self::MultipleSequences(name, count),
            Self::InvalidSequence(_, index) => Self::InvalidSequence(name, index),
            Self::NoMedia(_) => Self::NoMedia(name),
            Self::Unreadable { reason, .. } => Self::Unreadable { file: name, reason },
        }
    }

    fn unreadable(file: &str, reason: impl std::fmt::Display) -> Self {
        Self::Unreadable {
            file: file.to_string(),
            reason: reason.to_string().trim_end_matches('.').to_string(),
        }
    }

    fn filename(url: &Path) -> String {
        url.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("timeline")
            .to_string()
    }
}

// ------------------------------------------------------------ importer

/// Whether an XML file holds an editing timeline: its root element is FCP 7
/// `xmeml` or `fcpxml`. Cameras write clip metadata as XML beside their
/// media (Canon `CLIPS001/*.XML`); those are not timelines. Reads at most
/// 16 KiB. A file that cannot be read or parsed counts as a timeline, so
/// importing it reports why.
pub fn is_timeline_document(path: &Path) -> bool {
    use std::io::Read;
    let mut head = Vec::new();
    let read =
        std::fs::File::open(path).and_then(|file| file.take(16 * 1024).read_to_end(&mut head));
    if read.is_err() {
        return true;
    }
    // Timelines import only as UTF-8, but a UTF-16 project must still be
    // recognized, so that importing it says it cannot be read instead of the
    // project being skipped. Other encodings with NUL bytes count as
    // timelines for the same reason.
    let utf16 = |bytes: &[u8], unit: fn([u8; 2]) -> u16| {
        let units = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| unit([pair[0], pair[1]]));
        char::decode_utf16(units)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect::<String>()
            .into_bytes()
    };
    let head = match head.as_slice() {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        bytes if bytes.contains(&0) => return true,
        _ => head,
    };
    let mut reader = Reader::from_reader(head.as_slice());
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element) | Event::Empty(element)) => {
                let root = element.local_name();
                return root.as_ref().eq_ignore_ascii_case(b"xmeml")
                    || root.as_ref().eq_ignore_ascii_case(b"fcpxml");
            }
            Ok(Event::Eof) | Err(_) => return true,
            Ok(_) => buffer.clear(),
        }
    }
}

/// Dispatch by root element (`fcpxml` or FCP 7 XML).
pub fn read_timeline(
    path: &Path,
    sequence_index: Option<usize>,
) -> Result<TimelineDraft, ImportError> {
    read_timeline_with_proxies(path, sequence_index, false)
}

pub fn read_timeline_with_proxies(
    path: &Path,
    sequence_index: Option<usize>,
    prefer_proxies: bool,
) -> Result<TimelineDraft, ImportError> {
    let name = ImportError::filename(path);
    let text =
        std::fs::read_to_string(path).map_err(|error| ImportError::unreadable(&name, error))?;
    let doc = XmlDoc::parse(&text).map_err(|error| error.naming(&name))?;
    if doc.root_name().eq_ignore_ascii_case("fcpxml") {
        read_fcpxml(&doc, path, sequence_index, prefer_proxies)
    } else {
        read_fcp7(&doc, path, sequence_index)
    }
}

pub fn timeline_sequence_summaries(
    path: &Path,
) -> Result<Vec<TimelineSequenceSummary>, ImportError> {
    let name = ImportError::filename(path);
    let text =
        std::fs::read_to_string(path).map_err(|error| ImportError::unreadable(&name, error))?;
    let doc = XmlDoc::parse(&text).map_err(|error| error.naming(&name))?;
    if doc.root_name().eq_ignore_ascii_case("fcpxml") {
        summaries_fcpxml(&doc).map_err(|error| error.naming(&name))
    } else {
        summaries_fcp7(&doc, path)
    }
}

/// Rewrite only media locations in FCP 7 XML or FCPXML. Unchanged events are
/// copied verbatim by quick-xml; elements containing a changed location are
/// re-escaped without rebuilding the timeline document.
/// Where a rewritten document is written. Relative FCPXML media URLs resolve
/// against `from`, the source document's folder. Those that would resolve
/// differently from `to` and do not point into `copied` (a tree copied
/// along with the document) become absolute file URLs.
pub struct Relocation<'a> {
    pub from: &'a Path,
    pub to: &'a Path,
    pub copied: Option<&'a Path>,
}

impl<'a> Relocation<'a> {
    /// The document stays in its folder.
    pub fn none(folder: &'a Path) -> Self {
        Self {
            from: folder,
            to: folder,
            copied: None,
        }
    }
}

pub fn rewrite_media_paths(
    source: &[u8],
    replacements: &[(PathBuf, PathBuf)],
    relocation: &Relocation<'_>,
) -> Result<Vec<u8>, ImportError> {
    let moves = relocation.from != relocation.to;
    if replacements.is_empty() && !moves {
        return Ok(source.to_vec());
    }
    let replacement_for = |value: &str, file_url_only: bool| {
        let value = value.trim();
        let path = if file_url_only {
            local_file_path(value)?
        } else {
            fcpxml_url(value, relocation.from)
        };
        if let Some(new) = replacements
            .iter()
            .find_map(|(old, new)| (old == &path).then(|| crate::model::file_url(new)))
        {
            return Some(new);
        }
        let relative = !file_url_only && !value.contains("://");
        let still_resolves = !moves || relocation.copied.is_some_and(|tree| path.starts_with(tree));
        (relative && !still_resolves).then(|| crate::model::file_url(&path))
    };
    let mut reader = Reader::from_reader(source);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::with_capacity(source.len()));
    let mut in_pathurl = false;
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_into(&mut buf).map_err(malformed)?;
        match event {
            Event::Start(start) => {
                let local = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
                in_pathurl = local == "pathurl";
                if matches!(local.as_str(), "asset" | "media-rep") {
                    write_rewritten_location_tag(
                        &mut writer,
                        &reader,
                        start,
                        false,
                        &replacement_for,
                    )?;
                } else {
                    writer.write_event(Event::Start(start)).map_err(malformed)?;
                }
            }
            Event::Empty(empty) => {
                let local = String::from_utf8_lossy(empty.local_name().as_ref()).into_owned();
                if matches!(local.as_str(), "asset" | "media-rep") {
                    write_rewritten_location_tag(
                        &mut writer,
                        &reader,
                        empty,
                        true,
                        &replacement_for,
                    )?;
                } else {
                    writer.write_event(Event::Empty(empty)).map_err(malformed)?;
                }
            }
            Event::Text(text) if in_pathurl => {
                let value = text.unescape().map_err(malformed)?;
                if let Some(new) = replacement_for(&value, true) {
                    writer
                        .write_event(Event::Text(BytesText::new(&new)))
                        .map_err(malformed)?;
                } else {
                    writer.write_event(Event::Text(text)).map_err(malformed)?;
                }
            }
            Event::CData(text) if in_pathurl => {
                let value = String::from_utf8_lossy(text.as_ref());
                if let Some(new) = replacement_for(&value, true) {
                    writer
                        .write_event(Event::CData(BytesCData::new(&new)))
                        .map_err(malformed)?;
                } else {
                    writer.write_event(Event::CData(text)).map_err(malformed)?;
                }
            }
            Event::End(end) => {
                if end.local_name().as_ref() == b"pathurl" {
                    in_pathurl = false;
                }
                writer.write_event(Event::End(end)).map_err(malformed)?;
            }
            Event::Eof => break,
            other => writer.write_event(other).map_err(malformed)?,
        }
        buf.clear();
    }
    Ok(writer.into_inner())
}

fn write_rewritten_location_tag<F>(
    writer: &mut Writer<Vec<u8>>,
    reader: &Reader<&[u8]>,
    tag: quick_xml::events::BytesStart<'_>,
    empty: bool,
    replacement_for: &F,
) -> Result<(), ImportError>
where
    F: Fn(&str, bool) -> Option<String>,
{
    let mut attributes = Vec::new();
    let mut changed = false;
    for attribute in tag.attributes() {
        let attribute = attribute.map_err(malformed)?;
        let key = String::from_utf8_lossy(attribute.key.as_ref()).into_owned();
        let mut value = attribute
            .decode_and_unescape_value(reader.decoder())
            .map_err(malformed)?
            .into_owned();
        if key == "src"
            && let Some(new) = replacement_for(&value, false)
        {
            value = new;
            changed = true;
        }
        attributes.push((key, value));
    }
    if changed {
        let mut rewritten = tag.into_owned();
        rewritten.clear_attributes();
        for (key, value) in &attributes {
            rewritten.push_attribute((key.as_str(), value.as_str()));
        }
        writer
            .write_event(if empty {
                Event::Empty(rewritten)
            } else {
                Event::Start(rewritten)
            })
            .map_err(malformed)
    } else {
        writer
            .write_event(if empty {
                Event::Empty(tag)
            } else {
                Event::Start(tag)
            })
            .map_err(malformed)
    }
}

/// A document quick-xml cannot parse or re-serialize. The caller names the
/// file with [`ImportError::naming`].
fn malformed(error: impl std::fmt::Display) -> ImportError {
    ImportError::unreadable("", error)
}

/// Records warnings about the timeline document at `path`.
fn warner(path: &Path) -> impl Fn(&mut Vec<SyncWarning>, String) + '_ {
    move |warnings, message| {
        warnings.push(SyncWarning {
            url: path.to_path_buf(),
            message,
        })
    }
}
