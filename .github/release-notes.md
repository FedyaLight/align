Align synchronizes camera footage and separately recorded audio using waveform
matching, timecode, and recording metadata.

## What's new in @VERSION@

- **DaVinci Resolve XML stays in sync.** Resolve reads a recorder WAV's start
  timecode from its BWF metadata itself, and at 23.976 and 29.97 fps it gets a
  different value than the one Align wrote into the XML. Recorder audio then
  slipped by several seconds, more for later timecodes. The Resolve XML now
  places audio from the start of the file, so it imports exactly like the OTIO
  export. Camera audio also no longer lands one frame ahead of its picture in
  Resolve.
- **No more Windows Defender false positive.** Defender reported the Windows
  build as `Trojan:Win32/Bearfoos.A!ml`. The AAF module no longer unpacks a
  Python runtime to a temporary folder at launch: it now ships as a regular
  program folder with version details, built with a bootloader compiled from
  source.

Re-export Resolve XML files made with an earlier version. Premiere Pro, Final
Cut Pro, OTIO and AAF exports are unchanged.

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
and vendored Rust dependencies, the FFmpeg source used on Linux and Windows, and
the AAF component sources. License notices are included in every installer.
Align is licensed under GPL-3.0-only.

Each download has a `.sha256` file. Verify from the download directory, for
example `shasum -a 256 -c Align-@VERSION@-macOS-arm64.dmg.sha256`.

Every installer in this release was built and checked by GitHub Actions: tests
and lint on all three platforms, end-to-end synchronization and export checks,
and a package installation test on Fedora and Arch Linux.
