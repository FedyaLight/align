# Distribution

Align's own code is licensed under GPL-3.0-only. Dependencies retain their own
licenses. The full GPL text is in [LICENSE](../LICENSE).

## GitHub releases

A tag named `align-rs-vX.Y.Z` must match the workspace version in Cargo.toml.
The release workflow first runs the checks, then builds each platform's binaries,
source archive, dependency notices, and Velopack installers. It publishes only
after all platform jobs succeed and the source archive checksums pass.

Each release includes:

- Velopack installers and update packages.
- `align-source-Linux.tar.gz`, `align-source-macOS.tar.gz`, and
  `align-source-Windows.tar.gz`, each with a `.sha256` file.

The source archives contain the exact committed Align checkout, Cargo.lock,
vendored Rust dependencies, a Cargo configuration for offline dependency
resolution, and a build manifest. They also contain the FFmpeg source tarball
used for the build, its build script and configuration, the AAF component
sources, Python's compression and crypto library sources, and the source of the
pinned Velopack version. Checksums identify the included component files.

Installers include LICENSE, THIRD_PARTY_NOTICES.txt, AAF-Licenses,
FFMPEG-LICENSE.txt, and ThirdPartyLicenses. The latter contains the generated
Rust license report, upstream notice files, the Velopack license, and the build
manifest. Keep source downloads available alongside the corresponding binaries.
Do not replace an old release's source archive with a newer version.

## Preparing sources locally

Use a clean committed checkout and Python 3.11 or later. Generated files should
go under `target/`. Install the AAF build requirements in a virtual environment.

```sh
cargo install cargo-about --version 0.9.2 --features cli --locked
python script/build-aaf-sidecar.py target/release --release-sources
./script/build-ffmpeg-minimal.sh target/ffmpeg-minimal
python script/prepare-release-source.py target/release-sources \
  --platform macOS \
  --licenses target/release/ThirdPartyLicenses \
  --ffmpeg target/ffmpeg-minimal/Sources \
  --aaf target/release/AAF-Sources \
  --velopack-version 1.2.0
```

Use `Linux` or `Windows` for those platforms. The source preparation script
rejects modified tracked files and missing component sources. It uses
`cargo vendor --locked` and checks dependency resolution with `cargo metadata
--frozen`. Build instructions remain in [development](development.md); the
source archive's `.cargo/config.toml` lets Cargo use its included dependencies.
System SDKs, platform development libraries, and build tools are still required.

`about.toml` records the accepted Rust license choices. The generator fails if a
license cannot be resolved under those choices. Adding a new license requires
review rather than disabling that check. Automated collection is not a legal
opinion about every future dependency or packaging change.

## macOS DMG releases

A standalone macOS release can use a `vX.Y.Z` tag and the local packaging scripts.
This does not trigger the `align-rs-vX.Y.Z` multiplatform installer jobs. Match the
workspace version, commit all changes, and prepare the corresponding sources
with the commands above before packaging. Pass the same AAF and FFmpeg build
directories used by source preparation. Install `dmgbuild==1.6.7` in a Python
virtual environment and set `PYTHON` to that environment’s interpreter for the DMG
step. Layout metadata is written directly without automating Finder:

```sh
AAF_DIR="$PWD/target/release" \
FFMPEG_DIR="$PWD/target/ffmpeg-minimal" \
RELEASE_LICENSES="$PWD/target/release/ThirdPartyLicenses" \
  ./script/package-macos.sh "$PWD/target/macos-package"
./script/create-macos-dmg.sh target/macos-package/Align.app \
  target/Align-macOS-arm64.dmg
```

The DMG contains Align.app and an Applications shortcut with a custom Finder
layout. CLI, MCP, AAF, FFmpeg, and license notices are inside the app bundle.
The packaging script checks the bundle signature, CLI, MCP, and app launch.
Run `script/check-end-to-end.py` against the packaged CLI before publishing.
Upload the DMG, corresponding `align-source-macOS.tar.gz`, and both checksum
files together. Include the macOS installation instructions from the README in
the release notes. Verify uploaded assets before publishing a draft release.

Without `RELEASE_LICENSES`, `package-macos.sh` only makes a development bundle;
it does not collect release sources itself. The app is ad-hoc signed. Developer
ID signing and Apple notarization require separate credentials.

## Component details

FFmpeg and ffprobe are separate executables. The default build excludes GPL and
nonfree components and retains FFmpeg's LGPL license. See
[FFmpeg's license guidance](https://ffmpeg.org/legal.html).

The AAF builder records the Python and package versions and hashes the native
binaries collected by PyInstaller. It includes the Python, pyaaf2, and
PyInstaller license texts, including PyInstaller's distribution exception.
The source bundle records matching native library versions and includes their
licenses. A failure to identify a required version stops source preparation.
