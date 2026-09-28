#!/usr/bin/env python3
"""Reproducible synchronization benchmark with independent ground truth.

Builds a multicamera corpus (MOV/MP4 cameras with PCM or AAC scratch audio,
QuickTime timecode, and BWF recorder files), then runs the CLI with a fresh
cache (cold) and again with the populated cache (warm). The report records:

- wall time per run (median of --repeat runs);
- how many ffmpeg/ffprobe processes the CLI started (wrapper-counted);
- per-clip offset error against the generated ground truth;
- islands/unmatched counts and the size of the CLI binary.

Only fixture generation needs a full FFmpeg on PATH. Usage:
  python script/bench-sync.py target/release/align-cli bench-out [--cameras 6]
"""
import argparse
import json
import os
from pathlib import Path
import platform
import random
import shutil
import statistics
import struct
import subprocess
import sys
import time
import wave

RATE = 48000


def seconds(value):
    return value['value'] / value['timescale']


def scene(duration, seed):
    """Speech-like material: band-limited noise bursts with pauses."""
    rng = random.Random(seed)
    out = []
    level = 0.0
    low = 0.0
    while len(out) < duration * RATE:
        burst = int(rng.uniform(0.08, 0.6) * RATE)
        gain = rng.choice([0.0, 0.25, 0.5, 0.8])
        for _ in range(burst):
            white = rng.uniform(-1, 1)
            low += 0.18 * (white - low)
            level += 0.002 * (gain - level)
            out.append(low * level * 2.5)
    return out[: duration * RATE]


def write_wav(path, samples, noise_seed):
    rng = random.Random(noise_seed)
    with wave.open(str(path), 'wb') as writer:
        writer.setparams((1, 2, RATE, 0, 'NONE', 'not compressed'))
        frames = bytearray()
        for sample in samples:
            value = sample + rng.uniform(-0.01, 0.01)
            frames += struct.pack('<h', max(-32767, min(32767, int(value * 32767))))
        writer.writeframes(bytes(frames))


def ffmpeg(*arguments):
    subprocess.run(['ffmpeg', '-v', 'error', '-y', *map(str, arguments)], check=True)


def build_corpus(root, cameras, duration):
    media = root / 'media'
    media.mkdir(parents=True)
    body = scene(duration, 20240611)
    truth = {}
    # Two recorder files: a continuous take split in the middle.
    half = duration // 2
    for index, (start, end) in enumerate([(0, half + 5), (half - 5, duration)]):
        name = f'recorder-{index + 1}.wav'
        write_wav(media / name, body[start * RATE:end * RATE], 100 + index)
        truth[name] = start
    rng = random.Random(7)
    for index in range(cameras):
        length = rng.randint(25, 60)
        start = rng.uniform(1, duration - length - 1)
        stem = f'camera-{index + 1}'
        raw = root / f'{stem}.wav'
        first = int(start * RATE)
        write_wav(raw, body[first:first + length * RATE], 200 + index)
        codec, container = [('pcm_s16le', 'mov'), ('aac', 'mp4'), ('pcm_s24le', 'mov')][index % 3]
        name = f'{stem}.{container}'
        timecode = f'01:{index:02d}:00:00'
        ffmpeg('-f', 'lavfi', '-i', f'testsrc2=s=320x180:r=25:d={length}', '-i', raw,
               '-map', '0:v', '-map', '1:a', '-c:v', 'libx264', '-preset', 'ultrafast',
               '-g', '25', '-bf', '2', '-pix_fmt', 'yuv420p', '-c:a', codec,
               '-timecode', timecode, '-shortest', media / name)
        raw.unlink()
        truth[name] = first / RATE
    return media, truth


def counting_tools(root):
    """Wrappers that log each ffmpeg/ffprobe start, then exec the real tool."""
    tools = root / 'tools'
    tools.mkdir()
    log = root / 'spawns.log'
    env = {}
    for tool in ('ffmpeg', 'ffprobe'):
        real = shutil.which(tool)
        if real is None:
            continue
        wrapper = tools / tool
        wrapper.write_text(f'#!/bin/sh\necho {tool} >> "{log}"\nexec "{real}" "$@"\n')
        wrapper.chmod(0o755)
        env[f'ALIGN_{tool.upper()}'] = str(wrapper)
    return env, log


def run(cli, media, env, cache):
    # Isolated cache and settings: XDG on Linux, HOME-relative elsewhere.
    environment = dict(os.environ, **env, XDG_CACHE_HOME=str(cache),
                       XDG_CONFIG_HOME=str(cache / 'config'), ALIGN_BACKEND='portable')
    if platform.system() == 'Darwin':
        environment['HOME'] = str(cache)
    started = time.perf_counter()
    proc = subprocess.run([str(cli), 'sync', str(media)], env=environment,
                          capture_output=True, text=True, timeout=1800)
    elapsed = time.perf_counter() - started
    if proc.returncode:
        raise RuntimeError(f'CLI exited {proc.returncode}: {proc.stderr[-2000:]}')
    return elapsed, json.loads(proc.stdout)


def offset_errors(result, truth):
    names = {clip['id']: Path(clip['url']).name for clip in result['project']['clips']}
    errors = {}
    for island in result['islands']:
        placed = {}
        for placement in island['placements']:
            first = placement['mapping']['points'][0]
            placed[names[placement['clipID']]] = seconds(first['island']) - seconds(first['source'])
        reference = 'recorder-1.wav' if 'recorder-1.wav' in placed else next(iter(placed))
        for name, position in placed.items():
            expected = truth[name] - truth[reference]
            errors[name] = abs(position - placed[reference] - expected)
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('cli', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--cameras', type=int, default=6)
    parser.add_argument('--duration', type=int, default=240)
    parser.add_argument('--repeat', type=int, default=3)
    args = parser.parse_args()
    if shutil.which('ffmpeg') is None:
        parser.error('ffmpeg must be on PATH to generate the corpus')
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    cli = args.cli.resolve()
    media, truth = build_corpus(root, args.cameras, args.duration)
    env, log = counting_tools(root)

    def measure(label, cache, fresh):
        times, spawns = [], []
        result = None
        for attempt in range(args.repeat):
            if fresh and cache.exists():
                shutil.rmtree(cache)
            log.write_text('')
            elapsed, result = run(cli, media, env, cache)
            times.append(elapsed)
            spawns.append(log.read_text().split())
        counts = {tool: spawns[-1].count(tool) for tool in ('ffmpeg', 'ffprobe')}
        print(f'{label}: {statistics.median(times):.3f}s, spawns {counts}', flush=True)
        return {'medianSeconds': round(statistics.median(times), 4),
                'runs': [round(t, 4) for t in times], 'spawns': counts}, result

    cold, result = measure('cold', root / 'cache-cold', True)
    warm_cache = root / 'cache-warm'
    run(cli, media, env, warm_cache)
    warm, _ = measure('warm', warm_cache, False)
    errors = offset_errors(result, truth)
    report = {
        'platform': platform.platform(),
        'cliBytes': cli.stat().st_size,
        'cameras': args.cameras,
        'durationSeconds': args.duration,
        'cold': cold,
        'warm': warm,
        'islands': len(result['islands']),
        'unmatched': len(result['unmatched']),
        'maxOffsetErrorMs': round(max(errors.values(), default=float('nan')) * 1000, 4),
        'offsetErrorMs': {k: round(v * 1000, 4) for k, v in sorted(errors.items())},
    }
    (root / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))
    return 0


if __name__ == '__main__':
    sys.exit(main())
