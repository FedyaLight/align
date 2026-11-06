#!/usr/bin/env python3
"""Measure per-process GPU active intervals with Xcode Metal System Trace on macOS.

Separate from CPU/RAM runs because Instruments adds overhead. GPU busy fraction
is the union of Active vertex/fragment/compute intervals, not percent of all GPU
cores or power. Excludes WindowServer. Keep raw traces locally; they include
system metadata and must not be committed.
"""
import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import time
import xml.etree.ElementTree as ET


def parse(path, pid, seconds):
    root = ET.parse(path).getroot()
    ids = {e.get('id'): e for e in root.iter() if e.get('id')}
    def resolve(e):
        while e is not None and e.get('ref'):
            e = ids[e.get('ref')]
        return e
    def number(e):
        return int(resolve(e).text)
    # Ignore the first and last second of the recording to avoid trace edges.
    start, end = 1e9, (seconds - 1) * 1e9
    intervals = []
    channels = {}
    command_buffers = set()
    for row in root.findall('.//row'):
        cols = list(row)
        process = resolve(cols[10])
        p = resolve(process.find('pid'))
        if p is None or int(p.text) != pid or resolve(cols[7]).text != 'Active' or number(cols[5]) != 0:
            continue
        a = number(cols[0]); b = a + number(cols[1])
        a, b = max(a, start), min(b, end)
        if b <= a:
            continue
        command_buffers.add(number(cols[15]))
        channel = resolve(cols[2]).text
        channels[channel] = channels.get(channel, 0) + b - a
        intervals.append((a, b))
    merged = []
    for a, b in sorted(intervals):
        if merged and a <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], b)
        else:
            merged.append([a, b])
    active = sum(b-a for a, b in merged)
    return dict(window_seconds=(end-start)/1e9, gpu_active_ms_per_s=active/(end-start)*1000,
                gpu_active_percent=active/(end-start)*100, intervals=len(intervals),
                command_buffers_per_s=len(command_buffers)/((end-start)/1e9),
                channels_ms_per_s={k: v/(end-start)*1000 for k, v in channels.items()})


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--result', type=Path, required=True)
    p.add_argument('--gpui', type=Path, default=Path('target/release/align'))
    p.add_argument('--rune', type=Path, default=Path('target/release/align-rune'))
    p.add_argument('--out', type=Path, required=True)
    p.add_argument('--seconds', type=int, default=10)
    p.add_argument('--repeat', type=int, default=3)
    args = p.parse_args()
    if args.seconds < 4: p.error('--seconds must be at least 4')
    args.out.mkdir(parents=True, exist_ok=False)
    results = []
    binaries = [('gpui', args.gpui), ('rune', args.rune)]
    for scenario in ('loaded', 'zoom'):
        for run in range(args.repeat):
            for label, binary in (binaries if run % 2 == 0 else list(reversed(binaries))):
                prefix = args.out / f'{scenario}-{run}-{label}'
                env = {k: v for k,v in os.environ.items() if not k.startswith('ALIGN_EXPERIMENT_')}
                env.update(ALIGN_NO_UPDATE_CHECK='1', ALIGN_EXPERIMENT_RESULT=str(args.result.resolve()))
                if scenario == 'zoom': env['ALIGN_EXPERIMENT_ANIMATE'] = '1'
                with prefix.with_suffix('.log').open('w') as log:
                    process = subprocess.Popen([str(binary.resolve())], env=env, stdout=log, stderr=log)
                    try:
                        time.sleep(4)
                        if process.poll() is not None: raise RuntimeError(f'{label} exited')
                        trace = prefix.with_suffix('.trace')
                        subprocess.run(['xcrun','xctrace','record','--template','Metal System Trace','--attach',str(process.pid),'--time-limit',f'{args.seconds}s','--output',str(trace)], check=True, stdout=log, stderr=log)
                        xml = prefix.with_suffix('.xml')
                        subprocess.run(['xcrun','xctrace','export','--input',str(trace),'--xpath','/trace-toc/run[@number="1"]/data/table[@schema="metal-gpu-intervals"]','--output',str(xml)], check=True, stdout=log, stderr=log)
                        result = dict(frontend=label, scenario=scenario, repeat=run, **parse(xml, process.pid, args.seconds))
                        results.append(result)
                        (args.out/'measurements.json').write_text(json.dumps(results,indent=2))
                        print(json.dumps(result), flush=True)
                    finally:
                        process.terminate()
                        try: process.wait(timeout=5)
                        except subprocess.TimeoutExpired: process.kill(); process.wait()
    summary = []
    for scenario in ('loaded','zoom'):
        for label, _ in binaries:
            runs = [r for r in results if r['scenario']==scenario and r['frontend']==label]
            summary.append(dict(scenario=scenario, frontend=label, gpu_active_ms_per_s=statistics.median(r['gpu_active_ms_per_s'] for r in runs)))
    (args.out/'summary.json').write_text(json.dumps(summary,indent=2))


if __name__ == '__main__': main()
