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
- [10. Let it back up on its own](#10-let-it-back-up-on-its-own)
- [11. Removing it](#11-removing-it)
- [12. Building from source](#12-building-from-source)

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
need either a newer distribution or to build from source (§12).

One optional piece is worth installing by name: the **tray icon** needs
`libayatana-appindicator3` (on older distributions, `libappindicator3`), which is
also loaded at runtime and also not linked:

| Distribution | Package |
|---|---|
| Debian, Ubuntu, Mint, Pop!_OS | `libayatana-appindicator3-1` |
| Fedora, RHEL | `libayatana-appindicator-gtk3` |
| Arch, Manjaro, EndeavourOS | `libayatana-appindicator` |

Without it the app still runs, with its window and no tray icon; `install.sh`
mentions it when it is missing ([§10](#10-let-it-back-up-on-its-own) explains what
the tray is for, and [troubleshooting §18](troubleshooting.md#18-there-is-no-tray-icon)
what it looks like when it is absent).

## 4. Install — Windows

Unzip `android-backup-<version>-windows-x64-portable.zip` anywhere — Desktop,
`%LOCALAPPDATA%\Programs`, a USB stick; it does not care — and run
`android-backup.exe`. There is no installer and nothing is written outside your
own user profile (§8).

Once it is running, the app puts a small icon in the notification area — Windows
may keep it behind the `^` chevron until you drag it out. That icon is how the
program is quit and how the window is brought back after it has been closed
([§10](#10-let-it-back-up-on-its-own)).

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

**Step 3 — should new phones back up on their own?** The welcome panel's last
line is a checkbox: *Back up a phone when I plug it in*. Left unticked — the
default — nothing ever runs by itself and every backup is one you start. Tick it
and phones you plug in from now on back up as soon as they are unlocked; phones
already in the list are not touched, and each device can be set to something else
later (every 6 hours, or off) in its own panel. See
[§10](#10-let-it-back-up-on-its-own).

**Step 4 — pick the phone.** Plug it in and approve the prompt (§6). It appears
in the **Devices** list on the left with its model and Android version, and the
status line says `ready`.

**Step 5 — name it.** The **Name** field is what your backup folder is called.
Give it something you will recognise in a year — "Kitchen phone" beats
`SM-G991B`. Two phones with the same name get the last four characters of their
serial appended (`Kitchen phone_BCDE`), so nothing ever lands on top of
something else.

**Step 6 — confirm the destination.** **Backup to** shows the folder for this
device. **Change…** sets a different one for this device only; **Use the shared
location** puts it back to the global default plus the device's name.

**Step 7 — choose folders.** Tick what you want copied. The defaults are the
usual suspects — `DCIM`, `Pictures`, `Movies`, `Music`, `Documents`, `Download`,
`Android/media`, `Recordings`. Type a name (e.g. `Podcasts`) into the box and
press **Add folder** for anything else; `Remove` deletes a folder you added
yourself. Subfolders are typeable too (`DCIM/Camera`).

Some folders (`Android/data`, `Android/obb`) are protected by Android itself and
cannot be read over ADB on a normal phone — the app will report those files as
failed rather than pretending they were copied.

**Step 8 — Back up now.** The progress panel shows the current phase, a bar, the
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
| Single-instance lock | `~/.local/share/android-backup/instance.lock` | `%APPDATA%\android-backup\instance.lock` |
| Login entry, if turned on | `~/.config/autostart/AndroidBackup.desktop` | a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |

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

## 10. Let it back up on its own

Everything so far has been you pressing **Back up now**. This section is the other
mode: the program keeps running, notices the phone when it appears, and copies what
is new without being asked. It is off until you turn it on, per device, and a phone
you never tick stays a phone you back up by hand.

### The schedule, per device

In the device panel, under **Automatic backups**:

| Choice | What happens |
|---|---|
| **Off** | Nothing runs by itself. The default for every device, including the ones you have been backing up by hand. |
| **When connected** | A backup starts as soon as the phone is connected and usable — once per plug-in. |
| **Every** *n* hours (1–72) | A backup starts that long after the last *attempt* — not the last success, so a phone that keeps failing does not retry itself into a loop. A device with no history at all is due immediately: "every 6 hours" turned on now means "starting now". |

"Connected and usable" is the same gate the Start button uses: USB debugging on, the
prompt on the phone approved, at least one folder ticked, a destination that exists.
The line under the control says what will happen next, in words — *"Next automatic
backup in 2 h 05 m"*, *"An automatic backup is due — it starts as soon as Kitchen
phone is connected."*, *"Starts by itself whenever Kitchen phone is connected."* When
it says *"Not yet: …"*, the reason is the gate's own words, and fixing it takes effect
at the next check — the clock wakes about once a minute — rather than at the next
interval.

A job that ran because you asked for it counts as a run: backing up by hand at 14:00
also satisfies "every 6 hours", so the schedule does not fire again ten minutes later
on top of what you just did.

Three rules that a schedule does not override:

- **Your backup always wins.** If a job is already running — or you started one
  yourself — automation stays out of the way. It does not queue behind you, and it
  does not come back later with a second job.
- **Only automation's own work is continued by automation.** A job that ran out of
  cable, or hit a phone that went to sleep, is resumed when the phone is ready again,
  continuing the *same* job, so only the remainder is copied. A job *you* started and
  interrupted keeps its Resume button to itself: you may have had a reason.
- **Nothing is left flapping.** After a failed attempt the device is left alone for
  two minutes, so a cable that reconnects every few seconds produces no retries; and
  if an automatic job stops twice and cannot be resumed, the program says so and waits
  for a person instead of trying forever.

### The pause switch

**Pause all automatic backups** in the device panel stops every schedule at once —
keeping the schedules themselves — and is the *same* switch as **Pause automatic
backups** in the tray menu, so the two can never disagree. While it is on, the status
bar says `automatic backups paused` and each panel explains that its next run is
waiting. Manual backups work exactly as before. **Resume automatic backups** in the
status bar undoes it.

### The tray icon

The program puts an icon near the clock (Windows: notification area, possibly behind
the `^` chevron; Linux: the tray, and on GNOME it needs an AppIndicator extension —
see [troubleshooting §18](troubleshooting.md#18-there-is-no-tray-icon)). Closing the
window with that icon present **hides** it instead of quitting, and anything running
keeps running — that is what makes "plug it in overnight" work. The menu:

| Item | What it does |
|---|---|
| *Open Android Backup* | shows the window again |
| *Idle — watching for phones* / *Backing up…* / *Needs attention* | a status line, not a button: the same thing the panel says, where you can see it |
| *Back up all connected now* | starts an ordinary backup for every ready phone (the same gate as **Back up now**, and the same result in the history) and says which phones it could not start |
| *Pause automatic backups* / *Resume automatic backups* | the pause switch above |
| *Quit* | stops the program *and* any backup in flight |

**Closing the window is not quitting.** To stop for good, use **Quit** in the tray
menu or **Quit now** in **Background…** in the status bar. A job that is running when
you quit is cancelled the way the Cancel button cancels it: whatever was copied stays
copied and is recorded, the half-file is swept up, and the next run finishes the
remainder — an exit never looks like a crash to the next launch.

### Starting with the system

**Background… → Start in the tray when I log in** registers a login entry that starts
the program hidden, so a scheduled backup is not missed while the window is shut. The
dialog reports what the desktop actually did — `run at login: enabled`, or why not
(`not available (…)`, `could not be changed (…)`) — rather than what you asked for. If
the machine refuses, the tick stays where you put it and your wish is remembered for
the next launch.

Turning it back off removes the entry. So does `./install.sh --uninstall`.

### Notifications

While the window is hidden, the program announces what you would otherwise miss: an
automatic backup starting, a job finishing, and a job that needs attention. These are
system toasts (Windows: Action Center; Linux: your desktop's notification daemon).
They are deliberately quiet — a backup *starting* or *finishing* is not announced
while the window is visible and focused, since the panel is already showing it, and
the same news about the same phone within a minute is collapsed into one toast, so a
phone with a flaky cable cannot fill your screen. Trouble — a pause, a stop, a
"here is why nothing started" — is announced whether or not the window is in front
of you. Nothing depends on them;
the panel, the history and the log are the record. When one does not appear, see
[troubleshooting §17](troubleshooting.md#17-no-notification-appeared-when-a-backup-started-or-finished).

### What automation never does

- It never writes to the phone. Every transfer is phone → computer, in every mode;
  there is no restore.
- It never starts on a device whose schedule is **Off**, and never for a phone that is
  not in the manifest.
- It never deletes or overwrites anything: the two rules in §8 hold exactly as they do
  for a job you started yourself.

## 11. Removing it

The application never installs anything outside your user profile, so removing
it means deleting files:

```sh
# Linux, from the unpacked tarball
./install.sh --uninstall
rm -rf ~/.config/android-backup ~/.local/share/android-backup   # settings, logs, manifest, managed adb
```

`--uninstall` also removes the login entry, if you had turned
"start in the tray when I log in" on. On Windows, delete the folder you unzipped and,
if you want the settings, logs and downloaded platform-tools gone too,
`%APPDATA%\android-backup`; if you had turned on the login entry, remove the
`Android Backup` value under
`HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` in
`regedit` (turning the checkbox off in **Background…** does it for you).

**Your backups are wherever you told the app to put them (§8) and are not
touched by any of this.** Delete them deliberately, when you mean to.

## 12. Building from source

For anyone who wants a build for a different architecture, a newer glibc, or
just to hack on it:

```sh
git clone <repository>
cd android_backup_rs
cargo build --release -p android-backup-desktop
./target/release/android-backup
```

Rust 1.89 or newer (`Cargo.toml`'s `rust-version`; `rust-toolchain.toml` asks for
the `stable` channel, so that is what CI and the release builds use — rustc 1.99.0
when this was written). On Linux you also need a C
compiler and `pkg-config` for the bundled SQLite, plus the X11/Wayland/GTK
development headers for the window, the file dialogs and the tray icon
(`libgtk-3-dev`, or your distribution's equivalent). Packaging helpers live in
`scripts/` (`package-linux.sh`, `build-appimage.sh`, `package-windows.ps1`) and
are what CI runs; [release.md](release.md) explains how a release is cut.

The full test suite — 576 tests, no phone required — is:

```sh
cargo test --workspace
```
