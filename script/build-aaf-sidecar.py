#!/usr/bin/env python3
"""Freeze the AAF bridge for the current OS; the delivered binary needs no Python.

Build environment: install Support/aaf/build-requirements.txt into a venv.
Usage: python script/build-aaf-sidecar.py OUTPUT_DIRECTORY
"""
import argparse
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='align-aaf-build-') as temporary:
        subprocess.run([
            sys.executable, '-m', 'PyInstaller', '--noconfirm', '--clean',
            '--onefile', '--name', 'align-aaf', '--distpath', str(output),
            '--workpath', str(Path(temporary) / 'work'), '--specpath', temporary,
            '--collect-all', 'aaf2', str(root / 'Support/aaf/bridge.py'),
        ], check=True)
    executable = output / ('align-aaf.exe' if sys.platform == 'win32' else 'align-aaf')
    if not executable.is_file():
        raise RuntimeError('AAF build did not produce an executable')
    print(executable)


if __name__ == '__main__':
    main()
