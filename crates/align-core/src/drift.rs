//! Clock-drift policy and segment mapping.
//!
//! A correction is rendered when a validated mapping accumulates at least
//! 16 ms of slip. Rendering lives in `align-decode`; it resamples each segment
//! to its mapped duration and preserves discrete source channels.

use crate::model::MappingPoint;

/// Minimum correctable slip: 1 ms beyond the ±15 ms ATSC presentation
/// tolerance so numerical noise at the boundary cannot trigger resampling.
pub const MINIMUM_CORRECTABLE_SLIP: f64 = 0.016;
const COMPARISON_EPSILON: f64 = 1e-9;

fn is_correctable(slip: f64) -> bool {
    slip.abs() + COMPARISON_EPSILON >= MINIMUM_CORRECTABLE_SLIP
}

pub fn needs_correction_rate(duration: f64, rate: f64) -> bool {
    is_correctable(duration * (rate - 1.0))
}

pub fn needs_correction_points(points: &[MappingPoint]) -> bool {
    let Some(first) = points.first() else {
        return false;
    };
    points.iter().any(|p| {
        let source = p.source.as_seconds() - first.source.as_seconds();
        let island = p.island.as_seconds() - first.island.as_seconds();
        is_correctable(island - source)
    })
}

/// Validated (source_start, source_duration, target_duration) segments.
pub fn segments(points: &[MappingPoint]) -> Option<Vec<(f64, f64, f64)>> {
    if points.len() < 2 {
        return None;
    }
    let mut sorted: Vec<(f64, f64)> = points
        .iter()
        .map(|p| (p.source.as_seconds(), p.island.as_seconds()))
        .collect();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = Vec::with_capacity(sorted.len() - 1);
    for pair in sorted.windows(2) {
        let (source_duration, target_duration) = (pair[1].0 - pair[0].0, pair[1].1 - pair[0].1);
        if source_duration <= 0.0 || target_duration <= 0.0 {
            return None;
        }
        out.push((pair[0].0, source_duration, target_duration));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaTime;

    fn pt(s: f64, i: f64) -> MappingPoint {
        MappingPoint {
            source: MediaTime::seconds(s),
            island: MediaTime::seconds(i),
        }
    }

    #[test]
    fn slip_threshold() {
        assert!(!needs_correction_rate(600.0, 1.0 + 0.010 / 600.0));
        assert!(needs_correction_rate(600.0, 1.0 + 0.016 / 600.0));
        assert!(!needs_correction_rate(600.0, 1.0 + 0.015_999 / 600.0));
        assert!(!needs_correction_points(&[pt(0.0, 0.0), pt(10.0, 10.010)]));
        assert!(needs_correction_points(&[pt(0.0, 0.0), pt(10.0, 10.016)]));
        assert!(!needs_correction_points(&[]));
    }

    #[test]
    fn segment_validation() {
        let segs = segments(&[pt(0.0, 0.0), pt(10.0, 10.01)]).expect("segments");
        assert_eq!(segs.len(), 1);
        assert!((segs[0].0 - 0.0).abs() < 1e-12);
        assert!((segs[0].1 - 10.0).abs() < 1e-12);
        assert!((segs[0].2 - 10.01).abs() < 1e-12);
        assert!(segments(&[pt(0.0, 0.0)]).is_none());
        assert!(segments(&[pt(5.0, 0.0), pt(5.0, 1.0)]).is_none());
        assert!(segments(&[pt(0.0, 1.0), pt(1.0, 1.0)]).is_none());
    }
}
