use super::*;
use crate::profile_import::plan_import;
use crate::{CatalogChange, ImportConflictPolicy};
use cshell_domain::{FolderId, ProfileFolder, ProfileKind, ProfileRecord};
use std::error::Error;

fn fixture() -> CatalogSnapshot {
    let folder_id = FolderId::new();
    let profile_id = ProfileId::new();
    CatalogSnapshot {
        defaults: TerminalDefaults {
            terminal_type: "vt100".into(),
            theme: "paper".into(),
            logging: true,
        },
        folders: vec![ProfileFolder {
            id: folder_id,
            name: "tools".into(),
            parent_id: None,
            terminal: TerminalOverrides::default(),
        }],
        profiles: vec![ProfileRecord {
            id: profile_id,
            name: "shell".into(),
            kind: ProfileKind::Local,
            folder_id: Some(folder_id),
            tags: Default::default(),
            favorite: true,
            terminal: TerminalOverrides::default(),
        }],
        local_connections: vec![LocalConnectionRecord {
            profile_id,
            program: "shell".into(),
            args: vec!["ARG_SENTINEL".into()],
            cwd: LocalWorkingDirectory::Home,
            env_overrides: BTreeMap::from([("TOKEN".into(), "ENV_SENTINEL".into())]),
            close_policy: Default::default(),
        }],
        ..Default::default()
    }
}

#[test]
fn export_omits_sensitive_context_and_preserves_inherited_values() -> Result<(), Box<dyn Error>> {
    let source = fixture();
    let bytes = export_profile_catalog(&source, ProfileExportOptions::default())?;
    let text = std::str::from_utf8(&bytes)?;
    assert!(!text.contains("ARG_SENTINEL"));
    assert!(!text.contains("ENV_SENTINEL"));
    assert!(!text.contains("env_overrides"));
    let document = ProfileImportDocument::parse(&bytes)?;
    assert_eq!(document.version, PROFILE_TRANSFER_VERSION);
    let original = ProfileCatalog::from_snapshot(source.clone())?;
    let mut imported = ProfileCatalog::from_snapshot(CatalogSnapshot::default())?;
    let (changes, _) = plan_import(&imported, &document, ImportConflictPolicy::Fail)?;
    imported.apply_batch(0, &changes)?;
    let id = source.profiles[0].id;
    let old = original.resolve(id)?;
    let new = imported.resolve(id)?;
    assert_eq!(old.terminal_type.value, new.terminal_type.value);
    assert_eq!(old.theme.value, new.theme.value);
    assert_eq!(old.logging.value, new.logging.value);
    assert!(imported.snapshot().local_connections[0].args.is_empty());
    assert_eq!(imported.snapshot().folders[0].id, source.folders[0].id);
    let bytes = export_profile_catalog(
        &source,
        ProfileExportOptions {
            include_local_context: true,
        },
    )?;
    assert!(std::str::from_utf8(&bytes)?.contains("ENV_SENTINEL"));
    let full = ProfileImportDocument::parse(&bytes)?;
    assert!(!format!("{:?}", full.local_connections[0]).contains("SENTINEL"));
    let mut complete = ProfileCatalog::from_snapshot(CatalogSnapshot::default())?;
    let (changes, _) = plan_import(&complete, &full, ImportConflictPolicy::Fail)?;
    complete.apply_batch(0, &changes)?;
    assert_eq!(
        complete.snapshot().local_connections,
        source.local_connections
    );
    Ok(())
}

#[test]
fn replace_retains_omitted_context_and_explicit_empty_clears_it() -> Result<(), Box<dyn Error>> {
    let snapshot = fixture();
    let mut document = ProfileImportDocument::parse(&export_profile_catalog(
        &snapshot,
        ProfileExportOptions::default(),
    )?)?;
    let old_id = snapshot.profiles[0].id;
    let imported_id = ProfileId::new();
    document.profiles[0].id = imported_id;
    document.local_connections[0].profile_id = imported_id;
    document.local_connections[0].program = "replacement".into();
    let mut catalog = ProfileCatalog::from_snapshot(snapshot.clone())?;
    let (changes, preview) = plan_import(&catalog, &document, ImportConflictPolicy::Replace)?;
    assert!(preview.can_commit);
    assert!(!format!("{preview:?}").contains("SENTINEL"));
    catalog.apply_batch(0, &changes)?;
    let merged = &catalog.snapshot().local_connections[0];
    assert_eq!(merged.profile_id, old_id);
    assert_eq!(merged.program, "replacement");
    assert_eq!(merged.args, snapshot.local_connections[0].args);
    assert_eq!(
        merged.env_overrides,
        snapshot.local_connections[0].env_overrides
    );
    document.local_connections[0].args = Some(Vec::new());
    document.local_connections[0].env_overrides = Some(BTreeMap::new());
    let (changes, _) = plan_import(&catalog, &document, ImportConflictPolicy::Skip)?;
    assert!(changes.is_empty());
    let (changes, _) = plan_import(&catalog, &document, ImportConflictPolicy::Replace)?;
    catalog.apply_batch(1, &changes)?;
    assert!(catalog.snapshot().local_connections[0].args.is_empty());
    assert!(
        catalog.snapshot().local_connections[0]
            .env_overrides
            .is_empty()
    );
    Ok(())
}

#[test]
fn local_transfer_rejects_legacy_versions_invalid_environment_and_secret_fields()
-> Result<(), Box<dyn Error>> {
    let snapshot = fixture();
    let mut value = serde_json::from_slice::<serde_json::Value>(&export_profile_catalog(
        &snapshot,
        ProfileExportOptions::default(),
    )?)?;
    for version in [1, 2, 4] {
        value["version"] = serde_json::json!(version);
        assert!(ProfileImportDocument::parse(&serde_json::to_vec(&value)?).is_err());
    }
    value["version"] = serde_json::json!(3);
    value["local_connections"][0]["args"] = serde_json::json!("ARG_SENTINEL");
    let error = ProfileImportDocument::parse(&serde_json::to_vec(&value)?)
        .err()
        .ok_or("expected invalid arguments")?;
    assert!(!error.to_string().contains("SENTINEL"));
    assert!(!format!("{error:?}").contains("SENTINEL"));
    value["local_connections"][0]
        .as_object_mut()
        .ok_or("object")?
        .remove("args");
    value["local_connections"][0]["password"] = serde_json::json!("secret");
    assert!(ProfileImportDocument::parse(&serde_json::to_vec(&value)?).is_err());
    value["local_connections"][0]
        .as_object_mut()
        .ok_or("object")?
        .remove("password");
    value["local_connections"][0]["env_overrides"] = serde_json::json!({ "BAD=NAME": "value" });
    assert!(ProfileImportDocument::parse(&serde_json::to_vec(&value)?).is_err());
    let mut wrong = snapshot;
    wrong.profiles[0].kind = ProfileKind::Ssh;
    assert!(export_profile_catalog(&wrong, ProfileExportOptions::default()).is_err());
    Ok(())
}

#[test]
fn local_transfer_replaces_ssh_connection_in_one_batch() -> Result<(), Box<dyn Error>> {
    let snapshot = fixture();
    let mut ssh = crate::import_openssh_config(
        b"Host shell\nUser ops\n",
        crate::OpenSshImportOptions::default(),
    )?;
    ssh.profiles[0].folder_id = Some(snapshot.folders[0].id);
    let id = ssh.profiles[0].id;
    let mut catalog = ProfileCatalog::from_snapshot(CatalogSnapshot {
        folders: snapshot.folders.clone(),
        profiles: ssh.profiles,
        ssh_connections: ssh.ssh_connections,
        ..Default::default()
    })?;
    let document = ProfileImportDocument::parse(&export_profile_catalog(
        &snapshot,
        ProfileExportOptions::default(),
    )?)?;
    let (changes, preview) = plan_import(&catalog, &document, ImportConflictPolicy::Replace)?;
    assert!(preview.can_commit);
    assert!(changes.iter().any(
        |change| matches!(change, CatalogChange::RemoveSshConnection(target) if *target == id)
    ));
    catalog.apply_batch(0, &changes)?;
    assert!(catalog.snapshot().ssh_connections.is_empty());
    assert_eq!(catalog.snapshot().local_connections[0].profile_id, id);
    Ok(())
}
