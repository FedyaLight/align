# Distribution

Align's own code is licensed under GPL-3.0-only. Dependencies retain their own
licenses. The full GPL text is in [LICENSE](../LICENSE).

## GitHub releases

Push a tag named `vX.Y.Z` that matches the workspace version in Cargo.toml.
The release workflow runs the checks, then builds each platform's binaries,
source archive, dependency notices, and installers. Fedora and Arch packages are
installed and run in clean containers. The release is published only after every
job succeeds and all checksums pass; its notes come from
`.github/release-notes.md`. Running the workflow manually on a branch builds the
same installers as workflow artifacts without publishing anything.

Each release includes:

| Platform | Installer | Built by |
| --- | --- | --- |
| macOS (Apple Silicon) | `Align-X.Y.Z-macOS-arm64.dmg` | `package-macos.sh`, `create-macos-dmg.sh` |
| Windows x64 | `Align-X.Y.Z-Windows-x64-Setup.exe` and the Velopack update feed | `package-windows.sh` |
| Fedora x86_64 | `Align-X.Y.Z-Fedora-x86_64.rpm` and `-Setup.run` | `package-fedora.sh`, `package-fedora-gui.sh` |
| Omarchy / Arch x86_64 | `align-X.Y.Z-1-x86_64.pkg.tar.zst` and `Align-X.Y.Z-Omarchy-x86_64-Setup.run` | `package-arch.sh`, `package-arch-gui.sh` |

Also `align-source-Linux.tar.gz`, `align-source-macOS.tar.gz`, and
`align-source-Windows.tar.gz`, each with a `.sha256` file like every installer.

The installers share Align's look: the DMG window has a drag-to-Applications
layout, the Windows installer shows an animated splash (`Support/Installer`),
and the Linux `.run` files open a GTK 4 installer (`Support/Linux/installer.c`)
in the app's dark theme that installs the native package through polkit.
`script/render-installer-artwork.swift` regenerates the Windows artwork and icon
on macOS. The Windows executable carries the app icon and version details
(`crates/align-gpui/build.rs`), and only Windows installs update through
Velopack; Linux packages update through the package manager.

The source archives contain the exact committed Align checkout, Cargo.lock,
vendored Rust dependencies, a Cargo configuration for offline dependency
resolution, and a build manifest. Linux and Windows archives also contain the
FFmpeg source tarball used for the build, its build script and configuration;
macOS builds ship no FFmpeg. All archives contain the AAF component
sources, Python's compression and crypto library sources, and the source of the
pinned Velopack version. Checksums identify the included component files.

Installers include LICENSE, THIRD_PARTY_NOTICES.txt, AAF-Licenses,
ThirdPartyLicenses, and on Linux and Windows FFMPEG-LICENSE.txt. The latter contains the generated
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
  --platform Linux \
  --licenses target/release/ThirdPartyLicenses \
  --ffmpeg target/ffmpeg-minimal/Sources \
  --aaf target/release/AAF-Sources \
  --velopack-version 1.2.0
```

Use `Windows` for that platform. For `macOS`, skip the FFmpeg build and omit
`--ffmpeg`. The source preparation script
rejects modified tracked files and missing component sources. It uses
`cargo vendor --locked` and checks dependency resolution with `cargo metadata
--frozen`. Build instructions remain in [development](development.md); the
source archive's `.cargo/config.toml` lets Cargo use its included dependencies.
System SDKs, platform development libraries, and build tools are still required.

`about.toml` records the accepted Rust license choices. The generator fails if a
license cannot be resolved under those choices. Adding a new license requires
review rather than disabling that check. Automated collection is not a legal
opinion about every future dependency or packaging change.

## Packaging locally

The packaging scripts also run outside CI on a staged release directory (the
`dist-bundle` layout the workflow builds). The DMG needs macOS and
`dmgbuild==1.6.7` in a virtual environment passed as `PYTHON`:

```sh
AAF_DIR="$PWD/target/release" \
RELEASE_LICENSES="$PWD/target/release/ThirdPartyLicenses" \
  ./script/package-macos.sh "$PWD/target/macos-package"
./script/create-macos-dmg.sh target/macos-package/Align.app \
  target/Align-macOS-arm64.dmg
```

The Fedora scripts need `rpmbuild` and GTK 4 development files; the Arch script
needs `makepkg` and runs as an unprivileged user. The Windows installer needs the
`vpk` tool and can be built on any OS. Without `RELEASE_LICENSES`,
`package-macos.sh` only makes a development bundle. The app is ad-hoc signed.
Developer ID signing and Apple notarization require separate credentials.

## Component details

FFmpeg and ffprobe are separate executables, shipped on Linux and Windows for
containers Align does not read natively (MTS, MXF, R3D, and compressed audio
such as AC-3). The build has no video decoders: video is only parsed for timing.
It excludes GPL and nonfree components and retains FFmpeg's LGPL license. See
[FFmpeg's license guidance](https://ffmpeg.org/legal.html).

The AAF builder records the Python and package versions and hashes the native
binaries collected by PyInstaller. It includes the Python, pyaaf2, and
PyInstaller license texts, including PyInstaller's distribution exception.
The source bundle records matching native library versions and includes their
licenses. A failure to identify a required version stops source preparation.
