# Development

## Prerequisites

- Rust 1.100.0 with rustfmt and Clippy. `rust-toolchain.toml` pins the
  development and release toolchain; rustup installs it when needed.
- FFmpeg and ffprobe on PATH for test fixture generation and portable decoding
  of containers other than WAV/AIFF/MP3/M4A/MOV/MP4. Use a full FFmpeg build for
  fixtures; the packaged minimal build omits the synthetic sources and video
  encoders used by tests.
- Python 3.11 or later for acceptance scripts and AAF fixtures.
- macOS: Xcode command-line tools. The packaged application targets macOS 15+
  on Apple Silicon.
- Windows: the MSVC Rust target and Visual Studio C++ build tools.
- Linux: the desktop development libraries installed by
  [CI](../.github/workflows/align-rs.yml), including XKB, Wayland, ALSA, udev,
  and Vulkan headers.

The workflow is the reference for platform-specific package installation.
Run commands from the repository root.

Cargo checks unused dependencies through the workspace's inherited lint settings.
Run `cargo check --workspace --all-targets` so every production target is included
before removing a reported dependency; platform-specific dependencies still need
the corresponding platform check. Test fixture writers belong in dev-dependencies.

The registry resolver waits three days before selecting newly published package
versions during dependency updates. Versions already in `Cargo.lock` remain usable;
`--locked` builds keep the recorded dependency set. This setting lives in
`.cargo/config.toml` and does not change release dependencies automatically.

## Build

```sh
cargo build --release -p align-cli -p align-gpui -p align-aaf
cargo run -p align-gpui
cargo run -p align-cli -- --help
```

On macOS, `./script/build_and_run.sh` wraps a debug build in a signed
`dist/Align.app` and opens it; `--debug` starts it under LLDB, `--logs` streams
its unified log, and `--verify` checks that it launched.

Binaries are written to `target/release`: `align` and `align-cli`
(with `.exe` on Windows). Build only `align-cli` when desktop support is not
needed. Backend selection is automatic; `ALIGN_BACKEND=portable` selects the
portable implementation on macOS.

### AAF module

```sh
cargo build --locked --release -p align-aaf
```

The Rust helper is `align-aaf` (`align-aaf.exe` on Windows). Keep it beside the
desktop or CLI executable. It is included in installers and uses the same
cancellable process boundary on all platforms. For development, `ALIGN_AAF`
can point to a compatible helper built separately.

The Python bridge in `Support/aaf` is the reference for compatibility checks.
Install `Support/aaf/requirements.txt` in a Python virtual environment to run
those checks and generate AAF fixtures.

## Architecture

| Crate | Responsibility |
| --- | --- |
| `align-core` | Data model, fingerprints, matching, graph solving, time mappings, cache, and timeline writers. No platform media APIs. |
| `align-decode` | Media backends, import orchestration, AAF bridge, audio rendering, export jobs, and media ownership. |
| `align-aaf` | AAF reading, writing, PCM extraction, and path repair. |
| `align-cli` | Argument parsing, synchronization, saved result loading, and common single/batch export settings. |
| `align-gpui` | Desktop state, views, timeline interaction, and background job coordination. |

`Pipeline` expands and inspects inputs, obtains cached or newly decoded
fingerprints, matches candidates, refines them with audio windows, and solves
synchronization groups. `MediaBackend` separates native Apple media access from
portable decoding. Both feed the same core algorithms. QuickTime/MP4 headers
are read by `isobmff` (probe, timecode, AAF link metadata), linear PCM in those
files by `mp4pcm`, and camera-file export copies video through `remux`; FFmpeg
serves only the remaining containers.

Export assembles a writer-neutral timeline, plans every prepared audio file
(drift corrections and AAF channel stems), renders
the plan with progress, retains temporary sources, and invokes format writers.
A prepared file's name ends with a hash of everything it was rendered from, so
an existing file is reused as is; bump `AUDIO_SIDECAR_REVISION` in
`align-decode/src/export.rs` when rendering changes. `media_assets` owns
extracted AAF audio across the temporary-to-saved boundary. Desktop workers
report indexed results and use cancellation flags; generation checks reject
stale UI updates.

Every generated file is written through `align_core::staged`: a temporary
sibling replaces the destination only once complete, so readers never see a
partial file and a failed write keeps the previous one. Helper processes
(FFmpeg, the AAF module) start through `align_decode::process`, which hides
console windows on Windows and stops them on cancellation.

Keep algorithms in `align-core`, media I/O in `align-decode`, and presentation
state in the frontend. Preserve existing serialized field names when changing
internal names. Test behavior at the shared layer used by CLI and desktop.

`Support/` contains the application icon, macOS privacy manifest, and AAF bridge
with its pinned Python requirements. `script/` contains build, fixture, and
verification entry points. Generated files belong in `target/`; `cargo clean`
removes them, including local packages and acceptance output. Preserve any
results you need outside that directory before cleaning.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo doc --workspace --no-deps
cargo test --workspace
python -m unittest discover -s Support/aaf -v
cargo build --release -p align-cli -p align-aaf
python script/check-end-to-end.py target/release/align-cli target/acceptance
```

Run the Python commands in the AAF virtual environment. The acceptance output
directory must not already exist. On Windows, pass `target/release/align-cli.exe`.
The CLI must have the AAF module beside it; FFmpeg and ffprobe must be on PATH
to generate fixtures.
CI also builds API documentation with `RUSTDOCFLAGS="-D warnings"` to catch
broken Rust documentation links.

The acceptance runner checks general release behavior, basic editing
preservation, linked AAF, QuickTime and MXF OP-Atom picture links, embedded mono,
embedded stereo, mixed edit rates, and saved-result/media lifetime. It retains per-check logs and
a `verification.json` containing status and the tested CLI's SHA-256. It does not
open desktop or editor applications.

Tests that require external media tools need those tools installed even when
only the native backend is being developed. Without FFmpeg those tests are
skipped with a notice; set `ALIGN_REQUIRE_FFMPEG=1`, as CI does, to make a
missing tool fail them instead. CI runs the same acceptance command
on macOS, Windows, and Linux. Release tags matching `v*` also build and
publish the installers.

### Performance

```sh
cargo build --release -p align-cli
python script/bench-sync.py target/release/align-cli target/bench
```

The benchmark generates cameras with known offsets (MOV/MP4 with PCM or AAC,
timecode) and two split recorder takes, then reports cold and warm synchronization
time, the number of FFmpeg/ffprobe processes started, and each clip's offset
error against the generated ground truth. On macOS, compare both backends on
the same files by running again with a new output directory and
`--corpus target/bench --backend apple-native` (the benchmark defaults to
`portable`). Each report includes the CLI version and wall seconds for inspect,
fingerprint, match, refine and solve. To print the same timings to stderr for a
single sync, set `ALIGN_PHASE_TIMING=1`. Cold and warm refer to Align’s analysis
cache. The operating system can cache file data in both cases.
`--cameras`, `--duration`, and
`--repeat` scale it. Compare results only between runs on the same machine.

### Drift diagnostics

The `drift_audit` example measures additional audio windows for matched pairs in
a single saved result:

```sh
cargo run --release -p align-decode --example drift_audit -- result.json > windows.csv
```

It requires the referenced recordings and uses the same correlation algorithm as
the application. It is a diagnostic, not an independent measurement of device
clocks. See [drift correction](drift.md) for interpretation.

### Editor readback tools

The remaining scripts support checks that require an editor installation or a
a saved synchronization result. Their module docstrings and `--help` describe inputs.

| Scripts | Purpose |
| --- | --- |
| `generate-premiere-frac-fixture.py`, `build-frac-result.py` | Generate known fractional-placement media and saved results. |
| `verify-premiere-frac.py`, `verify-premiere-project.py` | Read saved Premiere projects and compare placements. |
| `check-premiere-render.py` | Compare rendered audio markers with independent ground truth. |
| `verify-resolve-precision.py` | Read a named Resolve timeline through its scripting interface. |
| `check-corpus-offset.py` | Compare exported XML placement with a saved synchronization result. |
| `generate-search-accuracy-fixture.py` | Generate shared-band audio and an unrelated recording. |

These checks have narrower supported structures than a full editor project.
A project-file readback and an acoustic render test prove different things;
record which one was performed. Keep local recordings, project files, screenshots,
and benchmark output out of version control.

Resolve readback requires its `Scripting/Modules` directory on `PYTHONPATH`
and a running editor with scripting enabled. It checks the named project and
timeline before reading them. It is not part of the unattended CLI test suite.

## Packaging

```sh
./script/package-macos.sh /path/to/new/Align-macOS
```

The script builds the application, CLI, and AAF module; includes
the privacy manifest and license files;
signs ad hoc; and runs a brief launch check. The output directory must not exist.
It produces a local macOS bundle, not a notarized installer.

Set `ALIGN_SKIP_LAUNCH_CHECK=1` to package without opening the desktop app.
Bundle validation and the CLI checks still run.

The macOS bundle has no FFmpeg: AVFoundation decodes media, and camera-file
export and AAF picture metadata read QuickTime/MP4 natively. `BUNDLE_FFMPEG=1`
adds the minimal sidecars for `ALIGN_BACKEND=portable`; set `FFMPEG_DIR` to
reuse a previous build. Linux and Windows installers include them. To build the
sidecars:

```sh
./script/build-ffmpeg-minimal.sh target/ffmpeg-minimal
```

That build requires a C toolchain, make, curl, tar, xz, and NASM where applicable.

On Windows, use MSYS2 MINGW64 to build the sidecars, then native Windows Python
to run `python script/check-windows-sidecars.py target/ffmpeg-minimal`. The check
runs isolated copies with a clean PATH, preventing the build machine's MSYS2
libraries from hiding missing runtime dependencies.

For a tag named `vX.Y.Z` matching the workspace version, the release workflow
builds the macOS DMG, the Windows installer with its Velopack update feed, and
Fedora and Arch packages with their graphical installers, and publishes them with
matching source archives in a GitHub Release. See [distribution](distribution.md)
for the installers and the source and license workflow.

Release builds check for a newer release at launch (`crates/align-gpui/src/updater.rs`)
and show an update card; **Align → Check for Updates…** does the same on demand.
Windows Setup installations update through the Velopack feed. Otherwise Align reads
the latest GitHub release: the macOS app downloads the DMG and replaces its bundle
when its folder is writable, and the Fedora and Arch packages download the package
and install it with `pkexec dnf`/`pkexec pacman`. Each download is checked against
its `.sha256` asset. Any other copy (a portable build, the app run from the DMG)
gets a link to the release page. Development builds and runs with
`ALIGN_NO_UPDATE_CHECK` set skip the launch check. The GitHub requests are not
authenticated, so release assets must be publicly accessible. Distribution code
signing and Apple notarization still require release credentials and are not
configured in this repository.

## Repository conventions

Documentation and user-facing text are in English. Describe the current behavior
and its limitations, not migration history or local development sessions. Keep
reusable fixtures and checks with the code; write generated artifacts under
`target/` or an ignored local directory. Avoid committing absolute machine paths,
editor projects, or dated acceptance reports.

The workspace uses GPL-3.0-only. Contributions must be compatible with that
license. Preserve third-party notices and follow the
[distribution notes](distribution.md) when publishing binaries.
