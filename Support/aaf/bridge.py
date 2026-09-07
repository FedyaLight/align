"""AAF sidecar protocol. JSON input; no subprocesses or media decoding.

Media inspection and mono stem creation belong to Align's decoder. This
module writes their validated sample ranges to an AAF object graph.
"""
import json
import os
from pathlib import Path
import sys
import struct
import tempfile
import hashlib
import math
from fractions import Fraction
from urllib.parse import urlparse, unquote

import aaf2


def integer(value, name, minimum=0):
    if type(value) is not int or value < minimum:
        raise ValueError(f'{name} must be an integer >= {minimum}')
    return value


def validate_wave(path, rate, frames, expected_channels=1):
    """Validate the real PCM/float source without decoding its samples."""
    size = path.stat().st_size
    with path.open('rb') as stream:
        header = stream.read(12)
        if len(header) != 12 or header[:4] != b'RIFF' or header[8:] != b'WAVE':
            raise ValueError('AAF stem must be a RIFF WAVE file')
        limit = struct.unpack('<I', header[4:8])[0] + 8
        if limit > size:
            raise ValueError('Truncated WAV container')
        fmt = None
        data_size = None
        while stream.tell() + 8 <= limit:
            kind, length = struct.unpack('<4sI', stream.read(8))
            start = stream.tell()
            if start + length > limit:
                raise ValueError('Truncated WAV chunk')
            if kind == b'fmt ':
                if fmt is not None or length < 16:
                    raise ValueError('Invalid WAV format chunk')
                fmt = struct.unpack('<HHIIHH', stream.read(16))
            elif kind == b'data':
                if data_size is not None:
                    raise ValueError('Multiple WAV data chunks')
                data_size = length
            stream.seek(start + length + (length & 1))
        if fmt is None or data_size is None:
            raise ValueError('WAV is missing format or audio data')
        encoding, channels, actual_rate, byte_rate, align, bits = fmt
        if encoding not in (1, 3) or channels != expected_channels or actual_rate != rate:
            raise ValueError('WAV encoding/channels/rate differ from AAF manifest')
        if bits not in (16, 24, 32) or (encoding == 3 and bits != 32):
            raise ValueError('Unsupported WAV sample format')
        if align != channels * (bits // 8) or byte_rate != rate * align:
            raise ValueError('Invalid WAV block alignment')
        if data_size % align or data_size // align != frames:
            raise ValueError('WAV frame count differs from AAF manifest')


def write_composition(container, document):
    if document.get('version') not in (1, 2):
        raise ValueError('Unsupported AAF bridge protocol')
    tracks = document['tracks']
    picture_tracks = document.get('picture_tracks', []) if document['version'] == 2 else []
    if not tracks and not picture_tracks:
        raise ValueError('No tracks')
    composition = container.create.CompositionMob(document['name'])
    composition.usage = 'Usage_TopLevel'
    container.content.mobs.append(composition)
    if picture_tracks:
        clock = picture_tracks[0]['edit_rate']
        rate = Fraction(integer(clock['numerator'], 'numerator', 1),
            integer(clock['denominator'], 'denominator', 1))
        duration = max((Fraction(clip['start'] + clip['length']) /
            Fraction(track['edit_rate']['numerator'], track['edit_rate']['denominator'])
            for track in picture_tracks for clip in track['clips']), default=Fraction(0))
        frames = duration * rate
        timecode = composition.create_timeline_slot(str(rate))
        timecode.name = 'Timecode'
        timecode.segment = container.create.Timecode(fps=round(rate), drop=False,
            length=(frames.numerator + frames.denominator - 1) // frames.denominator)
        timecode.segment.start = 0
    for track in picture_tracks:
        numerator = integer(track['edit_rate']['numerator'], 'edit_rate numerator', 1)
        denominator = integer(track['edit_rate']['denominator'], 'edit_rate denominator', 1)
        rate = Fraction(numerator, denominator)
        slot = composition.create_timeline_slot(str(rate))
        slot.name = track['name']
        slot.segment = container.create.Sequence(media_kind='picture')
        cursor = 0
        for clip in track['clips']:
            path = Path(clip['path']).resolve(strict=True)
            start = integer(clip['start'], 'start')
            source_in = integer(clip['source_in'], 'source_in')
            length = integer(clip['length'], 'length', 1)
            master, _, _ = container.content.create_ama_link(str(path), clip['metadata'])
            sources = [source for source in master.slots if source.media_kind == 'Picture']
            if len(sources) != 1:
                raise ValueError('AAF picture requires one video stream')
            source = sources[0]
            if Fraction(str(source.edit_rate)) != rate:
                raise ValueError('AAF picture rate conversion is not implemented')
            available = sum(part.length for part in source.segment.components)
            if start < cursor or source_in + length > available:
                raise ValueError('Overlapping or out-of-bounds AAF picture clip')
            if start > cursor:
                slot.segment.components.append(container.create.Filler('picture', start - cursor))
            slot.segment.components.append(master.create_source_clip(
                slot_id=source.slot_id, start=source_in, length=length, media_kind='picture'))
            cursor = start + length
        slot.segment.length = cursor
    for track in tracks:
        rate = integer(track['sample_rate'], 'sample_rate', 1)
        slot = composition.create_sound_slot(edit_rate=rate)
        slot.name = track['name']
        cursor = 0
        for clip in track['clips']:
            path = Path(clip['path']).resolve(strict=True)
            start = integer(clip['start'], 'start')
            source_in = integer(clip['source_in'], 'source_in')
            length = integer(clip['length'], 'length', 1)
            frames = integer(clip['source_frames'], 'source_frames', 1)
            if start < cursor or source_in + length > frames:
                raise ValueError('Overlapping or out-of-bounds AAF clip')
            if clip['channels'] != 1:
                raise ValueError('AAF audio requires one lossless mono stem per channel')
            validate_wave(path, rate, frames)
            metadata = {'format': {'format_name': 'wav'}, 'streams': [{
                'codec_type': 'audio', 'sample_rate': str(rate),
                'duration_ts': frames, 'channels': 1,
            }]}
            master, _, _ = container.content.create_ama_link(str(path), metadata)
            if start > cursor:
                gap = container.create.Filler(media_kind='sound', length=start-cursor)
                slot.segment.components.append(gap)
            slot.segment.components.append(master.create_source_clip(
                slot_id=1, start=source_in, length=length, media_kind='sound'))
            cursor = start + length
        slot.segment.length = cursor


def write_audio(document, destination):
    documents = document.get('sequences') if document.get('version') == 3 else [document]
    if not isinstance(documents, list) or not documents:
        raise ValueError('AAF batch has no sequences')
    destination = Path(destination).resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.align-aaf-', suffix='.aaf', dir=destination.parent)
    os.close(fd)
    try:
        with aaf2.open(temporary, 'w') as container:
            for sequence in documents:
                write_composition(container, sequence)
        os.replace(temporary, destination)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def locator_path(url):
    parsed = urlparse(url)
    if parsed.scheme != 'file' or parsed.netloc not in ('', 'localhost'):
        raise ValueError('AAF source locator is not a local file')
    path = unquote(parsed.path)
    if len(path) >= 3 and path[0] == '/' and path[1].isalpha() and path[2] == ':':
        path = path[1:]
    if not path or '\x00' in path:
        raise ValueError('Invalid AAF media path')
    return path


def read_audio(path):
    """Compatibility entry point for audio-only compositions."""
    return read_timeline(path, audio_only=True)


def read_timeline(path, audio_only=False, extract_dir=None):
    """Read linked picture/sound edits; retain each track's rational clock."""
    with aaf2.open(str(path), 'r') as container:
        extracted = {}

        def embedded_audio(mob):
            if extract_dir is None:
                raise ValueError('Embedded AAF audio requires an extraction directory')
            if not isinstance(mob.descriptor, aaf2.essence.PCMDescriptor):
                raise ValueError('Unsupported embedded AAF audio encoding')
            descriptor = mob.descriptor
            rate = Fraction(str(descriptor['SampleRate'].value))
            if not 1 <= descriptor['Channels'].value <= 64 or rate <= 0 or rate.denominator != 1:
                raise ValueError('Unsupported embedded AAF channel layout or sample rate')
            key = str(mob.mob_id)
            if key not in extracted:
                directory = Path(extract_dir).resolve()
                directory.mkdir(parents=True, exist_ok=True)
                fd, temporary = tempfile.mkstemp(prefix='.aaf-audio-', suffix='.wav', dir=directory)
                os.close(fd)
                try:
                    mob.export_audio(temporary)
                    validate_wave(Path(temporary), int(rate), int(descriptor['Length'].value),
                                  int(descriptor['Channels'].value))
                    with open(temporary, 'rb') as stream:
                        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
                    destination = directory / (digest + '.wav')
                    os.replace(temporary, destination)
                    extracted[key] = str(destination)
                finally:
                    if os.path.exists(temporary):
                        os.unlink(temporary)
            return extracted[key]

        def selected(segment):
            seen = set()
            while isinstance(segment, aaf2.components.Selector):
                identity = id(segment)
                if identity in seen:
                    raise ValueError('Cyclic AAF selector')
                seen.add(identity)
                choice = segment['Selected'].value
                if choice is None or choice.media_kind != segment.media_kind or choice.length != segment.length:
                    raise ValueError('Invalid AAF selected segment')
                segment = choice
            return segment

        def components(segment):
            segment = selected(segment)
            if isinstance(segment, aaf2.components.Sequence):
                for part in segment.components:
                    yield from components(part)
            else:
                yield segment

        def resolve(clip, rate, visited, offset=0, length=None):
            length = Fraction(int(clip.length), 1) / rate if length is None else length
            if int(clip.start) < 0 or offset < 0 or length < 0:
                raise ValueError('Negative AAF source range')
            mob = clip.mob
            if mob is None:
                raise ValueError('AAF source reference is unresolved')
            key = (str(mob.mob_id), clip.slot_id)
            if key in visited:
                raise ValueError('Cyclic AAF source reference')
            slot = mob.slot_at(clip.slot_id)
            if slot.media_kind != clip.media_kind:
                raise ValueError('AAF source media kind mismatch')
            source_rate = Fraction(str(slot.edit_rate))
            if source_rate <= 0:
                raise ValueError('Invalid AAF source edit rate')
            source_start = Fraction(int(clip.start), 1) / source_rate + offset
            if isinstance(mob, aaf2.mobs.SourceMob):
                descriptor = mob.descriptor
                if slot.media_kind == 'Sound' and mob.essence is not None:
                    physical = slot['PhysicalTrackNumber'].value if 'PhysicalTrackNumber' in slot else None
                    channels = descriptor['Channels'].value if 'Channels' in descriptor else 0
                    if physical is None and channels == 1:
                        physical = 1
                    if physical is None or not 1 <= physical <= channels:
                        raise ValueError('Ambiguous or invalid embedded AAF physical channel')
                    return [(embedded_audio(mob), source_start, int(physical) - 1, length)]
                if descriptor is not None and 'Locator' in descriptor:
                    urls = [loc['URLString'].value for loc in descriptor['Locator'].value
                            if 'URLString' in loc]
                    if len(urls) != 1:
                        raise ValueError('AAF source requires one media locator')
                    physical = slot['PhysicalTrackNumber'].value if 'PhysicalTrackNumber' in slot else None
                    if slot.media_kind == 'Picture':
                        return [(locator_path(urls[0]), source_start, None, length)]
                    if physical is None:
                        sound_slots = [s for s in mob.slots if s.media_kind == 'Sound']
                        if len(sound_slots) != 1:
                            raise ValueError('Ambiguous AAF source channel')
                        physical = 1
                    if physical < 1:
                        raise ValueError('Invalid AAF physical channel')
                    return [(locator_path(urls[0]), source_start, int(physical) - 1, length)]
            parts = components(slot.segment)
            output = []
            cursor = 0
            covered = 0
            for part in parts:
                part = selected(part)
                part_length = Fraction(int(part.length), 1) / source_rate
                if part_length < 0:
                    raise ValueError('Negative AAF source component length')
                begin = max(source_start, cursor)
                end = min(source_start + length, cursor + part_length)
                if begin < end:
                    if isinstance(part, aaf2.components.SourceClip):
                        output.extend(resolve(part, source_rate, visited | {key}, begin - cursor, end - begin))
                    elif isinstance(part, aaf2.components.Filler):
                        output.append((None, 0, None, end - begin))
                    else:
                        raise ValueError('Unsupported AAF source component')
                    covered += end - begin
                cursor += part_length
            if covered != length:
                raise ValueError('AAF source reference exceeds its source sequence')
            return output

        sequences = []
        for composition in container.content.toplevel():
            tracks = []
            for slot in composition.slots:
                if slot.media_kind == 'Timecode' and isinstance(slot.segment, aaf2.components.Timecode):
                    continue
                if slot.media_kind not in ('Sound', 'Picture') or (audio_only and slot.media_kind != 'Sound'):
                    raise ValueError('Unsupported AAF track kind: ' + slot.media_kind)
                rate = Fraction(str(slot.edit_rate))
                if rate <= 0:
                    raise ValueError('Unsupported AAF edit rate')
                parts = components(slot.segment)
                clips = []
                cursor = Fraction(0)
                for part in parts:
                    part = selected(part)
                    length = Fraction(int(part.length), 1) / rate
                    if length < 0:
                        raise ValueError('Negative AAF component length')
                    if isinstance(part, aaf2.components.SourceClip):
                        position = cursor
                        for media, source_in, channel, span in resolve(part, rate, set()):
                            if media is not None and span:
                                clips.append({'path': media, 'start': position, 'source_in': source_in,
                                    'length': span, 'channel': channel})
                            position += span
                    elif not isinstance(part, aaf2.components.Filler):
                        raise ValueError('Unsupported AAF timeline component')
                    cursor += length
                # Keep the declared frame clock when it represents every boundary.
                # Otherwise use an exact shared tick clock, retaining the edit rate.
                times = [clip[field] for clip in clips for field in ('start', 'source_in', 'length')]
                clock = rate
                if any((value * rate).denominator != 1 for value in times):
                    clock = Fraction(math.lcm(rate.numerator, *(value.denominator for value in times)), 1)
                if audio_only and clock.denominator != 1:
                    clock = Fraction(clock.numerator, 1)
                if clock.numerator > 2147483647:
                    raise ValueError('AAF exact clock exceeds supported precision')
                for clip in clips:
                    for field in ('start', 'source_in', 'length'):
                        clip[field] = int(clip[field] * clock)
                if audio_only:
                    tracks.append({'name': slot.name or '', 'sample_rate': int(clock), 'clips': clips})
                else:
                    tracks.append({'name': slot.name or '', 'media_kind': slot.media_kind.lower(),
                        'edit_rate': {'numerator': rate.numerator, 'denominator': rate.denominator},
                        'time_rate': {'numerator': clock.numerator, 'denominator': clock.denominator},
                        'clips': clips})
            sequences.append({'name': composition.name, 'tracks': tracks})
        if not sequences:
            raise ValueError('AAF has no top-level composition')
        return {'version': 1 if audio_only else 3, 'sequences': sequences}


def main():
    if len(sys.argv) in (3, 4) and sys.argv[1] == 'read-timeline':
        print(json.dumps(read_timeline(sys.argv[2], extract_dir=sys.argv[3] if len(sys.argv) == 4 else None)))
        return
    if len(sys.argv) == 3 and sys.argv[1] == 'read-audio':
        print(json.dumps(read_audio(sys.argv[2])))
        return
    if len(sys.argv) != 4 or sys.argv[1] not in ('write-audio', 'write-timeline'):
        raise ValueError('Usage: align-aaf write-audio manifest.json destination.aaf')
    with open(sys.argv[2], encoding='utf-8') as stream:
        write_audio(json.load(stream), sys.argv[3])
    print(json.dumps({'version': 1, 'path': str(Path(sys.argv[3]).resolve())}))


if __name__ == '__main__':
    main()
