#!/usr/bin/env python3
"""Verify bundled CLI extraction after the original WAV has been removed."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import struct
import subprocess
import wave
import aaf2

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('cli', type=Path)
parser.add_argument('output', type=Path)
args = parser.parse_args()
root = args.output.resolve()
root.mkdir(parents=True, exist_ok=True)
source = root / 'source.wav'
rng = random.Random(789)
pcm = b''.join(struct.pack('<h', rng.randrange(-20000, 20001)) for _ in range(480000))
with wave.open(str(source), 'wb') as writer:
    writer.setparams((1, 2, 48000, 0, 'NONE', 'PCM'))
    writer.writeframes(pcm)
with aaf2.open(str(root / 'embedded.aaf'), 'w') as container:
    master = container.create.MasterMob('Embedded')
    container.content.mobs.append(master)
    slot = master.import_audio_essence(str(source))
    composition = container.create.CompositionMob('Embedded edit')
    composition.usage = 'Usage_TopLevel'
    container.content.mobs.append(composition)
    track = composition.create_sound_slot(48000)
    track.segment.components.append(master.create_source_clip(slot_id=slot.slot_id,
        start=48001, length=240003, media_kind='sound'))
    track.segment.length = 240003
source.unlink()
env = dict(os.environ, ALIGN_BACKEND='portable')
env.pop('ALIGN_AAF', None)
result = subprocess.run([str(args.cli.resolve()), 'sync', str(root / 'embedded.aaf')],
    env=env, capture_output=True, text=True, encoding='utf-8', timeout=180)
(root / 'sync.stderr').write_text(result.stderr, encoding='utf-8')
if result.returncode:
    raise RuntimeError(result.stderr)
value = json.loads(result.stdout)
assert not value['project']['warnings']
assert len(value['project']['clips']) == 1
with wave.open(value['project']['clips'][0]['url'], 'rb') as reader:
    assert reader.readframes(reader.getnframes()) == pcm
edit = value['project']['importedTimeline']['edits'][0]
assert edit['sourceIn'] == {'value': 48001, 'timescale': 48000}
assert edit['sourceOut'] == {'value': 288004, 'timescale': 48000}
(root / 'verification.json').write_text(json.dumps({'passed': True,
    'pcm_sha256': hashlib.sha256(pcm).hexdigest(), 'source_in_samples': 48001,
    'length_samples': 240003, 'source_wav_exists': source.exists()}, indent=2) + '\n')
