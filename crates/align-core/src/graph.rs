//! Sync graph solve. Port of Sources/AlignCore/MatchGraph.swift.
//!
//! 1. Admit edges: spanned-metadata always, waveform only past the selected
//!    match threshold with ≥ 3 anchors, ≥ 3 s covered, residual ≤ 0.1 s.
//! 2. Connected components over admitted edges = independent sync islands.
//! 3. Per island: root at max confidence-degree (video clips get +1000 via
//!    `preferred_roots`), then 4 IRLS iterations of weighted least squares
//!    over log-rates and offsets with Cauchy-style robust reweighting
//!    (scale = 0.004 + 2 × residual), partial-pivot Gaussian elimination.
//! 4. Piecewise mappings propagated BFS from the root along ranked edges,
//!    falling back to the affine map; island shifted so min offset = 0.
//!
//! Fully deterministic: all order-dependent steps sort by explicit total
//! keys (quantized confidence/anchors/coverage/residual, then clip ids).

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::matcher::{MatchPolicy, PairAlignmentPoint, PairwiseMatch, policy};
use crate::model::{ClipId, MatchEvidence};
use crate::piecewise::{MapPoint, PiecewiseTimeMapping};

#[derive(Clone, Debug, PartialEq)]
pub struct SolvedPlacement {
    pub clip_id: ClipId,
    /// islandTime = rate × sourceTime + offset.
    pub rate: f64,
    pub offset: f64,
    pub confidence: f64,
    pub mapping_points: Vec<MapPoint>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SolvedIsland {
    pub placements: Vec<SolvedPlacement>,
}

pub fn solve(
    clips: &[ClipId],
    matches: &[PairwiseMatch],
    preferred_roots: &HashSet<ClipId>,
    match_policy: &MatchPolicy,
) -> Vec<SolvedIsland> {
    let mut usable: Vec<&PairwiseMatch> = matches
        .iter()
        .filter(|m| {
            m.evidence == MatchEvidence::SpannedMetadata
                || (m.confidence >= match_policy.minimum_confidence(&m.left, &m.right)
                    // Timecode analysis validates overlap and clock metadata;
                    // its single anchor is not a waveform fingerprint count.
                    && (m.evidence == MatchEvidence::Timecode
                        || m.anchors >= policy::GRAPH_MIN_ANCHORS)
                    && m.covered_seconds >= policy::GRAPH_MIN_COVERED_SECONDS
                    && m.residual_seconds <= policy::GRAPH_MAX_RESIDUAL_SECONDS)
        })
        .collect();
    usable.sort_by(|a, b| edge_key(a).cmp(&edge_key(b)));

    let mut adjacency: HashMap<&ClipId, Vec<&PairwiseMatch>> = HashMap::new();
    for m in &usable {
        adjacency.entry(&m.left).or_default().push(m);
        adjacency.entry(&m.right).or_default().push(m);
    }

    let mut ordered_roots: Vec<&ClipId> = clips.iter().collect();
    ordered_roots.sort_by(|a, b| a.0.cmp(&b.0));
    let mut visited: HashSet<&ClipId> = HashSet::new();
    let mut islands = Vec::new();
    for root in ordered_roots {
        if visited.contains(root) {
            continue;
        }
        let mut queue = VecDeque::from([root]);
        let mut component: Vec<ClipId> = Vec::new();
        visited.insert(root);
        while let Some(current) = queue.pop_front() {
            component.push((*current).clone());
            if let Some(edges) = adjacency.get(current) {
                for edge in edges {
                    let next = if *current == edge.left {
                        &edge.right
                    } else {
                        &edge.left
                    };
                    if visited.insert(next) {
                        queue.push_back(next);
                    }
                }
            }
        }
        let members: HashSet<&ClipId> = component.iter().collect();
        let edges: Vec<PairwiseMatch> = usable
            .iter()
            .filter(|m| members.contains(&m.left) && members.contains(&m.right))
            .map(|m| (*m).clone())
            .collect();
        islands.push(SolvedIsland {
            placements: solve_component(component, &edges, preferred_roots),
        });
    }
    islands.sort_by(|a, b| {
        let fa = a
            .placements
            .first()
            .map(|p| p.clip_id.0.as_str())
            .unwrap_or("");
        let fb = b
            .placements
            .first()
            .map(|p| p.clip_id.0.as_str())
            .unwrap_or("");
        fa.cmp(fb)
    });
    islands
}

/// Total sort key: confidence desc, anchors desc, coverage desc,
/// residual asc, then ids — quantized exactly like Swift `preferredEdge`.
fn edge_key(m: &PairwiseMatch) -> (Reverse<i64>, Reverse<usize>, Reverse<i64>, i64, &str, &str) {
    (
        Reverse((m.confidence * 10_000.0).round() as i64),
        Reverse(m.anchors),
        Reverse((m.covered_seconds * 10.0).round() as i64),
        (m.residual_seconds * 10_000.0).round() as i64,
        m.left.0.as_str(),
        m.right.0.as_str(),
    )
}

fn solve_component(
    clips: Vec<ClipId>,
    edges: &[PairwiseMatch],
    preferred_roots: &HashSet<ClipId>,
) -> Vec<SolvedPlacement> {
    if clips.len() <= 1 {
        return clips
            .into_iter()
            .map(|clip_id| SolvedPlacement {
                clip_id,
                rate: 1.0,
                offset: 0.0,
                confidence: 1.0,
                mapping_points: Vec::new(),
            })
            .collect();
    }
    let mut degree: HashMap<&ClipId, f64> = HashMap::new();
    for e in edges {
        *degree.entry(&e.left).or_default() += e.confidence;
        *degree.entry(&e.right).or_default() += e.confidence;
    }
    // First maximal in input order on full ties (strict improvement only),
    // ties on score broken toward the smaller id — like Swift `max(by:)`.
    let mut root = &clips[0];
    let mut best = degree.get(root).copied().unwrap_or(0.0)
        + if preferred_roots.contains(root) {
            1000.0
        } else {
            0.0
        };
    for clip in &clips[1..] {
        let score = degree.get(clip).copied().unwrap_or(0.0)
            + if preferred_roots.contains(clip) {
                1000.0
            } else {
                0.0
            };
        if score > best || (score == best && clip.0 < root.0) {
            root = clip;
            best = score;
        }
    }
    let root = root.clone();
    let mut unknowns: Vec<ClipId> = clips.iter().filter(|c| **c != root).cloned().collect();
    unknowns.sort_by(|a, b| a.0.cmp(&b.0));
    let indices: HashMap<&ClipId, usize> =
        unknowns.iter().enumerate().map(|(i, c)| (c, i)).collect();

    let mut weights: Vec<f64> = edges.iter().map(base_weight).collect();
    let mut log_rates: HashMap<ClipId, f64> = HashMap::new();
    let mut offsets: HashMap<ClipId, f64> = HashMap::new();
    for _ in 0..policy::IRLS_ITERATIONS {
        log_rates = solve_equations(&clips, &root, &indices, edges, &weights, |e| e.rate.ln());
        let lr = log_rates.clone();
        offsets = solve_equations(&clips, &root, &indices, edges, &weights, |e| {
            lr.get(&e.right).copied().unwrap_or(0.0).exp() * e.offset
        });
        for (i, edge) in edges.iter().enumerate() {
            let lr_l = log_rates.get(&edge.left).copied().unwrap_or(0.0);
            let lr_r = log_rates.get(&edge.right).copied().unwrap_or(0.0);
            let off_l = offsets.get(&edge.left).copied().unwrap_or(0.0);
            let off_r = offsets.get(&edge.right).copied().unwrap_or(0.0);
            let rate_error = (lr_l - lr_r - edge.rate.ln()).abs() * edge.covered_seconds;
            let offset_error = (off_l - off_r - lr_r.exp() * edge.offset).abs();
            let error = rate_error.hypot(offset_error);
            let scale = 0.004 + 2.0 * edge.residual_seconds;
            let robust = (scale / scale.max(error)).min(1.0);
            weights[i] = base_weight(edge) * robust * robust;
        }
    }

    let minimum_offset = clips
        .iter()
        .map(|c| offsets.get(c).copied().unwrap_or(0.0))
        .fold(f64::INFINITY, f64::min);
    let affine: HashMap<&ClipId, PiecewiseTimeMapping> = clips
        .iter()
        .map(|clip| {
            let lr = offsets_and_rate(&log_rates, offsets.get(clip).copied().unwrap_or(0.0), clip);
            let map = PiecewiseTimeMapping::new(vec![
                MapPoint::new(0.0, lr.1 - minimum_offset),
                MapPoint::new(1.0, lr.0 + lr.1 - minimum_offset),
            ]);
            (clip, map)
        })
        .collect();
    let propagated = propagate_mappings(&root, &clips, edges, &affine);

    let mut placements: Vec<SolvedPlacement> = clips
        .iter()
        .map(|clip| {
            let confidence = edges
                .iter()
                .filter(|e| &e.left == clip || &e.right == clip)
                .map(|e| e.confidence)
                .fold(None::<f64>, |acc, c| Some(acc.map_or(c, |a: f64| a.max(c))))
                .unwrap_or(1.0);
            SolvedPlacement {
                clip_id: clip.clone(),
                rate: log_rates.get(clip).copied().unwrap_or(0.0).exp(),
                offset: offsets.get(clip).copied().unwrap_or(0.0) - minimum_offset,
                confidence,
                mapping_points: propagated
                    .get(clip)
                    .map(|m| m.points.clone())
                    .unwrap_or_default(),
            }
        })
        .collect();
    placements.sort_by(|a, b| a.clip_id.0.cmp(&b.clip_id.0));
    placements
}

fn offsets_and_rate(log_rates: &HashMap<ClipId, f64>, offset: f64, clip: &ClipId) -> (f64, f64) {
    (log_rates.get(clip).copied().unwrap_or(0.0).exp(), offset)
}

fn propagate_mappings(
    root: &ClipId,
    clips: &[ClipId],
    edges: &[PairwiseMatch],
    affine: &HashMap<&ClipId, PiecewiseTimeMapping>,
) -> HashMap<ClipId, PiecewiseTimeMapping> {
    let mut result: HashMap<ClipId, PiecewiseTimeMapping> = HashMap::new();
    result.insert(root.clone(), affine[&root].clone());
    let mut nonlinear: HashSet<ClipId> = HashSet::new();
    let mut queue = VecDeque::from([root.clone()]);
    let mut ranked: Vec<&PairwiseMatch> = edges.iter().collect();
    ranked.sort_by(|a, b| edge_key(a).cmp(&edge_key(b)));

    // Kruskal first, root orientation second. Walking every root-adjacent
    // edge immediately would let a weaker direct match permanently mask a
    // stronger transitive one merely because it is one hop shorter.
    let indices: HashMap<&ClipId, usize> = clips.iter().enumerate().map(|(i, c)| (c, i)).collect();
    let mut parents: Vec<usize> = (0..clips.len()).collect();
    fn representative(parents: &mut [usize], mut index: usize) -> usize {
        while parents[index] != index {
            parents[index] = parents[parents[index]];
            index = parents[index];
        }
        index
    }
    let mut tree = Vec::with_capacity(clips.len().saturating_sub(1));
    for edge in ranked {
        let left = representative(&mut parents, indices[&edge.left]);
        let right = representative(&mut parents, indices[&edge.right]);
        if left != right {
            parents[right] = left;
            tree.push(edge);
        }
    }

    while let Some(parent) = queue.pop_front() {
        let Some(parent_map) = result.get(&parent).cloned() else {
            continue;
        };
        for edge in &tree {
            if edge.left != parent && edge.right != parent {
                continue;
            }
            let child = if edge.left == parent {
                &edge.right
            } else {
                &edge.left
            };
            if result.contains_key(child) {
                continue;
            }
            let pair_points: Vec<PairAlignmentPoint> = if edge.alignment_points.len() >= 2 {
                edge.alignment_points.clone()
            } else {
                vec![
                    PairAlignmentPoint {
                        left: edge.left_start_seconds,
                        right: edge.rate * edge.left_start_seconds + edge.offset,
                    },
                    PairAlignmentPoint {
                        left: edge.left_end_seconds,
                        right: edge.rate * edge.left_end_seconds + edge.offset,
                    },
                ]
            };
            let pair_map = PiecewiseTimeMapping::new(
                pair_points
                    .iter()
                    .map(|p| MapPoint::new(p.left, p.right))
                    .collect(),
            );
            let mut child_points: Vec<MapPoint> = Vec::new();
            if parent == edge.left {
                child_points.extend(
                    pair_points
                        .iter()
                        .map(|p| MapPoint::new(p.right, parent_map.value_at(p.left))),
                );
                child_points.extend(
                    parent_map
                        .points
                        .iter()
                        .filter(|pt| pair_map.contains(pt.source))
                        .map(|pt| MapPoint::new(pair_map.value_at(pt.source), pt.island)),
                );
            } else {
                let inverse = pair_map.inverted();
                child_points.extend(
                    pair_points
                        .iter()
                        .map(|p| MapPoint::new(p.left, parent_map.value_at(p.right))),
                );
                child_points.extend(
                    parent_map
                        .points
                        .iter()
                        .filter(|pt| inverse.contains(pt.source))
                        .map(|pt| MapPoint::new(inverse.value_at(pt.source), pt.island)),
                );
            }
            let map = PiecewiseTimeMapping::new(child_points);
            let fallback = affine[&child].clone();
            let carries_piecewise = nonlinear.contains(&parent) || edge.alignment_points.len() > 2;
            result.insert(
                child.clone(),
                if carries_piecewise && map.points.len() >= 2 {
                    nonlinear.insert(child.clone());
                    map
                } else {
                    fallback
                },
            );
            queue.push_back(child.clone());
        }
    }
    for clip in clips {
        result
            .entry(clip.clone())
            .or_insert_with(|| affine[&clip].clone());
    }
    result
}

fn solve_equations(
    nodes: &[ClipId],
    root: &ClipId,
    indices: &HashMap<&ClipId, usize>,
    edges: &[PairwiseMatch],
    weights: &[f64],
    rhs: impl Fn(&PairwiseMatch) -> f64,
) -> HashMap<ClipId, f64> {
    let count = indices.len();
    if count == 0 {
        return [(root.clone(), 0.0)].into_iter().collect();
    }
    let mut matrix = vec![vec![0.0; count]; count];
    let mut vector = vec![0.0; count];
    for (edge, weight) in edges.iter().zip(weights.iter()) {
        let value = rhs(edge);
        let terms = [
            (indices.get(&edge.left), 1.0),
            (indices.get(&edge.right), -1.0),
        ];
        let active: Vec<(usize, f64)> = terms
            .iter()
            .filter_map(|(i, c)| i.map(|idx| (*idx, *c)))
            .collect();
        for (row, rc) in &active {
            vector[*row] += weight * rc * value;
            for (col, cc) in &active {
                matrix[*row][*col] += weight * rc * cc;
            }
        }
    }
    for (i, row) in matrix.iter_mut().enumerate().take(count) {
        row[i] += 1e-12;
    }
    let solution = gaussian_solve(&matrix, &vector);
    let mut out: HashMap<ClipId, f64> = nodes.iter().map(|n| (n.clone(), 0.0)).collect();
    for (clip, idx) in indices {
        out.insert((*clip).clone(), solution[*idx]);
    }
    out
}

/// Gaussian elimination with partial pivoting. First-maximal pivot (strict
/// `>`), mirroring Swift `max(by:)` tie behaviour.
fn gaussian_solve(matrix: &[Vec<f64>], rhs: &[f64]) -> Vec<f64> {
    let n = matrix.len();
    let mut m: Vec<Vec<f64>> = matrix.to_vec();
    let mut v: Vec<f64> = rhs.to_vec();
    for col in 0..n {
        let mut pivot = col;
        for row in (col + 1)..n {
            if m[row][col].abs() > m[pivot][col].abs() {
                pivot = row;
            }
        }
        if pivot != col {
            m.swap(pivot, col);
            v.swap(pivot, col);
        }
        let divisor = m[col][col];
        if divisor.abs() <= 1e-18 {
            continue;
        }
        for row in (col + 1)..n {
            let factor = m[row][col] / divisor;
            if factor == 0.0 {
                continue;
            }
            // Indexed to mirror the Swift elimination loop 1:1; the borrow is
            // split so pivot row and target row coexist without aliasing.
            let (top, bottom) = m.split_at_mut(row);
            let pivot_row = &top[col];
            for (item, slot) in bottom[0].iter_mut().enumerate().take(n).skip(col) {
                *slot -= factor * pivot_row[item];
            }
            v[row] -= factor * v[col];
        }
    }
    let mut out = vec![0.0; n];
    for row in (0..n).rev() {
        let known: f64 = ((row + 1)..n).map(|c| m[row][c] * out[c]).sum();
        if m[row][row].abs() > 1e-18 {
            out[row] = (v[row] - known) / m[row][row];
        }
    }
    out
}

fn base_weight(edge: &PairwiseMatch) -> f64 {
    let residual_scale = 0.004 + 2.0 * edge.residual_seconds;
    edge.confidence * edge.confidence * (edge.anchors as f64 / 100.0).clamp(0.1, 16.0)
        / residual_scale.powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ClipId {
        ClipId::new(s)
    }

    fn edge(
        left: &str,
        right: &str,
        offset: f64,
        confidence: f64,
        evidence: MatchEvidence,
    ) -> PairwiseMatch {
        PairwiseMatch {
            left: id(left),
            right: id(right),
            rate: 1.0,
            offset,
            confidence,
            anchors: 20,
            left_start_seconds: 0.0,
            left_end_seconds: 60.0,
            covered_seconds: 60.0,
            residual_seconds: 0.005,
            alignment_points: vec![],
            evidence,
        }
    }

    #[test]
    fn timecode_edge_with_one_metadata_anchor_joins_graph() {
        let mut tc = edge("a", "b", 2.0, 0.95, MatchEvidence::Timecode);
        tc.anchors = 1;
        let islands = solve(
            &[id("a"), id("b")],
            &[tc],
            &HashSet::new(),
            &MatchPolicy::default(),
        );
        assert_eq!(islands.len(), 1);
        assert_eq!(islands[0].placements.len(), 2);
        for (evidence, confidence) in [
            (MatchEvidence::Waveform, 0.95),
            (MatchEvidence::Timecode, 0.1),
        ] {
            let mut rejected = edge("a", "b", 2.0, confidence, evidence);
            rejected.anchors = 1;
            assert_eq!(
                solve(
                    &[id("a"), id("b")],
                    &[rejected],
                    &HashSet::new(),
                    &MatchPolicy::default()
                )
                .len(),
                2
            );
        }
    }

    #[test]
    fn exact_long_pair_keeps_one_clock_rate_in_noisy_graph() {
        let mut camera_one = edge("camera", "track-1", 0.0, 0.9, MatchEvidence::Waveform);
        camera_one.rate = 1.000_010;
        camera_one.anchors = 500;
        camera_one.covered_seconds = 1_500.0;
        camera_one.residual_seconds = 0.003;

        let mut camera_two = edge("camera", "track-2", 0.0, 0.9, MatchEvidence::Waveform);
        camera_two.rate = 1.000_020;
        camera_two.anchors = 500;
        camera_two.covered_seconds = 1_500.0;
        camera_two.residual_seconds = 0.003;

        // Simultaneous recorder tracks share one hardware clock. Their direct,
        // longer observation is substantially more precise than either noisy
        // camera edge and must not be torn apart by the global fit.
        let mut same_clock = edge("track-1", "track-2", 0.0, 0.999, MatchEvidence::Waveform);
        same_clock.rate = 1.0;
        same_clock.anchors = 1_200;
        same_clock.covered_seconds = 4_100.0;
        same_clock.residual_seconds = 0.000_1;

        let preferred = [id("camera")].into_iter().collect();
        let solved = solve(
            &[id("camera"), id("track-1"), id("track-2")],
            &[camera_one, camera_two, same_clock],
            &preferred,
            &MatchPolicy::default(),
        );
        let rate = |name: &str| {
            solved[0]
                .placements
                .iter()
                .find(|p| p.clip_id == id(name))
                .expect("placement")
                .rate
        };
        let separation_ppm = ((rate("track-1") / rate("track-2")) - 1.0).abs() * 1_000_000.0;
        assert!(separation_ppm < 0.5, "separation={separation_ppm:.3} ppm");
        let mapping_rate = |name: &str| {
            let placement = solved[0]
                .placements
                .iter()
                .find(|p| p.clip_id == id(name))
                .expect("placement");
            let [first, last] = placement.mapping_points.as_slice() else {
                panic!("affine mapping must have two points")
            };
            (last.island - first.island) / (last.source - first.source)
        };
        let mapping_separation =
            ((mapping_rate("track-1") / mapping_rate("track-2")) - 1.0).abs() * 1_000_000.0;
        assert!(
            mapping_separation < 0.5,
            "mapping separation={mapping_separation:.3} ppm"
        );
    }

    #[test]
    fn mapping_propagation_prefers_stronger_transitive_edge() {
        let mut direct = edge("camera", "target", 0.0, 0.90, MatchEvidence::Waveform);
        direct.rate = 1.000_020;
        let bridge = edge("camera", "recorder", 0.0, 0.95, MatchEvidence::Waveform);
        let mut precise = edge("recorder", "target", 0.0, 0.99, MatchEvidence::Waveform);
        precise.rate = 1.000_010;
        precise.alignment_points = vec![
            PairAlignmentPoint {
                left: 0.0,
                right: 0.0,
            },
            PairAlignmentPoint {
                left: 30.0,
                right: 30.000_3,
            },
            PairAlignmentPoint {
                left: 60.0,
                right: 60.000_6,
            },
        ];
        let precise_rate = precise.rate;

        let preferred = [id("camera")].into_iter().collect();
        let solved = solve(
            &[id("camera"), id("recorder"), id("target")],
            &[direct, bridge, precise],
            &preferred,
            &MatchPolicy::default(),
        );
        let slope = |name: &str| {
            let placement = solved[0]
                .placements
                .iter()
                .find(|p| p.clip_id == id(name))
                .expect("placement");
            let first = placement.mapping_points.first().expect("mapping start");
            let last = placement.mapping_points.last().expect("mapping end");
            (last.island - first.island) / (last.source - first.source)
        };
        let relative = slope("target") / slope("recorder");

        assert!(
            (relative - 1.0 / precise_rate).abs() < 1e-9,
            "relative={relative}"
        );
        assert!((relative - 1.0 / 1.000_020).abs() > 1e-6);
    }

    fn offsets_of(island: &SolvedIsland) -> HashMap<String, f64> {
        island
            .placements
            .iter()
            .map(|p| (p.clip_id.0.clone(), p.offset))
            .collect()
    }

    #[test]
    fn singleton_is_identity() {
        let islands = solve(&[id("x")], &[], &HashSet::new(), &MatchPolicy::default());
        assert_eq!(islands.len(), 1);
        let p = &islands[0].placements[0];
        assert_eq!((p.rate, p.offset, p.confidence), (1.0, 0.0, 1.0));
        assert!(p.mapping_points.is_empty());
    }

    #[test]
    fn pair_preserves_relative_offset() {
        let islands = solve(
            &[id("a"), id("b")],
            &[edge("a", "b", 2.5, 0.9, MatchEvidence::Waveform)],
            &HashSet::new(),
            &MatchPolicy::default(),
        );
        assert_eq!(islands.len(), 1);
        assert_eq!(islands[0].placements.len(), 2);
        let o = offsets_of(&islands[0]);
        assert!((o["a"] - o["b"] - 2.5).abs() < 1e-9, "{o:?}");
        assert!(o.values().fold(f64::INFINITY, |a, b| a.min(*b)) == 0.0);
        for p in &islands[0].placements {
            assert!((p.rate - 1.0).abs() < 1e-9);
            assert!(p.mapping_points.len() >= 2);
        }
    }

    #[test]
    fn transitive_chain_is_one_island() {
        let islands = solve(
            &[id("a"), id("b"), id("c")],
            &[
                edge("a", "b", 1.0, 0.9, MatchEvidence::Waveform),
                edge("b", "c", 1.0, 0.85, MatchEvidence::Waveform),
            ],
            &HashSet::new(),
            &MatchPolicy::default(),
        );
        assert_eq!(islands.len(), 1);
        let o = offsets_of(&islands[0]);
        assert!((o["a"] - o["b"] - 1.0).abs() < 1e-6, "{o:?}");
        assert!((o["b"] - o["c"] - 1.0).abs() < 1e-6, "{o:?}");
    }

    #[test]
    fn weak_waveform_edges_do_not_connect() {
        let islands = solve(
            &[id("a"), id("b")],
            &[edge("a", "b", 1.0, 0.4, MatchEvidence::Waveform)],
            &HashSet::new(),
            &MatchPolicy::default(),
        );
        assert_eq!(islands.len(), 2);
    }

    #[test]
    fn match_threshold_changes_graph_admission() {
        let edge = edge("a", "b", 1.0, 0.50, MatchEvidence::Waveform);
        let policy = |default| MatchPolicy {
            default,
            thresholds: HashMap::new(),
        };
        assert_eq!(
            solve(
                &[id("a"), id("b")],
                std::slice::from_ref(&edge),
                &HashSet::new(),
                &policy(crate::matcher::MatchThreshold::Permissive),
            )[0]
            .placements
            .len(),
            2
        );
        assert_eq!(
            solve(
                &[id("a"), id("b")],
                &[edge],
                &HashSet::new(),
                &policy(crate::matcher::MatchThreshold::Conservative),
            )
            .len(),
            2
        );
    }

    #[test]
    fn spanned_metadata_bypasses_thresholds() {
        let islands = solve(
            &[id("a"), id("b")],
            &[edge("a", "b", 1.0, 0.1, MatchEvidence::SpannedMetadata)],
            &HashSet::new(),
            &MatchPolicy::default(),
        );
        assert_eq!(islands.len(), 1);
    }

    #[test]
    fn preferred_video_root_accepted_and_consistent() {
        let preferred: HashSet<ClipId> = [id("cam")].into_iter().collect();
        let islands = solve(
            &[id("cam"), id("rec")],
            &[edge("cam", "rec", 3.0, 0.9, MatchEvidence::Waveform)],
            &preferred,
            &MatchPolicy::default(),
        );
        assert_eq!(islands.len(), 1);
        let o = offsets_of(&islands[0]);
        assert!((o["cam"] - o["rec"] - 3.0).abs() < 1e-9);
    }

    #[test]
    fn gaussian_solver_matches_known_system() {
        // 2x + y = 5; x + 3y = 6  →  x = 1.8, y = 1.4.
        let x = gaussian_solve(&[vec![2.0, 1.0], vec![1.0, 3.0]], &[5.0, 6.0]);
        assert!((x[0] - 1.8).abs() < 1e-12);
        assert!((x[1] - 1.4).abs() < 1e-12);
    }
}
