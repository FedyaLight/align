#!/usr/bin/env python3
"""Verify Premiere audio-mixdown click markers against fixture ground truth.

Each fixture source carries a single-sample click at file offset 4800
(0.1 s). Expected render time = ground-truth timeline start + 0.1 s - source in.
Detection: peak absolute value inside a +/-50 ms window (click 0.9 over a
0.1 sine floor, threshold 0.5). Reports per-marker error in samples.

Usage:
  python3 script/check-premiere-render.py ground-truth.json SEQ-KEY render.wav
Channels are inspected independently; opposite-polarity markers do not cancel.
"""
import argparse
import json
import subprocess
import sys
from fractions import Fraction

import numpy as np

SR = 48000
CLICK_IN_FILE_S = Fraction(4800, SR)


def read_peak_channels(path, sr=SR):
    probe = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "a:0",
         "-show_entries", "stream=sample_rate,channels", "-of", "json", path],
        check=True, capture_output=True, text=True)
    streams = json.loads(probe.stdout)["streams"]
    assert len(streams) == 1, "Expected one audio stream"
    assert int(streams[0]["sample_rate"]) == sr, "Render must be native 48 kHz"
    channels = int(streams[0]["channels"])
    assert channels > 0, "Empty channels"
    out = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", path, "-map", "0:a:0",
         "-f", "f32le", "-c:a", "pcm_f32le", "-"],
        check=True, capture_output=True).stdout
    samples = np.frombuffer(out, dtype=np.float32).reshape(-1, channels)
    assert samples.size and np.isfinite(samples).all(), "Empty or invalid audio"
    return np.max(np.abs(samples), axis=1)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("ground_truth")
    ap.add_argument("seq_key")
    ap.add_argument("render")
    args = ap.parse_args()
    gt = json.load(open(args.ground_truth, encoding="utf-8"))
    seq = gt["sequences"][args.seq_key]
    audio = read_peak_channels(args.render)
    print(f"render samples: {len(audio)} ({len(audio) / SR:.3f} s)")
    errors = []
    ok = True
    for row in seq["items"]:
        if row["kind"] != "audio":
            continue
        source_in = Fraction(row["source_in_s"])
        source_out = Fraction(row["source_out_s"])
        markers = row.get("marker_source_samples", [int(CLICK_IN_FILE_S * SR)])
        if not markers:
            print(f"{row['key']}: EMPTY MARKER COVERAGE")
            ok = False
        for marker in markers:
            source_time = Fraction(marker, SR)
            if not source_in <= source_time < source_out:
                print(f"{row['key']}: NO MARKER IN SELECTED RANGE; fixture coverage fails")
                ok = False
                continue
            want_exact = (Fraction(row["timeline_start_s"]) + source_time - source_in) * SR
            want = round(want_exact)
            lo, hi = max(0, want - 2400), min(len(audio), want + 2400)
            window = audio[lo:hi]
            if window.size == 0:
                print(f"{row['key']:12s} marker {marker}: OUT OF RANGE")
                ok = False
                continue
            candidates = np.flatnonzero(window > 0.5) + lo
            peak = int(min(candidates, key=lambda n: abs(Fraction(int(n)) - want_exact))) if candidates.size else int(np.argmax(window)) + lo
            peak_val = float(audio[peak])
            err = float(Fraction(peak) - want_exact)
            status = "OK " if abs(err) <= 1 and peak_val > 0.5 else "FAIL"
            if status == "FAIL":
                ok = False
            errors.append(err)
            print(f"{row['key']:12s} marker {marker:8d} want {float(want_exact):12.3f} "
                  f"got {peak:8d} err {err:+8.3f} samples peak {peak_val:.2f} {status}")
    print("max abs error: %.3f samples" % max([abs(e) for e in errors] or [0]))
    return 0 if ok and errors else 1


if __name__ == "__main__":
    sys.exit(main())
