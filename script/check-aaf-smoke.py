#!/usr/bin/env python3
"""Check bundled AAF discovery using the release smoke's saved sync result."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import wave
import hashlib
import struct
import aaf2

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('cli', type=Path)
parser.add_argument('smoke', type=Path)
args = parser.parse_args()
env = dict(os.environ)
env.pop('ALIGN_AAF', None)
env['ALIGN_BACKEND'] = 'portable'
output = args.smoke.resolve() / 'aaf'
output.mkdir(parents=True, exist_ok=True)

def run_cli(arguments, name):
    proc = subprocess.run([str(args.cli.resolve()), *map(str, arguments)],
        env=env, capture_output=True, text=True, encoding='utf-8', timeout=180)
    (output / (name + '.stderr')).write_text(proc.stderr, encoding='utf-8')
    (output / (name + '.json')).write_text(proc.stdout, encoding='utf-8')
    if proc.returncode:
        raise RuntimeError(proc.stderr)
    return json.loads(proc.stdout)

artifacts = run_cli(['export-json', '--aaf', args.smoke.resolve() / 'sync.json', output], 'export')
assert len(artifacts) == 1 and artifacts[0]['format'] == 'aaf'
with aaf2.open(artifacts[0]['url']) as container:
    compositions = list(container.content.toplevel())
    assert len(compositions) == 1
    slots = list(compositions[0].slots)
    assert len(slots) == 4, 'Two overlapping stereo recordings require four mono tracks'
    assert all(str(slot.edit_rate) == '48000' for slot in slots)
    assert sorted(slot.segment.length for slot in slots) == [672000, 672000, 768000, 768000]
restored = run_cli(['sync', artifacts[0]['url']], 'import')
edits = restored['project']['importedTimeline']['edits']
assert len(edits) == 4 and len(restored['project']['clips']) == 4
assert len(restored['islands']) == 1 and not restored['unmatched']
assert not restored['project']['warnings']
assert sorted(edit['timelineStart']['value'] for edit in edits) == [0, 0, 96000, 96000]
assert all(edit['timelineStart']['timescale'] == 48000 and edit['audioSourceChannel'] == 0 for edit in edits)

# An external AAF links both physical channels of the original stereo files.
# A one-sample source trim exercises the native sample clock through import,
# synchronization, rendering, and AAF re-export.
linked = output / 'linked-stereo.aaf'
with aaf2.open(str(linked), 'w') as container:
    composition = container.create.CompositionMob('Discrete stereo sample edits')
    composition.usage = 'Usage_TopLevel'
    container.content.mobs.append(composition)
    for name, position in [('a.wav', 1), ('b.wav', 96001)]:
        source = args.smoke.resolve() / 'media' / name
        probe = subprocess.run(['ffprobe', '-v', 'error', '-show_format', '-show_streams',
            '-of', 'json', str(source)], capture_output=True, text=True, encoding='utf-8',
            check=True, timeout=30)
        master, _, _ = container.content.create_ama_link(str(source), json.loads(probe.stdout))
        for slot in master.slots:
            target = composition.create_sound_slot(edit_rate=48000)
            target.segment.components.append(container.create.Filler('sound', position))
            target.segment.components.append(master.create_source_clip(slot_id=slot.slot_id,
                start=1, length=671999, media_kind='sound'))
            target.segment.length = position + 671999
stereo = run_cli(['sync', linked], 'stereo-import')
edits = stereo['project']['importedTimeline']['edits']
assert len(stereo['project']['clips']) == 2 and len(edits) == 4
assert len(stereo['islands']) == 1 and not stereo['unmatched']
assert not stereo['project']['warnings']
assert [edit['audioSourceChannel'] for edit in edits] == [0, 1, 0, 1]
assert all(edit['sourceIn'] == {'value': 1, 'timescale': 48000} for edit in edits)
assert [edit['timelineStart'] for edit in edits] == [
    {'value': value, 'timescale': 48000} for value in [1, 1, 96001, 96001]]
artifacts = run_cli(['export-json', '--aaf', '--no-drift', output / 'stereo-import.json',
    output / 'stereo-reexport'], 'stereo-export')
channel_evidence = []
with aaf2.open(artifacts[0]['url']) as container:
    slots = list(next(container.content.toplevel()).slots)
    assert len(slots) == 4
    for index, slot in enumerate(slots):
        components = list(slot.segment.components)
        clips = [component for component in components if isinstance(component, aaf2.components.SourceClip)]
        assert len(clips) == 1 and clips[0].length == 671999
        assert slot.segment.length == (671999 if index < 2 else 767999)
        master_segment = clips[0].mob.slot_at(clips[0].slot_id).segment
        assert isinstance(master_segment, aaf2.components.Sequence) and len(master_segment.components) == 1
        source_mob = master_segment.components[0].mob
        locator = source_mob.descriptor['Locator'].value[0]['URLString'].value
        # The re-export uses rendered mono stems. Decode the file URL with the
        # same standard URL rules on Windows and POSIX, without importing the helper.
        from urllib.parse import urlparse, unquote
        stem = unquote(urlparse(locator).path)
        if os.name == 'nt' and len(stem) >= 3 and stem[0] == '/' and stem[2] == ':':
            stem = stem[1:]
        source = args.smoke.resolve() / 'media' / ('a.wav' if index < 2 else 'b.wav')
        channel = index % 2
        with wave.open(str(source), 'rb') as reader:
            reader.setpos(1)
            interleaved = reader.readframes(671999)
        expected = b''.join(struct.pack('<f', struct.unpack_from('<h', interleaved, i + channel * 2)[0] / 32768)
            for i in range(0, len(interleaved), 4))
        # AVFoundation may expose integer input as float; compare exact PCM
        # amplitudes independently of the stem's lossless storage format.
        actual = subprocess.run(['ffmpeg', '-v', 'error', '-i', stem, '-map', '0:a:0',
            '-f', 'f32le', '-acodec', 'pcm_f32le', '-'], capture_output=True,
            check=True, timeout=30).stdout
        assert actual == expected, 'AAF import/re-export changed or swapped a source channel'
        channel_evidence.append({'track': index + 1, 'source_channel': channel,
            'frames': 671999, 'pcm_f32le_sha256': hashlib.sha256(actual).hexdigest()})
(output / 'verification.json').write_text(json.dumps({
    'tracks': 4, 'imported_edits': 4, 'discrete_stereo_channels': channel_evidence,
    'passed': True}, indent=2) + '\n')
