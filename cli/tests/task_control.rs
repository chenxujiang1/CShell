use cshell_application::{CatalogChange, HostRepository, ProfileService};
use cshell_domain::*;
use cshell_ipc::{
    DiscoveryPublication, DiscoveryRecord, HandshakePolicy, RuntimePaths, features,
    transport::LocalListener,
};
use cshell_storage::SqliteProfileRepository;
use cshelld::{
    LocalSessionRegistry, ProfileIpcService, SessionIpcServer, SessionIpcService, TaskIpcService,
    TaskOutputStore,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    sync::Arc,
};

async fn command(
    paths: &RuntimePaths,
    args: &[&str],
) -> Result<std::process::Output, Box<dyn Error>> {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_cshell-cli"));
    command.kill_on_drop(true);
    command.env("CSHELL_RUNTIME_DIR", paths.root()).args(args);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    Ok(tokio::time::timeout(std::time::Duration::from_secs(15), command.output()).await??)
}

#[tokio::test]
async fn cli_discovers_real_daemon_saves_lists_cancels_and_rejects_unconfirmed_start()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let paths = RuntimePaths::prepare(directory.path().join("runtime"))?;
    let record = DiscoveryRecord::generate(&paths);
    #[cfg(windows)]
    let listener = LocalListener::bind(record.endpoint.to_string_lossy().into_owned())?;
    #[cfg(unix)]
    let listener = LocalListener::bind(std::path::Path::new(&record.endpoint))?;
    let profile_service = ProfileService::new(
        SqliteProfileRepository::open(directory.path().join("profiles.sqlite")).await?,
    );
    let profile_id = ProfileId::new();
    let profile = ProfileRecord {
        id: profile_id,
        name: "template".into(),
        kind: ProfileKind::Ssh,
        folder_id: None,
        tags: BTreeSet::new(),
        favorite: false,
        terminal: TerminalOverrides::default(),
    };
    let ssh = SshConnectionRecord {
        options: Default::default(),
        profile_id,
        host: "unreachable.test".into(),
        port: 22,
        username: "operator".into(),
        auth_method: SshAuthMethod::Password,
        private_key_path: None,
        certificate_path: None,
        agent_backend: SshAgentBackend::OpenSsh,
        agent_identity: None,
        route: SshRoute::Direct,
    };
    profile_service
        .apply_batch(
            0,
            &[
                CatalogChange::UpsertProfile(profile),
                CatalogChange::UpsertSshConnection(ssh),
            ],
        )
        .await?;
    let host = HostRecord {
        id: HostId::new(),
        name: "owned-test-target".into(),
        address: "unreachable.test".into(),
        port: 22,
        profile_id,
        tags: BTreeSet::new(),
        labels: BTreeMap::new(),
        platform_hint: HostPlatformHint::Unknown,
    };
    profile_service
        .repository()
        .save_hosts(
            0,
            &HostCatalog {
                hosts: vec![host],
                ..Default::default()
            },
        )
        .await?;
    let profiles = Arc::new(ProfileIpcService::new(profile_service.into_repository()));
    let tasks = TaskIpcService::new(
        profiles.clone(),
        TaskOutputStore::new(directory.path().join("output"))?,
    )?;
    let registry = Arc::new(LocalSessionRegistry::new(
        directory.path().join("journals"),
        2,
    )?);
    let server = SessionIpcServer::new(
        listener,
        HandshakePolicy::with_instance_id(
            record.instance_token,
            record.daemon_instance_id,
            features::TASK_CONTROL | features::TERMINAL_CONTROL,
        ),
        SessionIpcService::new(registry)
            .with_profiles(profiles)
            .with_tasks(tasks),
    );
    let publication = DiscoveryPublication::publish(&paths, &record)?;
    let (shutdown, stopped) = tokio::sync::watch::channel(false);
    let server_task = tokio::spawn(async move { server.run_until(stopped).await });
    let test = async {
        let status = command(&paths, &["status"]).await?;
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        let definition = TaskDefinition {
            id: TaskId::new(),
            name: "cli-plan".into(),
            shell: RemoteShellProfile::Posix,
            steps: vec![TaskStep {
                name: "check".into(),
                command: TaskCommand::Script {
                    text: "printf cli-test".into(),
                },
                success_codes: BTreeSet::from([0]),
                idempotent: true,
            }],
            variables: BTreeMap::new(),
            environment: BTreeMap::new(),
            policy: TaskPolicy::default(),
        };
        let definition_path = directory.path().join("task.json");
        let selection_path = directory.path().join("selection.json");
        std::fs::write(&definition_path, serde_json::to_vec(&definition)?)?;
        std::fs::write(
            &selection_path,
            serde_json::to_vec(&HostSelection::Selector {
                expression: "all".into(),
            })?,
        )?;
        let created = command(
            &paths,
            &[
                "task",
                "create",
                definition_path.to_str().ok_or("path")?,
                selection_path.to_str().ok_or("path")?,
                "1",
                "1",
            ],
        )
        .await?;
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let snapshot: serde_json::Value = serde_json::from_slice(&created.stdout)?;
        let id = snapshot["id"].as_str().ok_or("run ID")?;
        assert!(
            !command(&paths, &["task", "start", id, "1"])
                .await?
                .status
                .success()
        );
        let get = command(&paths, &["task", "get", id]).await?;
        assert!(get.status.success());
        let view: serde_json::Value = serde_json::from_slice(&get.stdout)?;
        assert_eq!(view["meta"]["state"], "Queued");
        let list = command(&paths, &["task", "list"]).await?;
        assert!(list.status.success());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&list.stdout)?
                .as_array()
                .ok_or("list")?
                .len(),
            1
        );
        assert!(
            command(&paths, &["task", "cancel", id])
                .await?
                .status
                .success()
        );
        let cancelled: serde_json::Value =
            serde_json::from_slice(&command(&paths, &["task", "get", id]).await?.stdout)?;
        assert_eq!(cancelled["meta"]["state"], "Cancelled");
        assert!(
            !command(&paths, &["task", "start", id, "1", "--confirm"])
                .await?
                .status
                .success()
        );
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    let _ = shutdown.send(true);
    server_task.await??;
    drop(publication);
    test
}
