#!/usr/bin/env python3
"""Run the release acceptance gates, retain per-gate logs and one final report.

Requires the release CLI with adjacent AAF/FFmpeg sidecars, pyaaf2 and system
FFmpeg for fixture generation. This command never opens GUI or editor apps.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('cli', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    cli = args.cli.resolve()
    if not cli.is_file():
        parser.error(f'CLI not found: {cli}')
    for executable in ('ffmpeg', 'ffprobe'):
        if shutil.which(executable) is None:
            parser.error(f'{executable} must be on PATH for acceptance fixtures')
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    scripts = Path(__file__).resolve().parent
    smoke = root / 'release'
    gates = [
        ('release', 'check-release-smoke.py', [cli, smoke], None),
        ('preserve-editing', 'check-preserve-editing-smoke.py', ['--cli', cli, '--output', root / 'preserve-editing'], None),
        ('linked-aaf', 'check-aaf-smoke.py', [cli, smoke], 'release'),
        ('picture-aaf', 'check-aaf-picture-smoke.py', [cli, root / 'picture-aaf'], None),
        ('embedded-mono', 'check-aaf-embedded-smoke.py', [cli, root / 'embedded-mono'], None),
        ('embedded-stereo', 'check-aaf-embedded-smoke.py', [cli, root / 'embedded-stereo', '--channels', '2'], None),
        ('mixed-clocks', 'check-aaf-embedded-smoke.py', [cli, root / 'mixed-clocks', '--channels', '2', '--mixed-clocks'], None),
        ('result-lifecycle', 'check-result-lifecycle.py', [cli, root / 'result-lifecycle'], None),
    ]
    report = {'cli': str(cli), 'sha256': hashlib.sha256(cli.read_bytes()).hexdigest(),
        'python': sys.executable, 'ffmpeg': shutil.which('ffmpeg'),
        'ffprobe': shutil.which('ffprobe'), 'gates': {}}
    for name, script, arguments, dependency in gates:
        if dependency and report['gates'][dependency]['status'] != 'passed':
            report['gates'][name] = {'status': 'blocked', 'dependency': dependency}
            continue
        command = [sys.executable, str(scripts / script), *map(str, arguments)]
        started = time.monotonic()
        with (root / f'{name}.log').open('w', encoding='utf-8') as log:
            try:
                code = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, timeout=600).returncode
            except subprocess.TimeoutExpired:
                code = 124
                log.write('\nAcceptance gate timed out after 600 seconds.\n')
        status = 'passed' if code == 0 else 'failed'
        report['gates'][name] = {'status': status, 'exit_code': code,
            'seconds': round(time.monotonic() - started, 3), 'command': command}
        print(f'{name}: {status}', flush=True)
        # Partial reports remain useful if a later gate is interrupted.
        (root / 'verification.json').write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    report['passed'] = all(gate['status'] == 'passed' for gate in report['gates'].values())
    (root / 'verification.json').write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
