use sqlx::{SqlitePool, query, query_scalar};

/// Remove additions made after v12 before marking a current temporary database
/// as an older fixture. Production migrations must still reject schema conflicts.
pub async fn strip_newer_fixture_schema(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    for statement in [
        "DROP TABLE IF EXISTS mixed_events",
        "DROP TABLE IF EXISTS mixed_targets",
        "DROP TABLE IF EXISTS mixed_runs",
        "DROP TABLE IF EXISTS download_events",
        "DROP TABLE IF EXISTS download_targets",
        "DROP TABLE IF EXISTS download_runs",
        "DROP TABLE IF EXISTS upload_events",
        "DROP TABLE IF EXISTS upload_targets",
        "DROP TABLE IF EXISTS upload_runs",
    ] {
        query(statement).execute(pool).await?;
    }
    let column: Option<String> = query_scalar(
        "SELECT name FROM pragma_table_info('profile_ssh_connections') WHERE name = 'options_json'",
    )
    .fetch_optional(pool)
    .await?;
    if column.is_some() {
        query("ALTER TABLE profile_ssh_connections DROP COLUMN options_json")
            .execute(pool)
            .await?;
    }
    Ok(())
}
