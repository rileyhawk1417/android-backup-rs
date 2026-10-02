//! Command-line surface of the desktop binary.
//!
//! The application is a GUI, so almost every argument is meaningless — but two
//! are not. `--version` and `--help` have to work with **no display, no config,
//! and no adb**, because they are what a bug report starts with and what the
//! packaging scripts use to prove that the binary in the tarball is the build
//! the tarball is named after. Anything else is rejected loudly rather than
//! ignored: silently launching the GUI when the user typed `--restore` would
//! teach them the wrong thing about what this build can do.
//!
//! Parsing lives here, in the library, so it can be tested without a window.

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    /// Start the application.
    Run,
    /// Print the version and exit successfully.
    Version,
    /// Print usage and exit successfully.
    Help,
    /// Print usage and exit unsuccessfully.
    Unknown(String),
}

/// Parse arguments *excluding* the program name.
pub fn parse<I, S>(args: I) -> Cli
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter();
    match args.next() {
        None => Cli::Run,
        Some(arg) => {
            let arg = arg.as_ref();
            match arg {
                "--version" | "-V" => Cli::Version,
                "--help" | "-h" => Cli::Help,
                other => Cli::Unknown(other.to_string()),
            }
        }
    }
}

/// The one line `--version` prints.
pub fn version_line() -> String {
    format!("android-backup {}", env!("CARGO_PKG_VERSION"))
}

/// The text `--help` prints.
pub fn help_text() -> String {
    format!(
        "{version}\n\
         \n\
         Back up files from an Android device over ADB.\n\
         \n\
         Usage: android-backup [OPTION]\n\
         \n\
         Options:\n\
         \x20 -h, --help       Print this help and exit\n\
         \x20 -V, --version    Print the version and exit\n\
         \n\
         The application is a graphical program: run it with no arguments to\n\
         open the window. It needs a display; everything else it needs can be\n\
         set up from the interface.\n",
        version = version_line()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_means_run() {
        assert_eq!(parse(Vec::<String>::new()), Cli::Run);
    }

    #[test]
    fn version_flags_are_recognised_in_both_spellings() {
        assert_eq!(parse(["--version"]), Cli::Version);
        assert_eq!(parse(["-V"]), Cli::Version);
    }

    #[test]
    fn help_flags_are_recognised_in_both_spellings() {
        assert_eq!(parse(["--help"]), Cli::Help);
        assert_eq!(parse(["-h"]), Cli::Help);
    }

    #[test]
    fn anything_else_is_reported_not_ignored() {
        // A `--restore` that silently opens the backup window would be worse
        // than an error message.
        assert_eq!(parse(["--restore"]), Cli::Unknown("--restore".to_string()));
        assert_eq!(parse(["photo"]), Cli::Unknown("photo".to_string()));
    }

    #[test]
    fn trailing_arguments_are_ignored_once_a_known_flag_is_seen() {
        // `--version --verbose` is a bug report's way of saying "tell me the
        // version"; it is not worth an error.
        assert_eq!(parse(["--version", "--verbose"]), Cli::Version);
    }

    #[test]
    fn the_version_line_carries_the_crate_version() {
        assert_eq!(
            version_line(),
            format!("android-backup {}", env!("CARGO_PKG_VERSION"))
        );
        assert!(version_line().starts_with("android-backup "));
    }

    #[test]
    fn help_mentions_both_flags_and_the_version() {
        let help = help_text();
        for needle in ["--help", "--version", &version_line()] {
            assert!(help.contains(needle), "help is missing {needle:?}:\n{help}");
        }
    }
}
