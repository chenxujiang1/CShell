use crate::StorageError;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{SqlitePool, query, query_scalar};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

pub(crate) const SCHEMA_VERSION: i64 = 1;

pub(crate) async fn open_pool(path: &Path) -> Result<SqlitePool, StorageError> {
    let parent = path.parent().ok_or(StorageError::InvalidPath)?;
    tokio::fs::create_dir_all(parent).await?;
    let existed = tokio::fs::try_exists(path).await?;
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Delete)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let version: i64 = query_scalar("PRAGMA user_version").fetch_one(&pool).await?;
    match version {
        0 => {
            if existed {
                create_backup(&pool, path).await?;
            }
            migrate_v1(&pool).await?;
        }
        SCHEMA_VERSION => {}
        other => return Err(StorageError::UnsupportedSchema(other)),
    }
    let check: String = query_scalar("PRAGMA quick_check").fetch_one(&pool).await?;
    if check != "ok" {
        return Err(StorageError::CorruptDatabase);
    }
    Ok(pool)
}

async fn migrate_v1(pool: &SqlitePool) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    query(
        "CREATE TABLE IF NOT EXISTS profile_catalog_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            revision INTEGER NOT NULL CHECK (revision >= 0),
            terminal_type TEXT NOT NULL,
            theme TEXT NOT NULL,
            logging INTEGER NOT NULL CHECK (logging IN (0, 1))
        )",
    )
    .execute(&mut *tx)
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS profile_folders (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            parent_id TEXT REFERENCES profile_folders(id) DEFERRABLE INITIALLY DEFERRED,
            terminal_type TEXT,
            theme TEXT,
            logging INTEGER CHECK (logging IS NULL OR logging IN (0, 1))
        )",
    )
    .execute(&mut *tx)
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS profile_records (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            kind INTEGER NOT NULL CHECK (kind IN (0, 1)),
            folder_id TEXT REFERENCES profile_folders(id) DEFERRABLE INITIALLY DEFERRED,
            favorite INTEGER NOT NULL CHECK (favorite IN (0, 1)),
            terminal_type TEXT,
            theme TEXT,
            logging INTEGER CHECK (logging IS NULL OR logging IN (0, 1))
        )",
    )
    .execute(&mut *tx)
    .await?;
    query(
        "CREATE TABLE IF NOT EXISTS profile_tags (
            profile_id TEXT NOT NULL REFERENCES profile_records(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
            tag TEXT NOT NULL,
            PRIMARY KEY (profile_id, tag)
        )",
    )
    .execute(&mut *tx)
    .await?;
    query(
        "INSERT INTO profile_catalog_meta
            (id, revision, terminal_type, theme, logging)
         VALUES (1, 0, 'xterm-256color', 'default', 0)",
    )
    .execute(&mut *tx)
    .await?;
    query("PRAGMA user_version = 1").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn create_backup(pool: &SqlitePool, path: &Path) -> Result<PathBuf, StorageError> {
    let parent = path.parent().ok_or(StorageError::InvalidPath)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(StorageError::InvalidPath)?;
    let suffix = Uuid::now_v7();
    let final_path = parent.join(format!("{file_name}.backup-{suffix}.sqlite"));
    let temporary_path = parent.join(format!("{file_name}.backup-{suffix}.tmp"));
    let temporary_name = temporary_path.to_str().ok_or(StorageError::InvalidPath)?;
    let result = async {
        query("VACUUM main INTO ?1")
            .bind(temporary_name)
            .execute(pool)
            .await?;
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&temporary_path)
            .await?
            .sync_all()
            .await?;
        tokio::fs::rename(&temporary_path, &final_path).await?;
        sync_parent(&final_path).await?;
        Ok(final_path)
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary_path).await;
    }
    result
}

/// Replaces a closed database with a verified backup. Callers must close all
/// repository handles first; a live connection could keep writing to old pages.
pub async fn restore_backup(database: &Path, backup: &Path) -> Result<(), StorageError> {
    if tokio::fs::canonicalize(database).await.ok() == tokio::fs::canonicalize(backup).await.ok()
        && tokio::fs::try_exists(database).await?
    {
        return Err(StorageError::InvalidPath);
    }
    let database_name = database.to_str().ok_or(StorageError::InvalidPath)?;
    for suffix in ["-wal", "-shm"] {
        if tokio::fs::try_exists(format!("{database_name}{suffix}")).await? {
            return Err(StorageError::LiveJournal);
        }
    }
    let options = SqliteConnectOptions::new()
        .filename(backup)
        .read_only(true)
        .create_if_missing(false);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|_| StorageError::CorruptBackup)?;
    let check: String = query_scalar("PRAGMA quick_check")
        .fetch_one(&pool)
        .await
        .map_err(|_| StorageError::CorruptBackup)?;
    let version: i64 = query_scalar("PRAGMA user_version")
        .fetch_one(&pool)
        .await
        .map_err(|_| StorageError::CorruptBackup)?;
    pool.close().await;
    if check != "ok" || !(0..=SCHEMA_VERSION).contains(&version) {
        return Err(StorageError::CorruptBackup);
    }

    let parent = database.parent().ok_or(StorageError::InvalidPath)?;
    tokio::fs::create_dir_all(parent).await?;
    let file_name = database
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(StorageError::InvalidPath)?;
    let temporary_path = parent.join(format!("{file_name}.restore-{}.tmp", Uuid::now_v7()));
    let result = async {
        tokio::fs::copy(backup, &temporary_path).await?;
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&temporary_path)
            .await?
            .sync_all()
            .await?;
        tokio::fs::rename(&temporary_path, database).await?;
        sync_parent(database).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary_path).await;
    }
    result
}

#[cfg(unix)]
async fn sync_parent(path: &Path) -> Result<(), StorageError> {
    let parent = path
        .parent()
        .ok_or(StorageError::InvalidPath)?
        .to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all()).await??;
    Ok(())
}

#[cfg(not(unix))]
async fn sync_parent(_path: &Path) -> Result<(), StorageError> {
    Ok(())
}
