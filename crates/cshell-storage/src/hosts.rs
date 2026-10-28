use crate::SqliteProfileRepository;
use cshell_application::{
    HostCatalogSnapshot, HostRepository, HostRepositoryError, MAX_HOST_DOCUMENT_BYTES,
    decode_hosts, encode_hosts,
};
use cshell_domain::HostCatalog;
use sqlx::Row;

#[async_trait::async_trait]
impl HostRepository for SqliteProfileRepository {
    async fn load_hosts(&self) -> Result<Option<HostCatalogSnapshot>, HostRepositoryError> {
        let row=sqlx::query("SELECT revision, CASE WHEN length(document_json) <= ? THEN document_json ELSE NULL END AS document_json FROM host_catalog_state WHERE id = 1").bind(MAX_HOST_DOCUMENT_BYTES as i64).fetch_optional(&self.pool).await.map_err(|_|HostRepositoryError::Unavailable)?;
        row.map(|row| {
            let revision: i64 = row
                .try_get("revision")
                .map_err(|_| HostRepositoryError::Corrupt)?;
            let bytes: Option<Vec<u8>> = row
                .try_get("document_json")
                .map_err(|_| HostRepositoryError::Corrupt)?;
            let bytes = bytes.ok_or(HostRepositoryError::Corrupt)?;
            if revision <= 0 {
                return Err(HostRepositoryError::Corrupt);
            }
            Ok(HostCatalogSnapshot {
                revision: revision as u64,
                catalog: decode_hosts(&bytes).map_err(|_| HostRepositoryError::Corrupt)?,
            })
        })
        .transpose()
    }
    async fn save_hosts(
        &self,
        expected_revision: u64,
        catalog: &HostCatalog,
    ) -> Result<HostCatalogSnapshot, HostRepositoryError> {
        let bytes = encode_hosts(catalog).map_err(|_| HostRepositoryError::Corrupt)?;
        let next = expected_revision
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(HostRepositoryError::Unavailable)?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|_| HostRepositoryError::Unavailable)?;
        let actual: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM host_catalog_state WHERE id = 1")
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| HostRepositoryError::Unavailable)?;
        let actual =
            u64::try_from(actual.unwrap_or(0)).map_err(|_| HostRepositoryError::Corrupt)?;
        if actual != expected_revision {
            return Err(HostRepositoryError::Conflict {
                expected: expected_revision,
                actual,
            });
        }
        sqlx::query("INSERT INTO host_catalog_state (id,revision,document_json) VALUES (1,?,?) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision, document_json=excluded.document_json").bind(next).bind(bytes).execute(&mut *tx).await.map_err(|_|HostRepositoryError::Unavailable)?;
        tx.commit()
            .await
            .map_err(|_| HostRepositoryError::Unavailable)?;
        Ok(HostCatalogSnapshot {
            revision: next as u64,
            catalog: catalog.clone(),
        })
    }
}
