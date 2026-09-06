#!/usr/bin/env python3
"""Verify CLI import/export of a real linked picture and stereo sound AAF."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import aaf2

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('cli', type=Path)
parser.add_argument('output', type=Path)
args = parser.parse_args()
root = args.output.resolve()
root.mkdir(parents=True, exist_ok=True)
media = root / 'camera.mov'
subprocess.run(['ffmpeg', '-v', 'error', '-y', '-f', 'lavfi', '-i',
    'color=c=blue:s=64x64:r=30000/1001', '-f', 'lavfi', '-i',
    'sine=frequency=997:sample_rate=48000', '-t', '6', '-ac', '2',
    '-c:v', 'mpeg4', '-c:a', 'pcm_s16le', str(media)], check=True, timeout=30)
metadata = json.loads(subprocess.check_output(['ffprobe', '-v', 'error',
    '-show_format', '-show_streams', '-of', 'json', str(media)], timeout=30))
source_aaf = root / 'source.aaf'
with aaf2.open(str(source_aaf), 'w') as container:
    master, _, _ = container.content.create_ama_link(str(media), metadata)
    composition = container.create.CompositionMob('Mixed picture smoke')
    composition.usage = 'Usage_TopLevel'
    container.content.mobs.append(composition)
    for source in master.slots:
        kind = source.media_kind.lower()
        length = 100 if kind == 'picture' else 160160
        target = composition.create_timeline_slot(source.edit_rate)
        target.segment = container.create.Sequence(media_kind=kind)
        for start, count in ([(0, 50), (50, 50)] if kind == 'picture' else [(0, length)]):
            target.segment.components.append(master.create_source_clip(
                slot_id=source.slot_id, start=start, length=count, media_kind=kind))
        target.segment.length = length
env = dict(os.environ, ALIGN_BACKEND='portable')
env.pop('ALIGN_AAF', None)

def run(arguments, name):
    result = subprocess.run([str(args.cli.resolve()), *map(str, arguments)],
        env=env, capture_output=True, text=True, encoding='utf-8', timeout=180)
    (root / (name + '.stderr')).write_text(result.stderr, encoding='utf-8')
    if result.returncode:
        raise RuntimeError(result.stderr)
    (root / (name + '.json')).write_text(result.stdout, encoding='utf-8')
    return json.loads(result.stdout)

result = run(['sync', source_aaf], 'sync')
edits = result['project']['importedTimeline']['edits']
assert len(edits) == 4 and not result['project']['warnings']
assert [edit.get('audioSourceChannel') for edit in edits] == [None, None, 0, 1]
artifacts = run(['export-json', '--aaf', '--no-drift', root / 'sync.json', root / 'export'], 'export')
with aaf2.open(artifacts[0]['url']) as container:
    all_slots = list(next(container.content.toplevel()).slots)
    timecode = [slot for slot in all_slots if slot.media_kind == "Timecode"]
    assert len(timecode) == 1 and str(timecode[0].edit_rate) == "30000/1001"
    slots = [slot for slot in all_slots if slot.media_kind != "Timecode"]
    assert [slot.media_kind for slot in slots] == ['Picture', 'Sound', 'Sound']
    assert [str(slot.edit_rate) for slot in slots] == ['30000/1001', '48000', '48000']
    assert [slot.segment.length for slot in slots] == [100, 160160, 160160]
    assert len(slots[0].segment.components) == 2
readback = run(['sync', artifacts[0]['url']], 'readback')
assert len(readback['project']['importedTimeline']['edits']) == 4
assert not readback['project']['warnings']
(root / 'verification.json').write_text(json.dumps({'passed': True,
    'picture_rate': '30000/1001', 'picture_frames': 100, 'sound_tracks': 2,
    'sound_samples': 160160}, indent=2) + '\n')
