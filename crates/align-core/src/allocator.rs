//! Timeline lane packing.
//!
//! Two phases: (1) each source's clips pack into
//! source-local lanes (takes and stems keep stable order), (2) whole source
//! lanes merge into shared global time lanes. Non-overlapping files of one
//! camera/recorder source reuse a row; unrelated sources share a track when
//! they never materially overlap.

use std::collections::HashMap;
use std::path::Path;

// Adjacent recorder chunks can disagree at their seam after independent
// waveform refinement. Keep a sub-half-second seam on one track; material
// overlaps still split into additional lanes.
const PLACEMENT_TOLERANCE: f64 = 0.5;

#[derive(Clone, Debug, PartialEq)]
pub struct TimelineTrackRequest {
    pub id: String,
    pub source_key: String,
    pub start: f64,
    pub duration: f64,
}

impl TimelineTrackRequest {
    pub fn new(
        id: impl Into<String>,
        source_key: impl Into<String>,
        start: f64,
        duration: f64,
    ) -> Self {
        Self {
            id: id.into(),
            source_key: source_key.into(),
            start,
            duration,
        }
    }
}

/// Mirrors `sourceKey(for:)`: hardware identity → span id → parent directory.
pub fn source_key_for_clip(
    url: &Path,
    source_identifier: Option<&str>,
    media_span: Option<&str>,
) -> String {
    if let Some(ident) = source_identifier {
        return ident.to_string();
    }
    if let Some(span) = media_span {
        return format!("span:{span}");
    }
    source_key_for_url(url)
}

/// Parent directory of the media file.
pub fn source_key_for_url(url: &Path) -> String {
    url.parent()
        .map(|p| p.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn allocate(requests: &[TimelineTrackRequest]) -> HashMap<String, usize> {
    // Phase 1: source-local lanes.
    let mut by_source: HashMap<&str, Vec<&TimelineTrackRequest>> = HashMap::new();
    for r in requests {
        by_source.entry(r.source_key.as_str()).or_default().push(r);
    }
    let mut sources: Vec<&str> = by_source.keys().copied().collect();
    sources.sort();

    let mut local: HashMap<&str, usize> = HashMap::new();
    for source in &sources {
        let mut clips = by_source[source].clone();
        clips.sort_by(|a, b| {
            a.start
                .total_cmp(&b.start)
                .then_with(|| b.duration.total_cmp(&a.duration))
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut ends: Vec<f64> = Vec::new();
        for clip in clips {
            let lane = ends
                .iter()
                .position(|e| *e <= clip.start + PLACEMENT_TOLERANCE)
                .unwrap_or_else(|| {
                    ends.push(0.0);
                    ends.len() - 1
                });
            local.insert(clip.id.as_str(), lane);
            ends[lane] = ends[lane].max(clip.start + clip.duration.max(0.0));
        }
    }

    // Phase 2: merge source lanes into shared global time lanes.
    struct Span {
        source: String,
        local_lane: usize,
        start: f64,
        end: f64,
    }
    let mut spans: Vec<Span> = Vec::new();
    for source in &sources {
        let clips = &by_source[source];
        let mut ends: Vec<f64> = Vec::new();
        let mut ordered = clips.clone();
        ordered.sort_by(|a, b| a.start.total_cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
        for clip in ordered {
            let lane = local[clip.id.as_str()];
            while ends.len() <= lane {
                ends.push(0.0);
            }
            ends[lane] = ends[lane].max(clip.start + clip.duration.max(0.0));
        }
        for (lane, _) in ends.iter().enumerate() {
            let start = clips
                .iter()
                .filter(|c| local[c.id.as_str()] == lane)
                .map(|c| c.start)
                .fold(None::<f64>, |acc, s| Some(acc.map_or(s, |a: f64| a.min(s))));
            if let Some(start) = start {
                spans.push(Span {
                    source: (*source).to_string(),
                    local_lane: lane,
                    start,
                    end: ends[lane],
                });
            }
        }
    }
    spans.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.local_lane.cmp(&b.local_lane))
    });
    let mut global_ends: Vec<f64> = Vec::new();
    let mut global_for_local: HashMap<&str, Vec<usize>> = HashMap::new();
    for span in &spans {
        let lane = global_ends
            .iter()
            .position(|e| *e <= span.start + PLACEMENT_TOLERANCE)
            .unwrap_or_else(|| {
                global_ends.push(0.0);
                global_ends.len() - 1
            });
        global_ends[lane] = global_ends[lane].max(span.end);
        let entry = global_for_local.entry(span.source.as_str()).or_default();
        while entry.len() <= span.local_lane {
            entry.push(0);
        }
        entry[span.local_lane] = lane;
    }

    let mut out = HashMap::new();
    for r in requests {
        let lane = global_for_local
            .get(r.source_key.as_str())
            .and_then(|v| v.get(local[r.id.as_str()]))
            .copied()
            .unwrap_or(0);
        out.insert(r.id.clone(), lane);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: &str, source: &str, start: f64, duration: f64) -> TimelineTrackRequest {
        TimelineTrackRequest::new(id, source, start, duration)
    }

    #[test]
    fn non_overlapping_same_source_share_lane() {
        let a = allocate(&[req("a", "s", 0.0, 10.0), req("b", "s", 10.0, 10.0)]);
        assert_eq!(a["a"], a["b"]);
    }

    #[test]
    fn overlapping_same_source_split_lanes() {
        let a = allocate(&[req("a", "s", 0.0, 10.0), req("b", "s", 5.0, 10.0)]);
        assert_ne!(a["a"], a["b"]);
    }

    #[test]
    fn small_refinement_seam_keeps_sequential_recorder_clips_together() {
        let a = allocate(&[
            req("0029", "recorder", 5_400.527_024, 1_800.164_296),
            req("0030", "recorder", 7_200.231_711, 593.504_155),
        ]);
        assert_eq!(a["0029"], a["0030"]);
    }

    #[test]
    fn disjoint_sources_share_global_lane() {
        // Phase 2: sources that never overlap in time share one track.
        let a = allocate(&[req("a", "s1", 0.0, 5.0), req("b", "s2", 10.0, 5.0)]);
        assert_eq!(a["a"], 0);
        assert_eq!(a["b"], 0);
    }

    #[test]
    fn overlapping_sources_get_distinct_lanes() {
        let a = allocate(&[req("a", "s1", 0.0, 10.0), req("b", "s2", 0.0, 10.0)]);
        assert_ne!(a["a"], a["b"]);
    }

    #[test]
    fn source_key_prefers_identity_then_span_then_dir() {
        assert_eq!(
            source_key_for_clip(Path::new("/v/a.mp4"), Some("SER123"), None),
            "SER123"
        );
        assert_eq!(
            source_key_for_clip(Path::new("/v/a.wav"), None, Some("take7")),
            "span:take7"
        );
        assert_eq!(
            source_key_for_clip(Path::new("/v/cam/a.mp4"), None, None),
            "/v/cam"
        );
    }
}
