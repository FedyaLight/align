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
from fractions import Fraction
from urllib.parse import unquote, urlparse
import aaf2

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('cli', type=Path)
parser.add_argument('output', type=Path)
parser.add_argument('--channels', type=int, choices=(1, 2), default=1)
parser.add_argument('--mixed-clocks', action='store_true')
args = parser.parse_args()
root = args.output.resolve()
root.mkdir(parents=True, exist_ok=True)
source = root / 'source.wav'
rng = random.Random(789)
pcm = b''.join(struct.pack('<h', rng.randrange(-20000, 20001)) for _ in range(480000 * args.channels))
with wave.open(str(source), 'wb') as writer:
    writer.setparams((args.channels, 2, 48000, 0, 'NONE', 'PCM'))
    writer.writeframes(pcm)
with aaf2.open(str(root / 'embedded.aaf'), 'w') as container:
    source_mob = container.create.SourceMob('Embedded PCM')
    container.content.mobs.append(source_mob)
    source_slot = source_mob.import_audio_essence(str(source))
    composition = container.create.CompositionMob('Embedded edit')
    composition.usage = 'Usage_TopLevel'
    container.content.mobs.append(composition)
    for channel in range(args.channels):
        if channel:
            source_slot = source_mob.create_timeline_slot(48000)
            source_slot.segment = container.create.SourceClip(media_kind='sound', length=480000)
        source_slot['PhysicalTrackNumber'].value = channel + 1
        track = composition.create_sound_slot('30000/1001' if args.mixed_clocks else 48000)
        if args.mixed_clocks:
            track.segment.components.append(container.create.Filler('sound', 1))
        track.segment.components.append(source_mob.create_source_clip(slot_id=source_slot.slot_id,
            start=48001, length=3 if args.mixed_clocks else 240003, media_kind='sound'))
        track.segment.length = 4 if args.mixed_clocks else 240003

source.unlink()
env = dict(os.environ, ALIGN_BACKEND='portable')
env.pop('ALIGN_AAF', None)
result = subprocess.run([str(args.cli.resolve()), 'sync', str(root / 'embedded.aaf')],
    env=env, capture_output=True, text=True, encoding='utf-8', timeout=180)
(root / 'sync.stderr').write_text(result.stderr, encoding='utf-8')
if result.returncode:
    raise RuntimeError(result.stderr)
(root / 'sync.json').write_text(result.stdout, encoding='utf-8')
value = json.loads(result.stdout)
assert not value['project']['warnings']
assert len(value['project']['clips']) == 1
# A saved result must retain its embedded source after the sync process exits.
saved_media = Path(value['project']['clips'][0]['url'])
assert saved_media.parent.name == 'Saved AAF Media'
with wave.open(str(saved_media), 'rb') as reader:
    assert reader.getnchannels() == args.channels
    assert reader.readframes(reader.getnframes()) == pcm
# Export in a separate process, using only the saved JSON and retained media.
exported = subprocess.run([str(args.cli.resolve()), 'export-json', '--aaf',
    str(root / 'sync.json'), str(root / 'export')], env=env,
    capture_output=True, text=True, encoding='utf-8', check=True, timeout=180)
(root / 'export.json').write_text(exported.stdout, encoding='utf-8')
(root / 'export.stderr').write_text(exported.stderr, encoding='utf-8')
artifacts = json.loads(exported.stdout)
assert len(artifacts) == 1 and artifacts[0]['format'] == 'aaf'
with aaf2.open(artifacts[0]['url']) as container:
    locators = []
    for mob in container.content.mobs:
        if isinstance(mob, aaf2.mobs.SourceMob) and mob.descriptor is not None:
            for locator in mob.descriptor['Locator'].value or []:
                path = unquote(urlparse(locator['URLString'].value).path)
                if os.name == 'nt' and len(path) >= 3 and path[0] == '/' and path[2] == ':':
                    path = path[1:]
                locators.append(Path(path))
    assert locators and all(path.is_file() for path in locators), 'export lost session media'
edits = value['project']['importedTimeline']['edits']
assert [edit['audioSourceChannel'] for edit in edits] == list(range(args.channels))
span = Fraction(3003, 30000) if args.mixed_clocks else Fraction(240003, 48000)
start = Fraction(1001, 30000) if args.mixed_clocks else Fraction(0)
for edit in edits:
    def time(field):
        return Fraction(edit[field]['value'], edit[field]['timescale'])
    assert time('sourceIn') == Fraction(48001, 48000)
    assert time('sourceOut') == Fraction(48001, 48000) + span
    assert time('timelineStart') == start
    assert time('timelineEnd') == start + span
(root / 'verification.json').write_text(json.dumps({'passed': True,
    'saved_media_survives_exit': True, 'export_json_roundtrip': True,
    'exported_media_survives_exit': True,
    'channels': args.channels,
    'pcm_sha256': hashlib.sha256(pcm).hexdigest(), 'source_in_samples': 48001,
    'mixed_clocks': args.mixed_clocks, 'length_seconds': str(span), 'source_wav_exists': source.exists()}, indent=2) + '\n')
