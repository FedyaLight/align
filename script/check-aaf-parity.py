#!/usr/bin/env python3
"""Compare the Rust and Python processes on acceptance and optional real AAF files."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import wave

import aaf2

BRIDGE = Path(__file__).resolve().parents[1] / 'Support/aaf/bridge.py'


def read(command, path, extraction):
    result = subprocess.run([*command, 'read-timeline', path, extraction],
                            capture_output=True, text=True, encoding='utf-8', timeout=90)
    if result.returncode:
        return None, result.stderr.strip().splitlines()[-1]
    data = json.loads(result.stdout)
    for sequence in data['sequences']:
        for track in sequence['tracks']:
            for clip in track['clips']:
                path = Path(clip['path']).resolve()
                if path.parent == extraction.resolve():
                    # Each implementation has its own extraction directory.
                    # Compare its actual bytes, and require hash-named WAVs.
                    digest = hashlib.sha256(path.read_bytes()).hexdigest()
                    assert path.name == f'{digest}.wav', path
                    clip['path'] = f'embedded:{digest}'
    return data, None


def compare_writer(rust, root):
    # Resolve refused a CFB with unsuffixed strong-reference storage names
    # even though pyaaf2 imported it. Compare those wire names against the
    # Python writer on the same manifest, in addition to timeline JSON.
    media = root / 'source & café.wav'
    with wave.open(str(media), 'wb') as output:
        output.setparams((1, 3, 48000, 0, 'NONE', ''))
        output.writeframes(bytes(144000))
    clip = {'path': str(media), 'start': 1, 'source_in': 37, 'length': 1100,
            'source_frames': 48000, 'channels': 1}
    track = {'name': 'Mono', 'sample_rate': 48000, 'clips': [clip]}
    sequences = [{'version': 1, 'name': name, 'tracks': [track]} for name in ('Take A', 'Take B')]
    source = root / 'write.json'
    source.write_text(json.dumps({'version': 3, 'sequences': sequences}), encoding='utf-8')

    def references(path):
        with aaf2.open(path) as file:
            return {stream.path() for _, _, streams in file.cfb.walk('/Header-2/Content-3b03')
                    for stream in streams if stream.name.endswith(' index')}

    a, b = root / 'python-written.aaf', root / 'rust-written.aaf'
    subprocess.run([sys.executable, BRIDGE, 'write-timeline', source, a], check=True,
                   capture_output=True, timeout=90)
    subprocess.run([rust, 'write-timeline', source, b], check=True, capture_output=True, timeout=90)
    assert references(a) == references(b), "AAF writer changed strong-reference names"
    baseline, error = read([sys.executable, BRIDGE], a, root / 'writer-py')
    assert error is None and len(baseline['sequences']) == 2
    assert read([sys.executable, BRIDGE], b, root / 'writer-rs') == (baseline, None)
    complete = b.read_bytes()
    sequences[1]['tracks'] = [dict(track, clips=[dict(clip, source_frames=48001)])]
    source.write_text(json.dumps({'version': 3, 'sequences': sequences}), encoding='utf-8')
    invalid = subprocess.run([rust, 'write-timeline', source, b], capture_output=True, timeout=90)
    assert invalid.returncode and b'WAV frame count differs' in invalid.stderr
    assert b.read_bytes() == complete, "Failed second sequence replaced the complete AAF"
    print('Writer comparison: strong references, batch readback and atomic failure passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('rust', type=Path)
    parser.add_argument('acceptance', type=Path)
    parser.add_argument('--editor-file', type=Path, action='append', default=[])
    args = parser.parse_args()
    expected = [
        'release/aaf/linked-stereo.aaf', 'release/aaf/repair-fixed.aaf',
        'picture-aaf/source.aaf', 'picture-aaf/export/Align.aaf',
        'embedded-mono/embedded.aaf', 'embedded-stereo/embedded.aaf',
        'mixed-clocks/embedded.aaf', 'result-lifecycle/batch-aaf/Align.aaf',
    ]
    cases = [(args.acceptance / name, True) for name in expected]
    cases += [(path, False) for path in args.editor_file]
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        compare_writer(args.rust.resolve(), root)
        for index, (path, must_read) in enumerate(cases):
            assert path.is_file(), path
            python, python_error = read([sys.executable, BRIDGE], path, root / f'py-{index}')
            rust, rust_error = read([args.rust.resolve()], path, root / f'rs-{index}')
            assert (rust == python and (python is not None or not must_read)), (
                path, python_error or python, rust_error or rust)
            if python is None:
                print(f'{path.name}: both reject ({python_error}; {rust_error})')
            else:
                print(f'{path.name}: matching JSON and extracted audio')


if __name__ == '__main__':
    main()
