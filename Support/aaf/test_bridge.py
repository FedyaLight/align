import copy
import tempfile
import unittest
import wave
import subprocess
import json
from pathlib import Path

from bridge import write_audio, read_timeline, locator_path, repair_paths
import aaf2


class SourceValidation(unittest.TestCase):
    def test_path_repair_writes_copy_and_preserves_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            old = root / 'missing clip.mov'
            replacement = root / 'found clip.mov'
            replacement.write_bytes(b'media')
            source = root / 'source.aaf'
            output = root / 'fixed.aaf'
            with aaf2.open(str(source), 'w') as container:
                mob = container.create.SourceMob('Missing')
                container.content.mobs.append(mob)
                descriptor = container.create.CDCIDescriptor()
                descriptor['SampleRate'].value = '25'
                descriptor['Length'].value = 25
                for key, value in {'ComponentWidth': 8, 'HorizontalSubsampling': 2,
                    'StoredHeight': 1080, 'StoredWidth': 1920, 'FrameLayout': 'FullFrame',
                    'VideoLineMap': [0, 0], 'ImageAspectRatio': '16/9'}.items():
                    descriptor[key].value = value
                locator = container.create.NetworkLocator()
                locator['URLString'].value = old.resolve().as_uri()
                descriptor['Locator'].append(locator)
                mob.descriptor = descriptor

            changed = repair_paths({'version': 1, 'replacements': [{
                'from': str(old.resolve()), 'to': str(replacement.resolve()),
            }]}, source, output)
            self.assertEqual(changed, 1)
            with aaf2.open(str(source), 'r') as container:
                locator = next(iter(container.content.sourcemobs())).descriptor['Locator'].value[0]
                self.assertEqual(locator_path(locator['URLString'].value), str(old.resolve()))
            with aaf2.open(str(output), 'r') as container:
                locator = next(iter(container.content.sourcemobs())).descriptor['Locator'].value[0]
                self.assertEqual(locator_path(locator['URLString'].value),
                                 str(replacement.resolve()))

    def test_embedded_pcm_extracts_exact_samples_without_external_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'source.wav'
            samples = b'\x01\x00\xff\x7f\x00\x80' * 100
            with wave.open(str(source), 'wb') as writer:
                writer.setparams((1, 2, 48000, 0, 'NONE', 'PCM'))
                writer.writeframes(samples)
            path = root / 'embedded.aaf'
            with aaf2.open(str(path), 'w') as container:
                master = container.create.MasterMob('Embedded')
                container.content.mobs.append(master)
                slot = master.import_audio_essence(str(source))
                composition = container.create.CompositionMob('Edit')
                composition.usage = 'Usage_TopLevel'
                container.content.mobs.append(composition)
                target = composition.create_sound_slot(48000)
                target.segment.components.append(master.create_source_clip(slot_id=slot.slot_id,
                    start=7, length=100, media_kind='sound'))
                target.segment.length = 100
            source.unlink()
            result = read_timeline(path, extract_dir=root / 'extracted')
            clip = result['sequences'][0]['tracks'][0]['clips'][0]
            self.assertEqual((clip['source_in'], clip['length'], clip['channel']), (7, 100, 0))
            with wave.open(clip['path'], 'rb') as reader:
                self.assertEqual(reader.readframes(300), samples)
            self.assertEqual(read_timeline(path, extract_dir=root / 'extracted'), result)
            self.assertEqual(len(list((root / 'extracted').iterdir())), 1)

    def test_embedded_stereo_preserves_numbered_channels(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'stereo.wav'
            samples = b'\x01\x00\xff\x7f' * 300
            with wave.open(str(source), 'wb') as writer:
                writer.setparams((2, 2, 48000, 0, 'NONE', 'PCM'))
                writer.writeframes(samples)
            path = root / 'stereo.aaf'
            with aaf2.open(str(path), 'w') as container:
                mob = container.create.SourceMob('Stereo PCM')
                container.content.mobs.append(mob)
                first = mob.import_audio_essence(str(source))
                first['PhysicalTrackNumber'].value = 1
                second = mob.create_timeline_slot(48000)
                second.segment = container.create.SourceClip(media_kind='sound', length=300)
                second['PhysicalTrackNumber'].value = 2
                composition = container.create.CompositionMob('Stereo edit')
                composition.usage = 'Usage_TopLevel'
                container.content.mobs.append(composition)
                for slot in (first, second):
                    track = composition.create_sound_slot(48000)
                    track.segment.components.append(mob.create_source_clip(
                        slot_id=slot.slot_id, start=7, length=100, media_kind='sound'))
                    track.segment.length = 100
            source.unlink()
            result = read_timeline(path, extract_dir=root / 'extracted')
            clips = [t['clips'][0] for t in result['sequences'][0]['tracks']]
            self.assertEqual([c['channel'] for c in clips], [0, 1])
            self.assertEqual(clips[0]['path'], clips[1]['path'])
            with wave.open(clips[0]['path'], 'rb') as reader:
                self.assertEqual(reader.getnchannels(), 2)
                self.assertEqual(reader.readframes(300), samples)
            with aaf2.open(str(path), 'rw') as container:
                mob = next(m for m in container.content.sourcemobs() if m.essence is not None)
                mob.slots[1]['PhysicalTrackNumber'].value = 3
            with self.assertRaisesRegex(ValueError, 'physical channel'):
                read_timeline(path, extract_dir=root / 'extracted')

    def test_mixed_clocks_keep_fractional_sample_boundaries(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'mono.wav'
            with wave.open(str(source), 'wb') as writer:
                writer.setparams((1, 2, 48000, 0, 'NONE', 'PCM'))
                writer.writeframes(b'\x01\x00' * 96000)
            path = root / 'clocks.aaf'
            with aaf2.open(str(path), 'w') as container:
                mob = container.create.SourceMob('PCM 48000')
                container.content.mobs.append(mob)
                native = mob.import_audio_essence(str(source))
                middle = container.create.MasterMob('Edit 25')
                container.content.mobs.append(middle)
                middle_slot = middle.create_timeline_slot(25)
                # Starts count in the containing track's edit units.
                middle_slot.segment = mob.create_source_clip(slot_id=native.slot_id,
                    start=26, length=5, media_kind='sound')
                composition = container.create.CompositionMob('Edit 29.97')
                composition.usage = 'Usage_TopLevel'
                container.content.mobs.append(composition)
                track = composition.create_sound_slot('30000/1001')
                track.segment.components.append(container.create.Filler('sound', 1))
                track.segment.components.append(middle.create_source_clip(
                    slot_id=middle_slot.slot_id, start=1, length=3, media_kind='sound'))
                track.segment.length = 4
            result = read_timeline(path, extract_dir=root / 'extracted')
            track = result['sequences'][0]['tracks'][0]
            self.assertEqual(track['edit_rate'], {'numerator': 30000, 'denominator': 1001})
            self.assertEqual(track['time_rate'], {'numerator': 30000, 'denominator': 1})
            clip = track['clips'][0]
            # 1/29.97 s into the 25 fps edit, whose clip starts 26/25 s in.
            self.assertEqual((clip['start'], clip['source_in'], clip['length']), (1001, 32201, 3003))
            # Embedded PCM's sample rate can differ from its SourceMob slot clock.
            with aaf2.open(str(path), 'rw') as container:
                native_mob = next(container.content.sourcemobs())
                native_mob.slots[0].edit_rate = 25
                middle_mob = next(container.content.mastermobs())
                middle_mob.slots[0].segment.start = 25
            track = read_timeline(path, extract_dir=root / 'extracted')['sequences'][0]['tracks'][0]
            clip = track['clips'][0]
            from fractions import Fraction
            clock = Fraction(track['time_rate']['numerator'], track['time_rate']['denominator'])
            self.assertEqual(Fraction(clip['source_in'], 1) / clock, Fraction(31001, 30000))


    def test_nested_source_sequence_splits_cuts_and_preserves_gaps(self):
        with tempfile.TemporaryDirectory() as directory:
            media = Path(directory) / 'mono.wav'
            with wave.open(str(media), 'wb') as writer:
                writer.setparams((1, 2, 48000, 0, 'NONE', 'PCM'))
                writer.writeframes(b'\0' * 2000)
            path = Path(directory) / 'nested.aaf'
            with aaf2.open(str(path), 'w') as container:
                metadata = {'format': {'format_name': 'wav'}, 'streams': [{
                    'codec_type': 'audio', 'sample_rate': '48000', 'duration_ts': 1000, 'channels': 1}]}
                master, _, _ = container.content.create_ama_link(str(media), metadata)
                nested = container.create.CompositionMob('Nested edits')
                container.content.mobs.append(nested)
                slot = nested.create_sound_slot(48000)
                slot.segment.components.append(master.create_source_clip(slot_id=1, start=100, length=100, media_kind='sound'))
                slot.segment.components.append(container.create.Filler('sound', 20))
                slot.segment.components.append(master.create_source_clip(slot_id=1, start=500, length=100, media_kind='sound'))
                slot.segment.length = 220
                top = container.create.CompositionMob('Top'); top.usage = 'Usage_TopLevel'
                container.content.mobs.append(top)
                target = top.create_sound_slot(48000)
                target.segment.components.append(nested.create_source_clip(slot_id=slot.slot_id, start=50, length=120, media_kind='sound'))
                target.segment.length = 120
            clips = read_timeline(path)['sequences'][0]['tracks'][0]['clips']
            self.assertEqual([(clip['start'], clip['source_in'], clip['length']) for clip in clips],
                [(0, 150, 50), (70, 500, 50)])
            # Wrap the nested reference in a selector. Its inactive filler
            # must not replace the selected montage or add extra duration.
            with aaf2.open(str(path), 'rw') as container:
                top = next(container.content.toplevel())
                sequence = top.slots[0].segment
                chosen = sequence.components.pop(0)
                selector = container.create.Selector(media_kind='sound', length=120)
                selector['Selected'].value = chosen
                selector['Alternates'].append(container.create.Filler('sound', 120))
                sequence.components.append(selector)
            self.assertEqual(read_timeline(path)['sequences'][0]['tracks'][0]['clips'], clips)
            with aaf2.open(str(path), 'rw') as container:
                top = next(container.content.toplevel())
                selector = top.slots[0].segment.components[0]
                chosen = selector['Selected'].value
                selected_sequence = container.create.Sequence(media_kind='sound', length=120)
                selector['Selected'].value = None
                selected_sequence.components.append(chosen.copy())
                selector['Selected'].value = selected_sequence
            self.assertEqual(read_timeline(path)['sequences'][0]['tracks'][0]['clips'], clips)
            with aaf2.open(str(path), 'rw') as container:
                top = next(container.content.toplevel())
                top.slots[0].segment.components[0].length = 171
                top.slots[0].segment.components[0]['Selected'].value.length = 171
                top.slots[0].segment.components[0]['Selected'].value.components[0].length = 171
                top.slots[0].segment.length = 171
            with self.assertRaisesRegex(ValueError, 'exceeds'):
                read_timeline(path)

    def test_picture_export_links_real_media_and_preserves_output_on_error(self):
        with tempfile.TemporaryDirectory() as directory:
            media = Path(directory) / 'camera.mov'
            subprocess.run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i',
                'color=c=blue:s=64x64:r=30000/1001', '-frames:v', '120',
                '-c:v', 'mpeg4', str(media)], check=True, timeout=30)
            metadata = json.loads(subprocess.check_output(['ffprobe', '-v', 'error',
                '-show_format', '-show_streams', '-of', 'json', str(media)], timeout=30))
            document = {'version': 2, 'name': 'Picture', 'tracks': [], 'picture_tracks': [{
                'name': 'V1', 'edit_rate': {'numerator': 30000, 'denominator': 1001},
                'clips': [{'path': str(media), 'start': 7, 'source_in': 11,
                    'length': 101, 'metadata': metadata,
                    'timecode': {'start': 1234, 'fps': 30, 'drop': True}}]}]}
            output = Path(directory) / 'picture.aaf'
            write_audio(document, output)
            with aaf2.open(str(output)) as container:
                # The tape mob carries the file's start timecode.
                tape_clocks = [slot.segment for mob in container.content.sourcemobs()
                    for slot in mob.slots if slot.media_kind == 'Timecode']
                self.assertEqual([(clock.start, clock.fps, clock.drop) for clock in tape_clocks],
                    [(1234, 30, True)])
                clocks = [slot for slot in next(container.content.toplevel()).slots
                    if slot.media_kind == 'Timecode']
                self.assertEqual(len(clocks), 1)
                self.assertEqual(str(clocks[0].edit_rate), '30000/1001')
                self.assertEqual(clocks[0].segment.start, 0)
                self.assertEqual(clocks[0].segment.length, 108)
            track = read_timeline(output)['sequences'][0]['tracks'][0]
            self.assertEqual(track['edit_rate'], {'numerator': 30000, 'denominator': 1001})
            clip = track['clips'][0]
            self.assertEqual((clip['start'], clip['source_in'], clip['length']), (7, 11, 101))
            self.assertEqual(Path(clip['path']), media.resolve())
            overridden = copy.deepcopy(document)
            overridden['composition_edit_rate'] = {'numerator': 25, 'denominator': 1}
            override_output = Path(directory) / 'picture-25.aaf'
            write_audio(overridden, override_output)
            with aaf2.open(str(override_output)) as container:
                top = next(container.content.toplevel())
                timecode = next(slot for slot in top.slots if slot.media_kind == 'Timecode')
                picture = next(slot for slot in top.slots if slot.media_kind == 'Picture')
                self.assertEqual(str(timecode.edit_rate), '25')
                self.assertEqual(timecode.segment.length, 91)
                # Picture counts in composition frames, as editors read it.
                self.assertEqual(str(picture.edit_rate), '25')
                clip = picture.segment.components[-1]
                self.assertEqual((clip.start, clip.length), (9, 84))
                self.assertEqual(picture.segment.components[0].length, 6)
            original = output.read_bytes()
            document['picture_tracks'][0]['clips'][0]['length'] = 121
            with self.assertRaises(ValueError):
                write_audio(document, output)
            self.assertEqual(output.read_bytes(), original)
            self.assertFalse(list(Path(directory).glob('.align-aaf-*')))

    def test_cameras_stack_as_layers_of_one_composition_rate_track(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tracks = []
            for name, rate, start, source_in, length in (
                    ('cam24', '24', 0, 0, 48), ('cam25', '25', 30, 5, 25)):
                media = root / f'{name}.mov'
                subprocess.run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i',
                    f'color=c=blue:s=64x64:r={rate}', '-frames:v', '60',
                    '-c:v', 'mpeg4', str(media)], check=True, timeout=30)
                metadata = json.loads(subprocess.check_output(['ffprobe', '-v', 'error',
                    '-show_format', '-show_streams', '-of', 'json', str(media)], timeout=30))
                tracks.append({'name': name, 'edit_rate': {'numerator': int(rate), 'denominator': 1},
                    'clips': [{'path': str(media), 'start': start, 'source_in': source_in,
                        'length': length, 'metadata': metadata}]})
            document = {'version': 2, 'name': 'Cameras', 'tracks': [], 'picture_tracks': tracks,
                'composition_edit_rate': {'numerator': 25, 'denominator': 1}}
            output = root / 'cameras.aaf'
            write_audio(document, output)
            with aaf2.open(str(output)) as container:
                top = next(container.content.toplevel())
                pictures = [slot for slot in top.slots if slot.media_kind == 'Picture']
                self.assertEqual(len(pictures), 1)
                self.assertEqual(str(pictures[0].edit_rate), '25')
                stack = pictures[0].segment
                self.assertIsInstance(stack, aaf2.components.NestedScope)
                layers = [[(type(part).__name__, part.length) for part in layer.components]
                    for layer in stack['Slots'].value]
                # 48 frames at 24 fps last 50 composition frames.
                self.assertEqual(layers, [[('SourceClip', 50), ('Filler', 5)],
                    [('ScopeReference', 30), ('SourceClip', 25)]])
                reference = stack['Slots'].value[1].components[0]
                self.assertEqual((reference['RelativeScope'].value,
                    reference['RelativeSlot'].value), (0, 1))
            read = read_timeline(output)['sequences'][0]['tracks']
            self.assertEqual([track['name'] for track in read], ['V1 1', 'V1 2'])
            self.assertEqual([(clip['start'], clip['source_in'], clip['length'])
                for track in read for clip in track['clips']], [(0, 0, 50), (30, 5, 25)])

    def test_sound_counts_in_composition_frames_beside_picture(self):
        # Premiere takes its sequence rate from sound tracks too, so beside
        # picture sound counts in composition frames, as Resolve writes it.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            media = root / 'camera.mov'
            subprocess.run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i',
                'color=c=blue:s=64x64:r=24000/1001', '-frames:v', '48',
                '-c:v', 'mpeg4', str(media)], check=True, timeout=30)
            metadata = json.loads(subprocess.check_output(['ffprobe', '-v', 'error',
                '-show_format', '-show_streams', '-of', 'json', str(media)], timeout=30))
            stem = root / 'stem.wav'
            with wave.open(str(stem), 'wb') as writer:
                writer.setparams((1, 2, 48000, 0, 'NONE', 'not compressed'))
                writer.writeframes(b'\0' * 2 * 50203)
            rate = {'numerator': 24000, 'denominator': 1001}
            document = {'version': 2, 'name': 'Frames', 'composition_edit_rate': rate,
                'picture_tracks': [{'name': 'V1', 'edit_rate': rate, 'clips': [{
                    'path': str(media), 'start': 0, 'source_in': 0, 'length': 48,
                    'metadata': metadata}]}],
                # Two frames in (2002 samples each), 1.004 s long; the stem
                # ends with a frame of silence, as Align renders it.
                'tracks': [{'name': 'A1', 'sample_rate': 48000, 'clips': [{
                    'path': str(stem), 'start': 4004, 'source_in': 0, 'length': 48200,
                    'source_frames': 50203, 'channels': 1}]}]}
            output = root / 'frames.aaf'
            write_audio(document, output)
            with aaf2.open(str(output)) as container:
                top = next(container.content.toplevel())
                sound = next(slot for slot in top.slots if slot.media_kind == 'Sound')
                self.assertEqual(str(sound.edit_rate), '24000/1001')
                self.assertEqual([(type(part).__name__, part.length)
                    for part in sound.segment.components], [('Filler', 2), ('SourceClip', 25)])
            track = next(track for track in read_timeline(output)['sequences'][0]['tracks']
                if track['media_kind'] == 'sound')
            self.assertEqual(track['edit_rate'], rate)
            clip = track['clips'][0]
            self.assertEqual((clip['start'], clip['source_in'], clip['length']), (2, 0, 25))
            # Sound alone counts in composition frames too: at the sample
            # rate Premiere opens it as 29.97.
            del document['picture_tracks']
            write_audio(document, output)
            with aaf2.open(str(output)) as container:
                top = next(container.content.toplevel())
                self.assertEqual([str(slot.edit_rate) for slot in top.slots],
                    ['24000/1001', '24000/1001'])
            # Read back on a 24000 tick clock: 2 and 25 frames of 1001 ticks.
            track = read_timeline(output, audio_only=True)['sequences'][0]['tracks'][0]
            clip = track['clips'][0]
            self.assertEqual((track['sample_rate'], clip['start'], clip['length']),
                (24000, 2002, 25025))
            # Without a composition rate sound keeps its samples.
            del document['composition_edit_rate']
            write_audio(document, output)
            clip = read_timeline(output, audio_only=True)['sequences'][0]['tracks'][0]['clips'][0]
            self.assertEqual((clip['start'], clip['length']), (4004, 48200))

    def test_camera_sound_links_the_camera_file_itself(self):
        # Like Resolve's AAF, a camera's channels come from the file its
        # picture links, without mono copies.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            media = root / 'camera.mov'
            subprocess.run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i',
                'color=c=blue:s=64x64:r=25', '-f', 'lavfi', '-i',
                'sine=frequency=440:sample_rate=48000', '-t', '2',
                '-ac', '2', '-c:v', 'mpeg4', '-c:a', 'pcm_s16le', str(media)],
                check=True, timeout=30)
            metadata = json.loads(subprocess.check_output(['ffprobe', '-v', 'error',
                '-show_format', '-show_streams', '-of', 'json', str(media)], timeout=30))
            sound = lambda channel: {'name': f'A{channel + 1}', 'sample_rate': 48000, 'clips': [{
                'path': str(media), 'start': 0, 'source_in': 1920, 'length': 48000,
                'source_frames': 96000, 'channels': 2, 'channel': channel,
                'metadata': metadata}]}
            document = {'version': 2, 'name': 'Camera', 'tracks': [sound(0), sound(1)],
                'composition_edit_rate': {'numerator': 25, 'denominator': 1},
                'picture_tracks': [{'name': 'V1', 'edit_rate': {'numerator': 25, 'denominator': 1},
                    'clips': [{'path': str(media), 'start': 0, 'source_in': 1, 'length': 25,
                        'metadata': metadata}]}]}
            output = root / 'camera.aaf'
            write_audio(document, output)
            with aaf2.open(str(output)) as container:
                self.assertEqual(len(list(container.content.mastermobs())), 1)
                top = next(container.content.toplevel())
                sounds = [slot for slot in top.slots if slot.media_kind == 'Sound']
                clips = [slot.segment.components[0] for slot in sounds]
                self.assertEqual([(clip.start, clip.length) for clip in clips], [(1, 25), (1, 25)])
                master = clips[0].mob
                channels = [master.slot_at(clip.slot_id)['PhysicalTrackNumber'].value
                    for clip in clips]
                self.assertEqual(channels, [1, 2])
            read = read_timeline(output)['sequences'][0]['tracks']
            heard = [clip for track in read if track['media_kind'] == 'sound'
                for clip in track['clips']]
            self.assertEqual([(Path(clip['path']), clip['channel']) for clip in heard],
                [(media.resolve(), 0), (media.resolve(), 1)])
            # Sound ending before its picture (a longer file) ends the clip
            # with it.
            for track in document['tracks']:
                track['clips'][0].update(source_in=72000, length=48000, source_frames=144000)
            write_audio(document, output)
            with aaf2.open(str(output)) as container:
                top = next(container.content.toplevel())
                clip = next(slot for slot in top.slots
                    if slot.media_kind == 'Sound').segment.components[0]
                self.assertEqual((clip.start, clip.length), (38, 12))
            read_timeline(output)

    def test_linked_picture_preserves_fractional_edit_rate(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'picture.aaf'
            media = Path(directory) / 'camera.mov'
            with aaf2.open(str(path), 'w') as container:
                source = container.create.SourceMob('Camera')
                container.content.mobs.append(source)
                descriptor = container.create.CDCIDescriptor()
                descriptor['SampleRate'].value = '30000/1001'
                descriptor['Length'].value = 300
                for key, value in {'ComponentWidth': 8, 'HorizontalSubsampling': 2,
                    'StoredHeight': 1080, 'StoredWidth': 1920, 'FrameLayout': 'FullFrame',
                    'VideoLineMap': [0, 0], 'ImageAspectRatio': '16/9'}.items():
                    descriptor[key].value = value
                locator = container.create.NetworkLocator()
                locator['URLString'].value = media.as_uri()
                descriptor['Locator'].append(locator)
                source.descriptor = descriptor
                slot = source.create_timeline_slot('30000/1001')
                slot.segment = container.create.SourceClip(media_kind='picture', length=300)
                comp = container.create.CompositionMob('Picture edit')
                comp.usage = 'Usage_TopLevel'
                container.content.mobs.append(comp)
                target = comp.create_timeline_slot('30000/1001')
                target.segment = container.create.Sequence(media_kind='picture')
                target.segment.components.append(container.create.Filler('picture', 7))
                target.segment.components.append(source.create_source_clip(
                    slot_id=slot.slot_id, start=11, length=101, media_kind='picture'))
                target.segment.length = 108
            result = read_timeline(path)
            self.assertEqual(result['version'], 3)
            track = result['sequences'][0]['tracks'][0]
            self.assertEqual(track['media_kind'], 'picture')
            self.assertEqual(track['edit_rate'], {'numerator': 30000, 'denominator': 1001})
            self.assertEqual(track['clips'], [{'path': str(media), 'start': 7,
                'source_in': 11, 'length': 101, 'channel': None}])
            with self.assertRaises(ValueError):
                read_timeline(path, audio_only=True)

    def test_locator_paths(self):
        self.assertEqual(locator_path('file:///C:/Media/My%20Clip.wav'), 'C:/Media/My Clip.wav')
        self.assertEqual(locator_path('file:///Volumes/Audio/a.wav'), '/Volumes/Audio/a.wav')
        with self.assertRaises(ValueError):
            locator_path('https://example.com/a.wav')

    def test_actual_header_mismatch_preserves_existing_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'mono.wav'
            with wave.open(str(source), 'wb') as wav:
                wav.setparams((1, 2, 48000, 0, 'NONE', 'not compressed'))
                wav.writeframes(b'\0' * 960)
            manifest = {'version': 1, 'name': 'Test', 'tracks': [{
                'name': 'Mono', 'sample_rate': 48000, 'clips': [{
                    'path': str(source), 'start': 0, 'source_in': 0,
                    'length': 480, 'source_frames': 480, 'channels': 1,
                }],
            }]}
            output = root / 'test.aaf'
            write_audio(manifest, output)
            restored = read_timeline(output, audio_only=True)['sequences'][0]
            generalized = read_timeline(output)['sequences'][0]['tracks'][0]
            self.assertEqual(generalized['media_kind'], 'sound')
            self.assertEqual(generalized['edit_rate'], {'numerator': 48000, 'denominator': 1})
            self.assertEqual(generalized['clips'], restored['tracks'][0]['clips'])
            self.assertEqual(restored['tracks'][0]['clips'][0]['length'], 480)
            self.assertEqual(Path(restored['tracks'][0]['clips'][0]['path']), source.resolve())
            original = output.read_bytes()
            for field in ('rate', 'frames', 'truncation'):
                bad = copy.deepcopy(manifest)
                if field == 'rate':
                    bad['tracks'][0]['sample_rate'] = 44100
                elif field == 'frames':
                    bad['tracks'][0]['clips'][0]['source_frames'] = 481
                else:
                    source.write_bytes(source.read_bytes()[:-2])
                with self.assertRaises(ValueError):
                    write_audio(bad, output)
                self.assertEqual(output.read_bytes(), original)
                self.assertFalse(list(root.glob('.align-aaf-*')))

    def test_batch_manifest_writes_two_top_level_compositions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'mono.wav'
            with wave.open(str(source), 'wb') as wav:
                wav.setparams((1, 2, 48000, 0, 'NONE', 'not compressed'))
                wav.writeframes(b'\0' * 960)
            sequence = lambda name: {'version': 1, 'name': name, 'tracks': [{
                'name': 'Mono', 'sample_rate': 48000, 'clips': [{
                    'path': str(source), 'start': 0, 'source_in': 0,
                    'length': 480, 'source_frames': 480, 'channels': 1,
                }],
            }]}
            output = root / 'two.aaf'
            write_audio({'version': 3, 'sequences': [
                sequence('Morning'), sequence('Evening'),
            ]}, output)
            result = read_timeline(output, audio_only=True)
            self.assertEqual([item['name'] for item in result['sequences']],
                             ['Morning', 'Evening'])


if __name__ == '__main__':
    unittest.main()
