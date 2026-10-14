# Releasing

How a version of this project is built, checked, published and, if it goes
wrong, retracted. The short version is that a tag does everything:

```sh
scripts/bump-version.sh 0.2.0      # Cargo.toml + Cargo.lock
$EDITOR CHANGELOG.md              # move Unreleased → 0.2.0, add the compare link
git commit -am "chore(release): 0.2.0"
rustup update stable              # the channel floats: check on the compiler CI gets
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # green here before the tag, not after
cargo build -p android-backup-desktop
scripts/smoke-linux.sh target/debug/android-backup   # window and tray, on Xvfb
git tag -a v0.2.0 -m v0.2.0
git push origin main v0.2.0        # ← this publishes (as a draft)
```

`scripts/verify-tag.sh` runs first and refuses to build if the tag, `Cargo.toml`,
`Cargo.lock`, the changelog and the required docs disagree. Nothing is built on a
tag that names a version the tree does not contain.

The commands before the tag are not ceremony. CI compiles on whatever `stable` is
*that day*, because `rust-toolchain.toml` floats, and a suite that is green on a
toolchain from months ago can still be red there — the 0.2.0 run went red on **both**
operating systems for exactly that reason, a std method that rustc 1.99 deprecates
and the compiler on this desk had never heard of (`docs/testing.md`). `rustup update
stable` first, then the three gates, then the tag.

`scripts/smoke-linux.sh` is in that list for the same reason, from the other
direction: the suites run the app's *logic*, and the 0.2.0 cycle produced a build
that passed all of them and then died behind a crash dialog on a desktop, because
the tray icon was being built where GTK had never been initialised. Running the
binary and its tray on `Xvfb` before tagging takes about a minute and would have
caught it (`docs/testing.md` §5).

## 1. What a release contains

| Artifact | Platform | Contents | Produced by |
| --- | --- | --- | --- |
| `android-backup-<v>-linux-x86_64.tar.gz` | Linux x86_64 | the binary, `install.sh`, `android-backup.desktop`, the icon, `README`, `LICENSE-*`, `docs/` | `scripts/package-linux.sh` |
| `android-backup-<v>-linux-x86_64.deps.txt` | — | the `ldd` list and the toolchain the binary was built with, including the glibc floor | `scripts/package-linux.sh` |
| `android-backup-<v>-linux-x86_64.AppImage` | Linux x86_64 | the same binary in a self-mounting wrapper | `scripts/build-appimage.sh` |
| `android-backup-<v>-windows-x86_64.zip` | Windows x86_64 | `android-backup.exe`, `README`, `LICENSE-*`, `docs/` | `scripts/package-windows.ps1` |
| `<artifact>.sha256` | — | one checksum per artifact | the packaging scripts |

Every archive carries its own documentation. `adb` is **not** bundled: on first
launch the app offers to download Google's official platform-tools and verifies
the download against the checksum Google publishes, so the version of `adb` a
user runs is the one they can see and audit.

On Linux the **tarball is the artifact we stand behind**. The AppImage is
best-effort and secondary: it is the same binary, and whether it starts depends
on the host (FUSE, and AppImageLauncher if it is installed). A failed AppImage
build or smoke test does not fail the release; the AppImage is dropped from it.

## 2. What the pipeline does

`.github/workflows/release.yml`, on a `v*` tag:

1. **verify** — `scripts/verify-tag.sh`: tag format, version agreement across
   `Cargo.toml`/`Cargo.lock`/`CHANGELOG.md`, changelog entry present, required
   docs and scripts present and executable.
2. **linux** — in a `debian:12-slim` container: install the toolchain, run
   `cargo test --workspace --locked`, package the tarball, smoke-test the
   tarball's binary on a private `Xvfb` display and a private session bus — window
   first, then the tray icon and its menu (`scripts/smoke-linux.sh` and
   `scripts/smoke-tray.sh`; the container installs `libayatana-appindicator3` so
   that the tray test cannot quietly skip) — then build and smoke-test the
   AppImage (non-blocking), then record the runtime requirements.
3. **windows** — on `windows-latest`: the same test suite, a release build, and
   `scripts/smoke-windows.ps1`, which checks the version, `--help`, and the
   handling of an unknown argument — the artifact checks cargo cannot substitute
   for. Launching the window is deliberately **not** part of it: a build agent is
   exactly the GL-less machine [`docs/troubleshooting.md`](troubleshooting.md) §22
   rules out, and the renderer refuses before the app creates anything. The window
   is hosted where a display exists: the Linux smoke job on `Xvfb`. Then it packages
   the ZIP.
4. **publish** — re-verifies every checksum it is about to publish, composes the
   notes from `CHANGELOG.md` plus the artifact list, and creates the release as a
   **draft**.

The draft is deliberate. Notes and checksums get one human read before they are
visible, publishing is one click, and a draft can be corrected instead of
retracted. Formatting and lints are not repeated in this workflow: a tag
triggers `release.yml` only — `ci.yml` runs on branch pushes, so the commits
under a tag should have gone through it on `main` first, and pushing a tag at a
commit that never did is the anti-pattern this split exists to prevent. (The
release tests are not the ci tests run twice, either: the linux job checks out
the tree and runs the suite inside the `debian:12` container the tarball is
built in, not on ubuntu-latest, so a toolchain or glibc surprise still has a
chance to surface here rather than at the first install.)

## 3. Signing: the decision

**The MVP ships unsigned binaries, on both platforms.** This is a decision, not
an oversight, and it is the one part of this document that should be revisited
first.

What signing would buy:

- **No Windows SmartScreen warning.** An unsigned, new binary is shown as "Windows
  protected your PC" until reputation accrues. The user has to click *More info →
  Run anyway*, which is exactly the dialog people are trained to be suspicious of.
- **Fewer heuristic antivirus and browser flags** on a fresh download.
- **Tamper evidence** independent of the transport.

Why it is not in the MVP:

- A code-signing certificate is an annual paid subscription. The certificate
  authority verifies the publisher's legal identity before issuing it, and since
  2023 the private key has to be held on hardware or in a cloud HSM rather than
  in a CI secret. That means a hardware token or a paid signing service, plus a
  verified identity to buy it — none of which exists for this project today.
  Microsoft's Trusted Signing is the cheapest realistic route and requires an
  established, verifiable organisation.
- Signing keys are a liability as well as a convenience. A key in CI is a key that
  can be stolen from CI, and the value of a signed artifact is only as high as the
  hygiene of the account holding it. Committing to that is a commitment to run it
  properly, which for a project with no users yet is work spent in the wrong order.
- Nothing about the app's *correctness* depends on it. The decision is about the
  first-run experience and about reputation, not about whether backups are
  correct.

What we do instead, and what it is worth:

- The checksums are published next to the artifacts and are re-verified by the
  pipeline before publishing.
- `README`, `docs/setup.md` and `docs/troubleshooting.md` state plainly that the
  build is unsigned and why, so the warning is expected rather than alarming.
- Be clear about the limit: a checksum served from the same release page as the
  artifact protects against a corrupted or tampered download, **not** against a
  compromised release account. Only signing closes that gap, and only if the key
  is held better than the account is.

Follow-up, when there is an audience for it: obtain a certificate (or Trusted
Signing), move signing into the publish job, sign the Windows `.exe` and the
Linux artifacts, and add a signature-verification step to
`docs/setup.md#verifying-a-download`. Until then, this section is the record.

## 4. Runtime requirements we promise

- **Linux:** x86_64, glibc as old as the `debian:12-slim` in the release job
  (currently 2.36 — Debian 12, Ubuntu 22.04 and newer). The exact floor of a
  given artifact is in its `.deps.txt`, which also records the Rust version used;
  `scripts/check-glibc.sh <binary>` prints the floor of any binary. We link against
  the system's `libgtk-3`, `libX11`, `libxkbcommon` and OpenGL rather than bundling
  them — a native file dialog that looks like the user's desktop is worth more
  than a bigger tarball. The tray icon's AppIndicator library
  (`libayatana-appindicator3`, or the older `libappindicator3`) is `dlopen`ed and
  optional, so it does not appear in `.deps.txt`: a desktop without it gets the
  window and no icon, which `docs/setup.md` §3 names as the package to install and
  `docs/troubleshooting.md` §18 covers when it is missing.
- **Windows:** x86_64, Windows 10 or newer. Nothing to install: the app carries its
  own GUI stack and downloads `adb` when it needs it.
- **Both:** no root or administrator rights, ever. Everything the app owns lives
  in the user's data directory and in the backup destination the user chooses.

## 5. What the pipeline proves, and what it cannot

Proven on every tag: the tree passes its tests on Windows and on Linux, the tarball
and ZIP are built from that tree, the tarball's binary starts, puts a visible window
of its own on a display in a container with no GPU, installs a tray icon on a private
session bus and answers its own menu — Open brings the window back, Pause/Resume
reaches the configuration, Quit ends the process with status 0 — the Windows binary
reports its version, refuses a bogus argument politely, and creates its data
directory, and every published checksum matches the file next to it.

Not proven by CI, and listed here so nobody assumes otherwise:

- **A real phone.** Device discovery, authorization and file transfer against
  actual hardware are mock-driven in the suites; the mock is faithful to the
  protocol, not to the device.
- **A real Windows desktop session.** The Windows job runs the binary and checks its
  observable effects; the window itself is only asserted on Linux.
- **The Windows managed `adb` install.** Downloading and installing platform-tools
  is verified end-to-end on Linux against `dl.google.com` (nightly, non-blocking);
  the Windows-specific pieces (`AdbWinApi.dll`, `AdbWinUsbApi.dll`) are not.
- **Mounting the AppImage.** The smoke test extracts the payload rather than
  mounting it, because mounting needs FUSE and containers do not have it, and a
  desktop with AppImageLauncher installed may intercept the file first. This is
  why the AppImage is secondary.
- **The tray icon as a panel actually draws it.** The tray test proves the
  conversation — the item registers with a status notifier host, publishes a menu,
  and acts on clicks — because that is the part the app owns. Whether a given panel
  then puts a picture in a corner of the screen is the panel's business, and GNOME
  needs an extension before it will (`docs/troubleshooting.md` §18). The one thing
  this cannot catch is an icon that renders as an empty square on a specific panel.

The status of each of these is tracked with evidence in
[testing.md](testing.md) and recorded per release in [acceptance.md](acceptance.md).

## 6. When a release is wrong

- **Bad artifact, good code** (a broken archive, a missing file, a wrong
  checksum): fix the packaging, bump the patch version, tag again, publish the
  new draft. Do **not** re-upload a file under a version that has already been
  published — anything already downloaded would silently differ from the
  published checksum, and the checksum is the whole point.
- **Bad code**: publish the next patch version with the fix, and say what
  happened in the changelog's *Security* or *Fixed* section. Deleting the release
  and the tag is the last resort for a leaked credential or a build that should
  never have existed; prefer a loud follow-up over a quiet disappearance, because
  anyone who already has the file will not see it disappear.
- **Every release is reproducible from its tag.** The toolchain is whatever
  `stable` was at that moment; the artifact records which one that was, which is
  the reproducibility we can honestly offer at this stage.

## 7. Deliberately out of scope

Not missing, chosen against for the MVP: code signing (§3), a `.deb` or `.rpm`
(a tarball with `install.sh` covers Debian, Fedora and Arch without a maintainer
per distribution), an MSI or an `.exe` installer, aarch64 and macOS builds,
auto-update, a crash reporter, and a notarized macOS build. Adding one of these
should start from a user asking for it.
