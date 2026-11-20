//! Allocate transition chains first, then pack the remaining recording channels.
use super::model::ExportItem;
use crate::{MediaKind, TimelineTrackRequest, allocate};
use std::collections::{BTreeMap, HashMap};

pub(super) enum Format {
    Premiere,
    Otio,
}

pub(super) fn allocate_entries<'a, E: Copy>(
    entries: &[E],
    kind: MediaKind,
    format: Format,
    parts: impl Fn(&E) -> (&'a ExportItem, usize),
    request: impl Fn(&E) -> TimelineTrackRequest,
) -> Vec<Vec<E>> {
    let position: HashMap<_, _> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let (item, channel) = parts(entry);
            ((item.instance_id.as_str(), channel), i)
        })
        .collect();
    let mut incoming = vec![false; entries.len()];
    let next: Vec<_> = entries
        .iter()
        .map(|entry| {
            let (item, channel) = parts(entry);
            let transition = item.transition_for(kind)?;
            if matches!(format, Format::Otio) && !transition.is_otio_portable {
                return None;
            }
            let &index = position.get(&(transition.right_instance_id.as_str(), channel))?;
            incoming[index] = true;
            Some(index)
        })
        .collect();
    let mut used = vec![false; entries.len()];
    let mut visited_from = vec![usize::MAX; entries.len()];
    let mut tracks = Vec::new();
    for head in 0..entries.len() {
        if incoming[head] || next[head].is_none() {
            continue;
        }
        let mut chain = Vec::new();
        let mut cursor = Some(head);
        while let Some(index) = cursor {
            if visited_from[index] == head {
                break;
            }
            visited_from[index] = head;
            // Premiere can repeat a shared tail on both tracks. OTIO emits
            // each entry once even when malformed transitions converge.
            if !used[index] || matches!(format, Format::Premiere) {
                chain.push(entries[index]);
            }
            used[index] = true;
            cursor = next[index];
        }
        tracks.push(chain);
    }
    let start = |entry: &E| parts(entry).0.timeline_start(kind);
    tracks.sort_by(|a, b| start(&a[0]).total_cmp(&start(&b[0])));
    let remaining: Vec<_> = entries
        .iter()
        .zip(used)
        .filter_map(|(e, u)| (!u).then_some(*e))
        .collect();
    let requests: Vec<_> = remaining.iter().map(request).collect();
    let assignments = allocate(&requests);
    let mut lanes: BTreeMap<usize, Vec<E>> = BTreeMap::new();
    for (e, r) in remaining.into_iter().zip(requests) {
        lanes
            .entry(assignments.get(&r.id).copied().unwrap_or(0))
            .or_default()
            .push(e);
    }
    for mut lane in lanes.into_values() {
        lane.sort_by(|a, b| {
            let (left, lc) = parts(a);
            let (right, rc) = parts(b);
            start(a)
                .total_cmp(&start(b))
                .then_with(|| left.clip.url.file_name().cmp(&right.clip.url.file_name()))
                .then_with(|| lc.cmp(&rc))
        });
        tracks.push(lane);
    }
    tracks
}
