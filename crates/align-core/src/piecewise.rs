//! Monotonic piecewise time maps. Port of
//! Sources/AlignCore/PiecewiseTimeMapping.swift (incl.
//! `SolvedMappingPoint` alias).
//!
//! Used by `MatchGraph.propagateMappings` and (later) `SyncEngine` island
//! assembly. Pure `f64` math, identical on all OSes.

/// One knot: `island = f(source)`, monotone non-decreasing by construction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapPoint {
    pub source: f64,
    pub island: f64,
}

impl MapPoint {
    pub fn new(source: f64, island: f64) -> Self {
        Self { source, island }
    }
}

pub type SolvedMappingPoint = MapPoint;

const DEFAULT_TOLERANCE: f64 = 0.0005;
const DEDUP_EPS: f64 = 1e-6;

#[derive(Clone, Debug, PartialEq)]
pub struct PiecewiseTimeMapping {
    pub points: Vec<MapPoint>,
}

impl PiecewiseTimeMapping {
    pub fn new(points: Vec<MapPoint>) -> Self {
        Self::with_tolerance(points, DEFAULT_TOLERANCE)
    }

    pub fn with_tolerance(points: Vec<MapPoint>, tolerance: f64) -> Self {
        let mut sorted = points;
        sorted.sort_by(|a, b| {
            a.source
                .total_cmp(&b.source)
                .then_with(|| a.island.total_cmp(&b.island))
        });
        let mut unique: Vec<MapPoint> = Vec::with_capacity(sorted.len());
        for p in sorted {
            if !p.source.is_finite() || !p.island.is_finite() {
                continue;
            }
            if let Some(last) = unique.last_mut() {
                if (last.source - p.source).abs() < DEDUP_EPS {
                    // Same source knot: last (largest island after sort) wins.
                    *last = p;
                    continue;
                }
                if p.island <= last.island {
                    // Breaks monotonicity: drop, never guess.
                    continue;
                }
            }
            unique.push(p);
        }
        Self {
            points: Self::simplify(&unique, tolerance),
        }
    }

    pub fn contains(&self, source: f64) -> bool {
        match (self.points.first(), self.points.last()) {
            (Some(f), Some(l)) => (f.source..=l.source).contains(&source),
            _ => false,
        }
    }

    /// Linear interpolation with clamping outside the knot range. With
    /// fewer than 2 knots returns the single island (or identity).
    pub fn value_at(&self, source: f64) -> f64 {
        if self.points.len() < 2 {
            return self.points.first().map(|p| p.island).unwrap_or(source);
        }
        let n = self.points.len();
        let upper = if source <= self.points[0].source {
            1
        } else if source >= self.points[n - 1].source {
            n - 1
        } else {
            self.points
                .iter()
                .position(|p| p.source >= source)
                .unwrap_or(n - 1)
        };
        let (lo, hi) = (self.points[upper - 1], self.points[upper]);
        let span = hi.source - lo.source;
        if span <= 0.0 {
            return lo.island;
        }
        lo.island + (source - lo.source) / span * (hi.island - lo.island)
    }

    pub fn inverted(&self) -> Self {
        Self::new(
            self.points
                .iter()
                .map(|p| MapPoint::new(p.island, p.source))
                .collect(),
        )
    }

    /// Ramer–Douglas–Peucker on the island axis, like Swift `simplify`.
    fn simplify(points: &[MapPoint], tolerance: f64) -> Vec<MapPoint> {
        if points.len() <= 2 {
            return points.to_vec();
        }
        let (first, last) = (points[0], points[points.len() - 1]);
        let span = last.source - first.source;
        if span <= 0.0 {
            return vec![first, last];
        }
        let mut maximum = 0.0;
        let mut split = 0;
        for (i, p) in points.iter().enumerate().take(points.len() - 1).skip(1) {
            let expected =
                first.island + (p.source - first.source) / span * (last.island - first.island);
            let error = (p.island - expected).abs();
            if error > maximum {
                maximum = error;
                split = i;
            }
        }
        if maximum <= tolerance {
            return vec![first, last];
        }
        let mut out = Self::simplify(&points[..=split], tolerance);
        out.pop();
        out.extend_from_slice(&Self::simplify(&points[split..], tolerance));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn interpolates_and_extrapolates_like_swift() {
        // Swift `value(at:)` clamps the *segment* but extrapolates the edge
        // segments linearly; callers guard range with `contains()`.
        let m =
            PiecewiseTimeMapping::new(vec![MapPoint::new(0.0, 10.0), MapPoint::new(10.0, 20.0)]);
        assert!(approx(m.value_at(5.0), 15.0));
        assert!(approx(m.value_at(-5.0), 5.0));
        assert!(approx(m.value_at(99.0), 109.0));
        assert!(m.contains(5.0) && !m.contains(-5.0) && !m.contains(99.0));
    }

    #[test]
    fn simplify_drops_collinear_keeps_kink() {
        let line = PiecewiseTimeMapping::new(vec![
            MapPoint::new(0.0, 0.0),
            MapPoint::new(5.0, 5.0),
            MapPoint::new(10.0, 10.0),
        ]);
        assert_eq!(line.points.len(), 2);
        let kink = PiecewiseTimeMapping::new(vec![
            MapPoint::new(0.0, 0.0),
            MapPoint::new(5.0, 6.0),
            MapPoint::new(10.0, 10.0),
        ]);
        assert_eq!(kink.points.len(), 3);
    }

    #[test]
    fn duplicate_source_last_wins_and_junk_filtered() {
        let m = PiecewiseTimeMapping::new(vec![
            MapPoint::new(0.0, 1.0),
            MapPoint::new(0.0, 2.0),
            MapPoint::new(f64::NAN, 3.0),
            MapPoint::new(1.0, f64::INFINITY),
        ]);
        assert_eq!(m.points.len(), 1);
        assert!(approx(m.value_at(0.0), 2.0));
    }

    #[test]
    fn non_monotone_points_dropped() {
        let m = PiecewiseTimeMapping::new(vec![
            MapPoint::new(0.0, 0.0),
            MapPoint::new(1.0, -5.0),
            MapPoint::new(2.0, 10.0),
        ]);
        assert_eq!(m.points.len(), 2);
        assert!(approx(m.value_at(1.0), 5.0));
    }

    #[test]
    fn inverted_roundtrip() {
        let m =
            PiecewiseTimeMapping::new(vec![MapPoint::new(0.0, 10.0), MapPoint::new(10.0, 30.0)]);
        let inv = m.inverted();
        assert!(approx(inv.value_at(20.0), 5.0));
        assert!(m.contains(5.0) && !m.contains(11.0));
    }
}
