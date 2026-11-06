#!/usr/bin/env python3
"""Paired macOS desktop measurements. Both apps must use the same release profile.

Idle samples exclude warmup. CPU is deltas of process CPU time, 100% = one core.
RAM is physical footprint, not virtual size or an arbitrary Activity Monitor column.
Raw samples and all repeats are retained. This does not measure GPU utilization.
Do not run builds, screen recordings or GPU traces during the timed comparison.
"""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import time


class Usage(ctypes.Structure):
    _fields_ = [('uuid', ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in (
        'user system package_wakeups interrupt_wakeups pageins wired resident footprint '
        'start exit child_user child_system child_package child_interrupt child_pageins '
        'child_elapsed read written qos_default qos_maintenance qos_background qos_utility '
        'qos_legacy qos_initiated qos_interactive billed_system serviced_system logical_writes '
        'peak instructions cycles billed_energy serviced_energy interval_peak runnable'
    ).split()]


class Timebase(ctypes.Structure):
    _fields_ = [('numer', ctypes.c_uint32), ('denom', ctypes.c_uint32)]


lib = ctypes.CDLL('/usr/lib/libSystem.B.dylib') if platform.system() == 'Darwin' else None


def snapshot(pid):
    usage = Usage()
    if lib.proc_pid_rusage(pid, 4, ctypes.byref(usage)):
        raise RuntimeError(f'Cannot read process {pid}')
    base = Timebase()
    lib.mach_timebase_info(ctypes.byref(base))
    return dict(time=time.monotonic(), cpu_s=(usage.user + usage.system) * base.numer / base.denom / 1e9,
                footprint_mib=usage.footprint / 2**20, rss_mib=usage.resident / 2**20,
                peak_mib=usage.peak / 2**20, wakeups=usage.package_wakeups + usage.interrupt_wakeups)


def measure(binary, scenario, args, run, label):
    env = {k: v for k, v in os.environ.items() if not k.startswith('ALIGN_EXPERIMENT_')}
    env['ALIGN_NO_UPDATE_CHECK'] = '1'
    prefix = args.out / f'{scenario}-{run}-{label}'
    if scenario in ('loaded', 'zoom'):
        env['ALIGN_EXPERIMENT_RESULT'] = str(args.result.resolve())
    if scenario == 'zoom':
        env['ALIGN_EXPERIMENT_ANIMATE'] = '1'
    saved = prefix.with_suffix('.result.json')
    if scenario == 'sync':
        env.update(ALIGN_EXPERIMENT_INPUT=str(args.media.resolve()), ALIGN_EXPERIMENT_SYNC='1',
                   ALIGN_EXPERIMENT_CACHE=str(prefix.with_suffix('.cache').resolve()),
                   ALIGN_EXPERIMENT_SAVE=str(saved.resolve()))
    with prefix.with_suffix('.log').open('w') as log:
        start = time.monotonic()
        process = subprocess.Popen([str(binary.resolve())], env=env, stdout=log, stderr=log)
        try:
            samples = []
            if scenario != 'sync':
                time.sleep(args.warmup)
            while True:
                if process.poll() is not None:
                    raise RuntimeError(f'{label} exited ({process.returncode}); see {log.name}')
                samples.append(snapshot(process.pid))
                elapsed = time.monotonic() - start
                if scenario == 'sync':
                    if saved.exists():
                        break
                    if elapsed > args.timeout:
                        raise RuntimeError(f'{label} sync timed out')
                elif elapsed >= args.warmup + args.seconds:
                    break
                time.sleep(0.1 if scenario == 'sync' else 0.25)
            duration = samples[-1]['time'] - samples[0]['time']
            cpu_s = samples[-1]['cpu_s'] - samples[0]['cpu_s']
            result = dict(frontend=label, scenario=scenario, repeat=run, sample_seconds=duration,
                          cpu_percent=100 * cpu_s / duration, cpu_seconds=cpu_s,
                          footprint_mib=statistics.median(s['footprint_mib'] for s in samples),
                          peak_mib=max(s['peak_mib'] for s in samples),
                          rss_mib=statistics.median(s['rss_mib'] for s in samples),
                          wakeups_per_s=(samples[-1]['wakeups']-samples[0]['wakeups']) / duration,
                          samples=samples)
            if scenario == 'sync':
                result['wall_seconds'] = time.monotonic() - start
                synced = json.loads(saved.read_text())
                result['clips'] = len(synced['project']['clips'])
                result['groups'] = len(synced['islands'])
                result['unmatched'] = len(synced['unmatched'])
            return result
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--gpui', type=Path, default=Path('target/release/align'))
    parser.add_argument('--rune', type=Path, default=Path('target/release/align-rune'))
    parser.add_argument('--result', type=Path, required=True)
    parser.add_argument('--media', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--repeat', type=int, default=3)
    parser.add_argument('--seconds', type=float, default=15)
    parser.add_argument('--warmup', type=float, default=5)
    parser.add_argument('--timeout', type=float, default=180)
    parser.add_argument('--scenarios', nargs='+', choices=['empty', 'loaded', 'zoom', 'sync'], default=['empty', 'loaded', 'zoom', 'sync'])
    args = parser.parse_args()
    if lib is None:
        parser.error('This sampler requires macOS (libproc); no cross-platform numbers are fabricated.')
    args.out.mkdir(parents=True, exist_ok=False)
    binaries = [('gpui', args.gpui), ('rune', args.rune)]
    report = dict(os=platform.platform(), architecture=platform.machine(),
                  rust=subprocess.check_output(['rustc', '--version'], text=True).strip(),
                  commit=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                  dirty=bool(subprocess.check_output(['git', 'status', '--porcelain'], text=True).strip()),
                  method=dict(cpu='libproc CPU deltas; 100% = one core', ram='physical footprint MiB',
                              gpu='not measured', warmup=args.warmup, seconds=args.seconds,
                              result_sha256=hashlib.sha256(args.result.read_bytes()).hexdigest()),
                  binaries={label: dict(bytes=path.stat().st_size, sha256=hashlib.sha256(path.read_bytes()).hexdigest()) for label, path in binaries},
                  runs=[])
    for scenario in args.scenarios:
        for run in range(args.repeat):
            for label, binary in (binaries if run % 2 == 0 else list(reversed(binaries))):
                result = measure(binary, scenario, args, run, label)
                report['runs'].append(result)
                (args.out / 'measurements.json').write_text(json.dumps(report, indent=2))
                print(json.dumps({k: v for k, v in result.items() if k != 'samples'}), flush=True)
    report['summary'] = []
    for scenario in args.scenarios:
        for label, _ in binaries:
            group = [r for r in report['runs'] if r['scenario'] == scenario and r['frontend'] == label]
            keys = ['cpu_percent', 'footprint_mib', 'peak_mib', 'wakeups_per_s'] + (['wall_seconds', 'cpu_seconds'] if scenario == 'sync' else [])
            report['summary'].append(dict(scenario=scenario, frontend=label,
                                         **{k: statistics.median(r[k] for r in group) for k in keys}))
    (args.out / 'measurements.json').write_text(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
