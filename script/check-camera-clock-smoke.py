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
from fractions import Fraction
import os
from pathlib import Path
import random
import shutil
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


def sony_box(created, duration, serial=SERIAL):
    xml = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<NonRealTimeMeta xmlns="urn:schemas-professionalDisc:nonRealTimeMeta:ver.2.00">\n'
        f'  <Duration value="{round(duration * FPS)}"/>\n'
        f'  <CreationDate value="{created.isoformat()}"/>\n'
        f'  <Device manufacturer="Sony" modelName="ILME-FX3" serialNo="{serial}"/>\n'
        '</NonRealTimeMeta>\n'
    ).encode()
    return struct.pack('>I', 8 + len(xml)) + b'free' + xml


def camera_file(media, scratch, name, start, duration, shared, recorder, serial=SERIAL):
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
        file.write(sony_box(created, duration, serial))
    return path


def premiere_clips(path, sequence_id="sequence-1"):
    """(file, kind) -> sorted [(track, start frame)] and names by file."""
    root = ElementTree.parse(path).getroot()
    sequence = next(s for s in root.iter('sequence') if s.get('id') == sequence_id)
    files = {}
    for file in root.iter('file'):
        url = file.findtext('pathurl')
        if url:
            files[file.get('id')] = Path(urllib.parse.unquote(urllib.parse.urlparse(url).path)).name
    clips, names, positions = {}, {}, {}
    media = sequence.find('media')
    for kind in ('video', 'audio'):
        for index, track in enumerate(media.find(kind).findall('track')):
            for ordinal, item in enumerate(track.findall('clipitem'), 1):
                positions[item.get('id')] = (kind, index + 1, ordinal)
                file = files[item.find('file').get('id')]
                clips.setdefault((file, kind), []).append((index, int(item.findtext('start'))))
                names[file] = item.findtext('name')
    for link in sequence.iter('link'):
        target = positions[link.findtext('linkclipref')]
        assert target == (link.findtext('mediatype'), int(link.findtext('trackindex')),
            int(link.findtext('clipindex'))), (target, ElementTree.tostring(link))
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


def aaf_events(path):
    """Read event names from the referenced mobs and paths from the file descriptors."""
    import aaf2
    events = []
    with aaf2.open(path) as container:
        for slot in next(container.content.toplevel()).slots:
            layers = getattr(slot.segment, 'slots', [slot.segment])
            for layer in layers:
                for event in getattr(layer, 'components', []):
                    if isinstance(event, aaf2.components.SourceClip):
                        media = event.slot.segment
                        master = media.mob if isinstance(media, aaf2.components.SourceClip) else event.mob
                        source = master.slot_at(event.slot_id).segment.components[0].mob
                        uri = source.descriptor['Locator'].value[0]['URLString'].value
                        events.append((Path(urllib.parse.unquote(urllib.parse.urlparse(uri).path)),
                            event.mob.name, event.slot_id, master.name))
    return events


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

    # Remove common gaps closes the place C3 left: its twelve seconds and the
    # second after them, and nothing else moves.
    cut_before, _ = premiere_clips(export('cut-ordinary', '--cut-common-gaps')[0]['premiereXML'])
    cut_after, _ = premiere_clips(export('cut-camera-clock', '--camera-clock',
        '--cut-common-gaps')[0]['premiereXML'])
    closed = (12 + 1) * FPS
    for key, spans in without(cut_before, 'C3.MOV').items():
        # A start between two frames may round either way after the move.
        assert len(spans) == len(cut_after[key]) and all(
            t == u and abs(s - closed - v) <= 1
            for (t, s), (u, v) in zip(spans, cut_after[key])), ('place kept', key)
    cut_origin = cut_after[('R.WAV', 'audio')][0][1]
    assert abs((cut_after[('C3.MOV', 'video')][0][1] - cut_origin) / FPS - estimated) <= 1 / FPS

    otio_before = otio_clips(ordinary['resolveOTIO'])
    otio_after = otio_clips(placed['resolveOTIO'])
    assert without(otio_before, 'C3.MOV') == without(otio_after, 'C3.MOV'), 'OTIO clip moved'
    otio_start = otio_after[('C3.MOV', 'Video')][0][1] - otio_after[('R.WAV', 'Audio')][0][1]
    assert abs(otio_start - estimated) <= 1 / FPS, (otio_start, estimated)

    aaf_ordinary, _ = export('aaf', '--aaf')
    aaf_placed, _ = export('aaf-camera-clock', '--aaf', '--camera-clock',
        '--unmatched-symbol', 'Review', '--unmatched-symbol-suffix')
    aaf_before = aaf_clips(cli, aaf_ordinary['aaf'], env, output / 'aaf-readback.stderr')
    aaf_after = aaf_clips(cli, aaf_placed['aaf'], env, output / 'aaf-camera-clock-readback.stderr')
    assert without(aaf_before, 'C3.MOV') == without(aaf_after, 'C3.MOV'), 'AAF clip moved'
    aaf_origin = min(start for (file, _), value in aaf_after.items() if file.startswith('R')
        for _, start in value)
    aaf_start = aaf_after[('C3.MOV', 'video')][0][1] - aaf_origin
    assert abs(aaf_start - estimated) <= 1 / FPS, (aaf_start, estimated)

    source = media / 'C3.MOV'
    assert sorted((name, slot, master) for path, name, slot, master in aaf_events(aaf_placed['aaf'])
        if path.resolve() == source.resolve()) == [
            ('[CAMERA CLOCK] C3.MOV Review', slot, 'C3') for slot in (1, 2, 3)]

    zero = output / 'zero'
    zero.mkdir()
    shutil.copyfile(media / 'R.WAV', zero / 'R.WAV')
    for path in media.glob('*.MOV'):
        original = path.read_bytes()
        assert original.count(SERIAL.encode()) == 1
        (zero / path.name).write_bytes(original.replace(SERIAL.encode(), b'0000000'))
    zero_result = output / 'zero-result.json'
    zero_summary, _ = run(['sync', '--summary', '--output-json', zero_result, zero], 'zero-sync')
    assert not zero_summary['sequences'][0].get('cameraClock', {}).get('estimates')
    assert all(not c.get('sourceIdentifier') for c in json.loads(zero_result.read_text())['project']['clips'])

    for fmt, path, tracks in [
        ('xml', ordinary['premiereXML'], {'V1'}),
        ('fcpxml', ordinary['finalCutProXML'], {'V1', 'A1'}),
        ('aaf', aaf_ordinary['aaf'], {'V1', 'A3', 'A4'}),
    ]:
        imported, _ = run(['sync', '--summary', '--sequence', '1', '--output-json',
            output / f'import-{fmt}-result.json', path], 'import-' + fmt)
        hints = imported['sequences'][0]['cameraClock']['editHints']
        assert {h['track'] for h in hints if h['clip'] == 'C3.MOV'} == tracks, (fmt, hints)

    # Three card folders put C3's stereo pair before the mono recorder.
    shoot = output / 'crossing'
    for card in ('A', 'M', 'Z'):
        (shoot / card).mkdir(parents=True)
    for path in media.glob('*.MOV'):
        if path.stem != 'C6':
            shutil.copyfile(path, shoot / ('A' if path.stem == 'C3' else 'Z') / path.name)
    subprocess.run(['ffmpeg', '-v', 'error', '-i', str(media / 'R.WAV'),
        '-af', 'pan=mono|c0=c0', str(shoot / 'M/R.WAV')], check=True)
    camera_file(shoot / 'Z', scratch, 'B0', 45.0, 10.0, True, recorder, serial='223344')
    saved = output / 'crossing-result.json'
    run(['sync', '--summary', '--output-json', saved, shoot], 'crossing-sync')
    # Mixed metadata in a saved result isolates the writers' selection of
    # sequence defaults from how the solver uses timecode to find matches.
    project = json.loads(saved.read_text())
    for clip in project['project']['clips']:
        video = clip.get('video')
        if video:
            hour = 2 if Path(clip['url']).stem == 'C3' else 1
            if hour == 2:
                video.update(width=128, height=72)
            video['sourceTimecode'] = {'text': f'0{hour}:00:00:00', 'frameNumber': hour * 3600 * FPS,
                'frameDuration': {'value': 1, 'timescale': FPS}, 'dropFrame': False}
    saved.write_text(json.dumps(project))
    base, _ = export('crossing-ordinary', '--replaced-audio')
    moved, _ = export('crossing-placed', '--replaced-audio', '--camera-clock')
    for seq in ('sequence-1', 'sequence-2'):
        old, _ = premiere_clips(base['premiereXML'], seq)
        new, _ = premiere_clips(moved['premiereXML'], seq)
        assert without(old, 'C3.MOV') == without(new, 'C3.MOV'), (seq, old, new)
        for artifact in (base, moved):
            sequence = ElementTree.parse(artifact['premiereXML']).find(f".//sequence[@id='{seq}']")
            size = sequence.find('media/video/format/samplecharacteristics')
            assert (size.findtext('width'), size.findtext('height')) == ('128', '72')
            if artifact is moved:
                channels = [clip.findtext('sourcetrack/trackindex') for clip in
                    sequence.findall('media/audio/track/clipitem') if 'C3.MOV' in clip.findtext('name')]
                assert channels == ['1', '2'], (seq, channels)
    for artifact in (base, moved):
        xml = ElementTree.parse(artifact['finalCutProXML'])
        assert xml.find('.//project/sequence').get('tcStart') == '7200s'
        fmt = xml.find(".//format[@id='r_fmt_sequence']")
        assert (fmt.get('width'), fmt.get('height')) == ('128', '72')

    # Put C3 on the same card to exercise FCP's folder-based multicam angle.
    c3 = next(c for c in project['project']['clips'] if Path(c['url']).stem == 'C3')
    c3['url'] = str(shoot / 'Z/C3.MOV')
    shutil.copyfile(shoot / 'A/C3.MOV', c3['url'])
    saved.write_text(json.dumps(project))
    angles = []
    for label, flags in [('ordinary', []), ('placed', ['--camera-clock'])]:
        urls, _ = export('angles-' + label, *flags)
        xml = ElementTree.parse(urls['finalCutProXML'])
        items = {}
        for angle in xml.iter('mc-angle'):
            end = 0
            for clip in angle.findall('asset-clip'):
                name = clip.get('name').removeprefix('[CAMERA CLOCK] ')
                start = Fraction(clip.get('offset')[:-1])
                duration = Fraction(clip.get('duration')[:-1])
                assert start >= end, (angle.get('name'), name, start, end)
                end = start + duration
                items[name] = (angle.get('angleID'), start, duration)
        angles.append(items)
    assert angles[1].pop('C3.MOV')[2] == angles[0].pop('C3.MOV')[2] == 12
    assert angles[1] == angles[0]

    # Each camera keeps its recorded source frame despite the different
    # sequence origin. FCP angle starts count from the asset timecode.
    for clip in xml.findall('.//mc-angle/asset-clip'):
        if '.MOV' in clip.get('name'):
            hour = 2 if 'C3.MOV' in clip.get('name') else 1
            assert Fraction(clip.get('start')[:-1]) * FPS == hour * 3600 * FPS

    # Order-only puts the unmatched take last at 101..113 s.
    for label, flags in [('ordinary', []), ('placed', ['--camera-clock'])]:
        urls, _ = export('tail-' + label, '--unmatched', 'order-only', *flags)
        for fmt in ('premiereXML', 'resolveXML'):
            assert ElementTree.parse(urls[fmt]).findtext('.//sequence/duration') == '2825'
        assert ElementTree.parse(urls['finalCutProXML']).find('.//project/sequence').get('duration') == '113s'
        tracks = json.loads(Path(urls['resolveOTIO']).read_text())['tracks']['children']
        assert abs(max(sum(c['source_range']['duration']['value'] for c in t['children'])
            for t in tracks) - 2825) < 0.01
        for fps, expected in [('25', [2825, 2825]), ('29.97', [3387, 3386])]:
            aaf, _ = export(f'tail-aaf-{fps}-{label}', '--aaf', '--aaf-fps', fps,
                '--unmatched', 'order-only', *flags)
            import aaf2
            with aaf2.open(aaf['aaf']) as container:
                lengths = [s.segment.length for s in next(container.content.toplevel()).slots
                    if s.media_kind in ('Picture', 'Timecode')]
                assert lengths == expected, lengths

    trimmed, _ = export('tail-trimmed', '--camera-clock', '--unmatched', 'order-only',
        '--trim-ends', '1')
    assert ElementTree.parse(trimmed['premiereXML']).findtext('.//sequence/duration') == '2475'

    report = {'cli': str(cli), 'sha256': hashlib.sha256(cli.read_bytes()).hexdigest(),
        'synthetic': True, 'clockAheadSeconds': CLOCK_AHEAD,
        'estimate': estimate, 'trueStartSeconds': truth,
        'exportedStartSeconds': estimated, 'errorSeconds': round(error, 4),
        'refusal': refusal}
    (output / 'verification.json').write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
