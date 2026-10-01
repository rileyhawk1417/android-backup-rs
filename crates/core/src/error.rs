//! Top-level error type.
//!
//! Every module exposes its own `thiserror` enum; this is the thin delegating
//! enum that lets `main`, the CLI and the UI report *anything* that can go wrong
//! without matching on module-specific types (see `lib.rs` § "Error convention").

use crate::adb::AdbError;
use crate::backup::BackupError;
use crate::config::ConfigError;
use crate::database::DbError;
use crate::paths::PathError;

/// Anything that can go wrong at the application boundary.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Adb(#[from] AdbError),

    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Database(#[from] DbError),

    #[error(transparent)]
    Backup(#[from] BackupError),

    #[error(transparent)]
    Path(#[from] PathError),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias used by the UI, the CLI and future SDK consumers.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_errors_convert_into_the_top_level_error() {
        let err: Error = PathError::Empty.into();
        assert_eq!(err.to_string(), "path is empty");
        assert!(matches!(err, Error::Path(_)));
    }

    #[test]
    fn display_delegates_to_the_inner_error() {
        let err: Error = BackupError::NoUsableStorage.into();
        assert_eq!(
            err.to_string(),
            "this device did not report a usable shared-storage location"
        );
    }
}
