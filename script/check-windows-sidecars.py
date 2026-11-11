#!/usr/bin/env python3
"""Run the shipped FFmpeg tools without MSYS2, PATH tools, or adjacent DLLs."""
import argparse
import ctypes
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
import wave


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('stage', type=Path)
    args = parser.parse_args()
    if os.name != 'nt':
        parser.error('Run with native Windows Python, outside the MSYS2 shell')
    # Inherited by children: missing DLLs must fail, never wait on a dialog.
    ctypes.windll.kernel32.SetErrorMode(0x0001 | 0x0002 | 0x8000)
    stage = args.stage.resolve()
    system = Path(os.environ['SystemRoot'])
    env = dict(os.environ, PATH=os.pathsep.join([str(system / 'System32'), str(system)]))
    with tempfile.TemporaryDirectory(prefix='align-sidecars-') as temporary:
        root = Path(temporary)
        for tool in ['ffmpeg', 'ffprobe']:
            shutil.copy2(stage / f'{tool}.exe', root)
        samples = [0, 8192, -8192, 16384] * 2000
        source = root / 'input.wav'
        with wave.open(str(source), 'wb') as writer:
            writer.setparams((1, 2, 8000, 0, 'NONE', 'not compressed'))
            writer.writeframes(struct.pack(f'<{len(samples)}h', *samples))

        def run(tool, *arguments):
            process = subprocess.run([str(root / f'{tool}.exe'), *arguments],
                                     cwd=root, env=env, capture_output=True, timeout=30)
            if process.returncode:
                raise RuntimeError(f'{tool} exited {process.returncode:#x}: '
                                   f'{process.stderr.decode(errors="replace")}')
            return process.stdout

        report = json.loads(run('ffprobe', '-v', 'error', '-show_streams',
                                '-of', 'json', str(source)))
        assert report['streams'][0]['sample_rate'] == '8000', report
        decoded = run('ffmpeg', '-v', 'error', '-i', str(source),
                      '-f', 'f32le', '-acodec', 'pcm_f32le', 'pipe:1')
        expected = [sample / 32768 for sample in samples]
        assert list(struct.unpack(f'<{len(samples)}f', decoded)) == expected
    print('Windows sidecars: probe and PCM decoding passed without MSYS2 or DLLs')


if __name__ == '__main__':
    main()
