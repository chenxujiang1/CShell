//! SQLite-backed profile catalog storage.

mod migration;
mod profiles;

pub use migration::restore_backup;
pub use profiles::SqliteProfileRepository;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] sqlx::Error),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(unix)]
    #[error("filesystem sync task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("database path cannot be represented safely")]
    InvalidPath,
    #[error("database schema version {0} is unsupported")]
    UnsupportedSchema(i64),
    #[error("database integrity check failed")]
    CorruptDatabase,
    #[error("backup integrity check failed")]
    CorruptBackup,
    #[error("restore requires a closed database without WAL sidecars")]
    LiveJournal,
}
