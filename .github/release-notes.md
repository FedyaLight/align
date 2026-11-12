Align synchronizes camera footage and separately recorded audio using waveform
matching, timecode, and recording metadata.

## What's new in @VERSION@

- **Damaged audio no longer stops synchronization.** On Windows and Linux, one
  file with a damaged audio packet could stop the whole run with "Decode error:
  symphonia: unexpected end of bitstream." The damaged part now becomes silence
  of the same length, and the rest of the file synchronizes at its correct
  offset. When Align's built-in decoder cannot read a file, FFmpeg reads it
  again. A file neither can read is left out with a warning that names it, and
  the other files still synchronize.
- **MXF on macOS.** macOS could not read MXF: Canon XF-AVC, Sony XAVC and
  other MXF clips were left out with "Decode error: invalid PCM." The macOS app
  now includes a minimal FFmpeg (about 7 MB) and uses it only for MXF.
- **Camera metadata XML is skipped.** Selecting camera clips together with the
  XML files cameras write beside them stopped the run with "Add one timeline
  at a time." Only FCP 7 XML and FCPXML documents are now imported as
  timelines.
- **Faster loading.** Each file is inspected with one ffprobe call instead of
  up to three, four files at a time. With eight Canon C70 MXF clips and five
  other files, reading metadata took 1.2 seconds instead of 24, and the
  timecode of the MXF clips, which was missing before, is now read.
- **FCPXML bundles.** `.fcpxmld` bundles from Final Cut Pro 10.6 and later
  import as timelines, and Path Fixer saves a repaired bundle as a bundle.
  Media paths relative to an FCPXML document now resolve from its folder.
- **More accurate offsets on Windows and Linux.** MP3 files were placed 23 to
  25 ms early, and AVCHD clips with AC-3 audio 5.3 ms early; both are now on
  time. MXF is no longer mistaken for MP3 audio. MP4 files with HE-AAC audio,
  which did not match at all, now match, though their offset can still be
  about 60 ms off.

On Windows and Linux, the first synchronization after the update analyses
every file again, because fingerprints from the previous decoder are not
reused.

## Downloads

| Platform | Installer |
| :--- | :--- |
| macOS 15+ (Apple Silicon) | `Align-@VERSION@-macOS-arm64.dmg` |
| Windows 10/11 (x64) | `Align-@VERSION@-Windows-x64-Setup.exe` |
| Fedora (x86_64) | `Align-@VERSION@-Fedora-x86_64-Setup.run` or `Align-@VERSION@-Fedora-x86_64.rpm` |
| Omarchy / Arch Linux (x86_64) | `Align-@VERSION@-Omarchy-x86_64-Setup.run` or `align-@VERSION@-1-x86_64.pkg.tar.zst` |

Every build includes the desktop app, the CLI, the MCP server, and the AAF
module. No separate Python installation is needed.

### macOS

Open the DMG and drag **Align** to **Applications**. The app is ad-hoc signed,
not Developer ID signed or notarized. If macOS says the app is damaged, move it
to `/Applications`, then for a copy downloaded from this release run:

```sh
sudo xattr -dr com.apple.quarantine "/Applications/Align.app"
open "/Applications/Align.app"
```

### Windows

Run `Align-@VERSION@-Windows-x64-Setup.exe`. It installs for the current user,
adds Align to the Start menu, and installs the Microsoft VC++ runtime if needed.
Running it over an earlier version upgrades that installation. The installer is
unsigned, so SmartScreen may ask for confirmation.

### Fedora and Omarchy / Arch

The `.run` installers open a graphical installer in Align's style and ask for
system authorization. Run them as your ordinary user:

```sh
chmod +x Align-@VERSION@-Fedora-x86_64-Setup.run
./Align-@VERSION@-Fedora-x86_64-Setup.run
```

Or install the native package directly with
`sudo dnf install ./Align-@VERSION@-Fedora-x86_64.rpm` or
`sudo pacman -U ./align-@VERSION@-1-x86_64.pkg.tar.zst`. Choose one method; each
`.run` contains the same package. Linux requires glibc 2.39+, a working Vulkan
driver, and GTK 4 with polkit for the graphical installer. Later versions install
from inside Align, behind the same system authorization prompt.

## Sources and verification

`align-source-<platform>.tar.gz` contains the corresponding Align source, locked
and vendored Rust dependencies, the FFmpeg source used for the bundled tools, and
the AAF component sources. License notices are included in every installer.
Align is licensed under GPL-3.0-only.

Each download has a `.sha256` file. Verify from the download directory, for
example `shasum -a 256 -c Align-@VERSION@-macOS-arm64.dmg.sha256`.

Every installer in this release was built and checked by GitHub Actions: tests
and lint on all three platforms, end-to-end synchronization and export checks,
and a package installation test on Fedora and Arch Linux.
