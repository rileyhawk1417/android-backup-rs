//! Run at login (Wave 9, E09.5).
//!
//! # Why
//!
//! A tray application that is only running when the user remembers to start it is
//! only half of what it is for. Automation happens when a phone is plugged in, and
//! phones are plugged in when the user thinks of it — the app has to already be
//! there. The plan's promise is "run at login", and this module is the whole of it.
//!
//! # What it is not
//!
//! Not a service, not a daemon, and not an installer. On Windows it is one value in
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (no administrator, no
//! service, and the user can see and remove it from Task Manager's Startup tab); on
//! Linux it is one file in `~/.config/autostart` (the freedesktop convention, which
//! every desktop environment reads and the user can delete). Nothing is written to
//! any other machine-wide location, and `disable` removes exactly what `enable`
//! wrote.
//!
//! # Declarative, not a switch
//!
//! The application does not enable startup when a button is clicked; it *reconciles*
//! at launch: the configuration file says whether startup is wanted, and the desktop
//! is brought in line with it if it is not already ([`reconcile`]). That makes the
//! setting survive a reinstall, makes it correctable by editing `config.toml`, and
//! means a login entry the user deleted by hand is simply restored — or, better, is
//! not restored at all if they also set `background.run_at_login = false`.
//!
//! # Failures are logged, never fatal
//!
//! A registry that refuses to be written, a home directory that cannot be found: the
//! user loses a convenience, not the application. Every entry point returns `Err` or
//! logs, and none of them stops the window from opening.

use std::path::PathBuf;

use android_backup_core::fsx;

/// The identifier shared by the two implementations: the registry value name on
/// Windows, the file name on Linux. Deliberately the same as the notification
/// application id ([`crate::notifications`]) so that one name describes the program
/// in every list the user can read.
pub const ENTRY_NAME: &str = "AndroidBackup";

/// What the login entry is doing right now, as far as this process can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The entry exists and starts this program.
    Enabled,
    /// There is no entry.
    Disabled,
    /// This platform has no mechanism we know how to use, or the mechanism cannot
    /// be read. The string is shown in the log and later in the settings panel
    /// (E09.6): "off and cannot be turned on" must not look like "off".
    Unavailable(String),
}

impl Status {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Status::Enabled)
    }
}

/// The login entry, as the rest of the application needs it.
pub trait Startup {
    /// Read the current state. Never fails hard: a mechanism that cannot be read is
    /// [`Status::Unavailable`], which is a fact about the machine.
    fn status(&self) -> Status;
    /// Write the entry, replacing any previous one.
    fn enable(&self) -> Result<(), String>;
    /// Remove the entry. Removing one that is not there is not an error.
    fn disable(&self) -> Result<(), String>;
    /// For the log line: where the entry lives.
    fn location(&self) -> String;
}

/// What [`reconcile`] did, for the caller to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciled {
    /// The desktop already said what the configuration says.
    AlreadyCorrect,
    /// The entry was added.
    Enabled,
    /// The entry was removed.
    Disabled,
    /// Nothing could be done. The string is the reason.
    Failed(String),
    /// The platform cannot do this at all.
    Unavailable(String),
}

impl Reconciled {
    /// The log line, and later the status text. Written once here so the wording
    /// cannot drift between the two.
    pub fn summary(&self) -> String {
        match self {
            Self::AlreadyCorrect => "run at login: no change needed".to_string(),
            Self::Enabled => "run at login: enabled".to_string(),
            Self::Disabled => "run at login: disabled".to_string(),
            Self::Failed(reason) => format!("run at login: could not be changed ({reason})"),
            Self::Unavailable(reason) => format!("run at login: not available ({reason})"),
        }
    }
}

/// Bring the desktop in line with `wanted`.
///
/// Idempotent, and cheap when nothing is wanted: a user who never turns this on has
/// no entry, and a `status()` call on a machine with no mechanism is one failed read.
/// The only writes are the transitions.
pub fn reconcile(wanted: bool, startup: &dyn Startup) -> Reconciled {
    match (wanted, startup.status()) {
        (_, Status::Unavailable(reason)) => Reconciled::Unavailable(reason),
        (true, Status::Enabled) | (false, Status::Disabled) => Reconciled::AlreadyCorrect,
        (true, Status::Disabled) => match startup.enable() {
            Ok(()) => Reconciled::Enabled,
            Err(err) => Reconciled::Failed(err),
        },
        (false, Status::Enabled) => match startup.disable() {
            Ok(()) => Reconciled::Disabled,
            Err(err) => Reconciled::Failed(err),
        },
    }
}

/// The login entry for the machine this build is running on.
///
/// `exe` is the program to start — the running binary, taken from the process rather
/// than guessed, so that a portable build in `~/Downloads/android-backup` and an
/// installed one in `Program Files` each point at themselves.
pub fn for_this_platform(exe: PathBuf) -> Box<dyn Startup> {
    #[cfg(windows)]
    {
        Box::new(WindowsRunKey::new(exe))
    }
    #[cfg(not(windows))]
    {
        Box::new(DesktopEntry::new(exe, autostart_dir()))
    }
}

/// The program's own path, as an absolute path where possible.
///
/// A relative path in a login entry is a bug waiting for the wrong working
/// directory, and both mechanisms store a path that will be used much later by
/// another process.
pub fn current_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("could not find this program's own path: {err}"))?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// The arguments a login start passes: `--hidden`, so the window waits in the
/// notification area instead of opening over whatever the user is doing at login.
pub const LOGIN_ARGUMENTS: &[&str] = &["--hidden"];

// ---------------------------------------------------------------------------
// Linux: a freedesktop autostart entry
// ---------------------------------------------------------------------------

/// `~/.config/autostart/<ENTRY_NAME>.desktop`.
///
/// `XDG_CONFIG_HOME` is honoured when it is set and absolute, as the spec says;
/// otherwise `~/.config`. An environment that provides neither (no `HOME`, no
/// `XDG_CONFIG_HOME`) has no autostart directory, and the entry reports
/// [`Status::Unavailable`] rather than writing to a guess.
pub fn autostart_dir() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                return Some(path.join("autostart"));
            }
        }
        _ => {}
    }
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".config").join("autostart"))
}

/// The Linux login entry: one `.desktop` file.
pub struct DesktopEntry {
    exe: PathBuf,
    /// `None` when the machine has no autostart directory to write to.
    dir: Option<PathBuf>,
}

impl DesktopEntry {
    pub fn new(exe: PathBuf, dir: Option<PathBuf>) -> Self {
        Self { exe, dir }
    }

    /// Where the file goes, when there is somewhere for it to go.
    pub fn path(&self) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|dir| dir.join(format!("{ENTRY_NAME}.desktop")))
    }

    /// The file's contents. Pure, so the escaping can be tested — the executable
    /// path may contain spaces, quotes and backslashes (a user's home directory
    /// might), and the format has rules for all three.
    pub fn contents(&self) -> String {
        // The freedesktop "Desktop Entry" spec: within a quoted `Exec` argument, `"`
        // and `\` are escaped with a backslash, and `$` and backtick must be escaped
        // too because the value is macro-expanded by the launcher.
        let escaped = self
            .exe
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('`', "\\`");
        let exec = format!("\"{escaped}\"");
        let args = LOGIN_ARGUMENTS.join(" ");
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Version=1.0\n\
             Name=Android Backup\n\
             Comment=Back up an Android device to this computer\n\
             Exec={exec} {args}\n\
             Terminal=false\n\
             StartupNotify=false\n\
             X-GNOME-Autostart-enabled=true\n\
             Categories=Utility;\n"
        )
    }
}

impl Startup for DesktopEntry {
    fn status(&self) -> Status {
        let Some(path) = self.path() else {
            return Status::Unavailable(
                "no autostart directory: neither XDG_CONFIG_HOME nor HOME is usable".to_string(),
            );
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) if contents.trim().is_empty() => Status::Disabled,
            Ok(_) => Status::Enabled,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Status::Disabled,
            Err(err) => Status::Unavailable(format!("could not read {}: {err}", path.display())),
        }
    }

    fn enable(&self) -> Result<(), String> {
        let Some(path) = self.path() else {
            return Err(
                "no autostart directory: neither XDG_CONFIG_HOME nor HOME is usable".to_string(),
            );
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
        }
        // Written whole or not at all, so a crash halfway through cannot leave a
        // truncated entry that a desktop reads as "start nothing" or, worse, half
        // a command.
        fsx::write_atomic(&path, &self.contents())
            .map_err(|err| format!("could not write {}: {err}", path.display()))
    }

    fn disable(&self) -> Result<(), String> {
        let Some(path) = self.path() else {
            return Err(
                "no autostart directory: neither XDG_CONFIG_HOME nor HOME is usable".to_string(),
            );
        };
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // Already gone is the state we wanted.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!("could not remove {}: {err}", path.display())),
        }
    }

    fn location(&self) -> String {
        match self.path() {
            Some(path) => path.display().to_string(),
            None => "nowhere (no autostart directory)".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: one value under HKCU\...\Run
// ---------------------------------------------------------------------------

/// How a command is run. A seam, not a convenience: it lets the Windows
/// implementation be tested on the machine that builds it, which is the only way
/// the argument list is ever checked before a user's registry pays for a mistake.
///
/// The whole Windows section is compiled for tests on every platform (and shipped
/// only on Windows), so the argument lists below are type-checked and asserted in
/// CI even though CI has no registry to write to.
#[cfg(any(windows, test))]
pub trait CommandRunner {
    /// Run a program with arguments, returning its output when it succeeded.
    fn run(&self, program: &str, args: &[String]) -> Result<(), String>;
}

/// The real one.
#[cfg(any(windows, test))]
pub struct RealCommandRunner;

#[cfg(any(windows, test))]
impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<(), String> {
        let mut command = std::process::Command::new(program);
        command.args(args);
        // Windows: `reg.exe` is a console program, and a console window flashing up
        // at every launch would be a worse bug than the one being fixed.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let output = command
            .output()
            .map_err(|err| format!("could not run {program}: {err}"))?;
        if output.status.success() {
            return Ok(());
        }
        // The first line of stderr is the useful part; the rest is the usage text
        // `reg.exe` prints under it.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .next()
            .unwrap_or("no output")
            .trim()
            .to_string();
        Err(format!("{program} failed: {reason}"))
    }
}

/// The Windows login entry: the `Run` value, in the current user's hive.
///
/// `reg.exe` rather than a registry API: it is present on every supported version of
/// Windows, it needs no dependency in the build, and the exact command it runs is
/// something a user can read in the log and reproduce by hand.
#[cfg(any(windows, test))]
pub struct WindowsRunKey {
    exe: PathBuf,
    runner: Box<dyn CommandRunner>,
}

/// The key the entry lives under. `HKCU`, so that no administrator is involved and
/// the user can remove it themselves.
#[cfg(any(windows, test))]
pub const WINDOWS_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(any(windows, test))]
impl WindowsRunKey {
    pub fn new(exe: PathBuf) -> Self {
        Self {
            exe,
            runner: Box::new(RealCommandRunner),
        }
    }

    /// Same, with a scripted runner (tests).
    pub fn with_runner(exe: PathBuf, runner: Box<dyn CommandRunner>) -> Self {
        Self { exe, runner }
    }

    /// The value `reg.exe` stores: the quoted executable, then the login arguments.
    ///
    /// Windows parses the value with `CreateProcess`'s rules, where the quotes are
    /// what keep `C:\Program Files\...` from being read as `C:\Program` with
    /// `Files\...` as its first argument.
    pub fn value(&self) -> String {
        let args = LOGIN_ARGUMENTS.join(" ");
        format!("\"{}\" {args}", self.exe.display())
    }

    /// The `reg add` argument list.
    pub fn add_args(&self) -> Vec<String> {
        vec![
            "add".to_string(),
            WINDOWS_RUN_KEY.to_string(),
            "/v".to_string(),
            ENTRY_NAME.to_string(),
            "/t".to_string(),
            "REG_SZ".to_string(),
            "/d".to_string(),
            self.value(),
            "/f".to_string(),
        ]
    }

    /// The `reg delete` argument list. `/f` because a missing value is the state we
    /// wanted, and `reg` would otherwise ask a question nobody is there to answer.
    pub fn delete_args(&self) -> Vec<String> {
        vec![
            "delete".to_string(),
            WINDOWS_RUN_KEY.to_string(),
            "/v".to_string(),
            ENTRY_NAME.to_string(),
            "/f".to_string(),
        ]
    }

    /// The `reg query` argument list.
    pub fn query_args(&self) -> Vec<String> {
        vec![
            "query".to_string(),
            WINDOWS_RUN_KEY.to_string(),
            "/v".to_string(),
            ENTRY_NAME.to_string(),
        ]
    }
}

#[cfg(any(windows, test))]
impl Startup for WindowsRunKey {
    fn status(&self) -> Status {
        match self.runner.run("reg.exe", &self.query_args()) {
            Ok(()) => Status::Enabled,
            // A value that is not there is `reg`'s "cannot find" error, which is the
            // ordinary disabled state; anything else (no `reg.exe`, no hive) is the
            // machine telling us it cannot answer.
            Err(err) if err.contains("cannot find") => Status::Disabled,
            Err(err) if err.contains("could not run") => Status::Unavailable(err),
            // Anything else `reg` says about a value that is not there (an older
            // Windows words it differently) is the disabled state.
            Err(_) => Status::Disabled,
        }
    }

    fn enable(&self) -> Result<(), String> {
        self.runner.run("reg.exe", &self.add_args())
    }

    fn disable(&self) -> Result<(), String> {
        self.runner.run("reg.exe", &self.delete_args())
    }

    fn location(&self) -> String {
        format!("{WINDOWS_RUN_KEY}\\{ENTRY_NAME}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::Mutex;

    /// A `Startup` that records what was asked of it.
    struct Fake {
        status: Status,
        fail: Option<String>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl Fake {
        fn new(status: Status) -> Self {
            Self {
                status,
                fail: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing(status: Status, reason: &str) -> Self {
            Self {
                status,
                fail: Some(reason.to_string()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Startup for Fake {
        fn status(&self) -> Status {
            self.calls.lock().unwrap().push("status");
            self.status.clone()
        }

        fn enable(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("enable");
            match &self.fail {
                Some(reason) => Err(reason.clone()),
                None => Ok(()),
            }
        }

        fn disable(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("disable");
            match &self.fail {
                Some(reason) => Err(reason.clone()),
                None => Ok(()),
            }
        }

        fn location(&self) -> String {
            "a test".to_string()
        }
    }

    #[test]
    fn reconcile_is_a_no_op_when_the_desktop_already_agrees() {
        let want = Fake::new(Status::Enabled);
        assert_eq!(reconcile(true, &want), Reconciled::AlreadyCorrect);
        assert_eq!(want.calls(), vec!["status"], "nothing may be written");

        let unwanted = Fake::new(Status::Disabled);
        assert_eq!(reconcile(false, &unwanted), Reconciled::AlreadyCorrect);
        assert_eq!(unwanted.calls(), vec!["status"], "nothing may be written");
    }

    #[test]
    fn reconcile_writes_only_the_transition() {
        let off = Fake::new(Status::Disabled);
        assert_eq!(reconcile(true, &off), Reconciled::Enabled);
        assert_eq!(off.calls(), vec!["status", "enable"]);

        let on = Fake::new(Status::Enabled);
        assert_eq!(reconcile(false, &on), Reconciled::Disabled);
        assert_eq!(on.calls(), vec!["status", "disable"]);
    }

    #[test]
    fn an_unavailable_platform_is_reported_and_never_written_to() {
        let fake = Fake::new(Status::Unavailable("no home directory".to_string()));
        let outcome = reconcile(true, &fake);
        assert_eq!(
            outcome,
            Reconciled::Unavailable("no home directory".to_string())
        );
        assert_eq!(fake.calls(), vec!["status"], "there is nothing to enable");
        assert!(outcome.summary().contains("no home directory"));
    }

    #[test]
    fn a_failed_write_is_reported_rather_than_pretended() {
        let fake = Fake::failing(Status::Disabled, "access denied");
        match reconcile(true, &fake) {
            Reconciled::Failed(reason) => assert_eq!(reason, "access denied"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn the_linux_entry_starts_this_program_hidden() {
        let entry = DesktopEntry::new(PathBuf::from("/opt/android-backup"), None);
        let contents = entry.contents();
        assert!(contents.contains("Type=Application"));
        assert!(contents.contains("Exec=\"/opt/android-backup\" --hidden"));
        // A login start that opens a window is the thing being avoided.
        assert!(contents.contains("--hidden"));
        assert!(contents.contains("X-GNOME-Autostart-enabled=true"));
        // The file has to end with a newline: tools that append assume it.
        assert!(contents.ends_with('\n'));
    }

    #[test]
    fn a_path_with_spaces_quotes_and_dollars_survives_the_entry_format() {
        let entry = DesktopEntry::new(
            PathBuf::from("/home/a b/\"odd\" $dir/back`up"),
            Some(PathBuf::from("/tmp/autostart")),
        );
        let contents = entry.contents();
        let exec = contents
            .lines()
            .find(|line| line.starts_with("Exec="))
            .expect("an Exec line");
        assert_eq!(
            exec,
            "Exec=\"/home/a b/\\\"odd\\\" \\$dir/back\\`up\" --hidden"
        );
    }

    #[test]
    fn enable_then_disable_round_trips_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let entry = DesktopEntry::new(
            PathBuf::from("/opt/android-backup"),
            Some(dir.path().join("autostart")),
        );
        assert_eq!(entry.status(), Status::Disabled);

        entry.enable().expect("enable");
        assert_eq!(entry.status(), Status::Enabled);
        let path = entry.path().expect("a path");
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), entry.contents());
        // The scratch file the write goes through must not be left behind.
        let leftovers: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers,
            vec![format!("{ENTRY_NAME}.desktop")],
            "{leftovers:?}"
        );

        entry.disable().expect("disable");
        assert_eq!(entry.status(), Status::Disabled);
        assert!(!path.exists());
    }

    #[test]
    fn disabling_an_entry_that_is_not_there_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let entry = DesktopEntry::new(
            PathBuf::from("/opt/android-backup"),
            Some(dir.path().join("autostart")),
        );
        entry.disable().expect("removing nothing must succeed");
    }

    #[test]
    fn without_an_autostart_directory_nothing_is_written_and_it_says_so() {
        let entry = DesktopEntry::new(PathBuf::from("/opt/android-backup"), None);
        assert!(matches!(entry.status(), Status::Unavailable(_)));
        assert!(entry.enable().is_err());
        assert_eq!(entry.location(), "nowhere (no autostart directory)");
    }

    /// The Windows entry's argument list, checked on the machine that builds it.
    #[test]
    fn the_windows_run_key_names_the_hive_the_value_and_the_hidden_flag() {
        let key = WindowsRunKey::with_runner(
            PathBuf::from(r"C:\Program Files\Android Backup\android-backup.exe"),
            Box::new(RealCommandRunner),
        );
        assert_eq!(
            key.value(),
            r#""C:\Program Files\Android Backup\android-backup.exe" --hidden"#
        );
        assert_eq!(
            key.add_args(),
            vec![
                "add",
                WINDOWS_RUN_KEY,
                "/v",
                ENTRY_NAME,
                "/t",
                "REG_SZ",
                "/d",
                r#""C:\Program Files\Android Backup\android-backup.exe" --hidden"#,
                "/f",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
        assert_eq!(
            key.delete_args(),
            vec!["delete", WINDOWS_RUN_KEY, "/v", ENTRY_NAME, "/f"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            key.query_args(),
            vec!["query", WINDOWS_RUN_KEY, "/v", ENTRY_NAME]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_windows_entry_lives_in_the_current_users_hive() {
        // A machine-wide entry would need an administrator and would be a different
        // promise: this is "when *I* log in".
        assert!(WINDOWS_RUN_KEY.starts_with("HKCU\\"));
        assert!(!WINDOWS_RUN_KEY.starts_with("HKLM\\"));
    }

    #[test]
    fn the_windows_entry_reads_its_own_status_through_the_runner() {
        // `reg query` prints "cannot find" for a value that is not there, which is
        // the ordinary disabled state rather than a broken machine.
        let (runner, _) = ScriptedRunner::failing(
            "reg.exe failed: ERROR: The system was unable to find the specified registry key or value.",
        );
        let key = WindowsRunKey::with_runner(PathBuf::from(r"C:\app.exe"), runner);
        assert_eq!(key.status(), Status::Disabled);

        let (runner, seen) = ScriptedRunner::succeeding();
        let key = WindowsRunKey::with_runner(PathBuf::from(r"C:\app.exe"), runner);
        assert_eq!(key.status(), Status::Enabled);
        key.enable().expect("enable");
        key.disable().expect("disable");
        assert_eq!(seen.calls().len(), 3, "query, add, delete — in that order");
    }

    /// A runner that records the programs and arguments it was asked to run.
    ///
    /// The recording lives behind an [`Arc`] so a test can hold a handle to it after
    /// the runner has been boxed into the entry under test.
    /// One recorded call: the program, then its arguments.
    type Call = (String, Vec<String>);

    #[derive(Clone, Default)]
    struct Recorder(std::sync::Arc<Mutex<Vec<Call>>>);

    impl Recorder {
        fn calls(&self) -> Vec<Call> {
            self.0.lock().unwrap().clone()
        }
    }

    struct ScriptedRunner {
        fail_with: Option<String>,
        seen: Recorder,
    }

    impl ScriptedRunner {
        fn succeeding() -> (Box<Self>, Recorder) {
            let seen = Recorder::default();
            let runner = Box::new(Self {
                fail_with: None,
                seen: seen.clone(),
            });
            (runner, seen)
        }

        fn failing(reason: &str) -> (Box<Self>, Recorder) {
            let seen = Recorder::default();
            let runner = Box::new(Self {
                fail_with: Some(reason.to_string()),
                seen: seen.clone(),
            });
            (runner, seen)
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<(), String> {
            self.seen
                .0
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            match &self.fail_with {
                Some(reason) => Err(reason.clone()),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn enabling_the_windows_entry_runs_exactly_one_well_formed_command() {
        let (runner, seen) = ScriptedRunner::succeeding();
        let key = WindowsRunKey::with_runner(PathBuf::from(r"C:\app.exe"), runner);
        key.enable().expect("enable");

        let calls = seen.calls();
        assert_eq!(calls.len(), 1, "one command, not a query and an add");
        assert_eq!(calls[0].0, "reg.exe");
        assert_eq!(calls[0].1, key.add_args());
        assert!(calls[0].1.contains(&WINDOWS_RUN_KEY.to_string()));
        assert!(calls[0].1.contains(&ENTRY_NAME.to_string()));
    }

    #[test]
    fn disabling_the_windows_entry_removes_the_same_value_and_never_asks_a_question() {
        let (runner, seen) = ScriptedRunner::succeeding();
        let key = WindowsRunKey::with_runner(PathBuf::from(r"C:\app.exe"), runner);
        key.disable().expect("disable");

        let calls = seen.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, key.delete_args());
        // `reg delete` prompts unless it is forced, and a login-time process has
        // nobody to answer.
        assert!(calls[0].1.contains(&"/f".to_string()));
    }

    #[test]
    fn a_runner_that_cannot_be_started_is_unavailable_rather_than_disabled() {
        // "There is no entry" and "this machine will not tell me" are different
        // states, and the settings panel must not confuse them (E09.6).
        let (runner, _) = ScriptedRunner::failing("could not run reg.exe: not found");
        let key = WindowsRunKey::with_runner(PathBuf::from(r"C:\app.exe"), runner);
        assert!(matches!(key.status(), Status::Unavailable(_)));
    }
}
