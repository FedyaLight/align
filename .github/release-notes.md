Align synchronizes camera footage and separately recorded audio using waveform
matching, timecode, and recording metadata.

## What's new in @VERSION@

- **Media loads again on Windows.** Adding media on Windows could fail with
  repeated "libwinpthread-1.dll was not found" errors, and reinstalling did not
  help. The bundled FFmpeg tools needed a library the installer did not
  include. They are now self-contained, and every Windows build is checked in a
  clean folder before release.
- **Report a Problem.** Help → Report a Problem… (in the Menu button on Windows
  and Linux), About, and error dialogs open a GitHub bug form with the Align
  version, operating system, and architecture already filled in. You review
  and submit the issue yourself; recordings, file paths, and logs are not sent.
  A small reminder appears once after each installation or update.
- **Stricter CLI arguments.** `align-cli` no longer ignores extra paths after a
  command: `export-json result.json out extra.wav` and `clear-cache extra.wav`
  now stop with a usage error before doing anything.

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
