# MVP acceptance run

The record of how this project's definition of done (plan §16, executed as
E08.4) was checked, item by item, with the evidence for each. It is written to be
audited: every row names the check that produced it, so anyone can re-run the
same thing and get the same answer.

It is also honest about the difference between three kinds of "yes":

| Marker | Means |
| --- | --- |
| **PASS** | Observed, on this machine, by the named check. |
| **PASS (simulated)** | The logic is proven against the scripted-phone harness (`crates/core/src/mock.rs`), which speaks ADB's protocol. Faithful to the protocol, not to a real device. |
| **PENDING** | Not observed yet, and why. Each has a procedure in §7 so it can be closed in minutes. |

## 1. The run

- Date: 2026-10-02.
- Machine: x86_64 Linux (CachyOS, kernel 7.2.6), no Android SDK, no Rust on the
  test path — the app under test was the packaged artifact, launched with a
  throwaway `HOME` on a private `Xvfb` display.
- Toolchain of the artifact: `rustc 1.97.1`, glibc floor 2.43 (this desktop). The
  published artifacts are built in a `debian:12-slim` container instead, with a
  2.36 floor — see §5.
- Suite: `cargo test --workspace` → **421 passed, 0 failed, 1 ignored**
  (`installs_platform_tools_from_google_end_to_end`, which downloads from
  `dl.google.com` and runs nightly, and passed on 2026-10-02 on this machine).

## 2. Definition of done, item by item

### 2.1 Connect a phone and get to a first backup

| Item | How it was checked | Evidence | Status |
| --- | --- | --- | --- |
| Install the artifact on a machine with no toolchain | Tarball extracted, `install.sh` run under a throwaway `HOME`; binary launched with `env -i` | `scripts/smoke-linux.sh` → `PASS`; observed `install.sh` write `~/.local/bin` and the launcher entry | **PASS** |
| The window opens on a bare desktop | Window title asserted on a private `Xvfb` display; process must survive the ask, not just start | `smoke-linux.sh` (title `Android Backup`) | **PASS** |
| First launch with no configuration | Fresh `HOME`, no config file, no database | Log: `no configuration file yet, using defaults` → `configuration loaded config_existed=false` → `applying database migration version=1` | **PASS** |
| A phone is discovered and enriched | Scripted `adb devices -l` and `getprop` timelines | `device_listing_fixtures.rs` (8 cases, incl. daemon chatter and unauthorised devices); `devices::watcher` unit tests | **PASS (simulated)** |
| Approval of USB debugging is requested, not assumed | Unauthorised device → job pauses with guidance rather than failing | `scenario_disconnect.rs::an_unauthorized_phone_pauses_with_authorization_guidance` | **PASS (simulated)** |
| A destination is chosen and validated | Missing or uncreatable destination refused before any work starts | `e2e_backup.rs::a_missing_destination_is_refused_at_start`, `::a_destination_that_cannot_be_created_is_reported_by_start` | **PASS** |
| Folders are selected from the phone | Whole storage, per-folder, and overlapping selections | `e2e_backup.rs::whole_storage_backs_up_everything_once`, `::overlapping_folders_copy_each_file_exactly_once`, `scenario_duplicate_names.rs::a_file_reachable_through_two_selections_is_copied_once` | **PASS (simulated)** |
| **A first backup completes** | **Maintainer's own phone, one folder, into a local folder** | Reported by the maintainer, 2026-10-02: connected, chose a folder, files copied over and verified. Details to complete: device model, Android version, file count, destination filesystem | **PASS (real device, partial)** |
| First backup, all rows recorded correctly | Manifest row per file with size and digest; counters reconciled from rows, not from progress | `e2e_backup.rs::five_files_are_copied_verified_and_recorded`; `backup::job::reconcile_counters` unit tests | **PASS (simulated)** |

### 2.2 Run it again, and again

| Item | How it was checked | Evidence | Status |
| --- | --- | --- | --- |
| Reconnecting and running again copies nothing | Second run over an unchanged phone | `e2e_backup.rs::a_second_run_copies_nothing` | **PASS (simulated)** |
| No unchanged file is re-copied | Manifest diff (size, then name/size/date, then digest) decides `Skip`/`Copy`/`Replace` | `backup::compare` unit tests; `scenario_large.rs::a_phone_with_fifty_thousand_files_is_scanned_and_copied_within_budget` (50 000 files, budget asserted) | **PASS (simulated)** |
| No previous copy is ever lost | Deletion on the phone never deletes on the PC; a replacement is *kept* in history | `e2e_backup.rs::a_file_deleted_on_the_phone_is_kept_on_the_pc`, `::a_changed_file_is_replaced_and_the_old_version_kept` | **PASS (simulated)** |
| History rows are correct over many versions | Ten versions of one file, a rename, and a new file taking another's place | `scenario_overlap_and_history.rs::ten_versions_of_one_photo_are_all_kept`, `::a_photo_renamed_on_the_phone_keeps_the_old_copy`, `::a_new_file_taking_another_files_place_is_never_a_silent_overwrite` | **PASS (simulated)** |
| A same-size edit is still noticed | Size-equal, mtime-equal change caught by digest | `scenario_overlap_and_history.rs::a_changed_photo_of_the_same_size_is_still_noticed` | **PASS (simulated)** |
| A locally damaged or lost copy is repaired | Truncated local file and deleted local file re-copied | `e2e_backup.rs::a_truncated_local_copy_is_re_copied`, `::a_locally_deleted_backup_is_re_copied_from_the_phone` | **PASS (simulated)** |
| Two phones with the same name do not collide | Serial-tail disambiguation in folders and in the manifest | `scenario_duplicate_names.rs::two_phones_with_the_same_name_get_different_folders`, `::the_same_file_name_on_two_phones_stays_two_backups` | **PASS (simulated)** |
| Hostile file names are handled, not rejected | Colons, `..`, Windows-reserved names, case clashes | `e2e_backup.rs::hostile_file_names_are_sanitized_without_colliding`; `scenario_duplicate_names.rs::a_case_only_clash_is_resolved_where_the_filesystem_cares`; `paths.rs` unit tests | **PASS** |
| Integrity checking is real | Hash mode catches corrupted bytes; a digest-less phone degrades honestly | `e2e_backup.rs::hash_verification_catches_corrupted_bytes`, `::hash_mode_without_a_remote_digest_still_backs_up` | **PASS (simulated)** |

### 2.3 Interruptions

| Item | How it was checked | Evidence | Status |
| --- | --- | --- | --- |
| Killing the app mid-backup leaves a readable manifest | Worker process killed during a copy; manifest reopened; next run finishes the job | `scenario_kill_db_write.rs::a_killed_job_leaves_a_readable_manifest_and_the_next_job_finishes_it` | **PASS** |
| An interrupted copy leaves no half-file in place | Staged `.incomplete/…part`, atomic `rename` on verify | `e2e_backup.rs::cancelling_mid_copy_keeps_what_was_already_verified` | **PASS (simulated)** |
| An unplugged cable pauses the job, it does not fail it | Device lost mid-copy → pause with a reason, named file, resume later | `scenario_disconnect.rs::the_next_job_copies_only_the_remainder`, `::an_interrupted_recopy_keeps_the_previous_version_verified`; `e2e_backup.rs::a_lost_device_pauses_the_job_with_a_reason` | **PASS (simulated)** |
| Transient failures are retried without bothering anyone | Retry in place; a pause only after the retry budget is spent | `e2e_backup.rs::a_transient_disconnect_is_retried_in_place`, `scenario_disconnect.rs::a_transient_failure_that_clears_itself_never_pauses_at_all` | **PASS (simulated)** |
| A paused job resumes and finishes | Pause/resume/cancel through the control API | `e2e_backup.rs::a_paused_job_can_be_resumed_and_finishes`, `::cancelling_before_the_first_file_copies_nothing` | **PASS (simulated)** |
| A crash never corrupts the manifest | Truncated and garbage databases quarantined; healthy one untouched | `scenario_corrupt_db.rs` (4 cases), `database::quarantine` unit tests | **PASS** |
| Stale state is reconciled on restart | Verified row whose file vanished; committed file with no row | `scenario_kill_db_write.rs::a_verified_row_whose_file_vanished_is_reconciled_on_the_next_start`, `::a_file_committed_without_its_manifest_row_is_copied_again_safely` | **PASS** |

### 2.4 The UI

| Item | How it was checked | Evidence | Status |
| --- | --- | --- | --- |
| Every panel, gate and modal renders its text | Headless `egui::Context` render passes | `desktop/tests/ui_headless.rs` (13 cases) | **PASS** |
| The UI keeps its distance from the engine | Dependency and single-`eframe::App` invariants | `desktop/tests/architecture.rs` (4 cases) | **PASS** |
| A job's progress, pause, fatal error and history reach the screen | `JobEvent`/`DeviceEvent` folded into the view model | `desktop/src/view.rs` unit tests (57 in the crate) | **PASS** |
| It is legible on a real screen, with real fonts and a real pointer | Not attempted: no display on this machine (see `testing.md` §2.2) | — | **PENDING (human)** |

## 3. Artifacts

Verified on the artifacts in `dist/` (a local build; the published ones come from
the release container, see §5):

| Item | How it was checked | Evidence | Status |
| --- | --- | --- | --- |
| Tarball contains what it should | Contents listed and installed under a throwaway `HOME` | `scripts/package-linux.sh`; `dist/android-backup-0.1.0-linux-x86_64.tar.gz`, 7 986 898 B, sha256 `1c8ca327bccd627811c5af711b4cc36f5139a297b7d4b074f95659038d206785` | **PASS** |
| The tarball's binary runs on a bare desktop | `env -i`, private `Xvfb`, window title asserted | `scripts/smoke-linux.sh` → `PASS` (2026-10-02) | **PASS** |
| The AppImage contains the same binary and runs | Payload extracted, then launched the same way | `scripts/smoke-linux.sh dist/*.AppImage` → `PASS`; 8 260 088 B, sha256 `55f3033d127c49ff98513065da28009517cc084354e6b51a5abd5a540005802c` | **PASS** (secondary artifact; see below) |
| Its runtime requirements are known and written down | `ldd` reviewed: only `libc`, `libm`, `libgcc_s`, `ld-linux`; X11/Wayland/GL/GTK are `dlopen`ed from the host desktop | `dist/*.deps.txt`; `docs/setup.md` §4 | **PASS** |
| Installing the AppImage *by mounting it* (FUSE) | Not attempted: the smoke test extracts the payload on purpose, so that it does not depend on FUSE | — | **PENDING (human)** — and this is why the tarball is the recommended Linux artifact (`docs/release.md` §1) |
| The Windows ZIP builds, and the `.exe` behaves | Script logic exercised under PowerShell 7 on a stand-in binary | `scripts/package-windows.ps1`, `scripts/smoke-windows.ps1` | **PENDING (CI)** — the Windows job builds it; nobody has run it on Windows yet |
| The Windows binary opens a window on a clean Windows 10/11 | Not attempted: no Windows machine here | — | **PENDING (human)** |
| Managed `adb` install on Linux | Real download from `dl.google.com`, checksum verified, version probed, activated | `installer_network.rs::installs_platform_tools_from_google_end_to_end` — `#[ignore]`d as the nightly job, **passed** on 2026-10-02 (adb 1.0.41, platform-tools 37.0.1) | **PASS** |
| Managed `adb` install on Windows (`AdbWinApi.dll`) | Not attempted: no Windows machine here | — | **PENDING (human)** |

## 4. Documentation

| Item | How it was checked | Status |
| --- | --- | --- |
| Install → first backup is documented per OS | Read through against the artifact: the tarball, the ZIP, the first-launch flow, USB debugging, authorisation, destination choice | **PASS (self-review)** |
| The traps users hit first are documented | `troubleshooting.md`: unauthorised device, udev rules, version conflicts, a destination that went away, recovery, un-associating a serial, SmartScreen | **PASS** |
| udev rules for both Ubuntu and Arch | Exact package and rule paths in `setup.md` §5 | **PASS** |
| Somebody who is not the author follows the docs without asking anything | Not attempted | **PENDING (human)** — the honest limit of a self-review |

## 5. What is still not proven

Nothing above says the app works on *your* phone and *your* desktop. The
remaining gaps, and why they are gaps rather than oversights:

1. **A real phone, beyond the one folder already tested.** The harness is
   faithful to `adb`, not to a device. Samsung's paths, `Android/data` and other
   restricted directories, and an unplugged cable are the things I would expect a
   real device to disagree about.
2. **A real Windows desktop.** The ZIP and the `.exe` are only as checked as CI
   can make them; Windows' DLL resolution and SmartScreen cannot be rehearsed
   anywhere else.
3. **The glibc floor of the published artifact.** 2.36 is what the
   `debian:12-slim` build job should produce; the number this machine produced
   (2.43) is *not* representative, which is exactly why the release builds in a
   container and records the result in `.deps.txt`.
4. **Mounting the AppImage**, and **AppImageLauncher** getting in the way. The
   tarball has none of these behaviours, which is why it is the artifact we stand
   behind.
5. **A dry run by someone who is not the author.**

Per plan E08.4, a *failure* would open a numbered fix task in `index.md`. There
are no failures — the rows above are either green or explicitly unobserved.

## 6. Verdict

The engine, the data model and the UI are done and green: 421 automated checks,
including every correctness property this project promised (append-only history,
no silent overwrite, no deletion propagating to the PC, recoverable interruption,
no duplicate copies). The Linux artifact is packaged, installs without root and
runs on a bare desktop. The Windows artifact is built by the same pipeline.

What stands between this and "MVP done" is observation, not code: a real phone, a
real Windows desktop, and one non-author reading the docs. §7 is how to close each
of those.

## 7. Closing the pending rows

Copy-paste procedures, all of them minutes long. Results go back into the tables
above, with the date and the artifact's checksum.

**A real phone.** Plug it in, enable USB debugging, approve the prompt, then:

1. Run a backup of one folder; on the phone, add a file and edit an existing one;
   run it again and confirm the status line reports the changed file and nothing
   else. → §2.2 row 1, for real.
2. On the phone, delete a file that has already been backed up; run again. The PC
   copy must still be there. → §2.2 row 3, for real.
3. Unplug the cable mid-backup. The job must pause with a named file, not fail;
   plug it back in, resume, and confirm the earlier files are not copied twice.
   → §2.3 row 3, for real.
4. Try `Android/data` and any other restricted directory; a refusal must be a
   warning, not a crash. → §5 item 1.
5. Repeat 1–4 on a second, non-Samsung phone if one is available. → §5 item 1.

**A real Windows machine.** Unzip, run `android-backup.exe` (expect SmartScreen →
*More info* → *Run anyway*), and let it install `adb` when it offers. Confirm the
window opens, the phone is listed, and a backup lands in the destination.
→ §3 rows "Windows ZIP" and "Managed adb install on Windows".

**The glibc floor.** After the first tagged build: download the tarball, then
`scripts/check-glibc.sh android-backup` — it must report the same floor as the
`.deps.txt` in the release, and no newer. → §5 item 3.

**A dry run.** Hand `README.md` and `docs/setup.md` to somebody with a phone and no
opinion about Rust, and watch. Anything they ask that the docs do not answer is a
documentation bug. → §4 row 4, and §5 item 5.

**The UI on a real screen.** Launch it on a desktop with a display and check that
the layout, fonts and dialogs hold up at the default and minimum window sizes.
→ §2.4 row 4.
