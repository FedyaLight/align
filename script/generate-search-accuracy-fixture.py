#!/usr/bin/env python3
"""Create a known-delay shared-band fixture plus an unrelated recording."""

import argparse
import json
import math
from pathlib import Path
import struct
import wave

RATE = 8000
MASK = (1 << 64) - 1
BANDS = [(12, 17), (35, 26), (68, 55), (133, 116), (262, 215)]


def recording(seed, shared):
    phases = [0.0] * 5
    for n in range(RATE * 30):
        slot = n // 2048
        sample = 0.0
        for band, (lo, width) in enumerate(BANDS):
            key = 17 if band == 2 and shared else seed + band * 71
            h = (((slot + 1) * 0x9E3779B97F4A7C15) & MASK) ^ (
                (key * 0xBF58476D1CE4E5B9) & MASK
            )
            h = ((h ^ (h >> 30)) * 0xBF58476D1CE4E5B9) & MASK
            h ^= h >> 27
            frequency_bin = lo + h % width
            phases[band] += 2 * math.pi * frequency_bin / 1024
            sample += math.sin(phases[band]) * (0.3 if band == 2 else 0.1)
        yield round(sample * 32767)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=False)
    for name, seed, shared, delay in [
        ("a.wav", 123, True, 0),
        ("b.wav", 456, True, 8192),
        ("unrelated.wav", 789, False, 0),
    ]:
        with wave.open(str(args.directory / name), "wb") as output:
            output.setparams((1, 2, RATE, 0, "NONE", "not compressed"))
            output.writeframes(bytes(delay * 2))
            samples = list(recording(seed, shared))
            output.writeframes(struct.pack(f"<{len(samples)}h", *samples))
    (args.directory / "ground-truth.json").write_text(
        json.dumps(
            {
                "sampleRate": RATE,
                "sharedPair": ["a.wav", "b.wav"],
                "rightDelaySamples": 8192,
                "rightDelaySeconds": 1.024,
                "unrelated": "unrelated.wav",
            },
            indent=2,
        )
        + "\n"
    )


if __name__ == "__main__":
    main()
