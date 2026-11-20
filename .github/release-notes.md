Align synchronizes camera footage and separately recorded audio using waveform
matching, timecode, and recording metadata.

## What's new in @VERSION@

- **Source clips organized by camera and recorder.** With **Organize imported
  source clips** in the export sheet (`--media-bins` in the CLI), editors open the
  export with the source files grouped by the folder they were copied into:
  Premiere Pro bins, Final Cut Pro keyword collections, and folders in the
  DaVinci Resolve media pool made by the import script. Files Align rendered
  for the timeline, such as drift corrections, get a group of their own.
  Thanks to @yingaboy for suggesting it (#26).
- **Premiere export of a camera whose audio ends early.** Exporting a MOV whose
  audio track is shorter than its video failed with "Cannot decode audio for
  drift correction." It now exports. Thanks to @Manuel36791 for reporting it
  (#27).
- **Exports checked in the editors.** Every export was opened and played back
  in Premiere Pro 2027, Final Cut Pro 12.3 and DaVinci Resolve 21.1, and what
  they showed is fixed:
  - Premiere and Resolve start imported sound only on a frame, so Premiere XML,
    Resolve XML and AAF place it on the nearest frame, within half a frame, as
    other synchronization tools do. Align no longer renders copies of
    recordings to shift them between frames; only drift corrections, and mono
    channel copies for AAF, are rendered.
  - A stereo recording or camera is one stereo clip in Premiere Pro, and a
    multichannel recording one clip in Final Cut Pro. The Resolve import script
    places every recording, and a camera's sound linked to its picture, as one
    clip on a mono, stereo or adaptive track, on the sample nearest its place.
  - Final Cut Pro imports projects without format warnings, multicam angles
    play their sound in time, and recordings that start inside a frame stay
    in place.
  - Takes of one recorder stay on one track, and where two takes overlap, the
    first now ends where the second starts in every format.
  - In edits kept from an imported timeline, Premiere no longer freezes on
    reversed clips, Resolve XML plays speed changes, and the Resolve import
    script keeps dissolves, crossfades and locked tracks.
  - Premiere opens AAF without picture at its frame rate rather than 29.97,
    AAF timecode is drop-frame when the source's is, and AAF cameras share one
    picture track.
  - The usage guide lists which file to open in each editor.
- **Faster synchronization.** Matches that would be discarded anyway are no
  longer refined, FFmpeg reads each file's header once rather than for every
  analysis window, and an AAF project is read once for all its sequences.
  Eight overlapping takes synchronized in 0.85 s instead of 1.34 s, and with
  the decoder Windows and Linux use, six AVCHD clips with AC-3 audio in 175 ms
  instead of 317 ms.
- **More reliable synchronization.** On macOS, a file AVFoundation stops
  reading is read with the bundled decoder instead of stopping the run. A
  clip whose timecode alignment with one partner was rejected now joins the
  next one.

Exports from this version place sound in Premiere Pro, DaVinci Resolve and AAF
on frames: an edit that relied on sub-frame placement in those formats can
move by up to half a frame. Final Cut Pro and the Resolve import script keep
sound on its sample.

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
and a package installation test on Fedora and Arch Linux.
