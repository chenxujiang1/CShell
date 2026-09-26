use crate::{StorageError, migration};
use async_trait::async_trait;
use cshell_application::{
    CatalogSnapshot, ProfileCatalog, ProfileRepository, ProfileRepositoryError,
};
use cshell_domain::{
    FolderId, ProfileFolder, ProfileId, ProfileKind, ProfileRecord, SshAgentBackend, SshAuthMethod,
    SshConnectionRecord, TerminalDefaults, TerminalOverrides,
};
use sqlx::{Row, SqlitePool, query};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug)]
pub struct SqliteProfileRepository {
    path: PathBuf,
    pool: SqlitePool,
}

impl SqliteProfileRepository {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        let pool = migration::open_pool(&path).await?;
        Ok(Self { path, pool })
    }

    /// Creates a consistent, standalone SQLite backup alongside the database.
    pub async fn backup(&self) -> Result<PathBuf, StorageError> {
        migration::create_backup(&self.pool, &self.path).await
    }

    pub async fn close(self) {
        self.pool.close().await;
    }
}

#[async_trait]
impl ProfileRepository for SqliteProfileRepository {
    async fn load(&self) -> Result<CatalogSnapshot, ProfileRepositoryError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        let meta = query(
            "SELECT revision, terminal_type, theme, logging
             FROM profile_catalog_meta WHERE id = 1",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| ProfileRepositoryError::Corrupt)?;
        let revision: i64 = meta
            .try_get("revision")
            .map_err(|_| ProfileRepositoryError::Corrupt)?;
        let revision = u64::try_from(revision).map_err(|_| ProfileRepositoryError::Corrupt)?;
        let defaults = TerminalDefaults {
            terminal_type: meta
                .try_get("terminal_type")
                .map_err(|_| ProfileRepositoryError::Corrupt)?,
            theme: meta
                .try_get("theme")
                .map_err(|_| ProfileRepositoryError::Corrupt)?,
            logging: read_bool(&meta, "logging")?,
        };

        let folder_rows = query(
            "SELECT id, name, parent_id, terminal_type, theme, logging
             FROM profile_folders ORDER BY id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| ProfileRepositoryError::Unavailable)?;
        let mut folders = Vec::with_capacity(folder_rows.len());
        for row in folder_rows {
            let id: String = row.try_get("id").map_err(corrupt)?;
            let parent_id: Option<String> = row.try_get("parent_id").map_err(corrupt)?;
            folders.push(ProfileFolder {
                id: parse_folder_id(&id)?,
                name: row.try_get("name").map_err(corrupt)?,
                parent_id: parent_id.as_deref().map(parse_folder_id).transpose()?,
                terminal: read_overrides(&row)?,
            });
        }

        let profile_rows = query(
            "SELECT id, name, kind, folder_id, favorite, terminal_type, theme, logging
             FROM profile_records ORDER BY id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| ProfileRepositoryError::Unavailable)?;
        let mut profiles = Vec::with_capacity(profile_rows.len());
        let mut indexes = HashMap::with_capacity(profile_rows.len());
        for row in profile_rows {
            let id: String = row.try_get("id").map_err(corrupt)?;
            let id = parse_profile_id(&id)?;
            let folder_id: Option<String> = row.try_get("folder_id").map_err(corrupt)?;
            let kind: i64 = row.try_get("kind").map_err(corrupt)?;
            let kind = match kind {
                0 => ProfileKind::Ssh,
                1 => ProfileKind::Local,
                _ => return Err(ProfileRepositoryError::Corrupt),
            };
            indexes.insert(id, profiles.len());
            profiles.push(ProfileRecord {
                id,
                name: row.try_get("name").map_err(corrupt)?,
                kind,
                folder_id: folder_id.as_deref().map(parse_folder_id).transpose()?,
                tags: BTreeSet::new(),
                favorite: read_bool(&row, "favorite")?,
                terminal: read_overrides(&row)?,
            });
        }
        let tag_rows = query("SELECT profile_id, tag FROM profile_tags ORDER BY profile_id, tag")
            .fetch_all(&mut *tx)
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        for row in tag_rows {
            let id: String = row.try_get("profile_id").map_err(corrupt)?;
            let tag: String = row.try_get("tag").map_err(corrupt)?;
            let index = indexes
                .get(&parse_profile_id(&id)?)
                .ok_or(ProfileRepositoryError::Corrupt)?;
            profiles[*index].tags.insert(tag);
        }
        let connection_rows = query(
            "SELECT profile_id, host, port, username, auth_method, private_key_path, certificate_path, agent_backend, agent_identity, route_json FROM profile_ssh_connections ORDER BY profile_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| ProfileRepositoryError::Unavailable)?;
        let mut ssh_connections = Vec::with_capacity(connection_rows.len());
        for row in connection_rows {
            let profile_id: String = row.try_get("profile_id").map_err(corrupt)?;
            let port: i64 = row.try_get("port").map_err(corrupt)?;
            ssh_connections.push(SshConnectionRecord {
                profile_id: parse_profile_id(&profile_id)?,
                host: row.try_get("host").map_err(corrupt)?,
                port: u16::try_from(port).map_err(|_| ProfileRepositoryError::Corrupt)?,
                username: row.try_get("username").map_err(corrupt)?,
                auth_method: match row.try_get::<i64, _>("auth_method").map_err(corrupt)? {
                    0 => SshAuthMethod::Password,
                    1 => SshAuthMethod::PrivateKey,
                    2 => SshAuthMethod::Certificate,
                    3 => SshAuthMethod::Agent,
                    _ => return Err(ProfileRepositoryError::Corrupt),
                },
                private_key_path: row.try_get("private_key_path").map_err(corrupt)?,
                certificate_path: row.try_get("certificate_path").map_err(corrupt)?,
                agent_backend: match row.try_get::<i64, _>("agent_backend").map_err(corrupt)? {
                    0 => SshAgentBackend::Auto,
                    1 => SshAgentBackend::OpenSsh,
                    2 => SshAgentBackend::Pageant,
                    _ => return Err(ProfileRepositoryError::Corrupt),
                },
                agent_identity: row.try_get("agent_identity").map_err(corrupt)?,
                route: serde_json::from_str(
                    &row.try_get::<String, _>("route_json").map_err(corrupt)?,
                )
                .map_err(|_| ProfileRepositoryError::Corrupt)?,
            });
        }
        tx.commit()
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        let snapshot = CatalogSnapshot {
            revision,
            defaults,
            folders,
            profiles,
            ssh_connections,
        };
        ProfileCatalog::from_snapshot(snapshot.clone())
            .map_err(|_| ProfileRepositoryError::Corrupt)?;
        Ok(snapshot)
    }

    async fn commit(
        &self,
        expected_revision: u64,
        next: CatalogSnapshot,
    ) -> Result<(), ProfileRepositoryError> {
        if next.revision
            != expected_revision
                .checked_add(1)
                .ok_or(ProfileRepositoryError::Corrupt)?
        {
            return Err(ProfileRepositoryError::Corrupt);
        }
        ProfileCatalog::from_snapshot(next.clone()).map_err(|_| ProfileRepositoryError::Corrupt)?;
        let revision = i64::try_from(next.revision).map_err(|_| ProfileRepositoryError::Corrupt)?;
        let expected =
            i64::try_from(expected_revision).map_err(|_| ProfileRepositoryError::Corrupt)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        let updated = query(
            "UPDATE profile_catalog_meta
             SET revision = ?1, terminal_type = ?2, theme = ?3, logging = ?4
             WHERE id = 1 AND revision = ?5",
        )
        .bind(revision)
        .bind(&next.defaults.terminal_type)
        .bind(&next.defaults.theme)
        .bind(i64::from(next.defaults.logging))
        .bind(expected)
        .execute(&mut *tx)
        .await
        .map_err(|_| ProfileRepositoryError::Unavailable)?
        .rows_affected();
        if updated != 1 {
            return Err(ProfileRepositoryError::Conflict);
        }

        query("DELETE FROM profile_ssh_connections")
            .execute(&mut *tx)
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        query("DELETE FROM profile_tags")
            .execute(&mut *tx)
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        query("DELETE FROM profile_records")
            .execute(&mut *tx)
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        query("DELETE FROM profile_folders")
            .execute(&mut *tx)
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        for folder in &next.folders {
            query(
                "INSERT INTO profile_folders
                 (id, name, parent_id, terminal_type, theme, logging)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .bind(folder.id.to_string())
            .bind(&folder.name)
            .bind(folder.parent_id.map(|id| id.to_string()))
            .bind(&folder.terminal.terminal_type)
            .bind(&folder.terminal.theme)
            .bind(folder.terminal.logging.map(i64::from))
            .execute(&mut *tx)
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        }
        for profile in &next.profiles {
            query(
                "INSERT INTO profile_records
                 (id, name, kind, folder_id, favorite, terminal_type, theme, logging)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .bind(profile.id.to_string())
            .bind(&profile.name)
            .bind(match profile.kind {
                ProfileKind::Ssh => 0_i64,
                ProfileKind::Local => 1_i64,
            })
            .bind(profile.folder_id.map(|id| id.to_string()))
            .bind(i64::from(profile.favorite))
            .bind(&profile.terminal.terminal_type)
            .bind(&profile.terminal.theme)
            .bind(profile.terminal.logging.map(i64::from))
            .execute(&mut *tx)
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
            for tag in &profile.tags {
                query("INSERT INTO profile_tags (profile_id, tag) VALUES (?1, ?2)")
                    .bind(profile.id.to_string())
                    .bind(tag)
                    .execute(&mut *tx)
                    .await
                    .map_err(|_| ProfileRepositoryError::Unavailable)?;
            }
        }
        for connection in &next.ssh_connections {
            query(
                "INSERT INTO profile_ssh_connections (profile_id, host, port, username, auth_method, private_key_path, certificate_path, agent_backend, agent_identity, route_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )
            .bind(connection.profile_id.to_string())
            .bind(&connection.host)
            .bind(i64::from(connection.port))
            .bind(&connection.username)
            .bind(match connection.auth_method {
                SshAuthMethod::Password => 0_i64,
                SshAuthMethod::PrivateKey => 1,
                SshAuthMethod::Certificate => 2,
                SshAuthMethod::Agent => 3,
            })
            .bind(&connection.private_key_path)
            .bind(&connection.certificate_path)
            .bind(match connection.agent_backend {
                SshAgentBackend::Auto => 0_i64,
                SshAgentBackend::OpenSsh => 1,
                SshAgentBackend::Pageant => 2,
            })
            .bind(&connection.agent_identity)
            .bind(serde_json::to_string(&connection.route).map_err(|_| ProfileRepositoryError::Corrupt)?)
            .execute(&mut *tx)
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        }
        tx.commit()
            .await
            .map_err(|_| ProfileRepositoryError::Unavailable)?;
        Ok(())
    }
}

fn corrupt(_error: sqlx::Error) -> ProfileRepositoryError {
    ProfileRepositoryError::Corrupt
}

fn parse_folder_id(value: &str) -> Result<FolderId, ProfileRepositoryError> {
    Ok(FolderId::from_uuid(
        Uuid::parse_str(value).map_err(|_| ProfileRepositoryError::Corrupt)?,
    ))
}

fn parse_profile_id(value: &str) -> Result<ProfileId, ProfileRepositoryError> {
    Ok(ProfileId::from_uuid(
        Uuid::parse_str(value).map_err(|_| ProfileRepositoryError::Corrupt)?,
    ))
}

fn read_bool(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<bool, ProfileRepositoryError> {
    let value: i64 = row.try_get(column).map_err(corrupt)?;
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(ProfileRepositoryError::Corrupt),
    }
}

fn read_overrides(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<TerminalOverrides, ProfileRepositoryError> {
    let logging: Option<i64> = row.try_get("logging").map_err(corrupt)?;
    let logging = match logging {
        None => None,
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => return Err(ProfileRepositoryError::Corrupt),
    };
    Ok(TerminalOverrides {
        terminal_type: row.try_get("terminal_type").map_err(corrupt)?,
        theme: row.try_get("theme").map_err(corrupt)?,
        logging,
    })
}
