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

/// Source identity: hardware identifier, then recording span, then parent directory.
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

    // Phase 2: merge source lanes by their occupied intervals. A lane's
    // first/last clip bounds include gaps that other sources can safely fill.
    struct SourceLane<'a> {
        source: &'a str,
        local_lane: usize,
        intervals: Vec<(f64, f64)>,
    }
    let mut source_lanes = Vec::new();
    for source in &sources {
        let mut intervals_by_lane: Vec<Vec<(f64, f64)>> = Vec::new();
        for clip in &by_source[source] {
            let lane = local[clip.id.as_str()];
            while intervals_by_lane.len() <= lane {
                intervals_by_lane.push(Vec::new());
            }
            intervals_by_lane[lane].push((clip.start, clip.start + clip.duration.max(0.0)));
        }
        for (local_lane, mut intervals) in intervals_by_lane.into_iter().enumerate() {
            intervals.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
            source_lanes.push(SourceLane {
                source,
                local_lane,
                intervals,
            });
        }
    }
    source_lanes.sort_by(|a, b| {
        a.intervals[0]
            .0
            .total_cmp(&b.intervals[0].0)
            .then_with(|| a.source.cmp(b.source))
            .then_with(|| a.local_lane.cmp(&b.local_lane))
    });
    let mut global_intervals: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut global_for_local: HashMap<&str, Vec<usize>> = HashMap::new();
    for source_lane in &source_lanes {
        let lane = global_intervals
            .iter()
            .position(|occupied| intervals_fit(occupied, &source_lane.intervals))
            .unwrap_or_else(|| {
                global_intervals.push(Vec::new());
                global_intervals.len() - 1
            });
        global_intervals[lane].extend_from_slice(&source_lane.intervals);
        global_intervals[lane]
            .sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
        let entry = global_for_local.entry(source_lane.source).or_default();
        while entry.len() <= source_lane.local_lane {
            entry.push(0);
        }
        entry[source_lane.local_lane] = lane;
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

// Both lists are sorted by start. Advance the interval that finishes before
// the other begins, allowing the same refinement seam as source-local packing.
fn intervals_fit(left: &[(f64, f64)], right: &[(f64, f64)]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        if left[i].1 <= right[j].0 + PLACEMENT_TOLERANCE {
            i += 1;
        } else if right[j].1 <= left[i].0 + PLACEMENT_TOLERANCE {
            j += 1;
        } else {
            return false;
        }
    }
    true
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
    fn another_source_can_fill_a_gap_between_clips() {
        let requests = [
            req("early", "/disk-a/camera", 0.0, 5.0),
            req("later", "/disk-a/camera", 100.0, 100.0),
            req("gap", "/disk-b/camera", 6.0, 94.0),
        ];
        let lanes = allocate(&requests);
        assert_eq!(lanes["early"], lanes["later"]);
        assert_eq!(lanes["gap"], lanes["later"]);
        let mut reversed = requests.to_vec();
        reversed.reverse();
        assert_eq!(lanes, allocate(&reversed));
    }

    #[test]
    fn gap_packing_checks_later_clips_in_both_sources() {
        let lanes = allocate(&[
            req("early", "a", 0.0, 5.0),
            req("later", "a", 100.0, 20.0),
            req("gap", "b", 6.0, 20.0),
            req("collision", "b", 110.0, 20.0),
        ]);
        assert_ne!(lanes["later"], lanes["collision"]);
        assert_eq!(lanes["gap"], lanes["collision"]);
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
