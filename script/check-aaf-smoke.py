#!/usr/bin/env python3
"""Check bundled AAF discovery using the release smoke's saved sync result."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import aaf2

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('cli', type=Path)
parser.add_argument('smoke', type=Path)
args = parser.parse_args()
env = dict(os.environ)
env.pop('ALIGN_AAF', None)
env['ALIGN_BACKEND'] = 'portable'
output = args.smoke.resolve() / 'aaf'
proc = subprocess.run([str(args.cli.resolve()), 'export-json', '--aaf',
    str(args.smoke.resolve() / 'sync.json'), str(output)],
    env=env, capture_output=True, text=True, encoding='utf-8', timeout=180)
output.mkdir(parents=True, exist_ok=True)
(output / 'stderr.txt').write_text(proc.stderr, encoding='utf-8')
if proc.returncode:
    raise RuntimeError(proc.stderr)
artifacts = json.loads(proc.stdout)
assert len(artifacts) == 1 and artifacts[0]['format'] == 'aaf'
with aaf2.open(artifacts[0]['url']) as container:
    compositions = list(container.content.toplevel())
    assert len(compositions) == 1
    slots = list(compositions[0].slots)
    assert len(slots) == 4, 'Two overlapping stereo recordings require four mono tracks'
    assert all(str(slot.edit_rate) == '48000' for slot in slots)
    assert sorted(slot.segment.length for slot in slots) == [672000, 672000, 768000, 768000]
(output / 'verification.json').write_text(json.dumps({'tracks':4,'passed':True},indent=2)+'\n')
