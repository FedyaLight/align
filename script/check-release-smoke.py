#!/usr/bin/env python3
"""Run an installed CLI against independent PCM ground truth; retain evidence."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import struct
import subprocess
import wave


def seconds(value):
    return value['value'] / value['timescale']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('cli', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    cli = args.cli.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    media = output / 'media'
    media.mkdir()
    rate = 48000
    rng = random.Random(19780713)
    body = [rng.randrange(-18000, 18001) for _ in range(16 * rate)]
    for name, start, end in [('a.wav', 0, 14 * rate), ('b.wav', 2 * rate, 16 * rate)]:
        with wave.open(str(media / name), 'wb') as writer:
            writer.setparams((2, 2, rate, 0, 'NONE', 'not compressed'))
            writer.writeframes(b''.join(struct.pack('<hh', v, -v) for v in body[start:end]))
    env = dict(os.environ, ALIGN_BACKEND='portable')
    def run(arguments, name):
        proc = subprocess.run([str(cli), *arguments], env=env, capture_output=True, text=True, encoding="utf-8", timeout=180)
        (output / (name + '.stderr')).write_text(proc.stderr, encoding='utf-8')
        (output / (name + '.json')).write_text(proc.stdout, encoding='utf-8')
        if proc.returncode:
            raise RuntimeError(f'{name}: CLI exited {proc.returncode}: {proc.stderr[-2000:]}')
        return json.loads(proc.stdout)
    result = run(['sync', str(media)], 'sync')
    assert len(result['project']['clips']) == 2, 'lost source'
    assert len(result['islands']) == 1 and not result['unmatched'], 'pair did not synchronize'
    assert not result['project'].get('warnings'), 'unexpected media warning'
    names = {clip['id']: Path(clip['url']).name for clip in result['project']['clips']}
    offsets = {}
    for placement in result['islands'][0]['placements']:
        points = placement['mapping']['points']
        first, last = points[0], points[-1]
        slope = (seconds(last['island']) - seconds(first['island'])) / (seconds(last['source']) - seconds(first['source']))
        assert abs(slope - 1) < 1e-6, 'false clock drift'
        offsets[names[placement['clipID']]] = seconds(first['island']) - seconds(first['source'])
    error = abs(offsets['b.wav'] - offsets['a.wav'] - 2)
    assert error < 0.001, f'known offset error: {error}s'
    artifacts = run(['export-json', str(output / 'sync.json'), str(output / 'export')], 'export')
    expected = {'resolveOTIO', 'resolveXML', 'resolveScript', 'premiereXML', 'finalCutProXML'}
    assert {item['format'] for item in artifacts} == expected, 'missing export format'
    for item in artifacts:
        assert Path(item['url']).stat().st_size > 0, 'empty artifact'

    timeline_only = run(['export-json', '--no-drift', '--no-fcpxml-multicam',
        '--label-synced', '--synced-color', 'Iris', '--synced-role',
        'dialogue.interview', str(output / 'sync.json'),
        str(output / 'timeline-only')], 'timeline-only')
    timeline_fcpxml = Path(next(item['url'] for item in timeline_only
        if item['format'] == 'finalCutProXML')).read_text(encoding='utf-8')
    assert '– synced' in timeline_fcpxml and 'r_multicam' not in timeline_fcpxml
    assert '[SYNCED] a.wav' in timeline_fcpxml
    assert 'audioRole="dialogue.interview"' in timeline_fcpxml
    timeline_premiere = Path(next(item['url'] for item in timeline_only
        if item['format'] == 'premiereXML')).read_text(encoding='utf-8')
    assert '<label2>Iris</label2>' in timeline_premiere
    assert '<name>[SYNCED] a.wav</name>' in timeline_premiere

    multicam_only = run(['export-json', '--no-drift', '--no-fcpxml-timeline',
        str(output / 'sync.json'), str(output / 'multicam-only')], 'multicam-only')
    multicam_fcpxml = Path(next(item['url'] for item in multicam_only
        if item['format'] == 'finalCutProXML')).read_text(encoding='utf-8')
    assert '– synced' not in multicam_fcpxml and 'r_multicam' in multicam_fcpxml
    without_fcpxml = run(['export-json', '--no-drift', '--no-fcpxml-timeline',
        '--no-fcpxml-multicam', str(output / 'sync.json'),
        str(output / 'without-fcpxml')], 'without-fcpxml')
    assert all(item['format'] != 'finalCutProXML' for item in without_fcpxml)

    corrected = output / 'export' / 'Corrected Audio'
    assert not corrected.exists() or not list(corrected.glob('*.wav')), 'no-drift pair created corrected audio'
    report = {'platform': platform.platform(), 'cli': str(cli), 'sha256': hashlib.sha256(cli.read_bytes()).hexdigest(),
              'clips': 2, 'islands': 1, 'offsetErrorSeconds': error,
              'exportFormats': sorted(expected),
              'finalCutOutputs': ['timeline-only', 'multicam-only'],
              'syncedLabels': {'symbol': '[SYNCED]', 'color': 'Iris',
                  'audioRole': 'dialogue.interview'}}
    (output / 'verification.json').write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
