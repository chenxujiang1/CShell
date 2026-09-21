use cshell_application::{PROFILE_IMPORT_FORMAT, PROFILE_IMPORT_VERSION, ProfileImportDocument};
use cshell_domain::{
    FolderId, ProfileFolder, ProfileId, ProfileKind, ProfileRecord, TerminalOverrides,
};
use cshell_ipc::{
    Envelope, ProfileChange, ProfileFolderData, ProfileImportPolicy, ProfileOperation,
    ProfileRecordData, ProfileRequest, ProfileResponse, ProfileStatus, envelope, profile_change,
    read_envelope, write_envelope,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{LocalSessionRegistry, ProfileIpcService, SessionIpcService};
use std::collections::BTreeSet;
use std::error::Error;
use std::sync::Arc;

fn folder(id: FolderId, name: &str) -> ProfileFolder {
    ProfileFolder {
        id,
        name: name.into(),
        parent_id: None,
        terminal: TerminalOverrides::default(),
    }
}

fn profile(id: ProfileId, name: &str, folder_id: FolderId) -> ProfileRecord {
    ProfileRecord {
        id,
        name: name.into(),
        kind: ProfileKind::Ssh,
        folder_id: Some(folder_id),
        tags: BTreeSet::new(),
        favorite: false,
        terminal: TerminalOverrides::default(),
    }
}

fn request(operation: ProfileOperation) -> ProfileRequest {
    ProfileRequest {
        operation: operation as i32,
        expected_revision: 0,
        changes: vec![],
        import_json: vec![],
        import_policy: ProfileImportPolicy::Fail as i32,
    }
}

async fn exchange(
    service: &SessionIpcService,
    request: ProfileRequest,
) -> Result<ProfileResponse, Box<dyn Error>> {
    let (mut client, mut server) = tokio::io::duplex(2 * 1024 * 1024);
    let client_work = async {
        write_envelope(
            &mut client,
            &Envelope {
                request_id: 42,
                deadline_unix_ms: 0,
                payload: Some(envelope::Payload::ProfileRequest(request)),
            },
        )
        .await?;
        read_envelope(&mut client).await
    };
    let (served, response) = tokio::join!(service.serve_one(&mut server), client_work);
    served?;
    let response = response?;
    assert_eq!(response.request_id, 42);
    let Some(envelope::Payload::ProfileResponse(response)) = response.payload else {
        return Err("daemon did not return a Profile response".into());
    };
    Ok(response)
}

#[tokio::test]
async fn profile_control_previews_and_commits_import_once() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("cshell.db");
    let repository = SqliteProfileRepository::open(&db).await?;
    let sessions = Arc::new(LocalSessionRegistry::new(temp.path().join("journals"), 16)?);
    let service = SessionIpcService::new(sessions)
        .with_profiles(Arc::new(ProfileIpcService::new(repository)));

    let first_folder = FolderId::new();
    let first_profile = ProfileId::new();
    let mut create = request(ProfileOperation::ApplyChanges);
    create.changes = vec![
        ProfileChange {
            change: Some(profile_change::Change::UpsertFolder(
                ProfileFolderData::from(&folder(first_folder, "Ops")),
            )),
        },
        ProfileChange {
            change: Some(profile_change::Change::UpsertProfile(
                ProfileRecordData::from(&profile(first_profile, "srv", first_folder)),
            )),
        },
    ];
    let response = exchange(&service, create).await?;
    assert_eq!(response.status, ProfileStatus::Ok as i32);
    assert_eq!(response.revision, 1);

    let source_folder = FolderId::new();
    let mut replacement = profile(ProfileId::new(), "srv", source_folder);
    replacement.tags.insert("imported".into());
    let document = ProfileImportDocument {
        format: PROFILE_IMPORT_FORMAT.into(),
        version: PROFILE_IMPORT_VERSION,
        folders: vec![folder(source_folder, "Ops")],
        profiles: vec![replacement, profile(ProfileId::new(), "new", source_folder)],
    };
    let bytes = serde_json::to_vec(&document)?;
    let mut preview = request(ProfileOperation::PreviewImport);
    preview.import_json = bytes.clone();
    let failed_policy = exchange(&service, preview.clone()).await?;
    assert_eq!(failed_policy.status, ProfileStatus::Ok as i32);
    assert!(!failed_policy.preview.ok_or("missing preview")?.can_commit);
    let before = exchange(&service, request(ProfileOperation::List)).await?;
    assert_eq!(before.revision, 1);

    preview.import_policy = ProfileImportPolicy::Skip as i32;
    let skipped = exchange(&service, preview.clone()).await?;
    let skipped_preview = skipped.preview.ok_or("missing skip preview")?;
    assert!(skipped_preview.can_commit);
    assert_eq!(skipped_preview.change_count, 1);

    preview.import_policy = ProfileImportPolicy::Replace as i32;
    let accepted = exchange(&service, preview).await?;
    let accepted_preview = accepted.preview.ok_or("missing replacement preview")?;
    assert!(accepted_preview.can_commit);
    assert_eq!(accepted_preview.base_revision, 1);
    assert_eq!(accepted_preview.change_count, 3);

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&db))
        .await?;
    sqlx::query("CREATE TRIGGER reject_import BEFORE INSERT ON profile_records WHEN NEW.name = 'new' BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .execute(&pool).await?;
    let mut commit = request(ProfileOperation::CommitImport);
    commit.expected_revision = 1;
    commit.import_json = bytes.clone();
    commit.import_policy = ProfileImportPolicy::Replace as i32;
    let blocked = exchange(&service, commit.clone()).await?;
    assert_eq!(blocked.status, ProfileStatus::Unavailable as i32);
    let intact = exchange(&service, request(ProfileOperation::List)).await?;
    assert_eq!(intact.revision, 1);
    assert_eq!(intact.catalog.ok_or("missing catalog")?.profiles.len(), 1);
    sqlx::query("DROP TRIGGER reject_import")
        .execute(&pool)
        .await?;
    pool.close().await;

    let committed = exchange(&service, commit.clone()).await?;
    assert_eq!(committed.status, ProfileStatus::Ok as i32);
    assert_eq!(committed.revision, 2);
    let catalog = committed.catalog.ok_or("missing committed catalog")?;
    assert_eq!(catalog.profiles.len(), 2);
    assert!(catalog.profiles.iter().any(
        |record| record.id == first_profile.as_uuid().as_bytes() && record.tags == ["imported"]
    ));
    let stale = exchange(&service, commit).await?;
    assert_eq!(stale.status, ProfileStatus::Conflict as i32);
    assert_eq!(
        exchange(&service, request(ProfileOperation::List))
            .await?
            .revision,
        2
    );
    Ok(())
}
