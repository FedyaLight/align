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
    if document.get('version') != 1:
        raise ValueError('Unsupported AAF bridge protocol')
    tracks = document['tracks']
    if not tracks:
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


def read_audio(path):
    """Read sound compositions; unsupported graph nodes fail explicitly."""
    with aaf2.open(str(path), 'r') as container:
        def resolve(clip, rate, visited):
            mob = clip.mob
            if mob is None:
                raise ValueError('AAF source reference is unresolved')
            key = (str(mob.mob_id), clip.slot_id)
            if key in visited:
                raise ValueError('Cyclic AAF source reference')
            slot = mob.slot_at(clip.slot_id)
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
                    parsed = urlparse(urls[0])
                    if parsed.scheme != 'file' or parsed.netloc not in ('', 'localhost'):
                        raise ValueError('AAF source locator is not a local file')
                    return unquote(parsed.path), int(clip.start)
            segment = slot.segment
            if isinstance(segment, aaf2.components.Sequence):
                parts = list(segment.components)
                if len(parts) != 1:
                    raise ValueError('Edited AAF source mob is not implemented')
                segment = parts[0]
            if not isinstance(segment, aaf2.components.SourceClip):
                raise ValueError('Unsupported AAF source component')
            media, source_in = resolve(segment, source_rate, visited | {key})
            return media, source_in + int(clip.start)

        sequences = []
        for composition in container.content.toplevel():
            tracks = []
            for slot in composition.slots:
                if slot.media_kind != 'Sound':
                    raise ValueError('Non-audio AAF slots are not implemented')
                rate = Fraction(str(slot.edit_rate))
                if rate.denominator != 1 or rate <= 0:
                    raise ValueError('Unsupported AAF sound edit rate')
                segment = slot.segment
                parts = list(segment.components) if isinstance(segment, aaf2.components.Sequence) else [segment]
                clips = []
                cursor = 0
                for part in parts:
                    length = int(part.length)
                    if length < 0:
                        raise ValueError('Negative AAF component length')
                    if isinstance(part, aaf2.components.SourceClip):
                        media, source_in = resolve(part, rate, set())
                        clips.append({'path': media, 'start': cursor, 'source_in': source_in, 'length': length})
                    elif not isinstance(part, aaf2.components.Filler):
                        raise ValueError('Unsupported AAF timeline component')
                    cursor += length
                tracks.append({'name': slot.name or '', 'sample_rate': int(rate), 'clips': clips})
            sequences.append({'name': composition.name, 'tracks': tracks})
        if not sequences:
            raise ValueError('AAF has no top-level composition')
        return {'version': 1, 'sequences': sequences}


def main():
    if len(sys.argv) == 3 and sys.argv[1] == 'read-audio':
        print(json.dumps(read_audio(sys.argv[2])))
        return
    if len(sys.argv) != 4 or sys.argv[1] != 'write-audio':
        raise ValueError('Usage: align-aaf write-audio manifest.json destination.aaf')
    with open(sys.argv[2], encoding='utf-8') as stream:
        write_audio(json.load(stream), sys.argv[3])
    print(json.dumps({'version': 1, 'path': str(Path(sys.argv[3]).resolve())}))


if __name__ == '__main__':
    main()
