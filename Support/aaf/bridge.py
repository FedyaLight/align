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
from fractions import Fraction
from urllib.parse import urlparse, unquote

import aaf2


def integer(value, name, minimum=0):
    if type(value) is not int or value < minimum:
        raise ValueError(f'{name} must be an integer >= {minimum}')
    return value


def validate_wave(path, rate, frames):
    """Validate the real mono PCM/float source without decoding its samples."""
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
        if encoding not in (1, 3) or channels != 1 or actual_rate != rate:
            raise ValueError('WAV encoding/channels/rate differ from AAF manifest')
        if bits not in (16, 24, 32) or (encoding == 3 and bits != 32):
            raise ValueError('Unsupported WAV sample format')
        if align != bits // 8 or byte_rate != rate * align:
            raise ValueError('Invalid WAV block alignment')
        if data_size % align or data_size // align != frames:
            raise ValueError('WAV frame count differs from AAF manifest')


def write_audio(document, destination):
    if document.get('version') not in (1, 2):
        raise ValueError('Unsupported AAF bridge protocol')
    tracks = document['tracks']
    picture_tracks = document.get('picture_tracks', []) if document['version'] == 2 else []
    if not tracks and not picture_tracks:
        raise ValueError('No tracks')
    destination = Path(destination).resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.align-aaf-', suffix='.aaf', dir=destination.parent)
    os.close(fd)
    try:
        with aaf2.open(temporary, 'w') as container:
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


def read_timeline(path, audio_only=False):
    """Read linked picture/sound edits; retain each track's rational clock."""
    with aaf2.open(str(path), 'r') as container:
        def resolve(clip, rate, visited, offset=0, length=None):
            length = int(clip.length) if length is None else length
            source_start = int(clip.start) + offset
            if source_start < 0 or length < 0:
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
            if source_rate != rate:
                raise ValueError('Mixed-rate AAF source chain is not implemented')
            if isinstance(mob, aaf2.mobs.SourceMob):
                descriptor = mob.descriptor
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
            segment = slot.segment
            parts = list(segment.components) if isinstance(segment, aaf2.components.Sequence) else [segment]
            output = []
            cursor = 0
            covered = 0
            for part in parts:
                part_length = int(part.length)
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
                if rate <= 0 or (slot.media_kind == 'Sound' and rate.denominator != 1):
                    raise ValueError('Unsupported AAF edit rate')
                segment = slot.segment
                parts = list(segment.components) if isinstance(segment, aaf2.components.Sequence) else [segment]
                clips = []
                cursor = 0
                for part in parts:
                    length = int(part.length)
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
                if audio_only:
                    tracks.append({'name': slot.name or '', 'sample_rate': int(rate), 'clips': clips})
                else:
                    tracks.append({'name': slot.name or '', 'media_kind': slot.media_kind.lower(),
                        'edit_rate': {'numerator': rate.numerator, 'denominator': rate.denominator},
                        'clips': clips})
            sequences.append({'name': composition.name, 'tracks': tracks})
        if not sequences:
            raise ValueError('AAF has no top-level composition')
        return {'version': 1 if audio_only else 2, 'sequences': sequences}


def main():
    if len(sys.argv) == 3 and sys.argv[1] == 'read-timeline':
        print(json.dumps(read_timeline(sys.argv[2])))
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
