from pathlib import Path
import subprocess
import tempfile
import unittest


COPY = Path(__file__).with_name('copy-disk-templates.sh')
NAMES = ('storage-template.ext4', 'overlay-template.ext4')


class DiskTemplates(unittest.TestCase):
    def test_compressed_release_assets_survive_packaging_without_expansion(self):
        with tempfile.TemporaryDirectory() as tmp:
            source, dest = Path(tmp) / 'engine release', Path(tmp) / 'CLI bundle'
            source.mkdir()
            for name in NAMES:
                (source / (name + '.zst')).write_bytes(b'compressed ' + name.encode())
                (source / name).write_bytes(b'legacy plain template')
            subprocess.run(['bash', str(COPY), str(source), str(dest)], check=True)
            self.assertEqual(sorted(p.name for p in dest.iterdir()), sorted(n + '.zst' for n in NAMES))
            for name in NAMES:
                self.assertEqual((dest / (name + '.zst')).read_bytes(), (source / (name + '.zst')).read_bytes())

    def test_older_plain_release_assets_remain_supported(self):
        with tempfile.TemporaryDirectory() as tmp:
            source, dest = Path(tmp) / 'engine', Path(tmp) / 'cli'
            source.mkdir()
            for name in NAMES:
                (source / name).write_bytes(name.encode())
            subprocess.run(['bash', str(COPY), str(source), str(dest)], check=True)
            for name in NAMES:
                self.assertEqual((dest / name).read_bytes(), name.encode())

    def test_missing_template_fails_before_publishing_a_partial_pair(self):
        for missing in NAMES:
            with self.subTest(missing=missing), tempfile.TemporaryDirectory() as tmp:
                source, dest = Path(tmp) / 'engine', Path(tmp) / 'cli'
                source.mkdir()
                for name in NAMES:
                    if name != missing:
                        (source / (name + '.zst')).write_bytes(b'compressed')
                result = subprocess.run(['bash', str(COPY), str(source), str(dest)], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(missing, result.stderr)
                self.assertFalse(dest.exists())

    def test_empty_template_does_not_produce_a_bundle(self):
        with tempfile.TemporaryDirectory() as tmp:
            source, dest = Path(tmp) / 'engine', Path(tmp) / 'cli'
            source.mkdir()
            (source / (NAMES[0] + '.zst')).write_bytes(b'compressed')
            (source / (NAMES[1] + '.zst')).touch()
            result = subprocess.run(['bash', str(COPY), str(source), str(dest)], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(NAMES[1], result.stderr)
            self.assertFalse(dest.exists())


if __name__ == '__main__':
    unittest.main()
