#!/usr/bin/env python3
"""Freeze the AAF bridge for the current OS; the delivered binary needs no Python.

Build environment: install Support/aaf/build-requirements.txt into a venv.
Usage: python script/build-aaf-sidecar.py OUTPUT_DIRECTORY
"""
import argparse
from importlib.metadata import distribution
import shutil
import sysconfig
from pathlib import Path
import subprocess
import sys
import tempfile
import ast
import hashlib
import json
import ctypes
import re
import ssl
import tarfile
import zlib


def download(url, destination):
    subprocess.run(['curl', '--fail', '--location', '--retry', '3',
                    url, '--output', str(destination)], check=True)


def native_version(module, symbol, windows_source, pattern):
    try:
        function = getattr(ctypes.CDLL(module.__file__), symbol)
        function.restype = ctypes.c_char_p
        return function().decode().split(',')[0]
    except (AttributeError, OSError):
        # Official Windows CPython links these libraries statically and does not
        # export their version functions. Its source pins the external versions.
        if sys.platform != 'win32':
            raise
        match = re.search(pattern, windows_source)
        if not match:
            raise RuntimeError(f'Cannot identify bundled {module.__name__} version')
        return match.group(1)


def collect_native_sources(sources, licenses, python_archive, python_version):
    import _bz2
    import _lzma
    with tarfile.open(python_archive) as tar:
        prefix = f'Python-{python_version}/'
        windows_source = tar.extractfile(prefix + 'PCbuild/get_externals.bat').read().decode()
        (licenses / 'Python-ThirdParty.txt').write_bytes(
            tar.extractfile(prefix + 'Doc/license.rst').read())
    bz = native_version(_bz2, 'BZ2_bzlibVersion', windows_source, r'bzip2-([0-9.]+)')
    xz = native_version(_lzma, 'lzma_version_string', windows_source, r'xz-([0-9.]+)')
    openssl = ssl.OPENSSL_VERSION.split()[1]
    z = zlib.ZLIB_RUNTIME_VERSION
    components = [
        ('bzip2', bz, f'https://sourceware.org/pub/bzip2/bzip2-{bz}.tar.gz'),
        ('xz', xz, f'https://tukaani.org/xz/xz-{xz}.tar.gz'),
        ('openssl', openssl, f'https://www.openssl.org/source/openssl-{openssl}.tar.gz'),
        ('zlib', z, f'https://zlib.net/fossils/zlib-{z}.tar.gz'),
    ]
    for name, version, url in components:
        archive = sources / f'{name}-{version}.tar.gz'
        download(url, archive)
        found = False
        with tarfile.open(archive) as tar:
            for member in tar.getmembers():
                filename = Path(member.name).name.lower()
                if member.isfile() and filename.startswith(('license', 'copying', 'notice', 'copyright')):
                    relative = Path(*Path(member.name).parts[1:])
                    if '..' in relative.parts:
                        raise RuntimeError('Unsafe notice path')
                    destination = licenses / name / relative
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(tar.extractfile(member).read())
                    found = True
        if not found:
            raise RuntimeError(f'No license found in {name} sources')
    return {name: version for name, version, _ in components}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--release-sources', action='store_true')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    bundled_binaries = []
    with tempfile.TemporaryDirectory(prefix='align-aaf-build-') as temporary:
        subprocess.run([
            sys.executable, '-m', 'PyInstaller', '--noconfirm', '--clean',
            '--onefile', '--name', 'align-aaf', '--distpath', str(output),
            '--workpath', str(Path(temporary) / 'work'), '--specpath', temporary,
            '--collect-all', 'aaf2', str(root / 'Support/aaf/bridge.py'),
        ], check=True)
        analysis = ast.literal_eval((Path(temporary) / 'work/align-aaf/Analysis-00.toc').read_text(encoding="utf-8"))
        for section in analysis:
            if not isinstance(section, list):
                continue
            for entry in section:
                if isinstance(entry, tuple) and len(entry) == 3 and entry[2] in ('BINARY', 'EXTENSION'):
                    with Path(entry[1]).open('rb') as stream:
                        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
                    bundled_binaries.append({'name': entry[0], 'sha256': digest})
    executable = output / ('align-aaf.exe' if sys.platform == 'win32' else 'align-aaf')
    if not executable.is_file():
        raise RuntimeError('AAF build did not produce an executable')
    licenses = output / 'AAF-Licenses'
    licenses.mkdir(exist_ok=True)
    for package in ('pyaaf2', 'pyinstaller'):
        dist = distribution(package)
        for entry in dist.files or []:
            if Path(entry).name.lower() in ('license', 'copying.txt'):
                shutil.copyfile(dist.locate_file(entry), licenses / (package + '-LICENSE.txt'))
    python_license = Path(sys.base_prefix) / 'Resources' / 'English.lproj' / 'Documentation' / 'LICENSE.txt'
    candidates = [Path(sysconfig.get_path("stdlib")) / "LICENSE.txt", python_license, Path(sys.base_prefix) / 'LICENSE.txt', Path(sys.base_prefix) / 'LICENSE']
    for candidate in candidates:
        if candidate.is_file():
            shutil.copyfile(candidate, licenses / 'Python-LICENSE.txt')
            break
    if args.release_sources:
        sources = output / 'AAF-Sources'
        sources.mkdir(exist_ok=True)
        packages = {name: distribution(name).version for name in
                    ('pyaaf2', 'pyinstaller', 'pyinstaller-hooks-contrib')}
        for name, version in packages.items():
            subprocess.run([sys.executable, '-m', 'pip', 'download', '--no-deps',
                            '--no-binary=:all:', '--dest', str(sources),
                            f'{name}=={version}'], check=True)
        version = '.'.join(map(str, sys.version_info[:3]))
        archive = sources / f'Python-{version}.tar.xz'
        download(f'https://www.python.org/ftp/python/{version}/{archive.name}', archive)
        native = collect_native_sources(sources, licenses, archive, version)
        (sources / 'versions.json').write_text(json.dumps({
            **packages, 'python': sys.version, 'platform': sys.platform,
            'bundled_binaries': bundled_binaries, 'native_libraries': native,
        }, indent=2) + '\n')
        shutil.copytree(licenses, sources / 'Licenses', dirs_exist_ok=True)
        for name in ('pyaaf2-LICENSE.txt', 'pyinstaller-LICENSE.txt', 'Python-LICENSE.txt'):
            if not (licenses / name).is_file():
                raise RuntimeError(f'Missing required AAF license: {name}')
    print(executable)


if __name__ == '__main__':
    main()
