"""AAF sidecar protocol. JSON input; no subprocesses or media decoding.

Media inspection and mono stem creation belong to Align's decoder. This
module writes their validated sample ranges to an AAF object graph.
"""
import json
import os
from pathlib import Path
import shutil
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
    links = {}

    def link(path, metadata):
        """One media link per file, shared by its picture and sound."""
        if path not in links:
            links[path] = container.content.create_ama_link(str(path), metadata)
        return links[path]

    clock = document.get('composition_edit_rate')
    if clock is None and picture_tracks:
        clock = picture_tracks[0]['edit_rate']
    composition_rate = None
    if clock is not None:
        rate = Fraction(integer(clock['numerator'], 'numerator', 1),
            integer(clock['denominator'], 'denominator', 1))
        picture_ends = (Fraction(clip['start'] + clip['length']) /
            Fraction(track['edit_rate']['numerator'], track['edit_rate']['denominator'])
            for track in picture_tracks for clip in track['clips'])
        sound_ends = (Fraction(clip['start'] + clip['length'], track['sample_rate'])
            for track in tracks for clip in track['clips'])
        duration = max(max(picture_ends, default=Fraction(0)),
            max(sound_ends, default=Fraction(0)))
        frames = duration * rate
        timecode = composition.create_timeline_slot(str(rate))
        timecode.name = 'Timecode'
        timecode['PhysicalTrackNumber'].value = 1
        timecode.segment = container.create.Timecode(fps=round(rate),
            drop=document.get('composition_drop_frame') is True,
            length=(frames.numerator + frames.denominator - 1) // frames.denominator)
        timecode.segment.start = 0
        composition_rate = rate
    if picture_tracks:
        write_picture(container, composition, picture_tracks, rate, link)
    # Premiere takes its sequence rate from the sound tracks too: at the
    # sample rate it opens 23.976, 59.94 and sound alone at 25 as 29.97. So
    # sound counts in composition frames, as Resolve writes it.
    clock = composition_rate
    for track in tracks:
        rate = integer(track['sample_rate'], 'sample_rate', 1)
        slot = composition.create_sound_slot(edit_rate=rate if clock is None else clock)
        slot.name = track['name']
        cursor = 0
        sound = []
        for clip in track['clips']:
            path = Path(clip['path']).resolve(strict=True)
            start = integer(clip['start'], 'start')
            source_in = integer(clip['source_in'], 'source_in')
            length = integer(clip['length'], 'length', 1)
            frames = integer(clip['source_frames'], 'source_frames', 1)
            if start < cursor or source_in + length > frames:
                raise ValueError('Overlapping or out-of-bounds AAF clip')
            if clip.get('channel') is not None:
                # A camera's own channel, through the link its picture uses.
                channel = integer(clip['channel'], 'channel') + 1
                master, _, _ = link(path, clip['metadata'])
                slots = [slot for slot in master.slots if slot.media_kind == 'Sound'
                    and 'PhysicalTrackNumber' in slot
                    and slot['PhysicalTrackNumber'].value == channel]
                if len(slots) != 1 or Fraction(str(slots[0].edit_rate)) != rate:
                    raise ValueError('AAF camera sound has no such channel')
                # A camera's sound can end before its picture; the clip ends
                # with it.
                available = sum(part.length for part in slots[0].segment.components)
                if source_in >= available:
                    continue
                length = min(length, available - source_in)
                sound.append((master, slots[0].slot_id, start, source_in, length, available))
                cursor = start + length
                continue
            if clip['channels'] != 1:
                raise ValueError('AAF audio requires one lossless mono stem per channel')
            validate_wave(path, rate, frames)
            metadata = {'format': {'format_name': 'wav'}, 'streams': [{
                'codec_type': 'audio', 'sample_rate': str(rate),
                'duration_ts': frames, 'channels': 1,
            }]}
            master, _, _ = container.content.create_ama_link(str(path), metadata)
            sound.append((master, 1, start, source_in, length, frames))
            cursor = start + length
        write_sound(container, slot, sound, rate, clock)


def write_sound(container, slot, sound, rate, clock):
    """Sound clips at sample positions, or counted in `clock` frames.

    In frames a clip starts on the frame nearest its start, which Align puts
    on a frame boundary, and ends on the frame that covers its last sample
    while the file lasts.
    """
    def units(samples, rounding):
        if clock is None:
            return samples
        exact = Fraction(samples) * clock / rate
        if rounding == 'up':
            return -(-exact.numerator // exact.denominator)
        if rounding == 'down':
            return exact.numerator // exact.denominator
        return (2 * exact.numerator + exact.denominator) // (2 * exact.denominator)

    cursor = 0
    for master, slot_id, start, source_in, length, frames in sound:
        begin, first = units(start, 'nearest'), units(source_in, 'nearest')
        end = min(units(start + length, 'up'), begin + units(frames, 'down') - first)
        if begin < cursor or end <= begin:
            raise ValueError('AAF sound clip does not fit its frames')
        if begin > cursor:
            slot.segment.components.append(
                container.create.Filler(media_kind='sound', length=begin - cursor))
        slot.segment.components.append(master.create_source_clip(
            slot_id=slot_id, start=first, length=end - begin, media_kind='sound'))
        cursor = end
    slot.segment.length = cursor


def write_picture(container, composition, picture_tracks, rate, link):
    """One picture track at the composition rate, layered like Resolve's own.

    Resolve opens a single picture track; with one per camera it asks which
    one to open and drops the others. Premiere counts every picture unit at
    its sequence rate. So each camera's frames are counted in composition
    frames, and cameras that overlap are stacked as layers of a nested scope,
    the upper layers showing the one below where they have no clip.
    """
    def frames(value, native):
        exact = Fraction(value) * rate / native
        return (2 * exact.numerator + exact.denominator) // (2 * exact.denominator)

    layers = []
    for track in picture_tracks:
        native = Fraction(integer(track['edit_rate']['numerator'], 'edit_rate numerator', 1),
            integer(track['edit_rate']['denominator'], 'edit_rate denominator', 1))
        layer = []
        cursor = 0
        for clip in track['clips']:
            path = Path(clip['path']).resolve(strict=True)
            start = integer(clip['start'], 'start')
            source_in = integer(clip['source_in'], 'source_in')
            length = integer(clip['length'], 'length', 1)
            master, _, tape = link(path, clip['metadata'])
            sources = [source for source in master.slots if source.media_kind == 'Picture']
            if len(sources) != 1:
                raise ValueError('AAF picture requires one video stream')
            source = sources[0]
            if Fraction(str(source.edit_rate)) != native:
                raise ValueError('AAF picture rate differs from its media')
            available = sum(part.length for part in source.segment.components)
            if start < cursor or source_in + length > available:
                raise ValueError('Overlapping or out-of-bounds AAF picture clip')
            has_clock = any(slot.media_kind == 'Timecode' for slot in tape.slots)
            if clip.get('timecode') is not None and not has_clock:
                # Editors locate the file by its start timecode, which the
                # tape mob carries; without it Resolve looks for frames from
                # 00:00:00:00 and leaves the picture offline. Clips of one
                # file share its link and so its clock.
                timecode = clip['timecode']
                tape_clock = tape.create_timeline_slot(str(native))
                tape_clock.name = 'Timecode'
                tape_clock.segment = container.create.Timecode(
                    fps=integer(timecode['fps'], 'timecode fps', 1),
                    drop=timecode['drop'] is True, length=available)
                tape_clock.segment.start = integer(timecode['start'], 'timecode start')
            # Boundaries convert separately, so gaps and clips do not
            # accumulate rounding; the clip stays within its file.
            begin, first = frames(start, native), frames(source_in, native)
            end = begin + min(frames(start + length, native) - begin,
                available * rate // native - first)
            if end <= begin:
                raise ValueError('AAF picture clip is shorter than a composition frame')
            layer.append((begin, end, master, source.slot_id, first))
            cursor = start + length
        layers.append(layer)
    duration = max(layer[-1][1] for layer in layers)

    def gap(length, below):
        if not below:
            return container.create.Filler('picture', length)
        reference = container.create.ScopeReference(media_kind='picture', length=length)
        reference['RelativeScope'].value = 0
        reference['RelativeSlot'].value = 1
        return reference

    def sequence(layer, below, length):
        segment = container.create.Sequence(media_kind='picture')
        cursor = 0
        for begin, end, master, slot_id, first in layer:
            if begin > cursor:
                segment.components.append(gap(begin - cursor, below))
            segment.components.append(master.create_source_clip(
                slot_id=slot_id, start=first, length=end - begin, media_kind='picture'))
            cursor = end
        if length > cursor:
            segment.components.append(gap(length - cursor, below))
        segment.length = length
        return segment

    slot = composition.create_timeline_slot(str(rate))
    slot.name = 'V1'
    slot['PhysicalTrackNumber'].value = 1
    if len(layers) == 1:
        slot.segment = sequence(layers[0], False, layers[0][-1][1])
        return
    stack = container.create.NestedScope(media_kind='picture', length=duration)
    for index, layer in enumerate(layers):
        stack.slots.append(sequence(layer, index > 0, duration))
    slot.segment = stack


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


def repair_paths(document, source, destination):
    """Copy an AAF and rewrite matching NetworkLocator URLs atomically."""
    if document.get('version') != 1 or not isinstance(document.get('replacements'), list):
        raise ValueError('Unsupported AAF path repair manifest')
    source = Path(source).resolve(strict=True)
    destination = Path(destination).resolve()
    if source == destination:
        raise ValueError('AAF path repair destination must be a copy')
    mapping = {}
    for replacement in document['replacements']:
        if not isinstance(replacement, dict) or set(replacement) != {'from', 'to'}:
            raise ValueError('Invalid AAF path replacement')
        old = replacement['from']
        new = replacement['to']
        if not isinstance(old, str) or not isinstance(new, str):
            raise ValueError('Invalid AAF path replacement')
        target = Path(new).resolve(strict=True)
        if not target.is_file():
            raise ValueError('AAF replacement target is not a file')
        mapping[os.path.normcase(os.path.normpath(old))] = target.as_uri()
    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.align-aaf-repair-', suffix='.aaf',
                                     dir=destination.parent)
    os.close(fd)
    changed = 0
    try:
        shutil.copyfile(source, temporary)
        with aaf2.open(temporary, 'rw') as container:
            for mob in container.content.mobs:
                if not isinstance(mob, aaf2.mobs.SourceMob) or mob.descriptor is None:
                    continue
                descriptor = mob.descriptor
                if 'Locator' not in descriptor:
                    continue
                for locator in descriptor['Locator'].value:
                    if 'URLString' not in locator:
                        continue
                    try:
                        old = os.path.normcase(os.path.normpath(locator_path(
                            locator['URLString'].value)))
                    except ValueError:
                        continue
                    if old in mapping:
                        locator['URLString'].value = mapping[old]
                        changed += 1
        os.replace(temporary, destination)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    return changed


def track_document(name, media_kind, rate, clips, audio_only):
    """A read track with times in its edit rate, or an exact tick clock."""
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
        return {'name': name, 'sample_rate': int(clock), 'clips': clips}
    return {'name': name, 'media_kind': media_kind.lower(),
        'edit_rate': {'numerator': rate.numerator, 'denominator': rate.denominator},
        'time_rate': {'numerator': clock.numerator, 'denominator': clock.denominator},
        'clips': clips}


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
            # A clip's start counts in its own track's edit units, as Resolve
            # writes it: frame 48 of a 24 fps file on a 25 fps track is 50.
            source_start = Fraction(int(clip.start), 1) / rate + offset
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
                    elif isinstance(part, (aaf2.components.Filler, aaf2.components.ScopeReference)):
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
                # A nested scope stacks layers, each its own track; where an
                # upper layer refers to the scope it shows the layer below.
                segment = selected(slot.segment)
                layers = (list(segment['Slots'].value)
                    if isinstance(segment, aaf2.components.NestedScope) else [segment])
                for index, layer in enumerate(layers):
                    clips = []
                    cursor = Fraction(0)
                    for part in components(layer):
                        part = selected(part)
                        length = Fraction(int(part.length), 1) / rate
                        if length < 0:
                            raise ValueError('Negative AAF component length')
                        if isinstance(part, aaf2.components.SourceClip):
                            position = cursor
                            for media, source_in, channel, span in resolve(part, rate, set()):
                                if media is not None and span:
                                    clips.append({'path': media, 'start': position,
                                        'source_in': source_in, 'length': span, 'channel': channel})
                                position += span
                        elif not isinstance(part, (aaf2.components.Filler,
                                aaf2.components.ScopeReference)):
                            raise ValueError('Unsupported AAF timeline component')
                        cursor += length
                    name = slot.name or ''
                    if len(layers) > 1:
                        name = f'{name} {index + 1}'.strip()
                    tracks.append(track_document(name, slot.media_kind, rate, clips, audio_only))
            sequences.append({'name': composition.name, 'tracks': tracks})
        if not sequences:
            raise ValueError('AAF has no top-level composition')
        return {'version': 1 if audio_only else 3, 'sequences': sequences}


def main():
    if len(sys.argv) == 5 and sys.argv[1] == 'repair-paths':
        with open(sys.argv[2], encoding='utf-8') as stream:
            document = json.load(stream)
        changed = repair_paths(document, sys.argv[3], sys.argv[4])
        print(json.dumps({'version': 1, 'path': str(Path(sys.argv[4]).resolve()),
                          'replacements': changed}))
        return
    if len(sys.argv) in (3, 4) and sys.argv[1] == 'read-timeline':
        print(json.dumps(read_timeline(sys.argv[2], extract_dir=sys.argv[3] if len(sys.argv) == 4 else None)))
        return
    if len(sys.argv) != 4 or sys.argv[1] not in ('write-audio', 'write-timeline'):
        raise ValueError('Usage: align-aaf COMMAND [arguments]')
    with open(sys.argv[2], encoding='utf-8') as stream:
        write_audio(json.load(stream), sys.argv[3])
    print(json.dumps({'version': 1, 'path': str(Path(sys.argv[3]).resolve())}))


if __name__ == '__main__':
    main()
