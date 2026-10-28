use cshell_application::{THEME_FORMAT, THEME_VERSION, ThemeDocument};
use cshell_domain::{TerminalPalette, ThemeRecord};
use cshell_ipc::{
    Envelope, ProfileImportPolicy, ProfileOperation, ProfileRequest, ProfileResponse,
    ProfileStatus, envelope, features, read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::error::Error;
use std::sync::Arc;

const BITS: u64 = features::PROFILE_CONTROL | features::THEME_TRANSFER;

fn theme(name: &str, foreground: [u8; 3]) -> ThemeRecord {
    ThemeRecord {
        name: name.into(),
        palette: TerminalPalette {
            foreground,
            background: [3, 4, 5],
            cursor: [6, 7, 8],
            selection: [9, 10, 11],
            ansi: [[12, 13, 14]; 16],
        },
    }
}

fn import(
    operation: ProfileOperation,
    revision: u64,
    themes: Vec<ThemeRecord>,
    policy: ProfileImportPolicy,
) -> Result<ProfileRequest, serde_json::Error> {
    let mut request = ProfileRequest::default();
    request.operation = operation as i32;
    request.expected_revision = revision;
    request.import_policy = policy as i32;
    request.import_json = serde_json::to_vec(&ThemeDocument {
        format: THEME_FORMAT.into(),
        version: THEME_VERSION,
        themes,
    })?;
    Ok(request)
}

fn export(revision: u64) -> ProfileRequest {
    let mut request = ProfileRequest::default();
    request.operation = ProfileOperation::ExportThemes as i32;
    request.expected_revision = revision;
    request
}

async fn exchange(
    service: &SessionIpcService,
    request: ProfileRequest,
    bits: u64,
) -> Result<ProfileResponse, Box<dyn Error>> {
    let (mut client, server) = tokio::io::duplex(1024 * 1024);
    let client_work = async move {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 23,
                payload: Some(envelope::Payload::ProfileRequest(request)),
                ..Default::default()
            },
        )
        .await?;
        read_envelope(&mut client).await
    };
    let (served, response) = tokio::join!(
        service.serve_connection_with_features(server, bits),
        client_work
    );
    served?;
    let response = response?;
    assert_eq!(response.request_id, 23);
    let Some(envelope::Payload::ProfileResponse(response)) = response.payload else {
        return Err("missing Profile response".into());
    };
    Ok(response)
}

#[tokio::test]
async fn theme_ipc_roundtrip_conflicts_capabilities_and_stale_commits() -> Result<(), Box<dyn Error>>
{
    let temp = tempfile::tempdir()?;
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(temp.path().join("themes.db")).await?,
    ));
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("journals"),
        8,
    )?))
    .with_profiles(Arc::clone(&profiles));
    let original = theme("paper", [230, 231, 232]);
    let request = import(
        ProfileOperation::CommitThemes,
        0,
        vec![original.clone()],
        ProfileImportPolicy::Fail,
    )?;
    assert_eq!(
        exchange(&service, request.clone(), BITS & !features::THEME_TRANSFER)
            .await?
            .status,
        ProfileStatus::Unsupported as i32
    );
    assert_eq!(
        exchange(&service, export(0), BITS & !features::THEME_TRANSFER)
            .await?
            .status,
        ProfileStatus::Unsupported as i32
    );
    let mut preview = request.clone();
    preview.operation = ProfileOperation::PreviewThemes as i32;
    let preview = exchange(&service, preview, BITS).await?;
    let preview = preview.preview.ok_or("preview")?;
    assert!(preview.can_commit);
    assert_eq!(
        preview.items[0].kind,
        cshell_ipc::ProfileImportItemKind::Theme as i32
    );
    assert_eq!(
        ThemeRecord::try_from(preview.items[0].theme.clone().ok_or("preview colors")?)?,
        original
    );
    assert_eq!(profiles.handle(ProfileRequest::default()).await.revision, 0);
    let created = exchange(&service, request, BITS).await?;
    assert_eq!(
        created.status,
        ProfileStatus::Ok as i32,
        "{}",
        created.detail
    );
    assert_eq!(created.revision, 1);
    let replacement = theme("paper", [21, 22, 23]);
    let preview = exchange(
        &service,
        import(
            ProfileOperation::PreviewThemes,
            1,
            vec![replacement.clone()],
            ProfileImportPolicy::Fail,
        )?,
        BITS,
    )
    .await?
    .preview
    .ok_or("conflict")?;
    assert!(!preview.can_commit);
    let skip = exchange(
        &service,
        import(
            ProfileOperation::PreviewThemes,
            1,
            vec![replacement.clone()],
            ProfileImportPolicy::Skip,
        )?,
        BITS,
    )
    .await?
    .preview
    .ok_or("skip")?;
    assert_eq!(
        ThemeRecord::try_from(skip.items[0].theme.clone().ok_or("retained colors")?)?,
        original
    );
    let skip = exchange(
        &service,
        import(
            ProfileOperation::CommitThemes,
            1,
            vec![replacement.clone()],
            ProfileImportPolicy::Skip,
        )?,
        BITS,
    )
    .await?;
    assert_eq!(skip.revision, 1);
    let stale = exchange(
        &service,
        import(
            ProfileOperation::CommitThemes,
            0,
            vec![replacement.clone()],
            ProfileImportPolicy::Replace,
        )?,
        BITS,
    )
    .await?;
    assert_eq!(stale.status, ProfileStatus::Conflict as i32);
    let replaced = exchange(
        &service,
        import(
            ProfileOperation::CommitThemes,
            1,
            vec![replacement.clone()],
            ProfileImportPolicy::Replace,
        )?,
        BITS,
    )
    .await?;
    assert_eq!(replaced.revision, 2);
    let stale = exchange(&service, export(1), BITS).await?;
    assert_eq!(stale.status, ProfileStatus::Conflict as i32);
    assert!(stale.export_json.is_empty());
    let exported = exchange(&service, export(2), BITS).await?;
    assert_eq!(
        ThemeDocument::parse(&exported.export_json)?.themes,
        vec![replacement.clone()]
    );
    let destination = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(temp.path().join("destination.db")).await?,
    ));
    let destination_service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("destination-journals"),
        8,
    )?))
    .with_profiles(destination);
    let mut restore = ProfileRequest::default();
    restore.operation = ProfileOperation::CommitThemes as i32;
    restore.import_json = exported.export_json;
    let restored = exchange(&destination_service, restore, BITS).await?;
    assert_eq!(restored.status, ProfileStatus::Ok as i32);
    assert_eq!(
        ThemeRecord::try_from(restored.catalog.ok_or("catalog")?.themes[0].clone())?,
        replacement
    );
    let mut forbidden = export(2);
    forbidden.include_local_context = true;
    assert_eq!(
        exchange(&service, forbidden, BITS).await?.status,
        ProfileStatus::Invalid as i32
    );
    Ok(())
}

#[tokio::test]
async fn theme_insert_failure_rolls_back_whole_catalog_and_reopen_keeps_saved_colors()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("themes.db");
    let profiles = Arc::new(ProfileIpcService::new(
        SqliteProfileRepository::open(&path).await?,
    ));
    let service = SessionIpcService::new(Arc::new(LocalSessionRegistry::new(
        temp.path().join("journals"),
        8,
    )?))
    .with_profiles(Arc::clone(&profiles));
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    let original = theme("paper", [1, 2, 3]);
    let seeded = exchange(
        &service,
        import(
            ProfileOperation::CommitThemes,
            0,
            vec![original.clone()],
            ProfileImportPolicy::Fail,
        )?,
        BITS,
    )
    .await?;
    assert_eq!(seeded.status, ProfileStatus::Ok as i32);
    sqlx::query("CREATE TRIGGER reject_theme BEFORE INSERT ON terminal_themes WHEN NEW.name = 'blocked' BEGIN SELECT RAISE(ABORT, 'injected failure'); END").execute(&pool).await?;
    let mut request = import(
        ProfileOperation::CommitThemes,
        1,
        vec![theme("paper", [20, 30, 40]), theme("blocked", [1, 2, 3])],
        ProfileImportPolicy::Replace,
    )?;
    let failed = exchange(&service, request.clone(), BITS).await?;
    assert_eq!(failed.status, ProfileStatus::Unavailable as i32);
    let intact = profiles.handle(ProfileRequest::default()).await;
    assert_eq!(intact.revision, 1);
    assert_eq!(
        ThemeRecord::try_from(intact.catalog.ok_or("intact")?.themes[0].clone())?,
        original
    );
    sqlx::query("DROP TRIGGER reject_theme")
        .execute(&pool)
        .await?;
    assert_eq!(exchange(&service, request.clone(), BITS).await?.revision, 2);
    // Older Profile metadata clients do not erase palettes they cannot decode.
    request = ProfileRequest::default();
    request.operation = ProfileOperation::ApplyChanges as i32;
    request.expected_revision = 2;
    request.changes.push(cshell_ipc::ProfileChange {
        change: Some(cshell_ipc::profile_change::Change::UpsertProfile(
            cshell_ipc::ProfileRecordData::from(&cshell_domain::ProfileRecord {
                id: cshell_domain::ProfileId::new(),
                name: "old-client".into(),
                kind: cshell_domain::ProfileKind::Local,
                folder_id: None,
                favorite: false,
                tags: Default::default(),
                terminal: Default::default(),
            }),
        )),
    });
    let legacy_bits = features::PROFILE_CONTROL
        | features::SSH_PROFILE_TARGET
        | features::SSH_PROFILE_AUTH
        | features::SSH_PROFILE_ROUTE
        | features::LOCAL_PROFILE;
    let kept = exchange(&service, request, legacy_bits).await?;
    assert_eq!(kept.status, ProfileStatus::Ok as i32, "{}", kept.detail);
    assert_eq!(kept.catalog.as_ref().ok_or("kept")?.themes.len(), 2);
    let saved = kept.catalog.ok_or("catalog")?.themes;
    drop(service);
    drop(profiles);
    pool.close().await;
    let reopened = ProfileIpcService::new(SqliteProfileRepository::open(&path).await?);
    let restored = reopened.handle(ProfileRequest::default()).await;
    assert_eq!(restored.revision, 3);
    assert_eq!(restored.catalog.ok_or("reopened")?.themes, saved);
    Ok(())
}
