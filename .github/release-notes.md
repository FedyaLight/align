Align synchronizes camera footage and separately recorded audio using waveform
matching, timecode, and recording metadata.

## What's new in @VERSION@

- **Projects that save themselves.** Align creates a `.align` project when
  synchronization starts and keeps it up to date as you work. Reopen it from
  **File → Recent Projects** to get back your sources, settings, relinks,
  corrections and placements; a timeline whose sources have not changed can be
  exported right away. **Save As…** keeps a named copy, autosaves are removed
  after seven days without use, and on Windows `.align` files open in Align
  from Explorer. Thanks to @REVPKD for suggesting and prototyping project
  recovery in [#33](https://github.com/FedyaLight/align/pull/33).
- **Unmatched clips placed by the camera's clock.** Sony cameras record their
  serial number and when each clip started. An unmatched clip recorded between
  two clips of the same camera that synchronized by sound can now be placed
  approximately from that clock: one clip at a time from its menu, with
  **Place All**, or with `--camera-clock` in the CLI. Placed clips are named
  `[CAMERA CLOCK]` and stay unmatched, and nothing is placed unless you ask.
  The usage guide explains which clips qualify and how accurate the place is.
  Thanks to @REVPKD for proposing and prototyping it in
  [#31](https://github.com/FedyaLight/align/issues/31) and
  [#32](https://github.com/FedyaLight/align/pull/32).
- **AAF built into Align.** The AAF module is now written in Rust and installed
  beside the app and CLI, replacing the bundled Python runtime. AAF export can
  also link VC-3 (DNxHD/DNxHR) picture in MXF OP-Atom files: DaVinci Resolve
  Studio 21.1 opens it, while Premiere Pro 27.0 left the tested DNxHR LB media
  offline. A damaged AAF now reports what the reader rejected.
- **A timeline that shows what it knows.** Each clip carries a mark for its
  state: a link when synchronized, a clock when placed by camera clock, a
  warning sign when unmatched. Clips stay striped until the analysis settles,
  so states read without relying on color. Rest the pointer on a clip to see
  how it got its place, with confidence and drift for a waveform match, its
  position and its source timecode. Imported timelines show which method
  synchronized each clip instead of marking all of them as waveform matches.
  Clips in dark mode use quieter shades with readable names, and ruler labels
  at 29.97 fps and other fractional rates no longer drift from their ticks.
- **Desktop app.** On Windows and Linux the menu is a sidebar, which Alt
  pressed on its own also opens. Delete removes the selected sources (the
  files stay on disk), Cmd or Ctrl with =, − and 0 zoom the timeline, and
  Escape closes every dialog. Export defaults are one **Defaults** menu. The
  export sheet remembers the last folder, and if an export with the same name
  is already there, Align says so before writing anything instead of
  replacing it. The timeline highlights while files are dragged over it.
- **Analysis cache.** By default, cached analysis is now removed after seven
  days without use, instead of 30 days after it was written, so media you work
  on every day is not analyzed again. **File → Analysis Cache** can also turn
  the cache off.
- **Faster synchronization and export.** Sequences of one project share the
  inspection and analysis of their media, in the app and with
  `--all-sequences`. On macOS, WAV and linear PCM movies are refined with
  eight workers instead of four. Repeat exports on Windows no longer read every
  source file again, multi-sequence Premiere projects are built in linear
  time, and prepared audio renders on a queue, longest file first.
- **Fixes.**
  - FCP 7 XML sequences were listed in reverse, so `--sequence 1` imported the
    last one.
  - Some HE-AAC recordings, including Apple's, are now decoded at their full
    sample rate and from their recorded start.
  - A drift correction of a Broadcast Wave recording close to 4 GiB failed. It
    is now written as RF64, and the audio is written once instead of twice.
  - A run that read Path Fixer redirections while another run saved them
    could erase them.
  - The Fedora installer installs through DNF over SSH or without GTK 4
    instead of failing.

**The MCP server is removed.** `align-mcp` is no longer included; remove it
from your MCP client's configuration. Agents run `align-cli` directly:
`align-cli sync --output-json result.json --summary /path/to/media` saves the
result and prints a short JSON summary. **Align → Use with AI Agents…** copies
the installed CLI path and a prompt.

## Downloads

| Platform | Installer |
| :--- | :--- |
| macOS 15+ (Apple Silicon) | `Align-@VERSION@-macOS-arm64.dmg` |
| Windows 10/11 (x64) | `Align-@VERSION@-Windows-x64-Setup.exe` |
| Fedora (x86_64) | `Align-@VERSION@-Fedora-x86_64-Setup.run` or `Align-@VERSION@-Fedora-x86_64.rpm` |
| Omarchy / Arch Linux (x86_64) | `Align-@VERSION@-Omarchy-x86_64-Setup.run` or `align-@VERSION@-1-x86_64.pkg.tar.zst` |

Every build includes the desktop app, the CLI, and the AAF
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
a package installation test on Fedora and Arch Linux, and an AAF check of every
installed package.
