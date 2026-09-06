import copy
import tempfile
import unittest
import wave
from pathlib import Path

from bridge import write_audio, read_audio


class SourceValidation(unittest.TestCase):
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
