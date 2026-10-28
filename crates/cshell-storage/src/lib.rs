//! SQLite-backed profile catalog storage.

mod clipboard;
mod downloads;
mod hosts;
mod migration;
mod mixed;
mod profiles;
mod quick_commands;
mod tasks;
mod uploads;
mod workspace;

pub use migration::restore_backup;
pub use profiles::SqliteProfileRepository;

pub enum StorageError {
    Sqlite(sqlx::Error),
    Io(std::io::Error),
    #[cfg(unix)]
    Join(tokio::task::JoinError),
    InvalidPath,
    UnsupportedSchema(i64),
    CorruptDatabase,
    CorruptBackup,
    LiveJournal,
}
impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(_) => f.write_str("SQLite operation failed"),
            Self::Io(error) => write!(f, "filesystem operation failed ({:?})", error.kind()),
            #[cfg(unix)]
            Self::Join(_) => f.write_str("filesystem sync worker failed"),
            Self::InvalidPath => f.write_str("database path cannot be represented safely"),
            Self::UnsupportedSchema(version) => {
                write!(f, "database schema version {version} is unsupported")
            }
            Self::CorruptDatabase => f.write_str("database integrity check failed"),
            Self::CorruptBackup => f.write_str("backup integrity check failed"),
            Self::LiveJournal => {
                f.write_str("restore requires a closed database without WAL sidecars")
            }
        }
    }
}
impl std::error::Error for StorageError {}
impl From<sqlx::Error> for StorageError {
    fn from(error: sqlx::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<std::io::Error> for StorageError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
#[cfg(unix)]
impl From<tokio::task::JoinError> for StorageError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::Join(error)
    }
}

impl std::fmt::Debug for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
