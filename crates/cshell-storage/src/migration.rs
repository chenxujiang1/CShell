use crate::StorageError;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{SqlitePool, query, query_scalar};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

pub(crate) const SCHEMA_VERSION: i64 = 17;
const CREATE_TASK_RUNS_SQL: &str = "CREATE TABLE task_runs(id TEXT PRIMARY KEY, revision INTEGER NOT NULL CHECK(revision>0), state INTEGER NOT NULL CHECK(state BETWEEN 0 AND 7), snapshot_json BLOB NOT NULL, snapshot_sha256 BLOB NOT NULL CHECK(length(snapshot_sha256)=32), meta_json BLOB NOT NULL, name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 128), operator TEXT NOT NULL CHECK(length(CAST(operator AS BLOB)) BETWEEN 1 AND 128), target_count INTEGER NOT NULL CHECK(target_count BETWEEN 1 AND 1000))";
const CREATE_TASK_TARGETS_SQL: &str = "CREATE TABLE task_targets(run_id TEXT NOT NULL REFERENCES task_runs(id) ON DELETE CASCADE, host_id TEXT NOT NULL, state INTEGER NOT NULL CHECK(state BETWEEN 0 AND 7), row_json BLOB NOT NULL, PRIMARY KEY(run_id,host_id))";
const CREATE_TASK_EVENTS_SQL: &str = "CREATE TABLE task_run_events(run_id TEXT NOT NULL REFERENCES task_runs(id) ON DELETE CASCADE, revision INTEGER NOT NULL CHECK(revision>0), event_json BLOB NOT NULL, PRIMARY KEY(run_id,revision))";

async fn create_task_tables(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<(), StorageError> {
    query(CREATE_TASK_RUNS_SQL).execute(&mut **tx).await?;
    query(CREATE_TASK_TARGETS_SQL).execute(&mut **tx).await?;
    query(CREATE_TASK_EVENTS_SQL).execute(&mut **tx).await?;
    create_upload_tables(tx).await
}
async fn create_upload_tables(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<(), StorageError> {
    query("CREATE TABLE upload_runs(id TEXT PRIMARY KEY, name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 128), created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0), snapshot_json BLOB NOT NULL, snapshot_sha256 BLOB NOT NULL CHECK(length(snapshot_sha256)=32), target_count INTEGER NOT NULL CHECK(target_count BETWEEN 1 AND 1000))").execute(&mut **tx).await?;
    query("CREATE TABLE upload_targets(run_id TEXT NOT NULL REFERENCES upload_runs(id) ON DELETE CASCADE, host_id TEXT NOT NULL, revision INTEGER NOT NULL CHECK(revision>0), phase INTEGER NOT NULL CHECK(phase BETWEEN 0 AND 10), row_json BLOB NOT NULL, PRIMARY KEY(run_id,host_id))").execute(&mut **tx).await?;
    query("CREATE TABLE upload_events(run_id TEXT NOT NULL, host_id TEXT NOT NULL, revision INTEGER NOT NULL, event_json BLOB NOT NULL, PRIMARY KEY(run_id,host_id,revision), FOREIGN KEY(run_id,host_id) REFERENCES upload_targets(run_id,host_id) ON DELETE CASCADE)").execute(&mut **tx).await?;
    query("CREATE TRIGGER upload_snapshot_immutable BEFORE UPDATE ON upload_runs BEGIN SELECT RAISE(ABORT,'immutable upload snapshot'); END").execute(&mut **tx).await?;
    create_download_tables(tx).await
}
async fn create_download_tables(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<(), StorageError> {
    query("CREATE TABLE download_runs(id TEXT PRIMARY KEY, name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 128), created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0), snapshot_json BLOB NOT NULL, snapshot_sha256 BLOB NOT NULL CHECK(length(snapshot_sha256)=32), target_count INTEGER NOT NULL CHECK(target_count BETWEEN 1 AND 1000))").execute(&mut **tx).await?;
    query("CREATE TABLE download_targets(run_id TEXT NOT NULL REFERENCES download_runs(id) ON DELETE CASCADE, host_id TEXT NOT NULL, revision INTEGER NOT NULL CHECK(revision>0), phase INTEGER NOT NULL CHECK(phase BETWEEN 0 AND 11), row_json BLOB NOT NULL, PRIMARY KEY(run_id,host_id))").execute(&mut **tx).await?;
    query("CREATE TABLE download_events(run_id TEXT NOT NULL, host_id TEXT NOT NULL, revision INTEGER NOT NULL, event_json BLOB NOT NULL, PRIMARY KEY(run_id,host_id,revision), FOREIGN KEY(run_id,host_id) REFERENCES download_targets(run_id,host_id) ON DELETE CASCADE)").execute(&mut **tx).await?;
    query("CREATE TRIGGER download_snapshot_immutable BEFORE UPDATE ON download_runs BEGIN SELECT RAISE(ABORT,'immutable download snapshot'); END").execute(&mut **tx).await?;
    create_mixed_tables(tx).await
}
async fn create_mixed_tables(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<(), StorageError> {
    query("CREATE TABLE mixed_runs(id TEXT PRIMARY KEY, name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 128), created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0), snapshot_json BLOB NOT NULL, snapshot_sha256 BLOB NOT NULL CHECK(length(snapshot_sha256)=32), target_count INTEGER NOT NULL CHECK(target_count BETWEEN 1 AND 1000))").execute(&mut **tx).await?;
    query("CREATE TABLE mixed_targets(run_id TEXT NOT NULL REFERENCES mixed_runs(id) ON DELETE CASCADE, host_id TEXT NOT NULL, revision INTEGER NOT NULL CHECK(revision>0), state INTEGER NOT NULL CHECK(state BETWEEN 0 AND 6), row_json BLOB NOT NULL, PRIMARY KEY(run_id,host_id))").execute(&mut **tx).await?;
    query("CREATE TABLE mixed_events(run_id TEXT NOT NULL, host_id TEXT NOT NULL, revision INTEGER NOT NULL, event_json BLOB NOT NULL, PRIMARY KEY(run_id,host_id,revision), FOREIGN KEY(run_id,host_id) REFERENCES mixed_targets(run_id,host_id) ON DELETE CASCADE)").execute(&mut **tx).await?;
    query("CREATE TRIGGER mixed_snapshot_immutable BEFORE UPDATE ON mixed_runs BEGIN SELECT RAISE(ABORT,'immutable mixed snapshot'); END").execute(&mut **tx).await?;
    Ok(())
}
const CREATE_HOST_TABLE_SQL: &str = "CREATE TABLE host_catalog_state (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL CHECK (revision > 0), document_json BLOB NOT NULL)";
const CREATE_QUICK_COMMAND_TABLE_SQL: &str = "CREATE TABLE quick_command_state (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL CHECK (revision > 0), document_json BLOB NOT NULL)";
const CREATE_THEME_TABLE_SQL: &str =
    "CREATE TABLE terminal_themes (name TEXT PRIMARY KEY, palette_json TEXT NOT NULL)";
const CREATE_CLIPBOARD_TABLE_SQL: &str = "CREATE TABLE clipboard_policy_state (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL CHECK (revision > 0), document_json TEXT NOT NULL)";

const CREATE_SSH_TABLE_SQL: &str = "CREATE TABLE profile_ssh_connections (
            profile_id TEXT PRIMARY KEY REFERENCES profile_records(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
            host TEXT NOT NULL CHECK (length(host) > 0),
            port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
            username TEXT NOT NULL CHECK (length(CAST(username AS BLOB)) BETWEEN 0 AND 128),
            auth_method INTEGER NOT NULL DEFAULT 0 CHECK (auth_method BETWEEN 0 AND 3),
            private_key_path TEXT,
            certificate_path TEXT,
            agent_backend INTEGER NOT NULL DEFAULT 0 CHECK (agent_backend BETWEEN 0 AND 2),
            agent_identity TEXT,
            route_json TEXT NOT NULL DEFAULT '{\"kind\":\"direct\"}'
        )";

const CREATE_LOCAL_TABLE_SQL: &str = "CREATE TABLE profile_local_connections (
    profile_id TEXT PRIMARY KEY REFERENCES profile_records(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
    configuration_json TEXT NOT NULL
)";

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
            migrate_new_database(&pool).await?;
        }
        1 => {
            create_backup(&pool, path).await?;
            migrate_v1_to_v15(&pool).await?;
        }
        2 => {
            create_backup(&pool, path).await?;
            migrate_v2_to_v15(&pool).await?;
        }
        3 => {
            create_backup(&pool, path).await?;
            migrate_v3_to_v15(&pool).await?;
        }
        4 => {
            create_backup(&pool, path).await?;
            migrate_v4_to_v15(&pool).await?;
        }
        5 => {
            create_backup(&pool, path).await?;
            migrate_v5_to_v15(&pool).await?;
        }
        6 => {
            create_backup(&pool, path).await?;
            migrate_v6_to_v15(&pool).await?;
        }
        7 => {
            create_backup(&pool, path).await?;
            migrate_v7_to_v15(&pool).await?;
        }
        8 => {
            create_backup(&pool, path).await?;
            migrate_v8_to_v15(&pool).await?;
        }
        9 => {
            create_backup(&pool, path).await?;
        }
        10 => {
            create_backup(&pool, path).await?;
        }
        11 => {
            create_backup(&pool, path).await?;
        }
        12 => {
            create_backup(&pool, path).await?;
        }
        13 => {
            create_backup(&pool, path).await?;
        }
        14 => {
            create_backup(&pool, path).await?;
        }
        15 | 16 => {create_backup(&pool,path).await?;}
        SCHEMA_VERSION => {}
        other => return Err(StorageError::UnsupportedSchema(other)),
    }
    let migrated_version: i64 = query_scalar("PRAGMA user_version").fetch_one(&pool).await?;
    if migrated_version == 9 {
        let mut tx = pool.begin().await?;
        query(CREATE_QUICK_COMMAND_TABLE_SQL)
            .execute(&mut *tx)
            .await?;
        query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
        create_task_tables(&mut tx).await?;
        query("PRAGMA user_version = 15").execute(&mut *tx).await?;
        tx.commit().await?;
    }
    if migrated_version == 10 {
        let mut tx = pool.begin().await?;
        query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
        create_task_tables(&mut tx).await?;
        query("PRAGMA user_version = 15").execute(&mut *tx).await?;
        tx.commit().await?;
    }
    if migrated_version == 11 {
        let mut tx = pool.begin().await?;
        create_task_tables(&mut tx).await?;
        query("PRAGMA user_version = 15").execute(&mut *tx).await?;
        tx.commit().await?;
    }
    if migrated_version == 12 {
        let mut tx = pool.begin().await?;
        create_upload_tables(&mut tx).await?;
        query("PRAGMA user_version = 15").execute(&mut *tx).await?;
        tx.commit().await?;
    }
    if migrated_version == 13 {
        let mut tx = pool.begin().await?;
        create_download_tables(&mut tx).await?;
        query("PRAGMA user_version = 15").execute(&mut *tx).await?;
        tx.commit().await?;
    }
    if migrated_version == 14 {
        let mut tx = pool.begin().await?;
        create_mixed_tables(&mut tx).await?;
        query("PRAGMA user_version = 15").execute(&mut *tx).await?;
        tx.commit().await?;
    }
    let options_version: i64 = query_scalar("PRAGMA user_version").fetch_one(&pool).await?;
    if options_version == 15 {
        let mut tx = pool.begin().await?;
        query("ALTER TABLE profile_ssh_connections ADD COLUMN options_json TEXT NOT NULL DEFAULT '{}'")
            .execute(&mut *tx).await?;
        query("PRAGMA user_version = 16").execute(&mut *tx).await?;
        tx.commit().await?;
    }
    let login_version:i64=query_scalar("PRAGMA user_version").fetch_one(&pool).await?;
    if login_version==16 {
        let mut tx=pool.begin().await?;
        query("CREATE TABLE profile_ssh_connections_v17 (
            profile_id TEXT PRIMARY KEY REFERENCES profile_records(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
            host TEXT NOT NULL CHECK(length(host)>0),
            port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
            username TEXT NOT NULL CHECK(length(CAST(username AS BLOB)) BETWEEN 0 AND 128),
            auth_method INTEGER NOT NULL DEFAULT 0 CHECK(auth_method BETWEEN 0 AND 3),
            private_key_path TEXT,certificate_path TEXT,
            agent_backend INTEGER NOT NULL DEFAULT 0 CHECK(agent_backend BETWEEN 0 AND 2),agent_identity TEXT,
            route_json TEXT NOT NULL DEFAULT '{\"kind\":\"direct\"}'
        )").execute(&mut *tx).await?;
        query("ALTER TABLE profile_ssh_connections_v17 ADD COLUMN options_json TEXT NOT NULL DEFAULT '{}'").execute(&mut *tx).await?;
        query("INSERT INTO profile_ssh_connections_v17 (profile_id,host,port,username,auth_method,private_key_path,certificate_path,agent_backend,agent_identity,route_json,options_json) SELECT profile_id,host,port,username,auth_method,private_key_path,certificate_path,agent_backend,agent_identity,route_json,options_json FROM profile_ssh_connections").execute(&mut *tx).await?;
        query("DROP TABLE profile_ssh_connections").execute(&mut *tx).await?;
        query("ALTER TABLE profile_ssh_connections_v17 RENAME TO profile_ssh_connections").execute(&mut *tx).await?;
        query("PRAGMA user_version = 17").execute(&mut *tx).await?;
        tx.commit().await?;
    }
    let check: String = query_scalar("PRAGMA quick_check").fetch_one(&pool).await?;
    if check != "ok" {
        return Err(StorageError::CorruptDatabase);
    }
    Ok(pool)
}

async fn migrate_new_database(pool: &SqlitePool) -> Result<(), StorageError> {
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
    query(CREATE_SSH_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_LOCAL_TABLE_SQL).execute(&mut *tx).await?;
    query("CREATE TABLE workspace_state (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL CHECK (revision > 0), document_json TEXT NOT NULL)").execute(&mut *tx).await?;
    query(CREATE_CLIPBOARD_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_THEME_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_QUICK_COMMAND_TABLE_SQL)
        .execute(&mut *tx)
        .await?;
    query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
    create_task_tables(&mut tx).await?;
    query("PRAGMA user_version = 15").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn migrate_v1_to_v15(pool: &SqlitePool) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    query(CREATE_SSH_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_LOCAL_TABLE_SQL).execute(&mut *tx).await?;
    query("CREATE TABLE workspace_state (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL CHECK (revision > 0), document_json TEXT NOT NULL)").execute(&mut *tx).await?;
    query(CREATE_CLIPBOARD_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_THEME_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_QUICK_COMMAND_TABLE_SQL)
        .execute(&mut *tx)
        .await?;
    query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
    create_task_tables(&mut tx).await?;
    query("PRAGMA user_version = 15").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn migrate_v2_to_v15(pool: &SqlitePool) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    for statement in [
        "ALTER TABLE profile_ssh_connections ADD COLUMN auth_method INTEGER NOT NULL DEFAULT 0 CHECK (auth_method BETWEEN 0 AND 3)",
        "ALTER TABLE profile_ssh_connections ADD COLUMN private_key_path TEXT",
        "ALTER TABLE profile_ssh_connections ADD COLUMN certificate_path TEXT",
        "ALTER TABLE profile_ssh_connections ADD COLUMN agent_backend INTEGER NOT NULL DEFAULT 0 CHECK (agent_backend BETWEEN 0 AND 2)",
        "ALTER TABLE profile_ssh_connections ADD COLUMN agent_identity TEXT",
        "ALTER TABLE profile_ssh_connections ADD COLUMN route_json TEXT NOT NULL DEFAULT '{\"kind\":\"direct\"}'",
    ] {
        query(statement).execute(&mut *tx).await?;
    }
    query(CREATE_LOCAL_TABLE_SQL).execute(&mut *tx).await?;
    query("CREATE TABLE workspace_state (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL CHECK (revision > 0), document_json TEXT NOT NULL)").execute(&mut *tx).await?;
    query(CREATE_CLIPBOARD_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_THEME_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_QUICK_COMMAND_TABLE_SQL)
        .execute(&mut *tx)
        .await?;
    query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
    create_task_tables(&mut tx).await?;
    query("PRAGMA user_version = 15").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn migrate_v3_to_v15(pool: &SqlitePool) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    query("ALTER TABLE profile_ssh_connections ADD COLUMN route_json TEXT NOT NULL DEFAULT '{\"kind\":\"direct\"}'")
        .execute(&mut *tx).await?;
    query(CREATE_LOCAL_TABLE_SQL).execute(&mut *tx).await?;
    query("CREATE TABLE workspace_state (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL CHECK (revision > 0), document_json TEXT NOT NULL)").execute(&mut *tx).await?;
    query(CREATE_CLIPBOARD_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_THEME_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_QUICK_COMMAND_TABLE_SQL)
        .execute(&mut *tx)
        .await?;
    query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
    create_task_tables(&mut tx).await?;
    query("PRAGMA user_version = 15").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn migrate_v4_to_v15(pool: &SqlitePool) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    query(CREATE_LOCAL_TABLE_SQL).execute(&mut *tx).await?;
    query("CREATE TABLE workspace_state (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL CHECK (revision > 0), document_json TEXT NOT NULL)").execute(&mut *tx).await?;
    query(CREATE_CLIPBOARD_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_THEME_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_QUICK_COMMAND_TABLE_SQL)
        .execute(&mut *tx)
        .await?;
    query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
    create_task_tables(&mut tx).await?;
    query("PRAGMA user_version = 15").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

// v5 local configuration gains close_policy; v7 also adds workspace_state.
// Legacy records keep their default without rewriting the catalog revision.
async fn migrate_v5_to_v15(pool: &SqlitePool) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    query("CREATE TABLE workspace_state (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL CHECK (revision > 0), document_json TEXT NOT NULL)").execute(&mut *tx).await?;
    query(CREATE_CLIPBOARD_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_THEME_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_QUICK_COMMAND_TABLE_SQL)
        .execute(&mut *tx)
        .await?;
    query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
    create_task_tables(&mut tx).await?;
    query("PRAGMA user_version = 15").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn migrate_v6_to_v15(pool: &SqlitePool) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    query("CREATE TABLE workspace_state (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL CHECK (revision > 0), document_json TEXT NOT NULL)").execute(&mut *tx).await?;
    query(CREATE_CLIPBOARD_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_THEME_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_QUICK_COMMAND_TABLE_SQL)
        .execute(&mut *tx)
        .await?;
    query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
    create_task_tables(&mut tx).await?;
    query("PRAGMA user_version = 15").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn migrate_v7_to_v15(pool: &SqlitePool) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    query(CREATE_CLIPBOARD_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_THEME_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_QUICK_COMMAND_TABLE_SQL)
        .execute(&mut *tx)
        .await?;
    query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
    create_task_tables(&mut tx).await?;
    query("PRAGMA user_version = 15").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn migrate_v8_to_v15(pool: &SqlitePool) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    query(CREATE_THEME_TABLE_SQL).execute(&mut *tx).await?;
    query(CREATE_QUICK_COMMAND_TABLE_SQL)
        .execute(&mut *tx)
        .await?;
    query(CREATE_HOST_TABLE_SQL).execute(&mut *tx).await?;
    create_task_tables(&mut tx).await?;
    query("PRAGMA user_version = 15").execute(&mut *tx).await?;
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
