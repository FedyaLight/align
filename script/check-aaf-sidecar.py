"""Exercise the staged AAF executable from an unrelated directory without Python on PATH."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import wave


def main():
    binary = Path(sys.argv[1]).absolute()
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        wav = root / 'mono.wav'
        with wave.open(str(wav), 'wb') as output:
            output.setparams((1, 2, 48000, 0, 'NONE', ''))
            output.writeframes(bytes(960))
        clip = {'path': str(wav), 'start': 0, 'source_in': 0, 'length': 480,
                'source_frames': 480, 'channels': 1}
        manifest = {'version': 1, 'name': 'Round trip', 'tracks': [
            {'name': 'Mono', 'sample_rate': 48000, 'clips': [clip]}]}
        (root / 'input.json').write_text(json.dumps(manifest))
        env = {k: os.environ[k] for k in ('SYSTEMROOT', 'WINDIR', 'TEMP', 'TMP') if k in os.environ}
        env.update(PATH='', PYTHONHOME=str(root / 'absent'), TMPDIR=str(root / 'absent'))
        def call(*args):
            return json.loads(subprocess.check_output([binary, *args], cwd=root, env=env))
        call('write-audio', 'input.json', 'out.aaf')
        result = call('read-timeline', 'out.aaf')
        track, = result['sequences'][0]['tracks']
        if track['name'] != 'Mono' or len(track['clips']) != 1:
            raise ValueError('Packaged AAF round trip lost the mono clip')


if __name__ == '__main__':
    main()
