"""Exercise the packaged AAF executable from an unrelated directory."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import wave


def run(binary, arguments, root, env):
    process = subprocess.run([binary, *map(str, arguments)], cwd=root, env=env,
                             capture_output=True, text=True, encoding='utf-8', timeout=60)
    if process.returncode:
        raise RuntimeError(f'{binary.name} exited with {process.returncode}: {process.stderr}')
    return process.stdout


def check_installed_cli(cli, root, env, source):
    sync = json.loads(run(cli, ['sync', source], root, env))
    assert len(sync['project']['importedTimeline']['edits']) == 1
    saved = root / 'sync.json'
    saved.write_text(json.dumps(sync), encoding='utf-8')
    exports = json.loads(run(cli, ['export-json', '--aaf', saved, root / 'export'], root, env))
    export, = [Path(item['url']) for item in exports if item['format'] == 'aaf']
    assert export.is_file()
    reopened = json.loads(run(cli, ['sync', export], root, env))
    assert len(reopened['project']['importedTimeline']['edits']) == 1

    truncated = root / 'truncated.aaf'
    with source.open('rb') as original:
        truncated.write_bytes(original.read(100))
    failure = subprocess.run([cli, 'sync', truncated], cwd=root, env=env, capture_output=True,
                             text=True, encoding='utf-8', timeout=15)
    assert failure.returncode == 1, failure
    assert 'Open AAF container' in failure.stderr, failure.stderr
    print('Installed CLI: AAF import, export, reopen and corrupt-input diagnostic passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--cli', type=Path)
    args = parser.parse_args()
    binary = args.binary.absolute()
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        wav = root / 'mono.wav'
        frames = 2 * 48000
        with wave.open(str(wav), 'wb') as output:
            output.setparams((1, 2, 48000, 0, 'NONE', ''))
            output.writeframes(bytes(frames * 2))
        clip = {'path': str(wav), 'start': 0, 'source_in': 0, 'length': frames,
                'source_frames': frames, 'channels': 1}
        manifest = {'version': 1, 'name': 'Round trip', 'tracks': [
            {'name': 'Mono', 'sample_rate': 48000, 'clips': [clip]}]}
        (root / 'input.json').write_text(json.dumps(manifest), encoding='utf-8')
        env = {k: os.environ[k] for k in ('SYSTEMROOT', 'WINDIR', 'TEMP', 'TMP') if k in os.environ}
        env.update(PATH='', PYTHONHOME=str(root / 'absent'), TMPDIR=str(root / 'absent'))
        source = root / 'out.aaf'
        run(binary, ['write-audio', 'input.json', source], root, env)
        result = json.loads(run(binary, ['read-timeline', source], root, env))
        track, = result['sequences'][0]['tracks']
        if track['name'] != 'Mono' or len(track['clips']) != 1:
            raise ValueError('Packaged AAF round trip lost the mono clip')
        if args.cli is not None:
            cli_env = dict(env, ALIGN_BACKEND='portable', TMPDIR=str(root))
            check_installed_cli(args.cli.absolute(), root, cli_env, source)


if __name__ == '__main__':
    main()
