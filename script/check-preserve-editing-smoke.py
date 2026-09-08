#!/usr/bin/env python3
"""Exercise Preserve basic editing through sync, export, and importer readback."""

import argparse
import json
import math
import random
import shutil
import struct
import subprocess
import tempfile
import wave
from pathlib import Path


parser = argparse.ArgumentParser()
parser.add_argument("--cli", type=Path, required=True)
parser.add_argument("--output", type=Path)
args = parser.parse_args()
root = args.output or Path(tempfile.mkdtemp(prefix="align-preserve-basic-"))
root.mkdir(parents=True, exist_ok=True)

random_source = random.Random(8128)
camera = root / "camera.wav"
with wave.open(str(camera), "wb") as output:
    output.setnchannels(1)
    output.setsampwidth(2)
    output.setframerate(48_000)
    samples = []
    for index in range(480_000):
        tone = 180 + (index // 24_000) % 9 * 37
        value = 0.48 * math.sin(2 * math.pi * tone * index / 48_000)
        value += 0.16 * (random_source.random() * 2 - 1)
        sample = max(-32768, min(32767, round(value * 32767)))
        samples.append(struct.pack("<h", sample))
    output.writeframes(b"".join(samples))
recorder = root / "recorder.wav"
shutil.copy2(camera, recorder)

source = root / "input.xml"
source.write_text(
    f'''<?xml version="1.0" encoding="UTF-8"?>
<xmeml version="4"><sequence><name>Preserve Basic Editing Smoke</name>
<duration>3000</duration><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate>
<media><audio>
<track><enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="trim"><name>Trim</name><enabled>TRUE</enabled><in>25</in><out>100</out><start>250</start><end>325</end>
<file id="fc"><name>camera.wav</name><pathurl>{camera.resolve().as_uri()}</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>250</duration></file></clipitem>
<clipitem id="duplicate"><name>Duplicate</name><enabled>TRUE</enabled><in>125</in><out>175</out><start>425</start><end>475</end><file id="fc"/></clipitem>
</track>
<track><enabled>TRUE</enabled><locked>FALSE</locked>
<clipitem id="recorder"><name>Recorder</name><enabled>TRUE</enabled><in>0</in><out>50</out><start>2500</start><end>2550</end>
<file id="fr"><name>recorder.wav</name><pathurl>{recorder.resolve().as_uri()}</pathurl><rate><timebase>25</timebase><ntsc>FALSE</ntsc></rate><duration>250</duration></file></clipitem>
</track></audio></media></sequence></xmeml>''',
    encoding="utf-8",
)


def run(arguments, name):
    process = subprocess.run(
        [str(args.cli.resolve()), *map(str, arguments)],
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=180,
    )
    (root / f"{name}.stderr").write_text(process.stderr, encoding="utf-8")
    (root / f"{name}.json").write_text(process.stdout, encoding="utf-8")
    if process.returncode:
        raise RuntimeError(process.stderr)
    return json.loads(process.stdout)


result = run(["sync", "--preserve-basic-editing", "A1", source], "sync")
assert result["preserveEditingTracks"] == ["imported-audio-000001"]
assert len(result["matches"]) == 1
export_dir = root / "export"
artifacts = run(["export-json", "--no-drift", root / "sync.json", export_dir], "export")
premiere = next(Path(item["url"]) for item in artifacts if item["format"] == "premiereXML")
readback = run(["sync", premiere], "readback")


def seconds(value):
    return value["value"] / value["timescale"]


edits = [
    {
        "name": edit["name"],
        "start": seconds(edit["timelineStart"]),
        "end": seconds(edit["timelineEnd"]),
        "source_in": seconds(edit["sourceIn"]),
        "source_out": seconds(edit["sourceOut"]),
        "track": edit["trackIndex"],
        "locked": edit["trackLocked"],
    }
    for edit in readback["project"]["importedTimeline"]["edits"]
]
assert edits[:2] == [
    {"name": "Trim", "start": 10, "end": 13, "source_in": 1, "source_out": 4,
        "track": 1, "locked": True},
    {"name": "Duplicate", "start": 17, "end": 19, "source_in": 5, "source_out": 7,
        "track": 1, "locked": True},
]
assert edits[2]["name"] == "Recorder" and edits[2]["track"] == 2
assert not edits[2]["locked"]

otio = next(Path(item["url"]) for item in artifacts if item["format"] == "resolveOTIO")
document = json.loads(otio.read_text(encoding="utf-8"))
placements = {}
for track in document["tracks"]["children"]:
    cursor = 0.0
    for child in track["children"]:
        duration = child["source_range"]["duration"]
        duration = duration["value"] / duration["rate"]
        if child["OTIO_SCHEMA"].startswith("Clip."):
            placements[child["name"]] = cursor
        cursor += duration
assert placements == {"Trim": 10.0, "Duplicate": 17.0, "Recorder": 10.5}

report = {
    "passed": True,
    "preserved_track": "A1",
    "premiere_readback": edits,
    "resolve_otio_starts": placements,
}
(root / "verification.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
print(json.dumps({"root": str(root), **report}, indent=2))
