#!/usr/bin/env python3
"""Check camera-clock placement end to end with an installed CLI.

Generates a recorder and Sony-style camera files whose recording times
come from a known clock, synchronizes them, and exports with and without
--camera-clock. The camera clock runs 1000.4 s ahead of the recorder and
writes whole seconds, as cameras do, so the estimate carries real rounding
error. Checks that only the estimated clip moves and keeps its marker, and
that export, export-json, exceptions and Remove agree. Retains evidence.
"""
import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import random
import struct
import subprocess
import urllib.parse
import wave
import xml.etree.ElementTree as ElementTree

RATE = 48000
FPS = 25
CLOCK_AHEAD = 1000.4
EPOCH = datetime.datetime(2025, 3, 4, 10, 0, tzinfo=datetime.timezone.utc)
SERIAL = '4711042'
# name, true start on the recorder, duration, shares the recorder's sound
CAMERA = [
    ('C1', 5.0, 10.0, True),
    ('C2', 25.3, 10.0, True),
    ('C3', 45.6, 12.0, False),
    ('C4', 70.1, 10.0, True),
    ('C6', 88.0, 8.0, False),
]


def write_wav(path, frames):
    with wave.open(str(path), 'wb') as writer:
        writer.setparams((2, 2, RATE, 0, 'NONE', 'not compressed'))
        writer.writeframes(b''.join(struct.pack('<hh', v, -v) for v in frames))


def noise(seed, count):
    rng = random.Random(seed)
    return [rng.randrange(-16000, 16001) for _ in range(count)]


def sony_box(created, duration):
    xml = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<NonRealTimeMeta xmlns="urn:schemas-professionalDisc:nonRealTimeMeta:ver.2.00">\n'
        f'  <Duration value="{round(duration * FPS)}"/>\n'
        f'  <CreationDate value="{created.isoformat()}"/>\n'
        f'  <Device manufacturer="Sony" modelName="ILME-FX3" serialNo="{SERIAL}"/>\n'
        '</NonRealTimeMeta>\n'
    ).encode()
    return struct.pack('>I', 8 + len(xml)) + b'free' + xml


def camera_file(media, scratch, name, start, duration, shared, recorder):
    count = round(duration * RATE)
    if shared:
        offset = round(start * RATE)
        frames = recorder[offset:offset + count]
    else:
        frames = noise(sum(name.encode()) * 7919, count)
    sound = scratch / f'{name}.wav'
    write_wav(sound, frames)
    path = media / f'{name}.MOV'
    subprocess.run(['ffmpeg', '-v', 'error', '-y', '-f', 'lavfi', '-i',
        f'color=c=gray:s=64x64:r={FPS}:d={duration}', '-i', str(sound),
        '-map', '0:v', '-map', '1:a', '-c:v', 'mpeg4', '-c:a', 'pcm_s16le',
        '-shortest', str(path)], check=True, timeout=60)
    # Whole seconds of a clock 1000.4 s ahead, in a box readers skip.
    created = EPOCH + datetime.timedelta(seconds=math.floor(start + CLOCK_AHEAD))
    with path.open('ab') as file:
        file.write(sony_box(created, duration))
    return path


def premiere_clips(path):
    """(file, kind) -> sorted [(track, start frame)] and names by file."""
    root = ElementTree.parse(path).getroot()
    sequence = next(s for s in root.iter('sequence') if s.get('id') == 'sequence-1')
    files = {}
    for file in sequence.iter('file'):
        url = file.findtext('pathurl')
        if url:
            files[file.get('id')] = Path(urllib.parse.unquote(urllib.parse.urlparse(url).path)).name
    clips, names = {}, {}
    media = sequence.find('media')
    for kind in ('video', 'audio'):
        for index, track in enumerate(media.find(kind).findall('track')):
            for item in track.findall('clipitem'):
                file = files[item.find('file').get('id')]
                clips.setdefault((file, kind), []).append((index, int(item.findtext('start'))))
                names[file] = item.findtext('name')
    return {key: sorted(value) for key, value in clips.items()}, names


def otio_clips(path):
    data = json.loads(Path(path).read_text(encoding='utf-8'))
    clips = {}
    numbers = {}
    for track in data['tracks']['children']:
        index = numbers.setdefault(track['kind'], 0)
        numbers[track['kind']] += 1
        at = 0.0
        for child in track['children']:
            duration = child['source_range']['duration']
            if child['OTIO_SCHEMA'] == 'Clip.2':
                file = child['media_references']['DEFAULT_MEDIA']['name']
                clips.setdefault((file, track['kind']), []).append((index, round(at, 4)))
            at += duration['value'] / duration['rate']
    return {key: sorted(value) for key, value in clips.items()}


def aaf_clips(cli, path, env, log):
    """(file, kind) -> sorted [(track, start s)], read back by Align's AAF import."""
    proc = subprocess.run([str(cli), str(path)], env=env, capture_output=True, text=True,
        encoding='utf-8', timeout=300)
    log.write_text(proc.stderr, encoding='utf-8')
    if proc.returncode:
        raise RuntimeError(f'AAF readback exited {proc.returncode}: {proc.stderr[-2000:]}')
    project = json.loads(proc.stdout)
    names = {clip['id']: Path(clip['url']).name for clip in project['clips']}
    clips = {}
    for edit in project['importedTimeline']['edits']:
        start = edit['timelineStart']
        clips.setdefault((names[edit['clipID']], edit['mediaType']), []).append(
            (edit['trackIndex'], start['value'] / start['timescale']))
    return {key: sorted(value) for key, value in clips.items()}


def without(clips, name):
    return {key: value for key, value in clips.items() if key[0] != name}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('cli', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    cli = args.cli.absolute()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    media, scratch = output / 'media', output / 'scratch'
    media.mkdir()
    scratch.mkdir()
    recorder = noise(19780713, 100 * RATE)
    write_wav(media / 'R.WAV', recorder)
    for name, start, duration, shared in CAMERA:
        camera_file(media, scratch, name, start, duration, shared, recorder)
    env = dict(os.environ, ALIGN_BACKEND='portable')

    def run(arguments, name):
        proc = subprocess.run([str(cli), *map(str, arguments)], env=env, capture_output=True,
            text=True, encoding='utf-8', timeout=300)
        (output / f'{name}.stderr').write_text(proc.stderr, encoding='utf-8')
        (output / f'{name}.json').write_text(proc.stdout, encoding='utf-8')
        if proc.returncode:
            raise RuntimeError(f'{name}: CLI exited {proc.returncode}: {proc.stderr[-2000:]}')
        return json.loads(proc.stdout), proc.stderr

    saved = output / 'result.json'
    summary, _ = run(['sync', '--summary', '--output-json', saved, media], 'sync')
    sequence, = summary['sequences']
    assert sorted(sequence['unmatched']) == ['C3.MOV', 'C6.MOV'], sequence
    clock = sequence['cameraClock']
    estimate, = clock['estimates']
    assert estimate['clip'] == 'C3.MOV' and estimate['between'] == ['C2.MOV', 'C4.MOV'], clock
    refusal, = clock['refusals']
    assert refusal['clip'] == 'C6.MOV' and 'after it' in refusal['reason'], refusal

    def export(name, *flags, fresh=False):
        target = output / name
        if fresh:
            artifacts, stderr = run(['export', '--no-drift', *flags, target, media], name)
        else:
            artifacts, stderr = run(['export-json', '--no-drift', *flags, saved, target], name)
        urls = {item['format']: item['url'] for item in artifacts}
        return urls, stderr

    ordinary, _ = export('ordinary')
    placed, stderr = export('camera-clock', '--camera-clock')
    assert 'camera clock: C3.MOV placed.' in stderr, stderr
    assert 'camera clock: C6.MOV stays unmatched.' in stderr, stderr
    fresh, _ = export('camera-clock-fresh', '--camera-clock', fresh=True)
    excepted, stderr = export('camera-clock-except', '--camera-clock',
        '--camera-clock-except', 'C3.MOV')
    assert '(--camera-clock-except)' in stderr, stderr
    removed, stderr = export('camera-clock-remove', '--camera-clock', '--unmatched', 'remove')
    assert '--unmatched remove leaves out 1 clip' in stderr, stderr

    before, _ = premiere_clips(ordinary['premiereXML'])
    after, names = premiere_clips(placed['premiereXML'])
    assert without(before, 'C3.MOV') == without(after, 'C3.MOV'), 'another clip moved'
    assert names['C3.MOV'] == '[CAMERA CLOCK] C3.MOV', names
    # The recorder starts at its own time zero, the clock of the truth.
    origin = after[('R.WAV', 'audio')][0][1]
    estimated = (after[('C3.MOV', 'video')][0][1] - origin) / FPS
    truth = 45.6
    error = estimated - truth
    assert abs(error) <= estimate['toleranceSeconds'] + 0.5 / FPS, (estimated, estimate)
    c1_track = after[('C1.MOV', 'video')][0][0]
    assert after[('C3.MOV', 'video')][0][0] == c1_track, 'C3 left its camera track'
    assert Path(fresh['premiereXML']).read_text() == Path(placed['premiereXML']).read_text(), \
        'export and export-json placed differently'
    assert premiere_clips(excepted['premiereXML'])[0] == before, 'exception still placed C3'
    removed_clips, _ = premiere_clips(removed['premiereXML'])
    assert not any(file == 'C3.MOV' for file, _ in removed_clips), 'Remove kept C3'

    otio_before = otio_clips(ordinary['resolveOTIO'])
    otio_after = otio_clips(placed['resolveOTIO'])
    assert without(otio_before, 'C3.MOV') == without(otio_after, 'C3.MOV'), 'OTIO clip moved'
    otio_start = otio_after[('C3.MOV', 'Video')][0][1] - otio_after[('R.WAV', 'Audio')][0][1]
    assert abs(otio_start - estimated) <= 1 / FPS, (otio_start, estimated)

    aaf_ordinary, _ = export('aaf', '--aaf')
    aaf_placed, _ = export('aaf-camera-clock', '--aaf', '--camera-clock')
    aaf_before = aaf_clips(cli, aaf_ordinary['aaf'], env, output / 'aaf-readback.stderr')
    aaf_after = aaf_clips(cli, aaf_placed['aaf'], env, output / 'aaf-camera-clock-readback.stderr')
    assert without(aaf_before, 'C3.MOV') == without(aaf_after, 'C3.MOV'), 'AAF clip moved'
    aaf_origin = min(start for (file, _), value in aaf_after.items() if file.startswith('R')
        for _, start in value)
    aaf_start = aaf_after[('C3.MOV', 'video')][0][1] - aaf_origin
    assert abs(aaf_start - estimated) <= 1 / FPS, (aaf_start, estimated)

    report = {'cli': str(cli), 'sha256': hashlib.sha256(cli.read_bytes()).hexdigest(),
        'synthetic': True, 'clockAheadSeconds': CLOCK_AHEAD,
        'estimate': estimate, 'trueStartSeconds': truth,
        'exportedStartSeconds': estimated, 'errorSeconds': round(error, 4),
        'refusal': refusal}
    (output / 'verification.json').write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
