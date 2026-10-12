# Android Backup Utility — High-Level Build Plan

## 1. Project Overview

Build a lightweight, cross-platform desktop application for Windows and Linux that allows users to back up files from Android devices through ADB.

The application should automatically detect connected devices, identify them, allow users to configure backup locations and folder selections, and maintain an append-only backup history.

The primary design goal is **reliable, understandable backups without requiring users to install Android Studio, learn command-line tools, or trust cloud storage.**

### Target platforms

* Windows 10/11
* Linux (initially supporting common x86_64 distributions, including Arch-based systems)

### Technology stack

| Component             | Technology                    |
| --------------------- | ----------------------------- |
| Language              | Rust                          |
| Desktop UI            | egui + eframe                 |
| Database              | SQLite via rusqlite           |
| Android communication | ADB subprocess                |
| File selection        | rfd                           |
| HTTP downloads        | reqwest                       |
| Serialization         | serde                         |
| Logging               | tracing                       |
| Packaging             | GitHub Actions release builds |

The application should be developed as a native desktop program, not a web application.

---

# 2. Core Design Principles

### Data preservation

* Never delete files from a PC backup because they were deleted from the phone.
* Never overwrite an existing backup file without explicit policy and verification.
* Interrupted transfers must be recoverable.
* A failed backup must not be recorded as successfully completed.

### Device separation

* Every device has its own identity and backup configuration.
* Multiple devices can be connected simultaneously.
* Each device has a human-friendly display name.
* Backup histories must remain separate.

### User control

* Users can select backup locations.
* Users can choose which phone directories to back up.
* Users can change settings without losing existing backup history.
* No cloud account or online service is required.

### Progressive complexity

The MVP should prioritize dependable file copying and device management. Scheduling, advanced synchronization, and other convenience features can be introduced later.

---

# 3. Application Architecture

Use a modular Rust workspace so the core backup logic remains independent of the desktop interface.

Suggested structure:

```text
android-backup/
├── Cargo.toml
├── crates/
│   ├── core/
│   │   ├── adb/
│   │   ├── devices/
│   │   ├── backup/
│   │   ├── manifest/
│   │   ├── database/
│   │   └── config/
│   ├── platform/
│   │   ├── windows/
│   │   └── linux/
│   ├── cli/
│   └── desktop/
├── assets/
├── tests/
└── .github/
    └── workflows/
```

### Core responsibilities

**ADB module**

* Locate or install ADB.
* Start or communicate with the ADB server.
* Enumerate connected devices.
* Query device information.
* Execute remote directory and file operations.
* Detect connection loss and authorization state.

**Device module**

* Track device identity.
* Manage display names.
* Maintain per-device settings.
* Detect connected, disconnected, unauthorized, and unavailable states.

**Backup module**

* Enumerate selected phone directories.
* Compare source files with the local manifest.
* Copy new or changed files.
* Resume interrupted work.
* Verify transferred files.
* Record backup results.

**Database module**

* Persist devices, settings, backup jobs, file records, and errors.
* Handle schema migrations.
* Prevent duplicate backup records.

**Desktop module**

* Display device status.
* Configure settings.
* Start, pause, resume, and cancel backups.
* Show progress and errors.

---

# 4. ADB Management

## 4.1 First-launch behavior

On startup:

1. Check whether a usable ADB executable is configured.
2. If found, validate that it can execute.
3. If missing, display an installation prompt.
4. Allow the user to download official Android SDK Platform Tools.
5. Extract the archive into an application-managed directory.
6. Verify the executable can run.
7. Save its location for future launches.

The application should not require Android Studio or a full Android SDK installation.

### ADB configuration options

* Automatically managed ADB (default).
* Use an existing system ADB installation.
* Manually select an ADB executable.

The application should avoid silently replacing a user's existing ADB installation.

### Download safety

* Use official Android developer distribution sources.
* Verify downloaded archives using an available trusted checksum or signature.
* Extract only expected files.
* Handle failed or interrupted downloads gracefully.
* Never execute an unverified downloaded binary.

ADB should be managed separately from the backup database and user data.

## 4.2 ADB server lifecycle

ADB uses a background server, which may already be running because of Android Studio or another developer tool.

The application should:

* Reuse a compatible running ADB server.
* Avoid unnecessarily restarting it.
* Display meaningful errors if another ADB installation or server causes conflicts.
* Avoid killing the shared server when the application closes.

---

# 5. Device Discovery and Identity

## 5.1 Automatic discovery

Poll ADB periodically while the application is open.

Possible device states:

| State        | Meaning                                        |
| ------------ | ---------------------------------------------- |
| Connected    | Device is available                            |
| Unauthorized | Phone requires user approval                   |
| Offline      | ADB sees the device but cannot communicate     |
| Disconnected | Previously known device is no longer connected |
| Error        | Device query failed                            |

The UI should update these states without requiring a manual refresh.

## 5.2 Device identity

Use the ADB serial as the primary device identifier.

Store additional information:

* Manufacturer
* Model
* Android version
* First-seen timestamp
* Last-seen timestamp
* User-defined display name

Serials may not always be permanent across every connection method or device configuration. If a device appears with a different serial, do not silently merge its history.

Instead, offer an explicit option to associate it with an existing device profile.

## 5.3 Multiple devices

The application must support multiple connected phones.

Each device should have:

* Independent backup settings.
* Independent destination directory.
* Independent backup history.
* Independent progress status.

Example:

```text
Backups/
├── Riley Phone/
├── Family Phone/
└── Samsung A53/
```

The user should be able to rename devices without changing their underlying identity.

---

# 6. Backup Location Configuration

This is a core MVP feature.

## 6.1 Global default location

On first launch, ask the user where backups should be stored.

Suggested defaults:

**Windows**

```text
%USERPROFILE%/AndroidBackups
```

**Linux**

```text
~/AndroidBackups
```

These are defaults only. The user must be able to choose another location.

## 6.2 Per-device destination

Each device should have its own configurable destination directory.

Example:

```text
/mnt/storage/PhoneBackups/
```

Within that location, the application can create a directory based on the device's configured name.

Changing a device's destination should affect future backups only. It must not delete or relocate previous backups.

## 6.3 Location validation

Before starting a backup:

* Check that the directory exists or can be created.
* Check that it is writable.
* Check available disk space where practical.
* Detect missing or disconnected external drives.
* Warn if the destination is unavailable.

If the destination disappears during backup, pause safely and report the issue.

## 6.4 Destination layout

Suggested structure:

```text
AndroidBackups/
└── Samsung A53/
    ├── DCIM/
    ├── Pictures/
    ├── Movies/
    ├── Download/
    ├── Android/
    │   └── media/
    └── .backup/
        └── manifest.db
```

Internal application metadata should be stored separately from user-visible media where practical.

Do not expose internal database files as ordinary backed-up phone content.

---

# 7. Folder Selection

Users should be able to select which directories are included in a backup.

### Initial supported directories

* `DCIM`
* `Pictures`
* `Movies`
* `Music`
* `Documents`
* `Download`
* `Android/media`
* `Recordings`
* `Screenshots`, where separately available

Some folders overlap. For example, screenshots may already exist under Pictures. The application should avoid copying the same source file twice merely because it appears in two configured categories.

### Custom directories

Allow users to enter an additional Android storage path manually.

Validate paths before accepting them.

### Important limitation

Android scoped storage and device-specific restrictions may prevent access to certain locations. The application must report inaccessible folders rather than silently treating them as empty.

The MVP should focus on shared user storage, not private application data or protected system partitions.

---

# 8. Backup Workflow

## 8.1 Starting a backup

When the user selects a device and starts a backup:

1. Confirm device availability and authorization.
2. Validate the destination.
3. Load the device's folder configuration.
4. Enumerate files on the phone.
5. Compare source metadata against the local manifest.
6. Identify new, changed, missing, and previously backed-up files.
7. Copy eligible files.
8. Verify copied files.
9. Update the manifest.
10. Save the final job status and summary.

## 8.2 File comparison

Use a practical staged comparison strategy.

Initial comparison fields:

* Relative source path
* File size
* Modification timestamp, when available
* Last successful backup status

Do not hash every file on every scan. That would make large photo libraries unnecessarily expensive to inspect.

For files that need copying, calculate a local cryptographic hash after transfer and retain it in the manifest.

The application should distinguish between:

* New file
* Changed file
* Unchanged file
* Missing from phone
* Failed transfer

A missing source file must never trigger deletion from the PC.

## 8.3 Duplicate handling

The application must not blindly overwrite a previous backup.

If a file changes on the phone, preserve the earlier copy.

Suggested structure:

```text
DCIM/
└── Camera/
    ├── IMG_001.jpg
    └── IMG_001.jpg.backup-conflict
```

However, avoid awkward filenames in the actual user backup tree.

Preferred approach: preserve earlier versions in a dedicated history directory or use a collision-safe naming strategy that retains the original extension.

For the MVP, use a clear versioning strategy and never overwrite silently.

## 8.4 Interrupted transfers

A transfer can fail because:

* USB cable disconnects.
* Phone sleeps or reboots.
* ADB loses authorization.
* Destination drive is removed.
* Application closes unexpectedly.

Use temporary destination files:

```text
IMG_001.jpg.partial
```

Only move the temporary file into its final location after successful transfer and verification.

On the next backup:

* Detect incomplete transfers.
* Clean up or resume them safely.
* Recheck source availability.
* Avoid treating partial files as completed backups.

For the first release, restarting an individual incomplete file is acceptable. Resuming at arbitrary byte offsets can be deferred.

## 8.5 Verification

After transfer:

* Confirm destination file exists.
* Compare source and destination sizes.
* Calculate a local SHA-256 hash.
* Record verification status.

ADB does not provide a universally reliable remote hashing mechanism across all Android devices, so size comparison alone should not be described as full integrity verification.

The MVP can provide transfer verification and local hashes without claiming cryptographic source-to-destination equivalence.

---

# 9. SQLite Database

Use SQLite for persistent metadata.

Suggested tables:

### `devices`

* `id`
* `serial`
* `display_name`
* `manufacturer`
* `model`
* `android_version`
* `destination_path`
* `created_at`
* `last_seen_at`

### `device_folders`

* `id`
* `device_id`
* `source_path`
* `enabled`

### `backup_jobs`

* `id`
* `device_id`
* `started_at`
* `completed_at`
* `status`
* `files_scanned`
* `files_copied`
* `files_skipped`
* `files_failed`
* `bytes_copied`
* `error_summary`

### `backup_files`

* `id`
* `device_id`
* `source_path`
* `destination_path`
* `source_size`
* `source_modified_at`
* `content_hash`
* `last_successful_backup`
* `status`

### Database requirements

* Use transactions when recording successful transfers.
* Include schema migrations.
* Use stable IDs.
* Never use a phone-provided filename as a trusted filesystem path without validation.
* Keep the database recoverable and backed up separately from the source phone.

The manifest should be treated as an index of known backup state, not as the only copy of the user's files.

---

# 10. Desktop Interface

Keep the initial interface simple and functional.

## Main screen

```text
┌────────────────────────────────────────────┐
│ Android Backup                             │
├────────────────────────────────────────────┤
│ Devices                                    │
│                                            │
│ Samsung A53       Connected                │
│ Family Phone      Unauthorized             │
│                                            │
├────────────────────────────────────────────┤
│ Selected Device: Samsung A53               │
│                                            │
│ Backup Location                            │
│ /mnt/storage/PhoneBackups/Samsung A53       │
│ [Change Location]                          │
│                                            │
│ Folders                                    │
│ [x] DCIM                                   │
│ [x] Pictures                               │
│ [x] Movies                                 │
│ [ ] Download                               │
│ [ ] Android/media                          │
│                                            │
│ [Start Backup]                             │
├────────────────────────────────────────────┤
│ Progress                                   │
│ 128 / 450 files                            │
│ 2.4 GB copied                              │
│                                            │
│ Current file: IMG_20260930.jpg             │
├────────────────────────────────────────────┤
│ Last Backup: Completed                     │
│ 430 files copied, 20 unchanged             │
└────────────────────────────────────────────┘
```

### Main UI features

* Connected device list.
* Authorization status.
* Device rename.
* Destination selection.
* Folder selection.
* Backup start, pause, resume, and cancel.
* Current file and progress.
* Transfer speed and estimated remaining time, where practical.
* Error and completion summaries.
* Backup history.

Use native folder selection dialogs through `rfd`.

---

# 11. Notifications and Background Behavior

### MVP

The application only needs to show live device and backup status while open.

If a phone becomes unauthorized, display the status clearly.

### Later enhancement

A background agent may provide:

* Desktop notifications.
* Device connection notifications.
* Optional scheduled backups.
* Automatic backup on connection.

Do not build a permanent background daemon into the first release.

> **Delivered in 0.2.0 (Wave 9).** All four bullets above are in, as a tray-resident
> application rather than a daemon: closing the window hides it and the same process
> keeps watching devices and running schedules, so there is no second copy of the
> manifest logic and no IPC. `docs/plan/wave-9-automation.md` is the plan of record
> and `docs/setup.md` §10 is the user-facing description; the daemon-free choice is
> the point of the wave, not a shortcut. Automation remains opt-in per device.

---

# 12. MVP Scope

The first usable version should include:

* [ ] Windows and Linux builds.
* [ ] ADB detection and managed installation.
* [ ] Automatic device discovery.
* [ ] Device authorization and connection states.
* [ ] Multiple device profiles.
* [ ] User-defined device names.
* [ ] Global and per-device backup locations.
* [ ] Folder selection.
* [ ] Recursive directory scanning.
* [ ] Incremental backups.
* [ ] SQLite manifest.
* [ ] No deletion of previous backups.
* [ ] Safe temporary file transfers.
* [ ] Interrupted backup recovery.
* [ ] Basic transfer verification.
* [ ] Progress and error reporting.
* [ ] Backup history.
* [ ] Release binaries through GitHub Actions.

The MVP is complete when a user can connect a phone, select a destination, back up media, disconnect, reconnect, and run another backup without recopying every unchanged file or losing previous copies.

---

# 13. Deferred Features

Do not implement these before the MVP is stable.

### Future features

* Scheduled backups. — *delivered in 0.2.0 (interval-based; a calendar scheduler is
  still not planned).*
* Automatic backup on device connection. — *delivered in 0.2.0.*
* Background service and notifications. — *delivered in 0.2.0 as a tray application;
  a service/daemon is still explicitly not built.*
* Backup encryption.
* Backup browsing and file restoration.
* Configurable file filters and exclusions.
* Advanced duplicate detection across different source paths.
* Backup retention policies.
* Multiple backup profiles per device.
* Network ADB support.
* Export and import of application configuration.
* Backup database integrity checks and repair tools.
* Optional CLI automation.

### Explicitly out of scope

* Bypassing Android lock screens.
* Accessing encrypted private application data.
* Bootloader unlocking.
* Partition flashing or raw filesystem extraction.
* Cloud synchronization.
* Phone-to-phone migration.

---

# 14. Testing Strategy

## Unit tests

* Device identity matching.
* Path sanitization.
* File comparison.
* Manifest updates.
* Collision handling.
* Backup state transitions.
* Interrupted job recovery.

## Integration tests

* Mock ADB command responses.
* Simulated disconnected devices.
* Unauthorized devices.
* Destination drive failures.
* Interrupted transfers.
* Database migration and recovery.

## Hardware tests

Test with at least:

* One Samsung device.
* One non-Samsung Android device.
* A large media directory.
* Files with duplicate names in different folders.
* A device with restricted or unavailable directories.

## Platform tests

* Windows USB device discovery.
* Linux USB permissions and udev behavior.
* ADB installation and updates.
* External destination drives.
* Application restart during backup.

---

# 15. Release and Distribution

Use GitHub Actions to build release artifacts for each platform.

Potential release outputs:

**Windows**

* Portable executable.
* Optional installer.

**Linux**

* AppImage.
* Optional `.deb` package.
* Optional Arch package later.

The target machine should not need Rust, Cargo, or a compiler installed.

Build and package platform-specific artifacts in CI, then attach them to versioned GitHub releases.

Linux packaging should account for the system libraries required by the chosen egui backend. Prefer a release format that minimizes user-side dependency installation.

---

# 16. Recommended Implementation Order

### Phase 1 — Core foundation

* Set up Rust workspace.
* Implement ADB executable discovery.
* Implement device enumeration.
* Build a minimal egui window showing connected devices.
* Validate Windows and Linux builds.

### Phase 2 — Device management

* Add device identity and display names.
* Add SQLite.
* Persist device configuration.
* Implement global and per-device destination selection.

### Phase 3 — Backup engine

* Implement remote directory enumeration.
* Implement folder selection.
* Implement file comparison.
* Implement safe file copying.
* Implement manifest recording.
* Implement interrupted transfer handling.

### Phase 4 — Usable MVP interface

* Add progress reporting.
* Add backup history.
* Add errors and completion summaries.
* Add ADB managed installation.
* Add validation for backup destinations.

### Phase 5 — Reliability

* Test disconnects and interrupted transfers.
* Test duplicate filenames and changed files.
* Test database recovery.
* Test large backup jobs.
* Verify Windows and Linux packaging.

### Phase 6 — Release

* Configure GitHub Actions.
* Publish Windows and Linux artifacts.
* Write concise setup and troubleshooting documentation.

---

## Final architectural recommendation

Keep the backup engine independent from egui. The UI should only invoke core operations and display their state.

This allows the project to begin with a small native interface while preserving the option to add a CLI, background service, or different GUI later.

**The MVP's defining behavior is simple: connect, select a location, back up, and repeat without losing anything already saved.**

