import copy
import tempfile
import unittest
import wave
from pathlib import Path

from bridge import write_audio, read_audio, read_timeline, locator_path
import aaf2


class SourceValidation(unittest.TestCase):
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
            self.assertEqual(result['version'], 2)
            track = result['sequences'][0]['tracks'][0]
            self.assertEqual(track['media_kind'], 'picture')
            self.assertEqual(track['edit_rate'], {'numerator': 30000, 'denominator': 1001})
            self.assertEqual(track['clips'], [{'path': str(media), 'start': 7,
                'source_in': 11, 'length': 101, 'channel': None}])
            with self.assertRaises(ValueError):
                read_audio(path)

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
            restored = read_audio(output)['sequences'][0]
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


if __name__ == '__main__':
    unittest.main()
