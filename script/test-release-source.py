"""Checks that release source preparation stops before packaging invalid input."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('release_source', ROOT / 'script/prepare-release-source.py')
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class SourceRequirements(unittest.TestCase):
    def invoke(self, root, status='', ffmpeg=True):
        args = ['prepare-release-source', str(root / 'output'), '--platform', 'Linux',
                '--licenses', str(root / 'licenses'),
                '--aaf-binary', str(root / 'align-aaf'), '--velopack-version', '1.2.0']
        if ffmpeg:
            args += ['--ffmpeg', str(root / 'ffmpeg')]
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

    def test_release_without_ffmpeg_still_requires_the_built_aaf_helper(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaisesRegex(RuntimeError, 'Missing AAF build output'):
                self.invoke(root, ffmpeg=False)


if __name__ == '__main__':
    unittest.main()
