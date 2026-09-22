"""Checks that release source preparation stops before packaging invalid input."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('release_source', ROOT / 'script/prepare-release-source.py')
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class SourceRequirements(unittest.TestCase):
    def invoke(self, root, status=''):
        args = ['prepare-release-source', str(root / 'output'), '--platform', 'macOS',
                '--licenses', str(root / 'licenses'), '--ffmpeg', str(root / 'ffmpeg'),
                '--aaf', str(root / 'aaf'), '--velopack-version', '1.2.0']
        with patch('sys.argv', args), patch.object(release, 'run', return_value=status) as run:
            try:
                release.main()
            finally:
                # All invalid cases must stop before cargo or any network operation.
                self.assertEqual(run.call_count, 1)

    def test_modified_checkout_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(RuntimeError, 'clean committed checkout'):
                self.invoke(Path(tmp), ' M Cargo.lock')

    def test_missing_ffmpeg_archive_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'ffmpeg').mkdir()
            (root / 'ffmpeg/version.txt').write_text('9.0.1')
            with self.assertRaisesRegex(RuntimeError, 'ffmpeg-9.0.1.tar.xz'):
                self.invoke(root)

    def test_missing_aaf_source_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'ffmpeg').mkdir()
            (root / 'ffmpeg/version.txt').write_text('9.0.1')
            for name in ('ffmpeg-9.0.1.tar.xz', 'build-ffmpeg-minimal.sh', 'config.mak'):
                (root / 'ffmpeg' / name).touch()
            (root / 'aaf/Licenses').mkdir(parents=True)
            (root / 'aaf/Licenses/Python-LICENSE.txt').touch()
            (root / 'aaf/versions.json').write_text(json.dumps({
                'pyaaf2': '1.7.1', 'pyinstaller': '6.22.2',
                'pyinstaller-hooks-contrib': '2026.7', 'python': '3.13.2',
                'native_libraries': {},
            }))
            with self.assertRaisesRegex(RuntimeError, 'pyaaf2 1.7.1'):
                self.invoke(root)


if __name__ == '__main__':
    unittest.main()
