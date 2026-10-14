# Troubleshooting

The problems people actually hit, in the order they hit them. Most of these end
at "look at the log file" — [§15](#15-reading-the-log-and-what-to-send-in-a-bug-report)
says where it is and what to include.

- [1. The phone never appears in the device list](#1-the-phone-never-appears-in-the-device-list)
- [2. The phone shows as `unauthorized`](#2-the-phone-shows-as-unauthorized)
- [3. `no permissions (user in plugdev group; are your udev rules wrong?)`](#3-no-permissions-user-in-plugdev-group-are-your-udev-rules-wrong)
- [4. `adb server version doesn't match this client`](#4-adb-server-version-doesnt-match-this-client)
- [5. adb is missing, or will not run](#5-adb-is-missing-or-will-not-run)
- [6. The backup will not start](#6-the-backup-will-not-start)
- [7. The destination drive went away mid-backup](#7-the-destination-drive-went-away-mid-backup)
- [8. `not enough free space`](#8-not-enough-free-space)
- [9. The app or the machine died mid-backup](#9-the-app-or-the-machine-died-mid-backup)
- [10. A file changed on the phone — where is the old copy?](#10-a-file-changed-on-the-phone--where-is-the-old-copy)
- [11. I deleted files on the phone; the computer still has them](#11-i-deleted-files-on-the-phone-the-computer-still-has-them)
- [12. Two devices share history, or a rename split it in two](#12-two-devices-share-history-or-a-rename-split-it-in-two)
- [13. "The backup manifest could not be read"](#13-the-backup-manifest-could-not-be-read)
- [14. Some folders always fail](#14-some-folders-always-fail)
- [15. Reading the log, and what to send in a bug report](#15-reading-the-log-and-what-to-send-in-a-bug-report)
- [16. Windows SmartScreen, and Linux artifacts that will not start](#16-windows-smartscreen-and-linux-artifacts-that-will-not-start)
- [17. No notification appeared when a backup started or finished](#17-no-notification-appeared-when-a-backup-started-or-finished)
- [18. There is no tray icon](#18-there-is-no-tray-icon)
- [19. A backup started by itself, or will not start by itself](#19-a-backup-started-by-itself-or-will-not-start-by-itself)
- [20. A backup stopped half-way and did not continue](#20-a-backup-stopped-half-way-and-did-not-continue)
- [21. Starting it a second time says it is already running](#21-starting-it-a-second-time-says-it-is-already-running)

---

## 1. The phone never appears in the device list

Work down this list; each step tells you whether the next one is worth doing.

1. **Is the cable a data cable?** Charge-only cables are extremely common and
   look identical. Try another cable before anything else.
2. **Is the phone unlocked?** Locked phones still advertise themselves, but the
   authorization prompt cannot be approved.
3. **Does the computer see the hardware at all?**
   - Linux: `lsusb` — look for your phone's vendor (e.g. `18d1` Google,
     `04e8` Samsung, `2717` Xiaomi).
   - Windows: Device Manager — look for an unknown device, or "Android ADB
     Interface" in the list.
   - If the hardware is missing, it is a cable, port, or driver problem
     ([setup §5](setup.md#5-let-your-computer-talk-to-the-phone)).
4. **Does `adb` see it?** This separates an adb problem from an app problem.
   Use **the same adb the app uses** — on Linux, if the app installed its own:
   `~/.local/share/android-backup/platform-tools/adb devices -l`. On Windows:
   `%APPDATA%\android-backup\platform-tools\adb.exe devices -l`.

   | Output | Meaning | Go to |
   |---|---|---|
   | nothing listed | hardware/driver/udev | [§3](#3-no-permissions-user-in-plugdev-group-are-your-udev-rules-wrong), [setup §5](setup.md#5-let-your-computer-talk-to-the-phone) |
   | `unauthorized` | the phone has not approved this computer | [§2](#2-the-phone-shows-as-unauthorized) |
   | `no permissions` | udev rules (Linux) | [§3](#3-no-permissions-user-in-plugdev-group-are-your-udev-rules-wrong) |
   | `offline` | adb's connection is stale | replug, or `adb kill-server` |
5. **Restart the adb server**:
   `<adb> kill-server` and replug the phone. The app polls every few seconds, so
   the list updates on its own.

## 2. The phone shows as `unauthorized`

The phone must approve this specific computer once.

1. Unlock the phone and replug it. You should see **"Allow USB debugging?"**.
   Tick **Always allow from this computer** and tap **Allow**.
2. No dialog? Open **Developer options → Revoke USB debugging authorizations**,
   then replug. This clears the stored approvals and forces the prompt again.
3. Still nothing? Delete this computer's adb key so a new one is offered:
   - Linux: remove `~/.android/adbkey` and `~/.android/adbkey.pub`, then
     `adb kill-server` and replug.
   - Windows: remove `%USERPROFILE%\.android\adbkey` and `adbkey.pub`, then
     `adb kill-server` and replug.
4. The app shows this state as a disabled **Back up now** button with the
   message *"device … is not authorized"*. It will not silently copy nothing.

## 3. `no permissions (user in plugdev group; are your udev rules wrong?)`

Linux only, and the single most common Linux problem: the phone is visible to
root and to nothing else. Install the udev rules and unplug/replug —
[setup §5](setup.md#5-let-your-computer-talk-to-the-phone) has the commands for
Debian/Ubuntu and Arch, plus the manual rule file if your distribution has no
package for it.

Two details that catch people out:

- **Group membership only applies to new sessions.** After
  `sudo usermod -aG plugdev "$USER"` you must log out and back in (or reboot);
  `groups` should list `plugdev`.
- **udev applies rules when the device is attached.** Reloading the rules is not
  enough — unplug and replug the cable.

## 4. `adb server version doesn't match this client`

There are two different `adb` binaries on the machine: one is already running as
a server (started by Android Studio, a system package, or an earlier session),
and the app's `adb` is a different build. They cannot talk to each other.

The app detects this and shows a **server conflict** panel with the two options
that fix it:

1. Stop the running server: `<adb> kill-server`, then **Try again**. The app's
   adb then starts the server itself.
2. Or tell the app to use that other adb instead: **Choose adb…** and pick the
   binary whose version matches the running server.

If you want to avoid this permanently, pick one adb and stay with it. A phone
that works with Android Studio's adb and one that works with the system package
are usually the same phone — the versions just have to agree.

## 5. adb is missing, or will not run

The app shows an **adb** panel with the exact error and four buttons; see
[setup §7](setup.md#7-first-launch) for what each one does. Specific cases:

- **"adb was found at … but could not be run"** — the file is not executable, or
  it is not an adb (a stale symlink, a partially downloaded archive). Either
  `chmod +x` it, or use **Install automatically**.
- **The app installed adb, but you also have an older one** — that is fine and
  intentional: the managed copy takes precedence because it is the one whose
  version the app verified. Switch to your own with **Choose adb…** if you
  prefer.
- **Offline machine, or a blocked download** — the app needs internet only for
  the managed install. Install platform-tools yourself from Google's
  `platform-tools` archive, then use **Choose adb…** and point at the binary.
- **A proxy** — the download respects the usual environment variables
  (`HTTPS_PROXY`/`HTTP_PROXY` on Linux, the system proxy on Windows).

## 6. The backup will not start

The **Back up now** button is disabled and says why, inline. The messages map to:

| Message | Fix |
|---|---|
| *Select a device first.* | Pick a phone in the **Devices** list. |
| *This device is not in the manifest yet — waiting for the first poll.* | Wait a few seconds; the app is waiting to record the device. |
| *A backup is already running (…).* | One job per device at a time. Let it finish, or **Cancel** it. |
| *No backup location is configured — choose one in the device panel.* | **Change…** next to **Backup to**. |
| *adb is not available, so backups cannot start.* | [§5](#5-adb-is-missing-or-will-not-run). |
| *device … is not authorized …* | [§2](#2-the-phone-shows-as-unauthorized). |
| *no device is connected …* | [§1](#1-the-phone-never-appears-in-the-device-list). |
| *Select at least one folder before starting.* | Tick a folder, or add one. |

If nothing is wrong and the button still refuses, the log ([§15](#15-reading-the-log-and-what-to-send-in-a-bug-report))
records the exact reason the UI is gating on.

## 7. The destination drive went away mid-backup

An external drive that is unplugged, unmounted, or that slept during a long
copy. The job **pauses** rather than failing: the message names the drive, and
the file it was copying is not recorded as copied. Reconnect the drive and press
**Resume**; already-completed files are not re-copied.

At start-up the app checks the destination before copying anything (so a missing
drive fails immediately, with *"the backup location … does not exist (is the
drive still connected?)"*, instead of after ten minutes of scanning).

## 8. `not enough free space`

The app compares the planned bytes against the free space of the destination
before the copy starts. Free some space, or point **Backup to** at another
drive. Note that the check is a prediction from the phone's reported file sizes:
a phone whose files grow during the backup can still run out, in which case the
individual file fails and the job finishes with a failure count rather than
pretending it copied everything.

## 9. The app or the machine died mid-backup

Nothing is lost and nothing is corrupt; the next run fixes itself up:

- Files being copied live in `<destination>/<device>/.incomplete/*.part` and are
  only moved into place (and recorded in the manifest) after the copy is
  complete and verified. An interrupted file is therefore simply not recorded.
- On start-up the app closes any job left open by the crash, sweeps stale
  `.part` files, and the next backup copies the unfinished files again.
- The manifest is a SQLite database in WAL mode, so a hard power loss leaves it
  either at the last committed transaction or recoverable — SQLite handles that
  part, not the app.

You do not need to delete anything by hand. If you want to, `.incomplete/` is
safe to remove while the app is closed.

## 10. A file changed on the phone — where is the old copy?

Nowhere was overwritten. Before replacing a file, the app moves the existing one
into the history tree:

```
<destination>/<device>/.backup/history/<original path>/<job start time>-<hash8>/<file>
```

So `DCIM/Camera/IMG_0001.jpg` that changed on 2026-03-01 leaves its previous
copy at, for example,
`.backup/history/DCIM/Camera/1772400000-1a2b3c4d/IMG_0001.jpg`. The timestamp is
the job's start time in Unix seconds and the hash is derived from the version,
which is what stops two history entries from colliding. To restore, copy the
file back out of `.backup/history/...`.

The **History** panel in the app shows each job and, expanded, the individual
files it copied or failed — including rows whose file was later versioned.

## 11. I deleted files on the phone; the computer still has them

That is by design: deleting a file on the phone never deletes your copy. The
computer's copy is the backup; a phone-side accident is exactly the case it
exists for.

If you want to reclaim the space, delete the files yourself — they are ordinary
files in an ordinary folder. Nothing in the app will fight you, and the next
backup only re-copies what is on the phone now.

## 12. Two devices share history, or a rename split it in two

- **Two phones with the same display name** get separate folders automatically:
  the last four characters of the serial are appended (`Kitchen phone_BCDE`), and
  the histories stay separate. Nothing to do.
- **Renaming a device** changes its default folder name but does not move or
  delete anything already backed up: the old folder stays where it is, and the
  next backup goes to the new one. If you want them together, either point the
  renamed device at the old folder with **Change…**, or set an explicit path.
- **A device re-appeared with a different serial** (repaired phone, replaced
  under warranty, a network ADB serial). By default it is a **new** device with
  its own profile and its own empty history — the safe default, since the app
  cannot know the two are the same phone. Associating the new serial with the
  existing profile is a deliberate, one-way action in this MVP and there is no
  un-associate button, so it is done in the manifest by hand:

  ```sh
  # close the app first, then, in the data directory from setup §8:
  sqlite3 manifest.db
  ```

  ```sql
  -- which serial is which profile?
  SELECT id, serial, display_name FROM devices;

  -- point the new serial at the existing profile (e.g. id 3)
  INSERT INTO device_aliases (alias_serial, device_id, created_at)
  VALUES ('R58M12ABCDE', 3, '2026-03-01T10:00:00Z');
  ```

  To undo it, delete that row (`DELETE FROM device_aliases WHERE alias_serial = '…';`).
  Historical rows are never rewritten, so the existing history stays byte-identical
  either way. Take a copy of `manifest.db` before editing, as with any database.

## 13. "The backup manifest could not be read"

The manifest is the index of what has been copied — never the only copy of
anything. If SQLite cannot read it (a truncated file, a power cut mid-write, a
disk error), the app:

1. keeps the damaged file as `manifest.db.corrupt-<timestamp>` (along with its
   `-wal`/`-shm` sidecars, so a repair is possible),
2. starts with a fresh manifest, and
3. tells you, with this text: *"Your backed-up files are not affected: the
   manifest only records what has already been copied. The next backup will
   re-copy the files it no longer has a record of."*

The practical consequence: your files are safe, but the next backup is a full
one and may version files into `.backup/history/` that were not really changed
(because the app no longer has the old size/mtime record to compare against).
That is why the damaged file is kept — with SQLite's own recovery tools
(`sqlite3 manifest.db.corrupt-… .recover`) it can often be salvaged, and the
manifest can be put back in place.

## 14. Some folders always fail

Android protects certain directories from apps and from ADB on a normal
(non-rooted) phone. The usual suspects:

| Path | Why |
|---|---|
| `Android/data` | per-app storage, locked by Android 11+ |
| `Android/obb` | app asset bundles, locked |
| `Android/media` subfolders | partially readable; app-dependent |
| DCIM subfolders of some OEM camera apps | protected until the phone is unlocked |

Failed files are counted and listed in the job's history rows rather than
silently dropped — a job that says `failed: 40` is telling you the truth. If a
folder you care about is in that list, the file itself is not readable over ADB
and no backup tool without root can read it either.

## 15. Reading the log, and what to send in a bug report

The log is a plain text file:

| Platform | Path |
|---|---|
| Linux | `~/.local/share/android-backup/logs/android-backup.log` |
| Windows | `%APPDATA%\android-backup\logs\android-backup.log` |

It is `info` level by default, with this project's own modules at `debug` — so it
records what the UI decided (which adb was chosen, why the start button was
gated, what each transfer did) without the noise of every dependency. For more
detail, set the level before launching:

```sh
# Linux
ANDROID_BACKUP_LOG=debug android-backup
```
```powershell
# Windows (PowerShell)
$env:ANDROID_BACKUP_LOG = "trace"; .\android-backup.exe
```

`trace` includes every adb command line and the engine's per-file decisions. It
is verbose (a large backup can produce tens of megabytes) — reproduce the
problem, then go back to `debug`.

Useful before reporting: `adb devices -l` from the adb the app is using, the log
file, the version (`android-backup --version`), and the platform (Windows or
Linux, distribution/version). Do not send your backup destination listings or
file names you would rather not share: the log records paths, and it is easy to
trim.

## 16. Windows SmartScreen, and Linux artifacts that will not start

- **Windows: "Windows protected your PC".** The build is unsigned —
  [release.md](release.md) records why, and what signing would change. Choose
  **More info → Run anyway**. If you would rather not, verify the artifact's
  SHA-256 first ([setup §2](setup.md#2-download)).
- **Linux: the AppImage does nothing.** Install FUSE (`sudo apt install
  libfuse2` on Ubuntu 22.04+) or run it with `--appimage-extract-and-run`, which
  unpacks to a temporary directory instead of mounting.
- **Linux: `GLIBC_2.xx not found`.** The binary was built against a newer glibc
  than your distribution provides. See [setup §3](setup.md#3-install--linux) —
  the release notes record the floor each artifact was built against, and
  building from source is the other way out.

## 17. No notification appeared when a backup started or finished

Notifications are for work that happens while you are not looking at the app —
a backup that starts on its own when you plug the phone in, or a job that
finishes or gets stuck in the background. Nothing is lost when a toast does not
appear; the panel, the history and the log are the record, and this is
decoration over them.

- **The app's window is on screen.** Success notifications are suppressed on
  purpose: the panel is already showing you the same thing and a toast over your
  own window is noise. Paused and failed notifications are never suppressed.
- **Windows: Focus Assist / Do Not Disturb.** Toasts are subject to notification
  settings like any other app's, so they can be silenced by an active focus
  session or by turning the app off in **Settings → System → Notifications**.
  Check there first, then check the Action Center — a suppressed toast that
  arrived while you were in a focus session is usually still in the list.
- **Windows: the toasts say the generic host process name.** The app asks Windows
  to show them under its own identity, which needs a Start Menu shortcut to be
  registered by an installer. Unpacked from the ZIP, Windows may attribute them
  to the binary instead. The text is still correct.
- **Linux: nothing appears at all.** Toasts go over the session bus to your
  desktop's notification daemon. On a bare window manager, or over SSH, there is
  no daemon and nothing is shown; the log records the attempt. GNOME's own
  quirks here are accepted (see [wave 9](plan/wave-9-automation.md)).
- **A repeat was coalesced.** The same kind of news about the same phone within a
  minute is collapsed into one toast, so flapping the cable or a job that pauses
  twice quickly does not stack notifications.

## 18. There is no tray icon

Nothing is broken, but one thing changes: **closing the window quits**, so an
automatic backup only happens while the window is open. The **Background…** dialog
in the status bar says which of the two reasons it is — they need different fixes.

**"the tray icon is switched off in the configuration file".** `background.tray_icon`
is `false` in `config.toml` ([setup §8](setup.md#8-where-everything-is-stored)). Set it
to `true` and restart. (This is also the switch for a desktop where the tray is
unwanted rather than unavailable.)

**"this desktop gives it nowhere to put one".**

| Situation | Fix |
|---|---|
| Linux without the appindicator library | Install it — Debian/Ubuntu: `sudo apt install libayatana-appindicator3-1`; Fedora: `libayatana-appindicator-gtk3`; Arch: `libayatana-appindicator` — then restart the app. The log names the two sonames it looked for. |
| GNOME | Tray icons come from an extension, not the shell: install **AppIndicator and KStatusNotifierItem Support** from extensions.gnome.org. Without it the app runs windowed, which is *supported*, not a failure. |
| A bare window manager (sway, i3, a minimal X session) | There may be no notification area anywhere. Nothing to fix; keep the window open while backing up. |
| Windows | The icon is probably behind the `^` chevron in the notification area — drag it out to keep it visible, or check **Settings → Personalisation → Taskbar → Other system tray icons**. |
| Over SSH, in a container, or on a machine with no display | There is no tray and no session bus. The window is the program. |

A hidden window whose tray icon is gone can only be found in the task manager (Task
Manager / `kill`). Killing it loses nothing: whatever a running job had already copied
is recorded in the manifest, and the next launch finishes the remainder — it looks like
[§9](#9-the-app-or-the-machine-died-mid-backup), because that is exactly what it is.

## 19. A backup started by itself, or will not start by itself

**It started without being asked.** That is the one feature that only ever does
something if a device's schedule was turned on — by the wizard's *Back up a phone when
I plug it in* checkbox ([setup §7](setup.md#7-first-launch)), or by hand in that
device's **Automatic backups** row. Look at the row: **Off** means the phone is not
enrolled, however it looks in the list. To stop it, set the row to **Off**, or press
**Pause all automatic backups** (the same switch the tray writes) to stop every device
at once without forgetting any of them. Nothing that was already copied is affected by
either. The log tells the two kinds of run apart — *automatic backup started*, naming
the device and the job, is automation; *backup started* is a person pressing a button.
The history table does not show which it was, and does not need to: the result is the
same either way.

**It will not start by itself.** The panel's status line names the reason whenever
there is one — *"Not yet: …"* is the same gate the Start button uses. In order:

1. The row is not **Off**, and automation is not paused (the status bar says
   `automatic backups paused` when it is).
2. The phone is connected, unlocked, and shows as `ready` rather than `unauthorized`;
   USB debugging stays on ([setup §6](setup.md#6-enable-usb-debugging-on-the-phone)).
3. The destination exists *now*. An external drive that is unplugged is a missing
   destination, and a scheduled backup will not create one.
4. At least one folder is ticked, and the job has room to write ([§8](#8-not-enough-free-space)).
5. **When connected** means once per plug-in: unplug and replug, or press Start.
   **Every *n* hours** counts from the last *attempt* — including a backup you started
   by hand, which is why a manual backup at 14:00 silences a 6-hour schedule.
6. A failed attempt puts that device to sleep for two minutes. Flapping the cable to
   make it retry sooner will not work; leaving it alone will.
7. An appearance that was refused three times (the phone answering `unauthorized`, say)
   is left alone until it is unplugged and plugged back in — deliberately, so a phone
   sitting there unauthorized does not produce an endless stream of attempts.

If the schedule is on, the phone is happy and nothing runs, the status line is the
thing to read: it is computed from the same manifest row the engine uses. Then the log
([§15](#15-reading-the-log-and-what-to-send-in-a-bug-report)) says what the driver
decided, once per decision.

## 20. A backup stopped half-way and did not continue

Automation resumes **its own** work, and only its own:

| Who started the job | What happens when the phone is ready again |
|---|---|
| Automation (a *Backing up…* notification, or *automatic backup started* in the log) | resumed on its own — up to twice per job |
| You, with the Start button | nothing; the job keeps its **Resume** button for you |

Resuming continues the *same* job from the manifest, so files that were already
verified are not copied again — only the remainder is. A job that stops twice and cannot
be resumed is reported instead of retried forever, with this wording: *"the connection
kept dropping, so resuming it automatically was stopped"*. That job then waits for a
person: press **Resume** (the phone needs to be connected and locked no longer) and it
picks up where it left off.

If a *manual* job is stuck, that is the design and not a fault: the app does not
second-guess a decision you made. Press **Resume** whenever you are ready — the next
run never re-copies what is already verified.

## 21. Starting it a second time says it is already running

```
Android Backup is already running (process 4242).

It keeps running after its window is closed, so that automatic backups can happen
while it is out of the way. Open the window from its icon in the notification area
(the system tray), next to the clock.
```

Two programs writing one manifest is the one way to lose data, so the second copy stops
before it opens anything. The first one is running — usually hidden, and usually
because of **Start in the tray when I log in**, which is the point of that setting. Find
its icon ([§18](#18-there-is-no-tray-icon) if it is not there), or end the process in the
task manager and start again.
`instance.lock`, next to the manifest
([setup §8](setup.md#8-where-everything-is-stored)), is the file that enforces this. It
is locked by the operating system, which releases the lock when the process ends — even
if the process was killed, so a leftover file is harmless and can be ignored. Deleting
it while the program runs does not unlock anything either: the lock lives on the open
file handle, not on the name. The owning process's id, when it can be read, is beside
it in `instance.pid` — display text for the message above, never consulted for
decisions, and removed when its claim ends. (The lock file itself is unreadable while
the lock is held on Windows — the operating system enforces the lock for reads too,
not just writes.) Both files are safe to delete while the program is not running.

## 22. The window does not open in a virtual machine

```
egui_glow: OpenGL ... requires OpenGL 2.0+
```

The interface draws with egui's glow renderer, which asks the graphics driver for a
real OpenGL context. A virtual machine's default display driver can answer with something
much older than that (VirtualBox without 3D acceleration reports OpenGL 1.1), and the
renderer refuses rather than draw nothing. The backup itself never touches the GPU —
ADB, the manifest and every file path are ordinary program logic — the window is the
only part that cares.

Two ways out, in order:

1. **Give the machine a GPU.** VirtualBox: Settings → Display → *Enable 3D
   Acceleration* (and install the Guest Additions so the accelerated driver loads).
   VMware Workstation: *Accelerate 3D graphics*. Then run again.
2. **No GPU available: let the CPU draw.** Mesa's Windows software driver
   (`mesa-dist-win`, the llvmpipe build) ships a drop-in `opengl32.dll` — put the
   64-bit one in the same folder as `android-backup.exe`. It implements a modern
   OpenGL on the CPU, it is a standard, widely used shim that touches nothing else,
   and it is more than fast enough for this UI — it only redraws when you interact.

If the window still refuses after both, the failure text names the OpenGL version the
driver reported — include it in a bug report.
