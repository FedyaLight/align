//! Structural matching of linked recording parts.
//!
//! Consecutive parts of one BWF/RF64/BW64 `link` set join seamlessly —
//! even with zero waveform overlap — staying separate source clips on one
//! NLE track. Edges are metadata-only (confidence 1, rate 1) and bypass
//! the waveform admission gates in the graph solve.
//! Incomplete sets warn instead of guessing across the gap.

use std::collections::{HashMap, HashSet};

use crate::matcher::{PairAlignmentPoint, PairwiseMatch};
use crate::model::{Clip, MatchEvidence, MediaKind, SyncWarning};

pub struct SpannedAnalysis {
    pub matches: Vec<PairwiseMatch>,
    pub warnings: Vec<SyncWarning>,
}

pub fn analyze_spans(clips: &[Clip]) -> SpannedAnalysis {
    let mut groups: HashMap<&str, Vec<&Clip>> = HashMap::new();
    for clip in clips {
        if let Some(span) = &clip.media_span {
            groups
                .entry(span.identifier.as_str())
                .or_default()
                .push(clip);
        }
    }
    let mut identifiers: Vec<&str> = groups.keys().copied().collect();
    identifiers.sort();

    let mut matches = Vec::new();
    let mut warnings = Vec::new();
    for identifier in identifiers {
        let group = &groups[identifier];
        let declared: HashSet<usize> = group
            .iter()
            .map(|c| c.media_span.as_ref().unwrap().part_count)
            .collect();
        let mut parts: HashMap<usize, Vec<&&Clip>> = HashMap::new();
        for clip in group {
            parts
                .entry(clip.media_span.as_ref().unwrap().part_number)
                .or_default()
                .push(clip);
        }
        let expected = if declared.len() == 1 {
            declared.iter().next().copied().unwrap_or(0)
        } else {
            0
        };
        let missing: Vec<usize> = if expected > 0 {
            (1..=expected).filter(|n| !parts.contains_key(n)).collect()
        } else {
            Vec::new()
        };
        let mut duplicates: Vec<usize> = parts
            .iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(k, _)| *k)
            .collect();
        duplicates.sort_unstable();
        let mut numbers: Vec<usize> = parts.keys().copied().collect();
        numbers.sort_unstable();
        let incompatible: Vec<String> = numbers
            .iter()
            .filter_map(|&number| {
                let (a, b) = (parts.get(&number)?, parts.get(&(number + 1))?);
                if a.len() == 1 && b.len() == 1 && compatible(a[0], b[0]) {
                    return None;
                }
                Some(format!("{number}-{}", number + 1))
            })
            .collect();

        if declared.len() != 1
            || !missing.is_empty()
            || !duplicates.is_empty()
            || !incompatible.is_empty()
        {
            let mut details = Vec::new();
            if declared.len() != 1 {
                details.push("conflicting part counts".to_string());
            }
            if !missing.is_empty() {
                details.push(format!(
                    "missing {}",
                    missing
                        .iter()
                        .map(|n| n.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !duplicates.is_empty() {
                details.push(format!(
                    "duplicate {}",
                    duplicates
                        .iter()
                        .map(|n| n.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !incompatible.is_empty() {
                details.push(format!("incompatible parts {}", incompatible.join(", ")));
            }
            if let Some(url) = group.first().map(|c| c.url.clone()) {
                warnings.push(SyncWarning {
                    url,
                    message: format!("Incomplete linked BWF file-set: {}.", details.join("; ")),
                });
            }
        }

        for &number in &numbers {
            let (a, b) = match (parts.get(&number), parts.get(&(number + 1))) {
                (Some(a), Some(b)) if a.len() == 1 && b.len() == 1 => (a[0], b[0]),
                _ => continue,
            };
            if !compatible(a, b) {
                continue;
            }
            let duration = a.duration.as_seconds();
            matches.push(PairwiseMatch {
                left: a.id.clone(),
                right: b.id.clone(),
                rate: 1.0,
                // Right part starts where the left part ends.
                offset: -duration,
                confidence: 1.0,
                anchors: 100,
                left_start_seconds: 0.0,
                left_end_seconds: duration,
                covered_seconds: duration,
                residual_seconds: 0.0,
                alignment_points: Vec::<PairAlignmentPoint>::new(),
                evidence: MatchEvidence::SpannedMetadata,
            });
        }
    }
    SpannedAnalysis { matches, warnings }
}

/// Same-source seamless parts only: audio kind, identical audio layouts,
/// identical declared part count.
fn compatible(left: &Clip, right: &Clip) -> bool {
    left.kind == MediaKind::Audio
        && right.kind == MediaKind::Audio
        && left.audio == right.audio
        && left.media_span.as_ref().map(|s| s.part_count)
            == right.media_span.as_ref().map(|s| s.part_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ClipId;
    use crate::model::{AudioSummary, MediaSpan, MediaTime};
    use std::path::PathBuf;

    fn clip(id: &str, span: Option<(usize, usize)>, seconds: f64) -> Clip {
        Clip {
            id: ClipId::new(id),
            url: PathBuf::from(format!("/v/{id}.wav")),
            kind: MediaKind::Audio,
            duration: MediaTime::seconds(seconds),
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
            media_span: span.map(|(n, c)| MediaSpan {
                identifier: "bwf:abc".into(),
                part_number: n,
                part_count: c,
            }),
        }
    }

    #[test]
    fn consecutive_parts_join() {
        let clips = vec![clip("a", Some((1, 2)), 10.0), clip("b", Some((2, 2)), 10.0)];
        let analysis = analyze_spans(&clips);
        assert!(analysis.warnings.is_empty());
        assert_eq!(analysis.matches.len(), 1);
        let m = &analysis.matches[0];
        assert_eq!((m.left.0.as_str(), m.right.0.as_str()), ("a", "b"));
        assert_eq!(m.evidence, MatchEvidence::SpannedMetadata);
        assert!((m.offset + 10.0).abs() < 1e-9);
        assert_eq!(m.confidence, 1.0);
    }

    #[test]
    fn missing_part_warns_and_skips_gap() {
        let clips = vec![clip("a", Some((1, 3)), 10.0), clip("c", Some((3, 3)), 10.0)];
        let analysis = analyze_spans(&clips);
        assert_eq!(analysis.matches.len(), 0);
        assert_eq!(analysis.warnings.len(), 1);
        assert!(analysis.warnings[0].message.contains("missing 2"));
    }

    #[test]
    fn duplicate_part_warns() {
        let clips = vec![
            clip("a", Some((1, 2)), 10.0),
            clip("a2", Some((1, 2)), 10.0),
            clip("b", Some((2, 2)), 10.0),
        ];
        let analysis = analyze_spans(&clips);
        assert!(
            analysis
                .warnings
                .iter()
                .any(|w| w.message.contains("duplicate 1"))
        );
        // No edge touches the duplicated part.
        assert!(analysis.matches.is_empty());
    }

    #[test]
    fn incompatible_layout_blocks_edge() {
        let mut clips = vec![clip("a", Some((1, 2)), 10.0), clip("b", Some((2, 2)), 10.0)];
        clips[1].audio[0] = AudioSummary {
            sample_rate: 44100.0,
            channels: 1,
            bit_depth: Some(24),
            is_float: Some(false),
            source_timecode: None,
        };
        let analysis = analyze_spans(&clips);
        assert!(analysis.matches.is_empty());
        assert!(
            analysis
                .warnings
                .iter()
                .any(|w| w.message.contains("incompatible parts 1-2"))
        );
    }

    #[test]
    fn video_parts_never_join() {
        let mut clips = vec![clip("a", Some((1, 2)), 10.0), clip("b", Some((2, 2)), 10.0)];
        clips[0].kind = MediaKind::Video;
        let analysis = analyze_spans(&clips);
        assert!(analysis.matches.is_empty());
    }
}
