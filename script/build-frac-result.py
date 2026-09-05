#!/usr/bin/env python3
"""Build synthetic SyncResult JSONs from the frac fixture ground truth.

Mechanical translation only: placements mirror the ground-truth content
positions with identity mappings. Imported edits retain every selected range,
including rec_trim and the camera range. Feed the outputs to the production CLI:

  cargo run -q -p align-cli -- export-json --sequence-name 'AlignFrac 25' \
      /tmp/align-premiere-frac/result-25.json /tmp/align-premiere-fixed-25
  cargo run -q -p align-cli -- export-json --sequence-name 'AlignFrac 2997' \
      /tmp/align-premiere-frac/result-2997.json /tmp/align-premiere-fixed-2997

Usage:
  python3 script/build-frac-result.py [--fixture DIR]
"""
import argparse
import json
import subprocess
from fractions import Fraction
from pathlib import Path

NS = 1_000_000_000


def probe_duration_ns(path: str) -> int:
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-print_format", "json",
         "-show_entries", "format=duration", path],
        check=True, capture_output=True, text=True)
    return int(round(float(json.loads(out.stdout)["format"]["duration"]) * NS))


def mt(ns: int) -> dict:
    return {"value": ns, "timescale": NS}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--fixture", default="/tmp/align-premiere-frac")
    args = ap.parse_args()
    fix = Path(args.fixture)
    gt = json.loads((fix / "ground-truth.json").read_text(encoding="utf-8"))

    for key in ("25", "2997"):
        seq = gt["sequences"][key]
        fps = Fraction(seq["fps"])
        clips = []
        placements = []
        # Camera clip first (starts at 0, anchors the island against shifts).
        cam = next(r for r in seq["items"] if r["key"] == "cam_video")
        cam_dur_ns = probe_duration_ns(cam["file"])
        tb = {"value": 1, "timescale": 25} if key == "25" else \
             {"value": 1001, "timescale": 30000}
        clips.append({
            "id": f"frac-cam-{key}", "url": cam["file"], "kind": "video",
            "duration": mt(cam_dur_ns),
            "audio": [{"sampleRate": 48000.0, "channels": 2, "bitDepth": 16,
                       "isFloat": False, "sourceTimecode": None}],
            "video": {"width": 640, "height": 360, "frameDuration": tb,
                      "sourceTimecode": None, "frameRateMode": "constant"},
            "recordedAt": None, "recordedAtSource": None,
            "sourceIdentifier": None, "mediaSpan": None,
        })
        placements.append({
            "clipID": f"frac-cam-{key}",
            "mapping": {"points": [
                {"source": mt(0), "island": mt(0)},
                {"source": mt(cam_dur_ns), "island": mt(cam_dur_ns)}]},
            "confidence": 0.99,
        })
        for row in seq["items"]:
            if row["kind"] != "audio" or row["track"] != 1:
                continue
            dur_ns = probe_duration_ns(row["file"])
            start_ns = round((Fraction(row["timeline_start_s"]) - Fraction(row["source_in_s"])) * NS)
            clips.append({
                "id": f"frac-{row['key']}-{key}", "url": row["file"], "kind": "audio",
                "duration": mt(dur_ns),
                "audio": [{"sampleRate": 48000.0, "channels": 1, "bitDepth": 16,
                           "isFloat": False, "sourceTimecode": None}],
                "video": None,
                "recordedAt": None, "recordedAtSource": None,
                "sourceIdentifier": None, "mediaSpan": None,
            })
            placements.append({
                "clipID": f"frac-{row['key']}-{key}",
                "mapping": {"points": [
                    {"source": mt(0), "island": mt(start_ns)},
                    {"source": mt(dur_ns), "island": mt(start_ns + dur_ns)}]},
                "confidence": 0.99,
            })
        edits = []
        for row in seq["items"]:
            if row["key"].startswith("cam_ch"):
                continue
            clip_id = f"frac-cam-{key}" if row["key"] == "cam_video" else f"frac-{row['key']}-{key}"
            edits.append({"id": row["key"], "clipID": clip_id,
                "name": Path(row["file"]).name, "mediaType": row["kind"],
                "sourceIn": mt(round(Fraction(row["source_in_s"]) * NS)),
                "sourceOut": mt(round(Fraction(row["source_out_s"]) * NS)),
                "timelineStart": mt(round(Fraction(row["timeline_start_s"]) * NS)),
                "timelineEnd": mt(round(Fraction(row["timeline_end_s"]) * NS)),
                "trackIndex": row["track"],
                "audioTrackIndex": 2 if row["key"] == "cam_video" else None})
        result = {"project": {"clips": clips, "warnings": [],
                               "importedTimeline": {"name": seq["sequence"],
                                   "frameDuration": tb, "edits": edits}},
                  "islands": [{"id": 0, "placements": placements}],
                  "unmatched": [], "matches": [],
                  "temporalPolicy": {"default": "auto", "modes": {}}}
        # SyncProject shape check: does it need more than clips/warnings?
        out = fix / f"result-{key}.json"
        out.write_text(json.dumps(result, indent=1), encoding="utf-8")
        print(f"wrote {out} ({len(clips)} clips)")


if __name__ == "__main__":
    main()
