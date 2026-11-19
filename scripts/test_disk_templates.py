from pathlib import Path
import os
import platform
import shutil
import subprocess
import tarfile
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


class DistributionRuntimeAssets(unittest.TestCase):
    def test_linker_changes_do_not_replace_verified_release_libraries(self):
        system = {'Darwin': 'darwin', 'Linux': 'linux'}.get(platform.system())
        arch = {'arm64': 'arm64', 'aarch64': 'arm64', 'x86_64': 'x86_64'}.get(platform.machine())
        if not system or not arch or (system == 'darwin' and arch != 'arm64'):
            self.skipTest('requires a supported Unix distribution host')
        target = f'{system}-{arch}'
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            sdk = root / 'SDK source'
            scripts = sdk / 'scripts'
            scripts.mkdir(parents=True)
            for name in ('build-smol-dist.sh', 'copy-disk-templates.sh', 'smol-wrapper.sh'):
                shutil.copy2(COPY.with_name(name), scripts / name)
            (sdk / 'Cargo.toml').write_text('version = "1.35.5"\n')
            release = root / f'smolvm-1.35.5-{target}'
            (release / 'lib').mkdir(parents=True)
            (release / 'agent-rootfs').mkdir()
            library = 'libkrun.dylib' if system == 'darwin' else 'libkrun.so'
            original = b'verified release library'
            (release / 'lib' / library).write_bytes(original)
            for name in NAMES:
                (release / (name + '.zst')).write_bytes(b'compressed fixture')
            archive = root / (release.name + '.tar.gz')
            with tarfile.open(archive, 'w:gz') as tar:
                tar.add(release, arcname=release.name)
            tools = root / 'tools'
            tools.mkdir()
            # Model the engine build script rewriting the supplied library's
            # install name and signature; no compiler or signing identity needed.
            cargo = tools / 'cargo'
            cargo.write_text('#!/bin/sh\nset -eu\n'
                             'printf rewritten > "$LIBKRUN_BUNDLE/$TEST_LIBRARY"\n'
                             'mkdir -p target/release\nprintf fixture > target/release/smol\n')
            cargo.chmod(0o755)
            signing = tools / 'codesign'
            signing.write_text('#!/bin/sh\nexit 0\n')
            signing.chmod(0o755)
            temporary = root / 'temporary'
            temporary.mkdir()
            env = {k: v for k, v in os.environ.items()
                   if k not in ('TARGET', 'USE_ZIG', 'CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET')}
            env.update(PATH=str(tools) + os.pathsep + env.get('PATH', ''),
                       ENGINE_TARBALL=str(archive), SMOLVM_ENGINE_VERSION='v1.35.5',
                       OUT_DIR=str(root / 'dist'), TMPDIR=str(temporary), TEST_LIBRARY=library)
            result = subprocess.run(['bash', str(scripts / 'build-smol-dist.sh'), target],
                                    cwd=sdk, env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            with tarfile.open(root / 'dist' / f'smol-1.35.5-{target}.tar.gz') as tar:
                bundled = tar.extractfile(f'smol-1.35.5-{target}/lib/{library}').read()
            self.assertEqual(bundled, original,
                             'Build-time library rewrites must not change shipped release assets')


if __name__ == '__main__':
    unittest.main()
