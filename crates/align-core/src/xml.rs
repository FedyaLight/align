//! FCP 7 XML + FCPXML timeline import. Port of `FCP7XMLImporter.swift`,
//! `FCPXMLImporter.swift` and `TimelineImporter.swift`.
//!
//! A minimal arena DOM over quick-xml streaming: elements keep children,
//! trimmed text, attributes, parent links and **verbatim byte spans** —
//! effect/filter/transition payloads round-trip byte-identically into
//! Premiere exports. No external entities are ever loaded (Swift parity:
//! `.nodeLoadExternalEntitiesNever`). XPath is replaced by targeted
//! traversal helpers covering exactly the shapes both importers query.
//!
//! Known limitation (documented, like other explicit boundaries): files
//! with a `DOCTYPE` internal subset (`[...]`) misparse in streaming mode.
//! Real FCP7/FCPXML exports carry none; such files error instead of
//! producing a plausible-but-wrong timeline.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::model::{
    Clip, ImportedTimeline, MediaKind, MediaTime, SyncWarning, TimelineEdit,
    TimelineLinkedAudioEdit, TimelineSequenceSummary, TimelineTransition, TimelineTransitionKind,
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
                    if let Ok(t) = e.unescape() {
                        if let Some(&top) = stack.last() {
                            doc.nodes[top].text.push_str(&t);
                        }
                    }
                }
                Ok(Event::CData(e)) => {
                    if let Ok(t) = std::str::from_utf8(e.as_ref()) {
                        if let Some(&top) = stack.last() {
                            doc.nodes[top].text.push_str(t);
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => return Err(ImportError::Unreadable(String::new())),
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

    /// First direct child text, trimmed (mirrors `childText`).
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
    pub playback_rate: f64,
    pub plays_backward: bool,
    pub fcp7_time_remap_xml: Option<String>,
    /// Verbatim non-timeremap `<filter>` payloads (e.g. audio levels):
    /// clip-relative like the remap graph, so a rigid track shift keeps
    /// them valid. Passed through to FCP 7 XML; Resolve OTIO has no
    /// portable gain/effect model and drops them with a warning.
    pub fcp7_filter_xmls: Vec<String>,
    pub fcp7_retime_in: Option<i64>,
    pub fcp7_retime_out: Option<i64>,
    pub fcp7_retime_duration: Option<i64>,
    pub fcp7_labels_xml: Option<String>,
    /// Resolution for imported edit times; AAF uses the track sample rate.
    pub time_scale: i32,
    pub include_embedded_audio: bool,
    pub audio_source_channel: Option<usize>,
    pub fcpxml_audio_role: Option<String>,
    pub track_index: usize,
    pub enabled: bool,
    pub track_enabled: bool,
    pub track_locked: bool,
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
        let mut replacements: HashMap<String, PathBuf> = HashMap::new();
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
                    replacements.insert(missing.to_string_lossy().into_owned(), choice.clone());
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
            if let Some(rewritten) = crate::redirect::rewrite(&missing, redirects) {
                if rewritten.is_file() {
                    replacements.insert(missing.to_string_lossy().into_owned(), rewritten.clone());
                    relink_warnings.push(SyncWarning {
                        url: self.source_url.clone(),
                        message: format!(
                            "Relinked {display} to {} via saved redirection.",
                            rewritten.display()
                        ),
                    });
                    continue;
                }
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
                replacements.insert(missing.to_string_lossy().into_owned(), matches[0].clone());
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
                    replacements.insert(
                        missing.to_string_lossy().into_owned(),
                        (*winners[0]).clone(),
                    );
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
            return self.clone();
        }
        let edits = self
            .edits
            .iter()
            .map(|edit| {
                let mut edit = edit.clone();
                if let Some(replacement) =
                    replacements.get(&edit.url.to_string_lossy().into_owned())
                {
                    edit.url = replacement.clone();
                }
                edit
            })
            .collect();
        Self {
            source_url: self.source_url.clone(),
            name: self.name.clone(),
            frame_duration: self.frame_duration,
            edits,
            transitions: self.transitions.clone(),
            warnings: [self.warnings.clone(), relink_warnings].concat(),
        }
    }

    /// Resolve draft edits against inspected clips (mirror Swift `resolve`).
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
                    .is_some_and(|e| e.playback_rate == 1.0)
                && edits_by_id
                    .get(&format!("{}:{}", t.media_type.raw(), t.right_edit_id))
                    .is_some_and(|e| e.playback_rate == 1.0)
                && edits_by_id
                    .get(&format!("{}:{}", t.media_type.raw(), t.left_edit_id))
                    .is_some_and(|e| !e.plays_backward)
                && edits_by_id
                    .get(&format!("{}:{}", t.media_type.raw(), t.right_edit_id))
                    .is_some_and(|e| !e.plays_backward);
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
                playback_rate: audio.playback_rate,
                plays_backward: audio.plays_backward,
                fcp7_time_remap_xml: audio.fcp7_time_remap_xml.clone(),
                fcp7_filter_xmls: audio.fcp7_filter_xmls.clone(),
                fcp7_retime_in: audio.fcp7_retime_in,
                fcp7_retime_out: audio.fcp7_retime_out,
                fcp7_retime_duration: audio.fcp7_retime_duration,
                fcp7_labels_xml: audio.fcp7_labels_xml.clone(),
                audio_source_channel: audio.audio_source_channel,
                fcpxml_audio_role: audio.fcpxml_audio_role.clone(),
                track_index: audio.track_index,
                enabled: audio.enabled,
                track_enabled: audio.track_enabled,
                track_locked: audio.track_locked,
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
                playback_rate: edit.playback_rate,
                plays_backward: edit.plays_backward,
                fcp7_time_remap_xml: edit.fcp7_time_remap_xml.clone(),
                fcp7_filter_xmls: edit.fcp7_filter_xmls.clone(),
                fcp7_retime_in: edit.fcp7_retime_in,
                fcp7_retime_out: edit.fcp7_retime_out,
                fcp7_retime_duration: edit.fcp7_retime_duration,
                fcp7_labels_xml: edit.fcp7_labels_xml.clone(),
                audio_source_channel: edit.audio_source_channel,
                fcpxml_audio_role: edit.fcpxml_audio_role.clone(),
                track_index: edit.track_index,
                audio_track_index: embedded.map(|a| a.track_index),
                enabled: edit.enabled,
                track_enabled: edit.track_enabled,
                track_locked: edit.track_locked,
                audio_enabled: if edit.include_embedded_audio {
                    embedded.map(|a| a.enabled)
                } else {
                    Some(false)
                },
                audio_track_enabled: embedded.map(|a| a.track_enabled),
                audio_track_locked: embedded.map(|a| a.track_locked),
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
            name: self.name.clone(),
            frame_duration: self.frame_duration,
            edits: resolved,
        }
    }

    /// Warnings for timeline media that could not be opened (deduped).
    /// Extensions in `omit` (Syncaila Omit extensions, e.g. timeline
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
            (c.source_in - video.source_in).abs() < 0.000_001
                && (c.source_out - video.source_out).abs() < 0.000_001
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

#[derive(Clone, Debug, PartialEq)]
pub enum ImportError {
    NoSequence(String),
    MultipleSequences(String, usize),
    InvalidSequence(String, usize),
    NoMedia(String),
    Unreadable(String),
}

impl ImportError {
    fn filename(url: &Path) -> String {
        url.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("timeline")
            .to_string()
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSequence(url) => {
                write!(f, "No FCP 7 XML sequence was found in {url}.")
            }
            Self::MultipleSequences(url, count) => {
                write!(
                    f,
                    "{url} contains {count} sequences. Choose one explicitly."
                )
            }
            Self::InvalidSequence(url, index) => {
                write!(f, "Sequence {} does not exist in {url}.", index + 1)
            }
            Self::NoMedia(url) => {
                write!(f, "No readable media clip items were found in {url}.")
            }
            Self::Unreadable(url) => write!(f, "Cannot read timeline {url}."),
        }
    }
}

impl std::error::Error for ImportError {}

// ------------------------------------------------------------ importer

/// Dispatch by root element (`fcpxml` vs anything else), like Swift.
pub fn read_timeline(
    path: &Path,
    sequence_index: Option<usize>,
) -> Result<TimelineDraft, ImportError> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| ImportError::Unreadable(ImportError::filename(path)))?;
    let doc = XmlDoc::parse(&text)?;
    if doc.root_name().eq_ignore_ascii_case("fcpxml") {
        read_fcpxml(&doc, path, sequence_index)
    } else {
        read_fcp7(&doc, path, sequence_index)
    }
}

pub fn timeline_sequence_summaries(
    path: &Path,
) -> Result<Vec<TimelineSequenceSummary>, ImportError> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| ImportError::Unreadable(ImportError::filename(path)))?;
    let doc = XmlDoc::parse(&text)?;
    if doc.root_name().eq_ignore_ascii_case("fcpxml") {
        summaries_fcpxml(&doc)
    } else {
        summaries_fcp7(&doc, path)
    }
}

// ------------------------------------------------------------ FCP7

fn fcp7_rate(doc: &XmlDoc, idx: usize, fallback: MediaTime) -> MediaTime {
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

fn summaries_fcp7(doc: &XmlDoc, path: &Path) -> Result<Vec<TimelineSequenceSummary>, ImportError> {
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
                for m in doc.children_named(seq, "media") {
                    let _ = m;
                }
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

fn top_sequences(doc: &XmlDoc) -> Vec<usize> {
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

fn fcp7_files_by_id(doc: &XmlDoc) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    let mut stack = vec![doc.root];
    while let Some(idx) = stack.pop() {
        if doc.nodes[idx].name == "file" && doc.child_text(idx, "pathurl").is_some() {
            if let Some(id) = doc.attr(idx, "id") {
                map.entry(id.to_string()).or_insert(idx);
            }
        }
        for &c in doc.nodes[idx].children.iter().rev() {
            stack.push(c);
        }
    }
    map
}

/// file:// URL → local path (percent-decoded). Non-file URLs are None.
fn local_file_path(value: &str) -> Option<PathBuf> {
    let rest = value
        .strip_prefix("file://")
        .or_else(|| value.strip_prefix("FILE://"))?;
    let rest = rest
        .strip_prefix("localhost/")
        .or_else(|| rest.strip_prefix("localhost"))
        .unwrap_or(rest);
    Some(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 {
            if let (Some(h), Some(l)) = (hex_val(bytes.get(i + 1)), hex_val(bytes.get(i + 2))) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: Option<&u8>) -> Option<u8> {
    match b.copied()? {
        v @ b'0'..=b'9' => Some(v - b'0'),
        v @ b'a'..=b'f' => Some(v - b'a' + 10),
        v @ b'A'..=b'F' => Some(v - b'A' + 10),
        _ => None,
    }
}

struct ConstantRetime {
    playback_rate: f64,
    plays_backward: bool,
    source_in: f64,
    source_out: f64,
}

fn graph_retime(
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
    let slope = (lv1 - fv0) / (l0 - f0);
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

fn adjacent_transition_frame(
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
const MAX_NESTED_DEPTH: usize = 8;

/// Intersect an edit's timeline range with `[lo, hi]` (same time units)
/// and remap source in/out proportionally, honouring playback direction.
/// Returns false when nothing of the edit remains. Shared by nested
/// FCP 7 sequences, multiclips and FCPXML compound/multicam segments so
/// flattened media is never guessed outside its container window.
fn clip_edit_to_window(edit: &mut DraftEdit, lo: f64, hi: f64) -> bool {
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
struct NestedPlacement {
    frames: (i64, i64, i64, i64),
    id: String,
    name: String,
    enabled: bool,
}

/// Shared context for one track's clip items, top-level or nested.
/// Groups the long parameter lists so clippy stays quiet and nesting
/// passes one value instead of ten.
struct TrackCtx<'a> {
    doc: &'a XmlDoc,
    files_by_id: &'a HashMap<String, usize>,
    path: &'a Path,
    /// Frame rate of the current level (nested rate when nested).
    sequence_rate: MediaTime,
    media_type: DraftMediaKind,
    /// 1-based track number for plain leaf edits on this track.
    track_number: usize,
    track_enabled: bool,
    track_locked: bool,
    depth: usize,
    window: Option<NestedWindow>,
}

/// Affine window mapping nested-sequence (or multiclip-angle) seconds to
/// parent-timeline seconds for one nested placement.
#[derive(Clone, Debug)]
struct NestedWindow {
    /// Nested window bounds, nested seconds.
    nested_in: f64,
    nested_out: f64,
    /// Parent placement start, parent seconds.
    parent_start: f64,
    /// Parent seconds per nested second (1.0 without retime/rate change).
    speed: f64,
    /// Prefix for flattened edit ids (keeps them unique).
    id_prefix: String,
    /// Parent clipitem enabled flag (AND-combined).
    parent_enabled: bool,
    /// Drop verbatim remap payloads (parent speed makes them inconsistent).
    drop_verbatim: bool,
}

/// Stable per-track clip ids (`id` attr or generated fallback).
fn stable_clip_ids(
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

fn read_fcp7(
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
    let warn = |warnings: &mut Vec<SyncWarning>, message: String| {
        warnings.push(SyncWarning {
            url: path.to_path_buf(),
            message,
        });
    };

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
        fn read_track_items(
            ctx: &TrackCtx,
            items: &[usize],
            clip_ids: &HashMap<usize, String>,
            warnings: &mut Vec<SyncWarning>,
            edits: &mut Vec<DraftEdit>,
        ) {
            let warn = |warnings: &mut Vec<SyncWarning>, message: String| {
                warnings.push(SyncWarning {
                    url: ctx.path.to_path_buf(),
                    message,
                });
            };
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
                let timeline_seconds =
                    (end_frames - start_frames) as f64 * ctx.sequence_rate.as_seconds();
                let timing_tolerance =
                    item_rate.as_seconds().max(ctx.sequence_rate.as_seconds()) * 2.0;
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
                if let Some(r) = &retime {
                    if ((timeline_seconds * r.playback_rate) - (r.source_out - r.source_in)).abs()
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
                }
                if retime.is_none() && (source_seconds - timeline_seconds).abs() > timing_tolerance
                {
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
        fn read_nested_clipitem(
            ctx: &TrackCtx,
            clip_item: usize,
            items: &[usize],
            item_offset: usize,
            clip_ids: &HashMap<usize, String>,
            warnings: &mut Vec<SyncWarning>,
            edits: &mut Vec<DraftEdit>,
        ) -> bool {
            let warn = |warnings: &mut Vec<SyncWarning>, message: String| {
                warnings.push(SyncWarning {
                    url: ctx.path.to_path_buf(),
                    message,
                });
            };
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
                    Some(-1) => adjacent_transition_frame(
                        ctx.doc,
                        "start",
                        items,
                        item_offset.wrapping_sub(1),
                    )?,
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
                    format!(
                        "Invalid timing in nested container {parent_name} cannot be preserved."
                    ),
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
        fn read_nested_sequence(
            ctx: &TrackCtx,
            nested_seq: usize,
            placement: NestedPlacement,
            warnings: &mut Vec<SyncWarning>,
            edits: &mut Vec<DraftEdit>,
        ) {
            let warn = |warnings: &mut Vec<SyncWarning>, message: String| {
                warnings.push(SyncWarning {
                    url: ctx.path.to_path_buf(),
                    message,
                });
            };
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
            let drop_verbatim = (speed - 1.0).abs() > 0.000_001;
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
                        let nested_clip_ids = stable_clip_ids(
                            ctx.doc,
                            &nested_items,
                            nested_kind,
                            nested_track_number,
                        );
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
        fn read_multiclip(
            ctx: &TrackCtx,
            multiclip: usize,
            placement: NestedPlacement,
            warnings: &mut Vec<SyncWarning>,
            edits: &mut Vec<DraftEdit>,
        ) {
            let warn = |warnings: &mut Vec<SyncWarning>, message: String| {
                warnings.push(SyncWarning {
                    url: ctx.path.to_path_buf(),
                    message,
                });
            };
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
                let Some(angle_clip) = ctx.doc.children_named(angle, "clip").first().copied()
                else {
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
                        format!(
                            "Invalid timing in multicam clip {parent_name} cannot be preserved."
                        ),
                    );
                    continue;
                }
                let speed = parent_dur / window_dur;
                let drop_verbatim = (speed - 1.0).abs() > 0.000_001;
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
                            ctx.doc.nodes[c].name == "clipitem"
                                || ctx.doc.nodes[c].name == "transitionitem"
                        })
                        .collect();
                    if angle_items.is_empty() {
                        continue;
                    }
                    let angle_track_number = ctx.track_number + angle_j;
                    let angle_clip_ids =
                        stable_clip_ids(ctx.doc, &angle_items, kind, angle_track_number);
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
            "Cross Dissolve is preserved in Premiere XML and Resolve OTIO; the optional Resolve precision importer currently flattens transitions to overlapping media.".into(),
        );
    }
    if transitions
        .iter()
        .any(|t| t.media_type == DraftMediaKind::Audio)
    {
        warn(
            &mut warnings,
            "Audio transitions are preserved in Premiere XML when clip identity is unambiguous; Resolve OTIO and the optional precision importer keep their media overlap but cannot represent the FCP 7 audio effect portably.".into(),
        );
    }
    if edits.iter().any(|e| e.plays_backward)
        && edits
            .iter()
            .any(|e| e.plays_backward && (e.playback_rate - 1.0).abs() > 0.000_001)
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

fn unique_warnings(warnings: Vec<SyncWarning>) -> Vec<SyncWarning> {
    let mut seen = HashSet::new();
    warnings
        .into_iter()
        .filter(|w| seen.insert(w.message.clone()))
        .collect()
}

// ------------------------------------------------------------ FCPXML

fn fcpxml_time(text: Option<&str>) -> f64 {
    let mut s = text.unwrap_or("").trim().to_string();
    if s.is_empty() {
        return 0.0;
    }
    if s.ends_with('s') {
        s.pop();
    }
    if let Some((num, den)) = s.split_once('/') {
        if let (Ok(n), Ok(d)) = (num.parse::<f64>(), den.parse::<f64>()) {
            if d != 0.0 {
                return n / d;
            }
        }
    }
    s.parse().unwrap_or(0.0)
}

fn fcpxml_frame_duration(text: Option<&str>) -> MediaTime {
    let mut s = text.unwrap_or("").trim().to_string();
    if s.is_empty() {
        return MediaTime::new(1, 25);
    }
    if s.ends_with('s') {
        s.pop();
    }
    if let Some((num, den)) = s.split_once('/') {
        if let (Ok(n), Ok(d)) = (num.parse::<i64>(), den.parse::<i32>()) {
            if d != 0 {
                return MediaTime::new(n, d);
            }
        }
    }
    if let Ok(seconds) = s.parse::<f64>() {
        if seconds > 0.0 {
            let fps = (1.0 / seconds).round().max(1.0) as i32;
            return MediaTime::new(1, fps);
        }
    }
    MediaTime::new(1, 25)
}

fn summaries_fcpxml(doc: &XmlDoc) -> Result<Vec<TimelineSequenceSummary>, ImportError> {
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
fn inside_clip(doc: &XmlDoc, mut idx: usize) -> bool {
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
fn inside_media(doc: &XmlDoc, mut idx: usize) -> bool {
    while let Some(p) = doc.nodes[idx].parent {
        if doc.nodes[p].name == "media" {
            return true;
        }
        idx = p;
    }
    false
}

fn fcpxml_url(src: &str) -> PathBuf {
    if let Some(rest) = src.strip_prefix("file://") {
        let rest = rest
            .strip_prefix("localhost/")
            .or_else(|| rest.strip_prefix("localhost"))
            .unwrap_or(rest);
        PathBuf::from(percent_decode(rest))
    } else {
        PathBuf::from(src)
    }
}

fn read_fcpxml(
    doc: &XmlDoc,
    path: &Path,
    sequence_index: Option<usize>,
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

    struct AssetInfo {
        url: PathBuf,
        has_video: bool,
        has_audio: bool,
    }

    /// Shared FCPXML lookup tables for the story walker.
    struct FcpxmlRefs<'a> {
        doc: &'a XmlDoc,
        path: &'a Path,
        assets: &'a HashMap<String, AssetInfo>,
        compounds: &'a HashMap<String, usize>,
        multicams: &'a HashMap<String, usize>,
    }

    /// Placement accumulated while descending FCPXML containers.
    #[derive(Clone, Copy)]
    struct StoryPlacement {
        shift: f64,
        enabled_and: bool,
        lane_shift: i64,
        depth: usize,
    }

    let mut assets: HashMap<String, AssetInfo> = HashMap::new();
    let mut stack = vec![doc.root];
    while let Some(n) = stack.pop() {
        if doc.nodes[n].name == "resources" {
            // Direct + nested asset elements under resources.
            let mut inner = vec![n];
            while let Some(m) = inner.pop() {
                for &c in doc.nodes[m].children.iter().rev() {
                    if doc.nodes[c].name == "asset" {
                        let (Some(id), Some(src)) = (doc.attr(c, "id"), {
                            doc.children_named(c, "media-rep")
                                .first()
                                .and_then(|&mr| doc.attr(mr, "src"))
                        }) else {
                            continue;
                        };
                        assets.insert(
                            id.to_string(),
                            AssetInfo {
                                url: fcpxml_url(src),
                                has_video: doc.attr(c, "hasVideo") != Some("0"),
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
            if doc.nodes[n].name == "media" {
                if let Some(id) = doc.attr(n, "id").map(str::to_string) {
                    if !doc.children_named(n, "sequence").is_empty() {
                        compounds.entry(id.clone()).or_insert(n);
                    }
                    if !doc.children_named(n, "multicam").is_empty() {
                        multicams.entry(id).or_insert(n);
                    }
                }
            }
            for &c in doc.nodes[n].children.iter().rev() {
                stack.push(c);
            }
        }
    }

    fn emit_fcpxml_asset_clip(
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
        let start = fcpxml_time(refs.doc.attr(clip, "start"));
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
                playback_rate: 1.0,
                plays_backward: false,
                fcp7_time_remap_xml: None,
                fcp7_filter_xmls: Vec::new(),
                fcp7_retime_in: None,
                fcp7_retime_out: None,
                fcp7_retime_duration: None,
                fcp7_labels_xml: None,
                time_scale: 1_000_000,
                include_embedded_audio: true,
                audio_source_channel: None,
                fcpxml_audio_role: None,
                track_index: (lane + place.lane_shift).max(0) as usize,
                enabled,
                track_enabled: true,
                track_locked: false,
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
                playback_rate: 1.0,
                plays_backward: false,
                fcp7_time_remap_xml: None,
                fcp7_filter_xmls: Vec::new(),
                fcp7_retime_in: None,
                fcp7_retime_out: None,
                fcp7_retime_duration: None,
                fcp7_labels_xml: None,
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
                enabled,
                track_enabled: true,
                track_locked: false,
                linked_edit_ids: if asset.has_video && video_enabled {
                    [video_id.clone()].into_iter().collect()
                } else {
                    HashSet::new()
                },
            });
        }
    }

    fn emit_fcpxml_std_clip(
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
        let mut videos = Vec::new();
        let mut audios = Vec::new();
        {
            let mut stack = vec![clip];
            while let Some(n) = stack.pop() {
                for &c in refs.doc.nodes[n].children.iter().rev() {
                    if refs.doc.nodes[c].name == "video" {
                        videos.push(c);
                    } else if refs.doc.nodes[c].name == "audio" {
                        audios.push(c);
                    }
                    stack.push(c);
                }
            }
        }
        for vid in videos {
            let (Some(id), Some(asset)) = (
                refs.doc.attr(vid, "ref"),
                refs.doc.attr(vid, "ref").and_then(|r| refs.assets.get(r)),
            ) else {
                continue;
            };
            let _ = id;
            let edit_id = format!("edit-{next_number}");
            *next_number += 1;
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
                media_type: DraftMediaKind::Video,
                source_in: start,
                source_out: start + duration,
                timeline_start: offset + place.shift,
                timeline_end: offset + place.shift + duration,
                playback_rate: 1.0,
                plays_backward: false,
                fcp7_time_remap_xml: None,
                fcp7_filter_xmls: Vec::new(),
                fcp7_retime_in: None,
                fcp7_retime_out: None,
                fcp7_retime_duration: None,
                fcp7_labels_xml: None,
                time_scale: 1_000_000,
                include_embedded_audio: true,
                audio_source_channel: None,
                fcpxml_audio_role: None,
                track_index: (lane + place.lane_shift).max(0) as usize,
                enabled,
                track_enabled: true,
                track_locked: false,
                linked_edit_ids: HashSet::new(),
            });
        }
        for aud in audios {
            let (Some(id), Some(asset)) = (
                refs.doc.attr(aud, "ref"),
                refs.doc.attr(aud, "ref").and_then(|r| refs.assets.get(r)),
            ) else {
                continue;
            };
            let _ = id;
            let edit_id = format!("edit-{next_number}");
            *next_number += 1;
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
                media_type: DraftMediaKind::Audio,
                source_in: start,
                source_out: start + duration,
                timeline_start: offset + place.shift,
                timeline_end: offset + place.shift + duration,
                playback_rate: 1.0,
                plays_backward: false,
                fcp7_time_remap_xml: None,
                fcp7_filter_xmls: Vec::new(),
                fcp7_retime_in: None,
                fcp7_retime_out: None,
                fcp7_retime_duration: None,
                fcp7_labels_xml: None,
                time_scale: 1_000_000,
                include_embedded_audio: true,
                audio_source_channel: None,
                fcpxml_audio_role: refs
                    .doc
                    .attr(aud, "role")
                    .or_else(|| refs.doc.attr(clip, "audioRole"))
                    .map(str::to_string),
                track_index: (lane + place.lane_shift).abs().max(0) as usize,
                enabled,
                track_enabled: true,
                track_locked: false,
                linked_edit_ids: HashSet::new(),
            });
        }
    }

    // Recursive story walker: flat clips emit, compound/multicam/sync
    // containers recurse, everything else is descended into. Timing is
    // absolute seconds throughout; `place.shift` translates container-local
    // offsets into the parent timeline.
    fn read_fcpxml_story(
        refs: &FcpxmlRefs,
        nodes: &[usize],
        place: StoryPlacement,
        visited: &mut Vec<String>,
        warnings: &mut Vec<SyncWarning>,
        edits: &mut Vec<DraftEdit>,
        next_number: &mut usize,
    ) {
        let warn = |warnings: &mut Vec<SyncWarning>, message: String| {
            warnings.push(SyncWarning {
                url: refs.path.to_path_buf(),
                message,
            });
        };
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
                    let start = fcpxml_time(refs.doc.attr(node, "start"));
                    let enabled = refs.doc.attr(node, "enabled") != Some("0") && place.enabled_and;
                    let lane = refs
                        .doc
                        .attr(node, "lane")
                        .and_then(|s| s.parse::<i64>().ok())
                        .unwrap_or(0)
                        + place.lane_shift;
                    let spine = refs
                        .doc
                        .children_named(seq_node, "sequence")
                        .first()
                        .and_then(|&s| {
                            // Compound media wraps one sequence; its spine
                            // holds the story elements.
                            refs.doc.children_named(s, "spine").first().copied()
                        });
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
    fn read_fcpxml_multicam(
        refs: &FcpxmlRefs,
        node: usize,
        place: StoryPlacement,
        visited: &mut Vec<String>,
        warnings: &mut Vec<SyncWarning>,
        edits: &mut Vec<DraftEdit>,
        next_number: &mut usize,
    ) {
        let warn = |warnings: &mut Vec<SyncWarning>, message: String| {
            warnings.push(SyncWarning {
                url: refs.path.to_path_buf(),
                message,
            });
        };
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
        let start = fcpxml_time(refs.doc.attr(node, "start"));
        let enabled = refs.doc.attr(node, "enabled") != Some("0") && place.enabled_and;
        let lane = refs
            .doc
            .attr(node, "lane")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
            + place.lane_shift;
        let mc_end = offset + duration;
        let mut angles: HashMap<String, usize> = HashMap::new();
        if let Some(&mcam) = refs.doc.children_named(mc_node, "multicam").first() {
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
                    ("all", _)
                        | ("audio", DraftMediaKind::Audio)
                        | ("video", DraftMediaKind::Video)
                );
                if kind_ok && clip_edit_to_window(&mut e, seg_lo, seg_hi) {
                    kept.push(e);
                }
            }
            edits.extend(kept);
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
        shift: 0.0,
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

#[cfg(test)]
mod tests {
    use super::*;

    const FCP7: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xmeml version="4">
<sequence>
<name>Seq</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media>
<video>
<track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="a1">
<name>Shot A</name>
<enabled>TRUE</enabled>
<in>0</in><out>104</out><start>0</start><end>-1</end>
<labels><label2>Mango</label2></labels>
<file id="fa"><name>A</name><pathurl>file:///tmp/a.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
<link><linkclipref>aa1</linkclipref></link>
</clipitem>
<transitionitem>
<start>96</start><end>104</end><alignment>center</alignment>
<effect>
<name>Cross Dissolve</name><effectid>Cross Dissolve</effectid>
<effectcategory>Dissolve</effectcategory>
<effecttype>transition</effecttype><mediatype>video</mediatype>
<startratio>0</startratio><endratio>1</endratio><reverse>FALSE</reverse>
</effect>
</transitionitem>
<clipitem id="b1">
<name>Shot B</name>
<enabled>TRUE</enabled>
<in>46</in><out>150</out><start>-1</start><end>200</end>
<file id="fb"><name>B</name><pathurl>file:///tmp/b.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
<filter>
<effect>
<name>Time Remap</name><effectid>timeremap</effectid>
<parameter authoringApp="PremierePro"><parameterid>graphdict</parameterid>
<keyframe><when>0</when><value>0</value></keyframe>
<keyframe><when>150</when><value>150</value></keyframe>
</parameter>
</effect>
</filter>
</clipitem>
</track>
</video>
<audio>
<track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="aa1">
<name>Shot A Audio</name>
<enabled>TRUE</enabled>
<in>0</in><out>100</out><start>0</start><end>100</end>
<file id="fa2"><name>A</name><pathurl>file:///tmp/a.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
<link><linkclipref>a1</linkclipref></link>
</clipitem>
</track>
</audio>
</media>
</sequence>
</xmeml>"#;

    const FCPXML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<fcpxml version="1.10">
<resources>
<format id="r1" frameDuration="1/25s"/>
<asset id="a1" hasVideo="1" hasAudio="1"><media-rep src="file:///tmp/c.mov"/></asset>
</resources>
<library><event><project name="Proj">
<sequence format="r1" name="Cut">
<spine>
<asset-clip ref="a1" name="Clip C" offset="0s" duration="100/25s" start="0s" lane="1" audioRole="dialogue.interview"/>
</spine>
</sequence>
</project></event></library>
</fcpxml>"#;

    #[test]
    fn fcp7_sequence_summary_and_edits() {
        let dir = std::env::temp_dir().join(format!("align-xml-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("edit.xml");
        std::fs::write(&xml, FCP7).unwrap();

        let summaries = timeline_sequence_summaries(&xml).expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "Seq");
        assert_eq!(summaries[0].clip_count, 3);

        let draft = read_timeline(&xml, None).expect("read");
        assert_eq!(draft.name, "Seq");
        assert_eq!(draft.frame_duration, MediaTime::new(1, 25));
        // 2 video + 1 audio edits.
        assert_eq!(draft.edits.len(), 3);
        let a = draft.edits.iter().find(|e| e.id == "a1").expect("a1");
        assert_eq!(a.url, PathBuf::from("/tmp/a.mov"));
        assert!((a.source_in - 0.0).abs() < 1e-9);
        assert!((a.source_out - 4.16).abs() < 1e-9);
        assert!((a.timeline_start - 0.0).abs() < 1e-9);
        assert_eq!(
            a.fcp7_labels_xml.as_deref(),
            Some("<labels><label2>Mango</label2></labels>")
        );
        // Retimed clip B keeps its graph: 100% forward.
        let b = draft.edits.iter().find(|e| e.id == "b1").expect("b1");
        assert!((b.playback_rate - 1.0).abs() < 1e-9);
        assert!(!b.plays_backward);
        assert!(
            b.fcp7_time_remap_xml
                .as_ref()
                .unwrap()
                .contains("timeremap")
        );
        assert_eq!(b.fcp7_retime_in, Some(46));
        // Portable cross dissolve detected.
        assert_eq!(draft.transitions.len(), 1);
        assert!(draft.transitions[0].is_otio_portable);
        assert_eq!(
            draft.transitions[0].kind,
            TimelineTransitionKind::CrossDissolve
        );
        assert!(draft.transitions[0].effect_xml.contains("Cross Dissolve"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fcp7_multi_sequence_requires_choice() {
        let doc = XmlDoc::parse(
            "<xmeml><sequence><name>A</name></sequence><sequence><name>B</name></sequence></xmeml>",
        )
        .unwrap();
        assert_eq!(top_sequences(&doc).len(), 2);
        let dir = std::env::temp_dir().join(format!("align-xml2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("multi.xml");
        std::fs::write(
            &xml,
            "<xmeml><sequence><name>A</name></sequence><sequence><name>B</name></sequence></xmeml>",
        )
        .unwrap();
        assert!(matches!(
            read_timeline(&xml, None),
            Err(ImportError::MultipleSequences(_, 2))
        ));
        assert!(matches!(
            read_timeline(&xml, Some(5)),
            Err(ImportError::InvalidSequence(_, 5))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fcpxml_reads_assets_and_links() {
        let dir = std::env::temp_dir().join(format!("align-xml3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("edit.fcpxml");
        std::fs::write(&xml, FCPXML).unwrap();
        let summaries = timeline_sequence_summaries(&xml).expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "Proj");
        assert_eq!(summaries[0].clip_count, 1);
        let draft = read_timeline(&xml, None).expect("read");
        assert_eq!(draft.frame_duration, MediaTime::new(1, 25));
        assert_eq!(draft.edits.len(), 2);
        let video = draft
            .edits
            .iter()
            .find(|e| e.media_type == DraftMediaKind::Video)
            .unwrap();
        let audio = draft
            .edits
            .iter()
            .find(|e| e.media_type == DraftMediaKind::Audio)
            .unwrap();
        assert_eq!(video.url, PathBuf::from("/tmp/c.mov"));
        assert!(video.linked_edit_ids.contains(&audio.id));
        assert!(audio.linked_edit_ids.contains(&video.id));
        assert_eq!(
            audio.fcpxml_audio_role.as_deref(),
            Some("dialogue.interview")
        );
        assert!((video.timeline_end - 4.0).abs() < 1e-9);
        assert_eq!(video.track_index, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_file_paths_decode() {
        assert_eq!(
            local_file_path("file:///tmp/my%20clip.mov"),
            Some(PathBuf::from("/tmp/my clip.mov"))
        );
        assert_eq!(local_file_path("https://x/y.mov"), None);
        assert_eq!(local_file_path("/tmp/plain.mov"), None);
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
    }

    #[test]
    fn resolve_links_and_dedupes() {
        use crate::model::{AudioSummary, VideoSummary};
        let dir = std::env::temp_dir().join(format!("align-xml4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("edit.xml");
        std::fs::write(&xml, FCP7).unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        let mk_clip = |id: &str, url: &str, kind: MediaKind| Clip {
            id: crate::model::ClipId::new(id),
            url: PathBuf::from(url),
            kind,
            duration: MediaTime::seconds(10.0),
            audio: vec![AudioSummary {
                sample_rate: 48000.0,
                channels: 2,
                bit_depth: None,
                is_float: None,
                source_timecode: None,
            }],
            video: if kind == MediaKind::Video {
                Some(VideoSummary {
                    width: 1920,
                    height: 1080,
                    frame_duration: Some(MediaTime::new(1, 25)),
                    source_timecode: None,
                    frame_rate_mode: None,
                })
            } else {
                None
            },
            recorded_at: None,
            recorded_at_source: None,
            source_identifier: None,
            media_span: None,
        };
        // /tmp/a.mov is video+audio, /tmp/b.mov video-only here.
        let clips = vec![
            mk_clip("ca", "/tmp/a.mov", MediaKind::Video),
            mk_clip("cb", "/tmp/b.mov", MediaKind::Video),
        ];
        let timeline = draft.resolve(&clips);
        assert_eq!(timeline.edits.len(), 2);
        let va = timeline.edits.iter().find(|e| e.id == "a1").expect("a1");
        assert_eq!(va.clip_id.0, "ca");
        // Embedded audio linked via mutual linkclipref.
        let linked = va.linked_audio_edit.as_ref().expect("linked");
        assert_eq!(linked.id, "aa1");
        assert_eq!(va.audio_track_index, Some(1));
        // b.mov has no inspected audio: video edit resolves without link.
        let vb = timeline.edits.iter().find(|e| e.id == "b1").expect("b1");
        assert!(vb.linked_audio_edit.is_none());
        // Unresolved media warns once per path.
        let warnings = draft.unresolved_warnings(&clips, &[]);
        assert!(warnings.is_empty());
        let warnings = draft.unresolved_warnings(&clips[..1], &[]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("/tmp/b.mov"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relink_unique_filename() {
        let dir = std::env::temp_dir().join(format!("align-xml5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("edit.xml");
        std::fs::write(&xml, FCP7).unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        // /tmp/*.mov do not exist; provide same-named candidates.
        let media = dir.join("media");
        std::fs::create_dir_all(&media).unwrap();
        let cand = media.join("a.mov");
        std::fs::write(&cand, b"fake").unwrap();
        let relinked = draft.relinking_missing_media(std::slice::from_ref(&cand), &[], &[]);
        assert!(relinked.edits.iter().any(|e| e.url == cand));
        assert!(
            relinked
                .warnings
                .iter()
                .any(|w| w.message.contains("Relinked a.mov"))
        );
        // Ambiguous candidates warn instead of guessing.
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let cand2 = other.join("a.mov");
        std::fs::write(&cand2, b"fake").unwrap();
        let relinked = draft.relinking_missing_media(&[cand, cand2], &[], &[]);
        assert!(
            relinked
                .warnings
                .iter()
                .any(|w| w.message.contains("Could not relink a.mov: 2"))
        );

        // An unchanged card/day suffix resolves duplicate filenames after
        // the project root moves; a shorter common suffix must not win.
        let mut deep = draft.clone();
        for edit in &mut deep.edits {
            if edit.url.file_name().and_then(|name| name.to_str()) == Some("a.mov") {
                edit.url = PathBuf::from("/offline/day-1/card-a/a.mov");
            }
        }
        let exact_dir = dir.join("new-root/day-1/card-a");
        let weaker_dir = dir.join("new-root/day-2/card-a");
        std::fs::create_dir_all(&exact_dir).unwrap();
        std::fs::create_dir_all(&weaker_dir).unwrap();
        let exact = exact_dir.join("a.mov");
        let weaker = weaker_dir.join("a.mov");
        std::fs::write(&exact, b"fake").unwrap();
        std::fs::write(&weaker, b"fake").unwrap();
        let relinked = deep.relinking_missing_media(&[weaker, exact.clone()], &[], &[]);
        assert!(relinked.edits.iter().any(|edit| edit.url == exact));
        assert!(
            relinked
                .warnings
                .iter()
                .any(|warning| warning.message.contains("matching 2 parent folders"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relink_saved_redirection_without_pool() {
        use crate::redirect::PathRedirection;

        let dir = std::env::temp_dir().join(format!("align-xml-redir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("edit.xml");
        std::fs::write(&xml, FCP7).unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        let media = dir.join("card");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("a.mov"), b"fake").unwrap();
        std::fs::write(media.join("b.mov"), b"fake").unwrap();
        let redirects = vec![PathRedirection::new("/tmp", media.clone())];
        let relinked = draft.relinking_missing_media(&[], &redirects, &[]);
        assert!(relinked.edits.iter().any(|e| e.url == media.join("a.mov")));
        assert!(relinked.edits.iter().any(|e| e.url == media.join("b.mov")));
        assert!(
            relinked
                .warnings
                .iter()
                .any(|w| w.message.contains("via saved redirection"))
        );
        // A rewrite pointing at nothing falls back to silence, not a guess.
        let dead = vec![PathRedirection::new("/tmp", dir.join("empty"))];
        let relinked = draft.relinking_missing_media(&[], &dead, &[]);
        assert!(
            relinked
                .warnings
                .iter()
                .all(|w| !w.message.contains("via saved redirection"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relink_manual_pick_breaks_ties() {
        let dir = std::env::temp_dir().join(format!("align-xml-manual-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("edit.xml");
        std::fs::write(&xml, FCP7).unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        let dup1 = dir.join("dup1");
        let dup2 = dir.join("dup2");
        std::fs::create_dir_all(&dup1).unwrap();
        std::fs::create_dir_all(&dup2).unwrap();
        let pick = dup1.join("a.mov");
        std::fs::write(&pick, b"fake").unwrap();
        std::fs::write(dup2.join("a.mov"), b"fake").unwrap();
        let manual = vec![("a.mov".to_string(), pick.clone())];
        let relinked =
            draft.relinking_missing_media(&[dup2.join("a.mov"), pick.clone()], &[], &manual);
        assert!(relinked.edits.iter().any(|e| e.url == pick));
        assert!(
            relinked
                .warnings
                .iter()
                .any(|w| w.message.contains("(manual)"))
        );
        // A manual target that does not exist warns instead of relinking.
        let missing = vec![("b.mov".to_string(), dir.join("ghost.mov"))];
        let relinked = draft.relinking_missing_media(&[], &[], &missing);
        assert!(
            relinked
                .warnings
                .iter()
                .any(|w| w.message.contains("does not exist"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unresolved_warnings_skip_omitted_extensions() {
        let dir = std::env::temp_dir().join(format!("align-xml-omit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("edit.xml");
        std::fs::write(&xml, FCP7).unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        assert!(!draft.unresolved_warnings(&[], &[]).is_empty());
        assert!(
            draft
                .unresolved_warnings(&[], &["mov".to_string()])
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn reverse_retime_imports_backward_with_negative_otio_scalar() {
        // Live-verified Resolve behavior: negative LinearTimeWarp plays
        // backward from the same source range as forward playback.
        let dir = std::env::temp_dir().join(format!("align-xml-rev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("rev.xml");
        // Reverse -100% with Premiere's +1 constant: values fall 1 per
        // frame from 101 to 1 over the clip (see VERIFICATION.md).
        // (0,101),(100,1): raw slope -1 → backward; shifted (0,100),(100,0).
        std::fs::write(
            &xml,
            r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>R</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<clipitem id="r1"><name>Rev</name>
<in>0</in><out>100</out><start>0</start><end>100</end>
<file id="f"><pathurl>file:///tmp/r.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>100</duration></file>
<filter><effect><name>Time Remap</name><effectid>timeremap</effectid>
<parameter><parameterid>graphdict</parameterid>
<keyframe><when>0</when><value>101</value></keyframe>
<keyframe><when>100</when><value>1</value></keyframe>
</parameter></effect></filter>
</clipitem>
</track></video></media>
</sequence></xmeml>"#,
        )
        .unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        assert_eq!(draft.edits.len(), 1);
        let edit = &draft.edits[0];
        assert!(edit.plays_backward);
        assert!((edit.playback_rate - 1.0).abs() < 1e-9);
        // Same source range as forward playback (no phase guessing).
        assert!((edit.source_in - 0.0).abs() < 1e-9);
        assert!((edit.source_out - 4.0).abs() < 1e-9);
        assert!(
            edit.fcp7_time_remap_xml
                .as_ref()
                .unwrap()
                .contains("timeremap")
        );

        // OTIO writer emits the negative scalar.
        let timeline = draft_to_otio_timeline(&draft);
        let bytes = crate::export::otio::data(&timeline).expect("otio");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(
            text.contains("\"time_scalar\": -1.0"),
            "negative warp missing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_portable_dissolve_warns_and_flattens_in_otio() {
        // Odd duration (not centered-even) → Premiere keeps it, OTIO gets
        // overlap instead of a Transition.
        let dir = std::env::temp_dir().join(format!("align-xml-np-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("np.xml");
        std::fs::write(
            &xml,
            r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>N</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<clipitem id="a1"><in>0</in><out>104</out><start>0</start><end>-1</end>
<file id="fa"><pathurl>file:///tmp/a.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem>
<transitionitem><start>96</start><end>103</end><alignment>center</alignment>
<effect><name>Cross Dissolve</name><effectid>Cross Dissolve</effectid>
<effectcategory>Dissolve</effectcategory><effecttype>transition</effecttype><mediatype>video</mediatype>
<startratio>0</startratio><endratio>1</endratio><reverse>FALSE</reverse></effect>
</transitionitem>
<clipitem id="b1"><in>46</in><out>150</out><start>-1</start><end>200</end>
<file id="fb"><pathurl>file:///tmp/b.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem>
</track></video></media>
</sequence></xmeml>"#,
        )
        .unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        assert_eq!(draft.transitions.len(), 1);
        assert!(!draft.transitions[0].is_otio_portable);
        assert!(
            draft
                .warnings
                .iter()
                .any(|w| w.message.contains("flattened in Resolve OTIO"))
        );
        // Premiere writer still emits the transition item verbatim.
        let timeline = draft_to_otio_timeline(&draft);
        let premiere = crate::export::premiere::write(
            &timeline,
            crate::export_model::TimelineExportFormat::PremiereXML,
            false,
        );
        assert!(premiere.contains("<transitionitem>"));
        // OTIO has clips + overlap but no Transition node.
        let bytes = crate::export::otio::data(&timeline).expect("otio");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(!text.contains("Transition.1"));
        assert!(text.contains("Clip.2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn variable_speed_imports_as_average_rate() {
        // 3-keyframe ramp: constant fit fails, average rate wins with warning.
        let dir = std::env::temp_dir().join(format!("align-xml-var-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("var.xml");
        std::fs::write(
            &xml,
            r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>V</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<clipitem id="v1"><name>Ramp</name>
<in>0</in><out>100</out><start>0</start><end>50</end>
<file id="f"><pathurl>file:///tmp/v.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
<filter><effect><name>Time Remap</name><effectid>timeremap</effectid>
<parameter><parameterid>graphdict</parameterid>
<keyframe><when>0</when><value>0</value></keyframe>
<keyframe><when>50</when><value>25</value></keyframe>
<keyframe><when>100</when><value>100</value></keyframe>
</parameter></effect></filter>
</clipitem>
</track></video></media>
</sequence></xmeml>"#,
        )
        .unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        assert_eq!(draft.edits.len(), 1);
        // Average slope (100-0)/(100-0) = 1 → 100% constant.
        assert!((draft.edits[0].playback_rate - 1.0).abs() < 1e-9);
        assert!(!draft.edits[0].plays_backward);
        assert!(
            draft
                .warnings
                .iter()
                .any(|w| w.message.contains("Variable speed change"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fcp7_nested_sequence_flattens_with_window_mapping() {
        // Parent uses nested seconds 1..5 of an 8 s inner timeline at 0..4 s.
        let dir = std::env::temp_dir().join(format!("align-xml-nest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("nest.xml");
        std::fs::write(
            &xml,
            r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>Main</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="n1"><name>Nested Story</name>
<in>25</in><out>125</out><start>0</start><end>100</end>
<sequence><name>Inner</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="inner-v1"><name>Inner Video</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fv"><pathurl>file:///tmp/nv.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem>
</track></video>
<audio><track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="inner-a1"><name>Inner Audio</name>
<in>10</in><out>210</out><start>0</start><end>200</end>
<file id="fa"><pathurl>file:///tmp/na.wav</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>300</duration></file>
</clipitem>
</track></audio></media>
</sequence>
</clipitem>
<clipitem id="d1"><name>Direct</name>
<in>0</in><out>50</out><start>100</start><end>150</end>
<file id="fd"><pathurl>file:///tmp/d.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>50</duration></file>
</clipitem>
</track></video></media>
</sequence></xmeml>"#,
        )
        .unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        assert_eq!(draft.edits.len(), 3);
        let v = draft
            .edits
            .iter()
            .find(|e| e.id == "n1/inner-v1")
            .expect("nested video");
        assert_eq!(v.url, PathBuf::from("/tmp/nv.mov"));
        assert!((v.source_in - 1.0).abs() < 1e-9);
        assert!((v.source_out - 5.0).abs() < 1e-9);
        assert!((v.timeline_start - 0.0).abs() < 1e-9);
        assert!((v.timeline_end - 4.0).abs() < 1e-9);
        assert!((v.playback_rate - 1.0).abs() < 1e-9);
        assert_eq!(v.track_index, 1);
        let a = draft
            .edits
            .iter()
            .find(|e| e.id == "n1/inner-a1")
            .expect("nested audio");
        assert_eq!(a.url, PathBuf::from("/tmp/na.wav"));
        assert!((a.source_in - 1.4).abs() < 1e-9);
        assert!((a.source_out - 5.4).abs() < 1e-9);
        assert!((a.timeline_start - 0.0).abs() < 1e-9);
        assert!((a.timeline_end - 4.0).abs() < 1e-9);
        assert_eq!(a.track_index, 1);
        let d = draft.edits.iter().find(|e| e.id == "d1").expect("direct");
        assert!((d.timeline_start - 4.0).abs() < 1e-9);
        assert!(
            !draft
                .warnings
                .iter()
                .any(|w| w.message.contains("no readable local file path")),
            "nested container must not raise the missing-file warning"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fcp7_multiclip_uses_marked_angles_with_fallback_warning() {
        let dir = std::env::temp_dir().join(format!("align-xml-mc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("mc.xml");
        std::fs::write(
            &xml,
            r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>MC</name>
<rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><video><track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="m1"><name>Interview MC</name>
<in>0</in><out>100</out><start>0</start><end>100</end>
<multiclip><name>Interview MC</name>
<angle><clip><name>Cam A</name><media>
<video><videotrack><clipitem id="ang-a-v"><name>Cam A Video</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fca"><pathurl>file:///tmp/ca.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></videotrack></video>
<audio><audiotrack><clipitem id="ang-a-a"><name>Cam A Audio</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fca2"><pathurl>file:///tmp/ca.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></audiotrack></audio>
</media></clip>
<activevideoangle/></angle>
<angle><clip><name>Cam B</name><media>
<video><videotrack><clipitem id="ang-b-v"><name>Cam B Video</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fcb"><pathurl>file:///tmp/cb.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></videotrack></video>
<audio><audiotrack><clipitem id="ang-b-a"><name>Cam B Audio</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fcb2"><pathurl>file:///tmp/cb.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></audiotrack></audio>
</media></clip>
<activeaudioangle/></angle>
</multiclip>
</clipitem>
</track></video>
<audio><track>
<enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="m2"><name>Lone Angle</name>
<in>0</in><out>100</out><start>0</start><end>100</end>
<multiclip><name>Lone</name>
<angle><clip><name>Cam C</name><media>
<video><videotrack><clipitem id="ang-c-v"><name>Cam C Video</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fcc"><pathurl>file:///tmp/cc.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></videotrack></video>
<audio><audiotrack><clipitem id="ang-c-a"><name>Cam C Audio</name>
<in>0</in><out>200</out><start>0</start><end>200</end>
<file id="fcc2"><pathurl>file:///tmp/cc.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>200</duration></file>
</clipitem></audiotrack></audio>
</media></clip>
</angle>
</multiclip>
</clipitem>
</track></audio></media>
</sequence></xmeml>"#,
        )
        .unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        // Marked angles: video from Cam A, audio from Cam B.
        let v = draft
            .edits
            .iter()
            .find(|e| {
                e.media_type == DraftMediaKind::Video && e.url.to_string_lossy() == "/tmp/ca.mov"
            })
            .expect("marked video angle");
        assert!((v.timeline_start - 0.0).abs() < 1e-9);
        assert!((v.timeline_end - 4.0).abs() < 1e-9);
        assert!((v.source_in - 0.0).abs() < 1e-9);
        assert!((v.source_out - 4.0).abs() < 1e-9);
        let a = draft
            .edits
            .iter()
            .find(|e| {
                e.media_type == DraftMediaKind::Audio && e.url.to_string_lossy() == "/tmp/cb.mov"
            })
            .expect("marked audio angle");
        assert!((a.timeline_start - 0.0).abs() < 1e-9);
        assert!((a.timeline_end - 4.0).abs() < 1e-9);
        // Unmarked multiclip falls back to the first angle with a warning.
        assert!(
            draft
                .edits
                .iter()
                .any(|e| e.url.to_string_lossy() == "/tmp/cc.mov"),
            "fallback angle flattened"
        );
        assert!(
            draft
                .warnings
                .iter()
                .any(|w| w.message.contains("first angle")),
            "fallback warns"
        );
        assert!(
            !draft
                .warnings
                .iter()
                .any(|w| w.message.contains("no readable local file path")),
            "multiclip must not raise the missing-file warning"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fcpxml_compound_and_nested_compound_flatten() {
        // ref-clip C maps compound [0.8s, 5.6s] onto [0.4s, 5.2s];
        // inner ref-clip D maps [0s, 2s] of compound E onto [4s, 6s] of C.
        let dir = std::env::temp_dir().join(format!("align-xml-cmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("cmp.fcpxml");
        std::fs::write(
            &xml,
            r#"<?xml version="1.0"?><fcpxml version="1.10">
<resources>
<format id="r1" frameDuration="1/25s"/>
<asset id="a1" hasVideo="1" hasAudio="1"><media-rep src="file:///tmp/x.mov"/></asset>
<asset id="a2" hasVideo="0" hasAudio="1"><media-rep src="file:///tmp/y.wav"/></asset>
<media id="e1" name="Inner"><sequence format="r1"><spine>
<asset-clip ref="a2" name="Deep" offset="0s" duration="50/25s" start="0s"/>
</spine></sequence></media>
<media id="c1" name="Compound"><sequence format="r1"><spine>
<asset-clip ref="a1" name="X" offset="0s" duration="100/25s" start="0s"/>
<asset-clip ref="a2" name="Y" offset="100/25s" duration="50/25s" start="0s"/>
<ref-clip ref="e1" name="D" offset="100/25s" duration="50/25s" start="0s"/>
</spine></sequence></media>
</resources>
<library><event><project name="Proj">
<sequence format="r1" name="Cut">
<spine>
<ref-clip ref="c1" name="C" offset="10/25s" duration="120/25s" start="20/25s"/>
</spine>
</sequence>
</project></event></library>
</fcpxml>"#,
        )
        .unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        // X: compound [0.8, 4.0] -> [0.4, 3.6]; Y: [4.0, 5.6] -> [3.6, 5.2].
        let x = draft
            .edits
            .iter()
            .find(|e| e.url.to_string_lossy() == "/tmp/x.mov")
            .expect("x");
        assert!((x.source_in - 0.8).abs() < 1e-9);
        assert!((x.source_out - 4.0).abs() < 1e-9);
        assert!((x.timeline_start - 0.4).abs() < 1e-9);
        assert!((x.timeline_end - 3.6).abs() < 1e-9);
        let y: Vec<_> = draft
            .edits
            .iter()
            .filter(|e| e.url.to_string_lossy() == "/tmp/y.wav")
            .collect();
        assert_eq!(y.len(), 2);
        let y_direct = y
            .iter()
            .find(|e| (e.timeline_start - 3.6).abs() < 1e-9)
            .expect("y");
        assert!((y_direct.source_in - 0.0).abs() < 1e-9);
        assert!((y_direct.source_out - 1.6).abs() < 1e-9);
        // Depth-2: E [0, 2] of C -> C [4, 6] -> parent [3.6, 5.6],
        // capped by the C window [0.4, 5.2].
        let deep = y
            .iter()
            .find(|e| (e.timeline_start - 3.6).abs() < 0.01)
            .expect("deep");
        assert!((deep.timeline_end - 5.2).abs() < 0.01);
        assert!((deep.source_in - 0.0).abs() < 1e-9);
        assert!((deep.source_out - 1.6).abs() < 0.01);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fcpxml_multicam_segments_and_audition() {
        let dir = std::env::temp_dir().join(format!("align-xml-mc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("mc.fcpxml");
        std::fs::write(
            &xml,
            r#"<?xml version="1.0"?><fcpxml version="1.10">
<resources>
<format id="r1" frameDuration="1/25s"/>
<asset id="a1" hasVideo="1" hasAudio="1"><media-rep src="file:///tmp/va.mov"/></asset>
<asset id="a2" hasVideo="0" hasAudio="1"><media-rep src="file:///tmp/au.wav"/></asset>
<media id="m1" name="MC"><multicam format="r1">
<mc-angle name="A" angleID="ang1">
<asset-clip ref="a1" offset="0s" duration="200/25s" start="0s"/>
</mc-angle>
<mc-angle name="B" angleID="ang2">
<asset-clip ref="a2" offset="0s" duration="200/25s" start="0s"/>
</mc-angle>
</multicam></media>
</resources>
<library><event><project name="Proj">
<sequence format="r1" name="Cut">
<spine>
<mc-clip ref="m1" name="M1" offset="0s" duration="100/25s" start="0s">
<mc-source angleID="ang1" srcEnable="all"/>
</mc-clip>
<mc-clip ref="m1" name="M2" offset="100/25s" duration="100/25s" start="100/25s">
<mc-source angleID="ang1" srcEnable="video" offset="100/25s" duration="50/25s"/>
<mc-source angleID="ang2" srcEnable="audio" offset="150/25s" duration="50/25s"/>
</mc-clip>
<audition>
<asset-clip ref="a1" name="Pick" offset="200/25s" duration="25/25s" start="0s"/>
<asset-clip ref="a2" name="Alt" offset="200/25s" duration="25/25s" start="0s"/>
</audition>
</spine>
</sequence>
</project></event></library>
</fcpxml>"#,
        )
        .unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        // M1: whole-span angle A (video+audio of va.mov).
        let m1: Vec<_> = draft
            .edits
            .iter()
            .filter(|e| e.url.to_string_lossy() == "/tmp/va.mov" && e.timeline_start < 4.0)
            .collect();
        assert_eq!(m1.len(), 2);
        // M2: video from angle A [4, 6], audio from angle B [6, 8].
        let m2v = draft
            .edits
            .iter()
            .find(|e| {
                e.url.to_string_lossy() == "/tmp/va.mov"
                    && e.media_type == DraftMediaKind::Video
                    && (e.timeline_start - 4.0).abs() < 1e-9
            })
            .expect("m2 video");
        assert!((m2v.timeline_end - 6.0).abs() < 1e-9);
        assert!((m2v.source_in - 4.0).abs() < 1e-9);
        let m2a = draft
            .edits
            .iter()
            .find(|e| {
                e.url.to_string_lossy() == "/tmp/au.wav" && (e.timeline_start - 6.0).abs() < 1e-9
            })
            .expect("m2 audio");
        assert!((m2a.timeline_end - 8.0).abs() < 1e-9);
        assert!((m2a.source_in - 6.0).abs() < 1e-9);
        assert!((m2a.source_out - 8.0).abs() < 1e-9);
        // Audition: active pick only, with a warning.
        let picks: Vec<_> = draft
            .edits
            .iter()
            .filter(|e| (e.timeline_start - 8.0).abs() < 1e-9)
            .collect();
        assert_eq!(picks.len(), 2);
        assert!(
            picks
                .iter()
                .all(|e| e.url.to_string_lossy() == "/tmp/va.mov")
        );
        assert!(
            draft
                .warnings
                .iter()
                .any(|w| w.message.contains("Audition alternatives")),
            "audition warns"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clip_edit_to_window_maps_forward_and_backward() {
        let base = || DraftEdit {
            id: "e".to_string(),
            name: None,
            url: PathBuf::from("/tmp/m.mov"),
            media_type: DraftMediaKind::Video,
            source_in: 10.0,
            source_out: 20.0,
            timeline_start: 0.0,
            timeline_end: 10.0,
            playback_rate: 1.0,
            plays_backward: false,
            fcp7_time_remap_xml: None,
            fcp7_filter_xmls: Vec::new(),
            fcp7_retime_in: None,
            fcp7_retime_out: None,
            fcp7_retime_duration: None,
            fcp7_labels_xml: None,
            time_scale: 1_000_000,
            include_embedded_audio: true,
            audio_source_channel: None,
            fcpxml_audio_role: None,
            track_index: 1,
            enabled: true,
            track_enabled: true,
            track_locked: false,
            linked_edit_ids: HashSet::new(),
        };
        let mut fwd = base();
        assert!(clip_edit_to_window(&mut fwd, 2.0, 5.0));
        assert!((fwd.source_in - 12.0).abs() < 1e-9);
        assert!((fwd.source_out - 15.0).abs() < 1e-9);
        assert!((fwd.timeline_start - 2.0).abs() < 1e-9);
        assert!((fwd.timeline_end - 5.0).abs() < 1e-9);
        let mut outside = base();
        assert!(!clip_edit_to_window(&mut outside, 20.0, 30.0));
        let mut bwd = base();
        bwd.plays_backward = true;
        assert!(clip_edit_to_window(&mut bwd, 2.0, 5.0));
        // Reversed: timeline 2..5 maps to source 18..15.
        assert!((bwd.source_in - 15.0).abs() < 1e-9);
        assert!((bwd.source_out - 18.0).abs() < 1e-9);
        assert!((bwd.timeline_start - 2.0).abs() < 1e-9);
        assert!((bwd.timeline_end - 5.0).abs() < 1e-9);
    }

    #[test]
    fn fcpxml_cyclic_compound_is_skipped_without_hanging() {
        let dir = std::env::temp_dir().join(format!("align-xml-cyc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("cyc.fcpxml");
        std::fs::write(
            &xml,
            r#"<?xml version="1.0"?><fcpxml version="1.10">
<resources>
<format id="r1" frameDuration="1/25s"/>
<asset id="a1" hasVideo="1" hasAudio="0"><media-rep src="file:///tmp/x.mov"/></asset>
<media id="c1" name="A"><sequence format="r1"><spine>
<ref-clip ref="c2" name="ToB" offset="0s" duration="10/25s" start="0s"/>
</spine></sequence></media>
<media id="c2" name="B"><sequence format="r1"><spine>
<asset-clip ref="a1" name="X" offset="0s" duration="10/25s" start="0s"/>
<ref-clip ref="c1" name="ToA" offset="10/25s" duration="10/25s" start="0s"/>
</spine></sequence></media>
</resources>
<library><event><project name="Proj">
<sequence format="r1" name="Cut">
<spine>
<ref-clip ref="c1" name="C" offset="0s" duration="20/25s" start="0s"/>
</spine>
</sequence>
</project></event></library>
</fcpxml>"#,
        )
        .unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        // B's asset flattens once; the A<->B cycle is cut with a warning.
        assert_eq!(draft.edits.len(), 1);
        assert_eq!(draft.edits[0].url, PathBuf::from("/tmp/x.mov"));
        assert!(
            draft
                .warnings
                .iter()
                .any(|w| w.message.contains("Cyclic compound")),
            "cycle warns"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fcp7_deep_nesting_hits_the_depth_cap() {
        // Programmatic 12-deep inline nesting; the cap stops expansion.
        let mut inner = String::from(
            r#"<clipitem id="leaf"><name>Leaf</name><in>0</in><out>100</out><start>0</start><end>100</end><file id="f"><pathurl>file:///tmp/leaf.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>100</duration></file></clipitem>"#,
        );
        for level in 0..12 {
            inner = format!(
                r#"<clipitem id="n{level}"><name>N{level}</name><in>0</in><out>100</out><start>0</start><end>100</end><sequence><name>S{level}</name><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><media><video><track><enabled>TRUE</enabled><locked>FALSE</locked>{inner}</track></video></media></sequence></clipitem>"#
            );
        }
        let doc = format!(
            r#"<?xml version="1.0"?><xmeml version="4"><sequence><name>Deep</name><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><media><video><track><enabled>TRUE</enabled><locked>FALSE</locked>{inner}<clipitem id="flat"><name>Flat</name><in>0</in><out>100</out><start>0</start><end>100</end><file id="ff"><pathurl>file:///tmp/flat.mov</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>100</duration></file></clipitem></track></video></media></sequence></xmeml>"#
        );
        let dir = std::env::temp_dir().join(format!("align-xml-deep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("deep.xml");
        std::fs::write(&xml, doc).unwrap();
        let draft = read_timeline(&xml, None).expect("read");
        // The direct clip survives; the over-deep branch is cut with a warning.
        assert_eq!(draft.edits.len(), 1);
        assert_eq!(draft.edits[0].url, PathBuf::from("/tmp/flat.mov"));
        assert!(
            draft
                .warnings
                .iter()
                .any(|w| w.message.contains("too deep")),
            "depth cap warns"
        );
        // Terminates (this assertion is the test): no stack overflow.
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Minimal ExportTimeline from a draft for writer-level assertions.
    fn draft_to_otio_timeline(draft: &TimelineDraft) -> crate::export_model::ExportTimeline {
        use crate::export_model::{ExportIsland, ExportItem};
        use crate::model::{AudioSummary, Clip, ClipId, MediaKind, VideoSummary};
        let mut clips = Vec::new();
        for edit in &draft.edits {
            let kind = match edit.media_type {
                DraftMediaKind::Video => MediaKind::Video,
                DraftMediaKind::Audio => MediaKind::Audio,
            };
            let clip = Clip {
                id: ClipId::new(format!("clip-{}", edit.id)),
                url: edit.url.clone(),
                kind,
                duration: MediaTime::seconds(edit.source_out.max(8.0)),
                audio: vec![AudioSummary {
                    sample_rate: 48000.0,
                    channels: 1,
                    bit_depth: None,
                    is_float: None,
                    source_timecode: None,
                }],
                video: (kind == MediaKind::Video).then(|| VideoSummary {
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
            let mut item = ExportItem::new(clip, edit.timeline_start, 1.0, vec![], 1.0);
            item.instance_id = edit.id.clone();
            item.source_in = edit.source_in;
            item.source_out = edit.source_out;
            item.timeline_duration = edit.timeline_end - edit.timeline_start;
            item.playback_rate = edit.playback_rate;
            item.plays_backward = edit.plays_backward;
            item.fcp7_time_remap_xml = edit.fcp7_time_remap_xml.clone();
            item.transition_after = draft
                .transitions
                .iter()
                .find(|t| t.left_edit_id == edit.id)
                .map(|t| crate::export_model::ExportTransition {
                    kind: crate::export_model::ExportTransitionKind::from_model(t.kind),
                    right_instance_id: t.right_edit_id.clone(),
                    start: t.start,
                    end: t.end,
                    alignment: t.alignment.clone(),
                    fcp7_effect_xml: t.effect_xml.clone(),
                    fcp7_transition_xml: Some(t.transition_xml.clone()),
                    is_otio_portable: t.is_otio_portable,
                });
            clips.push(item);
        }
        crate::export_model::ExportTimeline::new(
            vec![ExportIsland {
                id: 0,
                clips,
                duration: 12.0,
            }],
            draft.frame_duration,
            &draft.name,
        )
    }
}
