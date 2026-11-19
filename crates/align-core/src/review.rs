//! Serializable editor review state layered over immutable sync placements.

use crate::model::{Clip, ClipId, MediaKind, MediaSpan, MediaTime};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Per-clip source details captured when an editor creates a manual placement.
/// Matching these details prevents an override from silently following a clip
/// ID that has since been reused for different media.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewClipIdentity {
    pub url: PathBuf,
    pub duration: MediaTime,
    pub kind: MediaKind,
    pub source_identifier: Option<String>,
    pub media_span: Option<MediaSpan>,
}

impl From<&Clip> for ReviewClipIdentity {
    fn from(clip: &Clip) -> Self {
        Self {
            url: clip.url.clone(),
            duration: clip.duration,
            kind: clip.kind,
            source_identifier: clip.source_identifier.clone(),
            media_span: clip.media_span.clone(),
        }
    }
}

/// An editor-authored timeline start and the source identity it belongs to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualPlacementOverride {
    pub timeline_start: f64,
    pub source_identity: ReviewClipIdentity,
}

impl ManualPlacementOverride {
    pub fn for_clip(clip: &Clip, timeline_start: f64) -> Self {
        Self {
            timeline_start,
            source_identity: ReviewClipIdentity::from(clip),
        }
    }
}

/// Persisted navigation and manual placement state for a review session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSession {
    #[serde(default)]
    pub overrides: HashMap<ClipId, ManualPlacementOverride>,
    #[serde(default)]
    pub selected_clip: Option<ClipId>,
    #[serde(default)]
    pub playhead: f64,
    #[serde(default = "default_zoom")]
    pub zoom: f64,
    #[serde(default)]
    pub horizontal_scroll: f64,
}

fn default_zoom() -> f64 {
    1.0
}

impl Default for ReviewSession {
    fn default() -> Self {
        Self {
            overrides: HashMap::new(),
            selected_clip: None,
            playhead: 0.0,
            zoom: default_zoom(),
            horizontal_scroll: 0.0,
        }
    }
}

impl ReviewSession {
    /// Report missing or changed source references and invalid session values.
    /// The input is the current result's project clips; automatic placements
    /// are never modified by validation.
    pub fn validate(&self, clips: &[Clip]) -> Vec<ReviewValidationIssue> {
        let clips_by_id: HashMap<_, _> = clips.iter().map(|clip| (&clip.id, clip)).collect();
        let mut issues = Vec::new();

        for (clip_id, placement) in &self.overrides {
            let Some(clip) = clips_by_id.get(clip_id) else {
                issues.push(ReviewValidationIssue {
                    clip_id: Some(clip_id.clone()),
                    reason: ReviewValidationReason::MissingClip,
                });
                if !placement.timeline_start.is_finite() {
                    issues.push(ReviewValidationIssue {
                        clip_id: Some(clip_id.clone()),
                        reason: ReviewValidationReason::InvalidPosition,
                    });
                }
                continue;
            };

            if placement.source_identity != ReviewClipIdentity::from(*clip) {
                issues.push(ReviewValidationIssue {
                    clip_id: Some(clip_id.clone()),
                    reason: ReviewValidationReason::StaleClipIdentity,
                });
            }
            if !placement.timeline_start.is_finite() {
                issues.push(ReviewValidationIssue {
                    clip_id: Some(clip_id.clone()),
                    reason: ReviewValidationReason::InvalidPosition,
                });
            }
        }

        if let Some(selected) = &self.selected_clip
            && !clips_by_id.contains_key(selected)
        {
            issues.push(ReviewValidationIssue {
                clip_id: Some(selected.clone()),
                reason: ReviewValidationReason::MissingClip,
            });
        }

        for value in [self.playhead, self.zoom, self.horizontal_scroll] {
            if !value.is_finite() {
                issues.push(ReviewValidationIssue {
                    clip_id: None,
                    reason: ReviewValidationReason::InvalidPosition,
                });
            }
        }
        if self.zoom <= 0.0 {
            issues.push(ReviewValidationIssue {
                clip_id: None,
                reason: ReviewValidationReason::InvalidPosition,
            });
        }

        issues
    }

    /// Resolve an effective start without changing the automatic base value.
    /// Invalid or stale overrides return an explicit reason so callers can
    /// surface the edit for review rather than silently applying it.
    pub fn effective_start(
        &self,
        clip: &Clip,
        automatic_start: f64,
    ) -> Result<f64, ReviewValidationReason> {
        let Some(placement) = self.overrides.get(&clip.id) else {
            return if automatic_start.is_finite() {
                Ok(automatic_start)
            } else {
                Err(ReviewValidationReason::InvalidPosition)
            };
        };

        if !placement.timeline_start.is_finite() {
            return Err(ReviewValidationReason::InvalidPosition);
        }
        if placement.source_identity != ReviewClipIdentity::from(clip) {
            return Err(ReviewValidationReason::StaleClipIdentity);
        }
        Ok(placement.timeline_start)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewValidationIssue {
    pub clip_id: Option<ClipId>,
    pub reason: ReviewValidationReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewValidationReason {
    MissingClip,
    StaleClipIdentity,
    InvalidPosition,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_defaults_omitted_state_and_ignores_unknown_fields() {
        let restored: ReviewSession =
            serde_json::from_str(r#"{"overrides":{},"futureField":{"ignored":true}}"#)
                .expect("deserialize forward-compatible review session");

        assert_eq!(restored, ReviewSession::default());
        let empty: ReviewSession = serde_json::from_str("{}").expect("deserialize defaults");
        assert_eq!(empty, ReviewSession::default());
    }

    #[test]
    fn legacy_sync_result_json_still_deserializes_without_review_state() {
        let legacy = serde_json::json!({
            "project": {
                "clips": [{
                    "id": "clip-a",
                    "url": "/media/a.wav",
                    "kind": "audio",
                    "duration": {"value": 120, "timescale": 1},
                    "audio": []
                }],
                "warnings": []
            },
            "islands": [],
            "unmatched": [],
            "matches": []
        });

        let result: crate::model::SyncResult =
            serde_json::from_value(legacy).expect("deserialize older sync result JSON");
        assert_eq!(result.project.clips[0].id, ClipId::new("clip-a"));
        assert!(result.islands.is_empty());
    }
}
