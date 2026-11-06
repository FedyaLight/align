#!/usr/bin/env python3
"""Build a local experimental Rune .app (no installer or auto-updater)."""
import argparse
from pathlib import Path
import plistlib
import shutil
import subprocess
import tomllib
import platform

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('output', type=Path, help='New .app directory; must not exist')
parser.add_argument('--no-build', action='store_true', help='Package the existing measured release executable')
args = parser.parse_args()
if platform.system() != "Darwin": parser.error("macOS packaging requires macOS")
root = Path(__file__).resolve().parent.parent
if args.output.exists() or args.output.suffix != '.app':
    parser.error('output must be a new .app directory')
if not args.no_build:
    subprocess.run(['cargo','build','--locked','--release','-p','align-rune'],cwd=root,check=True)
version = tomllib.loads((root/"Cargo.toml").read_text())["workspace"]["package"]["version"]
contents = args.output / 'Contents'
(contents / 'MacOS').mkdir(parents=True)
(contents / 'Resources').mkdir()
shutil.copy2(root/'target/release/align-rune',contents/'MacOS/align-rune')
shutil.copy2(root/'Support/AppIcon.icns',contents/'Resources/AppIcon.icns')
shutil.copy2(root/'Support/PrivacyInfo.xcprivacy',contents/'Resources/PrivacyInfo.xcprivacy')
for name in ('LICENSE','THIRD_PARTY_NOTICES.txt'):
    shutil.copy2(root/name,contents/'Resources'/name)
shutil.copytree(root/'Support/Rune', contents/'Resources/Rune-Licenses')
with (contents/'Info.plist').open('wb') as f:
    plistlib.dump(dict(CFBundleExecutable='align-rune',CFBundleIconFile='AppIcon',
                      CFBundleIdentifier='com.align.rune.experiment',CFBundleName='Align Rune',
                      CFBundlePackageType='APPL',CFBundleShortVersionString=version,CFBundleVersion='1',
                      LSMinimumSystemVersion='15.0',NSHighResolutionCapable=True,
                      NSPrincipalClass='NSApplication'),f)
subprocess.run(['codesign','--force','--deep','--sign','-',str(args.output)],check=True)
print(args.output.resolve())
