#!/usr/bin/env python3
"""Read-only check of a saved Premiere import against fixture ground truth.

Compares Premiere .prproj Tick positions (254016000000 ticks/second) with
exact timeline/source positions from ground-truth.json (independent of the
XML writer encoding). Fails closed on unknown structure. Never writes the
NLE project. Tolerance: one 48 kHz sample for audio start/source/end.

Usage:
  python3 script/verify-premiere-frac.py ground-truth.json SEQUENCE-KEY
      input.xml project.prproj

SEQUENCE-KEY is 25 or 2997. Prints JSON with per-item sample errors.
"""
import argparse
import gzip
import hashlib
import json
import sys
import xml.etree.ElementTree as ET
from fractions import Fraction
from pathlib import Path

TICKS = 254016000000
SR = 48000


def rate_of(node):
    base = int(node.findtext("timebase"))
    if node.findtext("ntsc") == "TRUE":
        return Fraction(base * 1000, 1001)
    return Fraction(base)


def load_project(project_path):
    project = ET.fromstring(gzip.decompress(Path(project_path).read_bytes()))
    objects = {}
    for node in project:
        for key in ("ObjectID", "ObjectUID"):
            if key in node.attrib:
                assert node.attrib[key] not in objects, "Duplicate object id"
                objects[node.attrib[key]] = node

    def deref(node):
        assert node is not None, "Missing reference"
        return objects[node.get("ObjectRef") or node.get("ObjectURef")]

    seqs = project.findall("Sequence")
    assert len(seqs) == 1, "Use an isolated single-sequence project"
    return project, seqs[0], deref


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("ground_truth", type=Path)
    ap.add_argument("seq_key")
    ap.add_argument("xml", type=Path)
    ap.add_argument("project", type=Path)
    args = ap.parse_args()

    gt = json.loads(args.ground_truth.read_text(encoding="utf-8"))
    assert args.seq_key in gt["sequences"], "Unknown sequence key"
    seq_gt = gt["sequences"][args.seq_key]

    original = ET.parse(args.xml).getroot()
    sequences = original.findall(".//sequence")
    assert len(sequences) == 1, "Expected exactly one input sequence"
    seq = sequences[0]
    assert not seq.findall(".//transitionitem"), "Transitions unsupported"
    assert not seq.findall(".//filter"), "Filters/retimes unsupported"
    assert seq.findtext("name") == seq_gt["sequence"], "Wrong sequence"

    project, actual_seq, deref = load_project(args.project)
    assert actual_seq.findtext("Name") == seq_gt["sequence"], "Wrong project sequence"

    fps = rate_of(seq.find("rate"))
    assert fps == Fraction(seq_gt["fps"]), f"FPS mismatch: {fps} vs {seq_gt['fps']}"

    groups = [deref(n.find("Second")) for n in actual_seq.findall("./TrackGroups/TrackGroup")]
    # Map (kind, track_index_1based, position) -> actual item, in XML order.
    actual_items = {}
    for kind in ("video", "audio"):
        group = next(g for g in groups if g.tag == kind.title() + "TrackGroup")
        expected_tracks = seq.findall(f"./media/{kind}/track")
        actual_tracks = group.findall("./TrackGroup/Tracks/Track")
        assert len(expected_tracks) == len(actual_tracks), f"{kind}: track count"
        for expected_track, track_ref in zip(expected_tracks, actual_tracks):
            track = deref(track_ref)
            track_index = int(track_ref.get("Index")) + 1
            expected_clips = expected_track.findall("clipitem")
            actual_refs = track.findall("./ClipTrack/ClipItems/TrackItems/TrackItem")
            assert len(expected_clips) == len(actual_refs), f"{kind}{track_index}: count"
            for pos, (expected, ref) in enumerate(zip(expected_clips, actual_refs)):
                actual = deref(ref)
                sub = deref(actual.find("./ClipTrackItem/SubClip"))
                assert sub.findtext("Name") == expected.findtext("name"), "Clip order/name mismatch"
                actual_items[(kind, track_index, pos)] = (expected, actual, sub)

    # Ground truth lookup: (kind, track) -> list in timeline order.
    gt_lists: dict = {}
    for row in seq_gt["items"]:
        gt_lists.setdefault((row["kind"], row["track"]), []).append(row)
    for key in gt_lists:
        gt_lists[key].sort(key=lambda r: Fraction(r["timeline_start_s"]))

    report = {"sequence": seq_gt["sequence"], "fps": str(fps), "items": [],
              "errors": [], "gaps": []}
    for (kind, track), rows in sorted(gt_lists.items()):
        for pos, row in enumerate(rows):
            expected, actual, sub = actual_items[(kind, track, pos)]
            clip = deref(sub.find("Clip"))
            start_text = actual.findtext("./ClipTrackItem/TrackItem/Start")
            start = int(start_text) if start_text is not None else 0
            end = int(actual.findtext("./ClipTrackItem/TrackItem/End"))
            want_start = Fraction(row["timeline_start_s"]) * TICKS
            want_end = Fraction(row["timeline_end_s"]) * TICKS
            start_err_s = (Fraction(start, 1) - want_start) / TICKS
            end_err_s = (Fraction(end, 1) - want_end) / TICKS
            start_err_samples = float(start_err_s * SR)
            end_err_samples = float(end_err_s * SR)
            in_ticks = int(clip.findtext("./Clip/InPoint", "0"))
            out_ticks = int(clip.findtext("./Clip/OutPoint"))
            want_in = Fraction(row["source_in_s"]) * TICKS
            want_out = Fraction(row["source_out_s"]) * TICKS
            in_err_samples = float((Fraction(in_ticks, 1) - want_in) * SR / TICKS)
            out_err_samples = float((Fraction(out_ticks, 1) - want_out) * SR / TICKS)
            entry = {
                "key": row["key"], "type": kind, "track": track,
                "name": sub.findtext("Name"),
                "start_error_samples": start_err_samples,
                "end_error_samples": end_err_samples,
                "in_error_samples": in_err_samples,
                "out_error_samples": out_err_samples,
                "start_error_ms": float(start_err_s * 1000),
            }
            if kind == "audio":
                channels = [int(deref(n).findtext("ChannelIndex")) for n in
                            clip.findall("./SecondaryContents/SecondaryContentItem")]
                entry["source_channels"] = channels
                assert channels == row["source_channels"], f"{row['key']}: channels {channels}"
                for field in ("start_error_samples", "end_error_samples",
                              "in_error_samples", "out_error_samples"):
                    if abs(entry[field]) > 1.0 + 1e-9:
                        report["errors"].append(f"{row['key']}: {field}={entry[field]:.3f} samples")
            report["items"].append(entry)

    # Gap check on A1: no overlaps, gaps match ground truth within 1 sample.
    a1 = [r for r in report["items"] if r["type"] == "audio" and r["track"] == 1]
    a1_sorted = sorted(a1, key=lambda r: Fraction(
        next(x["timeline_start_s"] for x in seq_gt["items"] if x["key"] == r["key"])))
    for prev, cur in zip(a1_sorted, a1_sorted[1:]):
        pg = next(x for x in seq_gt["items"] if x["key"] == prev["key"])
        cg = next(x for x in seq_gt["items"] if x["key"] == cur["key"])
        want_gap_samples = float((Fraction(cg["timeline_start_s"]) - Fraction(pg["timeline_end_s"])) * SR)
        # Actual gap = wanted gap + (current start error - previous end error).
        got_gap_samples = want_gap_samples + (cur["start_error_samples"] - prev["end_error_samples"])
        report["gaps"].append({"after": prev["key"], "before": cur["key"],
                               "want_gap_samples": want_gap_samples,
                               "got_gap_samples": got_gap_samples})
        assert want_gap_samples > 0, f"overlap or touch: {prev['key']}->{cur['key']}"
        if abs(got_gap_samples - want_gap_samples) > 1.0 + 1e-9:
            report["errors"].append(f"gap {prev['key']}->{cur['key']}: want {want_gap_samples:.2f} got {got_gap_samples:.2f}")

    # A/V link groups: expect exactly one 3-item group (cam video + 2 audios).
    id_map = {}
    for (kind, track, pos), (expected, actual, sub) in actual_items.items():
        id_map[expected.get("id")] = actual.get("ObjectID")
    actual_links = {frozenset(n.get("ObjectRef") for n in deref(ref).findall("./TrackItemGroup/TrackItems/TrackItem"))
                    for ref in actual_seq.findall("./PersistentGroupContainer/LinkContainer/Links/Link")}
    expected_links = {frozenset(id_map[n.findtext("linkclipref")] for n in item.findall("link"))
                      for item in seq.findall(".//clipitem") if item.findall("link")}
    if actual_links != expected_links:
        report["errors"].append("A/V link groups differ")
    report["av_link_groups"] = len(actual_links)
    if report["av_link_groups"] != 1:
        report["errors"].append("Expected exactly one A/V link group")

    report["sha256"] = {str(p): hashlib.sha256(Path(p).read_bytes()).hexdigest()
                        for p in (args.ground_truth, args.xml, args.project)}
    report["passed"] = not report["errors"]
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
