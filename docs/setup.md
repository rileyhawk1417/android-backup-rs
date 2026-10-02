# Setup

Getting from "I have a phone and a laptop" to "my files are on the laptop", on
Windows or Linux, without installing a toolchain, an Android SDK, or Android
Studio.

- [1. What you need](#1-what-you-need)
- [2. Download](#2-download)
- [3. Install — Linux](#3-install--linux)
- [4. Install — Windows](#4-install--windows)
- [5. Let your computer talk to the phone](#5-let-your-computer-talk-to-the-phone)
- [6. Enable USB debugging on the phone](#6-enable-usb-debugging-on-the-phone)
- [7. First launch](#7-first-launch)
- [8. Where everything is stored](#8-where-everything-is-stored)
- [9. The second backup (and every one after it)](#9-the-second-backup-and-every-one-after-it)
- [10. Removing it](#10-removing-it)
- [11. Building from source](#11-building-from-source)

---

## 1. What you need

| | |
|---|---|
| A computer | Windows 10/11 (x64) or Linux (x86_64 desktop) |
| A phone | Android 6 or newer, with a **data** USB cable |
| Internet | only for the first-time adb download (optional — see §7) |
| Disk space | roughly 16 MB for the app, plus room for your files |

You do **not** need Android Studio, the Android SDK, Rust, or `adb` — if you do
not already have `adb`, the app downloads Google's official platform-tools for
you on first launch (§7). If you do already have `adb`, the app will happily use
it instead and leave it alone.

## 2. Download

Grab the artifact for your system from the project's Releases page:

| Platform | File | Notes |
|---|---|---|
| Linux | `android-backup-<version>-linux-x86_64.AppImage` | single file, nothing to unpack |
| Linux | `android-backup-<version>-linux-x86_64.tar.gz` | same binary, plus an `install.sh` |
| Windows | `android-backup-<version>-windows-x64-portable.zip` | one `.exe`, no installer |

Each artifact ships with a `.sha256` next to it. Verifying takes a second and is
the only way to know the download arrived intact:

```sh
# Linux
sha256sum -c android-backup-<version>-linux-x86_64.tar.gz.sha256
```
```powershell
# Windows (PowerShell)
Get-FileHash .\android-backup-<version>-windows-x64-portable.zip -Algorithm SHA256
```

Both artifacts contain the same documentation and the Apache-2.0 / MIT licence
texts. Neither bundles `adb` or any Android platform-tools: `adb` is fetched at
runtime (and kept up to date) by the application itself.

## 3. Install — Linux

### Tarball (recommended)

```sh
tar -xzf android-backup-<version>-linux-x86_64.tar.gz
cd android-backup-<version>-linux-x86_64
./install.sh                    # installs into ~/.local (PREFIX=... to change)
```

`install.sh` puts the binary in `~/.local/bin`, and the launcher entry plus icon
in `~/.local/share`, so the app appears in your application menu. It never needs
root. If `~/.local/bin` is not on your `PATH` the script tells you the line to
add. `./install.sh --uninstall` reverses it.

This is the artifact to prefer: it is the same binary the AppImage wraps, with
no FUSE, no runtime, and nothing that a desktop environment has to cooperate
with.

### AppImage (also available)

```sh
chmod +x android-backup-*-linux-x86_64.AppImage
./android-backup-*-linux-x86_64.AppImage
```

One double-clickable file, which is the whole point of the format. It is
best-effort rather than the primary artifact, because *how* it runs depends on
your machine: it mounts itself with FUSE, and that is the part that goes wrong.
If nothing happens:

```sh
# FUSE is unavailable (a container, or Ubuntu 22.04+ without libfuse2):
sudo apt install libfuse2
# …or run it without mounting:
./android-backup-*-linux-x86_64.AppImage --appimage-extract-and-run
```

If you have **AppImageLauncher** installed, it intercepts the file before our
application runs, usually to offer desktop integration. Either let it integrate
the app, or use the tarball.
Any of these is a reason to prefer the tarball above — the app itself is
identical, and the tarball has no opinion about how it is started.

### Will it run on my distribution?

The binary's only dynamic dependencies are `libc`, `libm`, `libgcc_s` and the
loader — everything else (X11, Wayland, OpenGL, GTK for the file chooser) is
loaded at runtime from your desktop, so any normal desktop environment works.
That leaves exactly one compatibility question: **glibc**.

The released Linux binaries are built inside a pinned older container, and each
release includes a `.deps.txt` recording the floor it got. If the app refuses to
start with a message about `GLIBC_2.xx not found`, that is what happened; you
need either a newer distribution or to build from source (§11).

## 4. Install — Windows

Unzip `android-backup-<version>-windows-x64-portable.zip` anywhere — Desktop,
`%LOCALAPPDATA%\Programs`, a USB stick; it does not care — and run
`android-backup.exe`. There is no installer and nothing is written outside your
own user profile (§8).

**Windows SmartScreen.** The build is not code-signed, so the first launch shows
"Windows protected your PC". Choose **More info → Run anyway**. This is a
deliberate MVP decision, documented in [release.md](release.md): signing
certificates cost money per year and the project has no budget for one yet, so
rather than pretend otherwise the build is unsigned *and* the checksums are
published so you can verify what you downloaded.

## 5. Let your computer talk to the phone

Windows usually works out of the box for Pixel/Google devices; Linux almost
always needs a udev rule, because otherwise the phone is only accessible to
root. Symptoms of a missing rule: the app lists nothing, or `adb devices` says
`no permissions` or `unauthorized` no matter how many times you approve the
prompt.

### Linux

Install the distribution's udev rules, then unplug and replug the phone:

| Distribution | Command |
|---|---|
| Debian, Ubuntu, Mint, Pop!_OS | `sudo apt install android-sdk-platform-tools-common` |
| Arch, Manjaro, EndeavourOS | `sudo pacman -S android-udev` |

The Debian/Ubuntu rules let members of the `plugdev` group use the device, so
also run `sudo usermod -aG plugdev "$USER"` and log out and back in — the group
only applies to new sessions. Arch's `android-udev` uses `uaccess` instead and
needs no group.

If your distribution has no such package, the rule is three lines. Create
`/etc/udev/rules.d/51-android.rules`:

```
# Google / Pixel / Nexus
SUBSYSTEM=="usb", ATTR{idVendor}=="18d1", MODE="0660", GROUP="plugdev", TAG+="uaccess"
# Samsung
SUBSYSTEM=="usb", ATTR{idVendor}=="04e8", MODE="0660", GROUP="plugdev", TAG+="uaccess"
# Xiaomi / Redmi / POCO
SUBSYSTEM=="usb", ATTR{idVendor}=="2717", MODE="0660", GROUP="plugdev", TAG+="uaccess"
# OnePlus
SUBSYSTEM=="usb", ATTR{idVendor}=="2a70", MODE="0660", GROUP="plugdev", TAG+="uaccess"
```

Find your own vendor id with `lsusb` (the part after `ID`, e.g. `18d1:4ee7`),
then:

```sh
sudo chmod 644 /etc/udev/rules.d/51-android.rules
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Unplug and replug the cable afterwards — udev rules are applied when the device
is attached, not retroactively.

### Windows

Windows 10/11 installs a generic ADB interface driver for most phones. If
`android-backup.exe` says no device is connected even though the phone is
plugged in and unlocked, open Device Manager:

- an unknown device appears under **Other devices** → install the manufacturer's
  USB driver (Samsung: "Samsung Android USB Driver"; Google: "Google USB
  Driver" from the SDK page), or any generic ADB driver;
- the phone appears under **Portable Devices** but not as an ADB interface →
  switch the phone's USB mode to **File transfer (MTP)** and replug.

## 6. Enable USB debugging on the phone

1. **Settings → About phone →** tap **Build number** seven times. The phone says
   "You are now a developer!".
   *Samsung:* the build number is under **Settings → About phone → Software
   information → Build number**.
   *Xiaomi/Redmi/POCO:* also enable **Settings → Additional settings →
   Developer options → USB debugging (Security settings)** if you later see
   permission errors while reading folders.
2. **Settings → System → Developer options** (on Samsung: at the bottom of the
   main settings list) → turn on **USB debugging**.
3. Plug the phone in. A dialog appears on the phone: **"Allow USB debugging?"**
   with the computer's key fingerprint. Tick **Always allow from this computer**
   and tap **Allow**.
4. Keep the phone **unlocked** while backing up. A locked phone can still be
   read, but some folders are protected until you unlock it.

If no dialog appears, see [troubleshooting.md](troubleshooting.md) — the fix is
almost always the udev rule in §5 or a revoked authorization.

## 7. First launch

The window opens with a short welcome panel, then (if needed) the adb panel.

**Step 1 — where should backups go?** The suggested location is
`~/AndroidBackups` on Linux and `%USERPROFILE%\AndroidBackups` on Windows.
Choose **Use the suggested folder**, **Choose another…** (any folder, including
an external drive), or **Later** to decide per device.

**Step 2 — adb.** If you already have `adb`, the panel says
`Found: /usr/bin/adb (system)`. If not, you get four routes:

| Button | What it does |
|---|---|
| **Install automatically** | downloads Google's official platform-tools (verified against the checksum Google publishes) into this app's data directory. Your own `adb`, if any, is untouched until the download has been verified and answers `adb version`. |
| **Choose adb…** | pick an existing `adb` executable you already have. |
| **Use the system adb** | search `PATH` again — useful after installing platform-tools yourself. |
| **Try again** | re-run the search and version check. |

**Step 3 — pick the phone.** Plug it in and approve the prompt (§6). It appears
in the **Devices** list on the left with its model and Android version, and the
status line says `ready`.

**Step 4 — name it.** The **Name** field is what your backup folder is called.
Give it something you will recognise in a year — "Kitchen phone" beats
`SM-G991B`. Two phones with the same name get the last four characters of their
serial appended (`Kitchen phone_BCDE`), so nothing ever lands on top of
something else.

**Step 5 — confirm the destination.** **Backup to** shows the folder for this
device. **Change…** sets a different one for this device only; **Use the shared
location** puts it back to the global default plus the device's name.

**Step 6 — choose folders.** Tick what you want copied. The defaults are the
usual suspects — `DCIM`, `Pictures`, `Movies`, `Music`, `Documents`, `Download`,
`Android/media`, `Recordings`. Type a name (e.g. `Podcasts`) into the box and
press **Add folder** for anything else; `Remove` deletes a folder you added
yourself. Subfolders are typeable too (`DCIM/Camera`).

Some folders (`Android/data`, `Android/obb`) are protected by Android itself and
cannot be read over ADB on a normal phone — the app will report those files as
failed rather than pretending they were copied.

**Step 7 — Back up now.** The progress panel shows the current phase, a bar, the
file being copied, and running totals. **Pause** and **Resume** work at any
point; **Cancel** stops the job and leaves the manifest honest about what was
actually copied — nothing is deleted, and the next run finishes the remainder.
Below it, **History** lists every job for this device, newest first, and expands
to the individual files.

## 8. Where everything is stored

| What | Linux | Windows |
|---|---|---|
| Settings | `~/.config/android-backup/config.toml` | `%APPDATA%\android-backup\config.toml` |
| Manifest (SQLite) | `~/.local/share/android-backup/manifest.db` | `%APPDATA%\android-backup\manifest.db` |
| Logs | `~/.local/share/android-backup/logs/android-backup.log` | `%APPDATA%\android-backup\logs\android-backup.log` |
| Managed adb | `~/.local/share/android-backup/platform-tools/` | `%APPDATA%\android-backup\platform-tools\` |

Inside a backup destination, the layout is:

```
~/AndroidBackups/
└── Kitchen phone_BCDE/          <- one folder per device
    ├── DCIM/
    │   └── Camera/IMG_0001.jpg  <- your files, at the phone's path
    ├── .incomplete/             <- in-progress copies, safe to ignore
    └── .backup/history/         <- previous versions of files that changed
        └── DCIM/Camera/1700000000-1a2b3c4d/IMG_0001.jpg
```

Two rules this layout exists to enforce:

- **Nothing on the computer is ever deleted because it was deleted on the
  phone.** Deleting a file from the phone has no effect on your copy.
- **A file is never silently overwritten.** If a file changed on the phone since
  the last backup, the old copy moves into `.backup/history/...` first, so both
  versions exist. The folder name is the job's start time plus a short hash.

## 9. The second backup (and every one after it)

Plug the phone in, pick it in the list, press **Back up now**. Only files that
are new or changed are copied: size and modification time are compared first,
and the app also notices if the local copy has gone missing or was truncated, in
which case it copies it again. Unchanged files are counted as *skipped*, which
is what you want to see.

Interrupting a backup is safe. A copy is only recorded in the manifest after it
has been fully written **and** verified, so an interrupted transfer leaves a
`.part` file in `.incomplete/` and no manifest row; the next run sweeps the stale
partial and copies the file again from scratch.

## 10. Removing it

The application never installs anything outside your user profile, so removing
it means deleting files:

```sh
# Linux, from the unpacked tarball
./install.sh --uninstall
rm -rf ~/.config/android-backup ~/.local/share/android-backup   # settings, logs, manifest, managed adb
```

On Windows, delete the folder you unzipped and, if you want the settings, logs
and downloaded platform-tools gone too, `%APPDATA%\android-backup`.

**Your backups are wherever you told the app to put them (§8) and are not
touched by any of this.** Delete them deliberately, when you mean to.

## 11. Building from source

For anyone who wants a build for a different architecture, a newer glibc, or
just to hack on it:

```sh
git clone <repository>
cd android_backup_rs
cargo build --release -p android-backup-desktop
./target/release/android-backup
```

Rust 1.85 or newer (see `rust-toolchain.toml`). On Linux you also need a C
compiler and `pkg-config` for the bundled SQLite, plus the X11/Wayland/GTK
development headers for the window and file dialogs. Packaging helpers live in
`scripts/` (`package-linux.sh`, `build-appimage.sh`, `package-windows.ps1`) and
are what CI runs; [release.md](release.md) explains how a release is cut.

The full test suite — 400-odd tests, no phone required — is:

```sh
cargo test --workspace
```
