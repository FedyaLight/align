//! Long-drift fallback for an ambiguous offset histogram. Fit lines in
//! (source time, target time), counting independent time cells as support.

use std::collections::{HashMap, HashSet};

use super::{
    ANCHOR_BUCKET_FRAMES, Anchor, BUCKET_WIDTH_FRAMES, Fit, Record, SECONDS_PER_FRAME, policy,
    robust_fit,
};

const SECTIONS: usize = 8;
const MODES_PER_SECTION: usize = 4;
const MAX_POINTS: usize = 16_384;
const INLIER_SECONDS: f64 = 0.16;

pub(super) struct Candidate {
    buckets: HashSet<i64>,
    points: Option<HashMap<(u32, i64), Anchor>>,
}

impl Candidate {
    pub(super) fn new(histogram: &[(i64, usize)]) -> Option<Self> {
        let mut buckets: Vec<_> = histogram
            .iter()
            .filter(|(_, votes)| *votes >= policy::VOTE_THRESHOLD)
            .map(|(bucket, _)| *bucket)
            .collect();
        buckets.sort_unstable();
        // A tilted ridge must span more than the ordinary ±2-bucket window.
        // Isolated repeated-take peaks do not pay for the line search.
        if !buckets.windows(6).any(|w| w[5] - w[0] == 5) {
            return None;
        }
        Some(Self {
            buckets: buckets.into_iter().collect(),
            points: Some(HashMap::new()),
        })
    }

    pub(super) fn observe(&mut self, left: &[Record], right: &[Record]) {
        let Some(points) = self.points.as_mut() else {
            return;
        };
        for l in left {
            for r in right {
                let bucket = (r.frame as i64 - l.frame as i64).div_euclid(BUCKET_WIDTH_FRAMES);
                if !self.buckets.contains(&bucket) {
                    continue;
                }
                let key = (l.frame / ANCHOR_BUCKET_FRAMES as u32, bucket);
                if points.len() == MAX_POINTS && !points.contains_key(&key) {
                    // Bound optional search memory; overflow declines the rescue.
                    self.points = None;
                    return;
                }
                points.insert(
                    key,
                    Anchor {
                        left: l.frame as f64 * SECONDS_PER_FRAME,
                        right: r.frame as f64 * SECONDS_PER_FRAME,
                    },
                );
            }
        }
    }

    pub(super) fn fit(self) -> Option<(Fit, f64)> {
        let mut points: Vec<_> = self.points?.into_iter().collect();
        points.sort_by(|a, b| {
            a.1.left
                .total_cmp(&b.1.left)
                .then_with(|| a.1.right.total_cmp(&b.1.right))
        });
        let first = points.first()?.1.left;
        let span = points.last()?.1.left - first;
        if span < policy::DRIFT_MIN_SPAN_SECONDS {
            return None;
        }
        let section = |x: f64| (((x - first) / span * SECTIONS as f64) as usize).min(SECTIONS - 1);
        let mut cells: [HashMap<i64, (f64, f64, usize)>; SECTIONS] =
            std::array::from_fn(|_| HashMap::new());
        for &((_, bucket), p) in &points {
            let cell = cells[section(p.left)].entry(bucket).or_default();
            cell.0 += p.left;
            cell.1 += p.right;
            cell.2 += 1;
        }
        let modes: Vec<Vec<Anchor>> = cells
            .into_iter()
            .map(|cell| {
                let mut ranked: Vec<_> = cell.into_iter().collect();
                ranked.sort_by(|a, b| b.1.2.cmp(&a.1.2).then_with(|| a.0.cmp(&b.0)));
                ranked
                    .into_iter()
                    .take(MODES_PER_SECTION)
                    .map(|(_, (x, y, n))| Anchor {
                        left: x / n as f64,
                        right: y / n as f64,
                    })
                    .collect()
            })
            .collect();
        let mut hypotheses = Vec::new();
        for a in 0..SECTIONS / 2 {
            for b in (a + SECTIONS / 2)..SECTIONS {
                for left in &modes[a] {
                    for right in &modes[b] {
                        let rate = (right.right - left.right) / (right.left - left.left);
                        if !(0.98..=1.02).contains(&rate) {
                            continue;
                        }
                        let offset = left.right - rate * left.left;
                        let mut count = 0;
                        let mut sections = [0usize; SECTIONS];
                        let mut previous = None;
                        for &((cell, _), p) in &points {
                            if previous != Some(cell)
                                && (p.right - (rate * p.left + offset)).abs() <= INLIER_SECONDS
                            {
                                count += 1;
                                sections[section(p.left)] += 1;
                                previous = Some(cell);
                            }
                        }
                        if count >= policy::DRIFT_MIN_ANCHORS
                            && sections.iter().filter(|&&n| n >= 3).count() >= 6
                        {
                            hypotheses.push((count, rate, offset));
                        }
                    }
                }
            }
        }
        hypotheses.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.total_cmp(&b.1))
                .then_with(|| a.2.total_cmp(&b.2))
        });
        let &(_, rate, offset) = hypotheses.first()?;
        let mut inliers = Vec::<Anchor>::new();
        let mut previous_cell = None;
        for &((cell, _), p) in points
            .iter()
            .filter(|(_, p)| (p.right - (rate * p.left + offset)).abs() <= INLIER_SECONDS)
        {
            if previous_cell == Some(cell) {
                let previous = inliers.last_mut().expect("cell has an inlier");
                if (p.right - (rate * p.left + offset)).abs()
                    < (previous.right - (rate * previous.left + offset)).abs()
                {
                    *previous = p;
                }
            } else {
                inliers.push(p);
                previous_cell = Some(cell);
            }
        }
        let fit = robust_fit(&inliers)?;
        let mut covered = [0usize; SECTIONS];
        for p in &fit.points {
            covered[section(p.left)] += 1;
        }
        if covered.iter().filter(|&&n| n >= 3).count() < 6 {
            return None;
        }
        if !(0.98..=1.02).contains(&fit.rate)
            || fit.inliers < policy::DRIFT_MIN_ANCHORS
            || fit.span < policy::DRIFT_MIN_SPAN_SECONDS
            || fit.residual > 0.08
            || (fit.rate - 1.0).abs() * fit.span
                < 4.0 * BUCKET_WIDTH_FRAMES as f64 * SECONDS_PER_FRAME
        {
            return None;
        }
        let distinct = |rate: f64, offset: f64| {
            [fit.start, fit.end].into_iter().any(|x| {
                ((rate - fit.rate) * x + offset - fit.offset).abs()
                    > 2.0 * BUCKET_WIDTH_FRAMES as f64 * SECONDS_PER_FRAME
            })
        };
        let runner_up = hypotheses
            .iter()
            .filter(|(_, r, o)| distinct(*r, *o))
            .map(|(n, _, _)| *n)
            .max()
            .unwrap_or(0);
        let margin = fit.inliers as f64 / runner_up.max(1) as f64;
        (margin >= policy::REPEATED_TAKE_MARGIN).then_some((fit, margin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_search_declines_when_point_budget_is_exhausted() {
        let mut candidate = Candidate::new(&(0..6).map(|b| (b, 12)).collect::<Vec<_>>()).unwrap();
        for k in 0..=MAX_POINTS {
            let frame = k as u32 * ANCHOR_BUCKET_FRAMES as u32;
            candidate.observe(
                &[Record {
                    hash: k as u64,
                    clip: 0,
                    frame,
                }],
                &[Record {
                    hash: k as u64,
                    clip: 1,
                    frame: frame + (k % 6) as u32 * 8,
                }],
            );
        }
        assert!(candidate.points.is_none());
        assert!(candidate.fit().is_none());
    }
}
