#!/usr/bin/env python3
"""Prepare exact checkout and vendored Rust sources for a binary release."""
import argparse
import hashlib
import json
import re
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib


def run(*args, **kwargs):
    return subprocess.check_output(args, text=True, encoding="utf-8", **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--platform', required=True, choices=('Linux', 'macOS', 'Windows'))
    parser.add_argument('--licenses', type=Path, required=True)
    parser.add_argument('--ffmpeg', type=Path, required=True)
    parser.add_argument('--aaf', type=Path, required=True)
    parser.add_argument('--velopack-version', required=True)
    args = parser.parse_args()
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', args.velopack_version):
        parser.error('Expected a stable Velopack version such as 1.2.0')
    root = Path(__file__).resolve().parent.parent
    if run('git', 'status', '--porcelain', '--untracked-files=no', cwd=root).strip():
        raise RuntimeError('Release sources require a clean committed checkout')
    ffmpeg_version = (args.ffmpeg / 'version.txt').read_text(encoding="utf-8").strip()
    for name in (f'ffmpeg-{ffmpeg_version}.tar.xz', 'build-ffmpeg-minimal.sh', 'config.mak'):
        if not (args.ffmpeg / name).is_file():
            raise RuntimeError(f'Missing FFmpeg source file: {name}')
    aaf_versions = json.loads((args.aaf / 'versions.json').read_text(encoding="utf-8"))
    if not (args.aaf / 'Licenses/Python-LICENSE.txt').is_file():
        raise RuntimeError('Missing AAF runtime licenses')
    versions = {key: aaf_versions[key] for key in ('pyaaf2', 'pyinstaller', 'pyinstaller-hooks-contrib')}
    versions.update(aaf_versions['native_libraries'])
    versions['Python'] = aaf_versions['python'].split()[0]
    for name, version in versions.items():
        matches = list(args.aaf.glob(f'{name}-{version}.tar.*')) + list(args.aaf.glob(f'{name.replace("-", "_")}-{version}.tar.*'))
        if not matches:
            raise RuntimeError(f'Missing AAF source archive: {name} {version}')
    args.output.mkdir(parents=True, exist_ok=True)
    args.licenses.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='align-release-source-') as tmp:
        report = Path(tmp) / 'about.json'
        run('cargo', 'about', 'generate', '--locked', '--workspace', '--fail',
            '--format', 'json', '-o', str(report), cwd=root)
        licensing = json.loads(report.read_text(encoding="utf-8"))
        lines = ['Rust dependency licenses', '']
        for crate in licensing['crates']:
            package = crate['package']
            lines.extend([f"{package['name']} {package['version']} — {crate['license']}",
                          package.get('repository') or '',
                          ', '.join(package.get('authors', [])), ''])
        for license in licensing['licenses']:
            users = ', '.join(f"{entry['crate']['name']} {entry['crate']['version']}"
                              for entry in license['used_by'])
            lines.extend([license['name'], f'Used by: {users}', license['text'], ''])
        (args.licenses / 'Rust-Licenses.txt').write_text('\n'.join(lines), encoding='utf-8')
        source = Path(tmp) / 'align-source'
        source.mkdir()
        for name in run('git', 'ls-files', cwd=root).splitlines():
            dest = source / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(root / name, dest)
        config = run('cargo', 'vendor', '--locked', '--versioned-dirs', 'vendor', cwd=source)
        (source / '.cargo').mkdir(exist_ok=True)
        (source / '.cargo/config.toml').write_text(config, encoding='utf-8')
        inventory = []
        for crate in sorted((source / 'vendor').iterdir()):
            # Keep complete vendored trees in the source archive, including notices
            # embedded below the crate root and sources shipped by native libraries.
            notices = [p for p in crate.rglob('*') if p.is_file() and
                       p.name.lower().startswith(('license', 'licence', 'copying', 'notice', 'copyright', 'authors'))]
            for p in notices:
                dest = args.licenses / 'Rust' / crate.name / p.relative_to(crate)
                dest.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(p, dest)
            package = tomllib.loads((crate / 'Cargo.toml').read_text(encoding="utf-8"))['package']
            inventory.append({key: package.get(key) for key in
                              ('name', 'version', 'license', 'repository', 'authors')})
        for folder, origin in [('FFmpeg', args.ffmpeg), ('AAF', args.aaf)]:
            if not origin.is_dir() or not any(origin.iterdir()):
                raise RuntimeError(f'Missing {folder} source bundle: {origin}')
            shutil.copytree(origin, source / folder)
        velopack = source / 'Velopack'
        velopack.mkdir()
        archive = velopack / f'velopack-{args.velopack_version}.tar.gz'
        subprocess.run(['curl', '--fail', '--location', '--retry', '3',
                        f'https://codeload.github.com/velopack/velopack/tar.gz/refs/tags/{args.velopack_version}',
                        '--output', str(archive)], check=True)
        with tarfile.open(archive) as tar:
            license = tar.extractfile(f'velopack-{args.velopack_version}/LICENSE')
            if license is None:
                raise RuntimeError('Missing Velopack license')
            (args.licenses / 'Velopack-LICENSE.txt').write_bytes(license.read())
        # Check the shipped vendor configuration, rather than relying on the
        # developer's registry cache to resolve dependencies.
        run('cargo', 'metadata', '--frozen', '--format-version', '1', cwd=source)
        manifest = {
            'commit': run('git', 'rev-parse', 'HEAD', cwd=root).strip(),
            'platform': args.platform,
            'rustc': run('rustc', '-Vv'),
            'cargo': run('cargo', '-V'),
            'rust_packages': inventory,
            'velopack': args.velopack_version,
            'component_files': {},
            'component_binaries': {},
        }
        extension = '.exe' if args.platform == 'Windows' else ''
        for path in [args.ffmpeg.parent / f'ffmpeg{extension}',
                     args.ffmpeg.parent / f'ffprobe{extension}',
                     args.aaf.parent / f'align-aaf{extension}']:
            with path.open('rb') as stream:
                manifest['component_binaries'][path.name] = hashlib.file_digest(stream, 'sha256').hexdigest()
        for folder in ('FFmpeg', 'AAF', 'Velopack'):
            for path in sorted((source / folder).rglob('*')):
                if path.is_file():
                    with path.open('rb') as stream:
                        manifest['component_files'][path.relative_to(source).as_posix()] = hashlib.file_digest(stream, 'sha256').hexdigest()
        shutil.copytree(args.licenses, source / 'ThirdPartyLicenses')
        (source / 'BUILD-MANIFEST.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
        shutil.copy2(source / 'BUILD-MANIFEST.json', args.licenses / 'BUILD-MANIFEST.json')
        archive = args.output / f'align-source-{args.platform}.tar.gz'
        with tarfile.open(archive, 'w:gz') as tar:
            tar.add(source, arcname='align-source')
        with archive.open('rb') as stream:
            digest = hashlib.file_digest(stream, 'sha256').hexdigest()
        archive.with_suffix(archive.suffix + '.sha256').write_text(f'{digest}  {archive.name}\n', encoding='utf-8')
        print(archive)


if __name__ == '__main__':
    main()
