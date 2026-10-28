use super::*;
use crate::{OpenSshImportOptions, import_openssh_config};
use cshell_domain::{ProfileKind, ProfileRecord, SshConnectionRecord};
use std::{error::Error, process::Command};

fn target(id: ProfileId) -> SshConnectionRecord {
    SshConnectionRecord {
        options: Default::default(),
        profile_id: id,
        host: "example.test".into(),
        port: 2202,
        username: "ops".into(),
        auth_method: SshAuthMethod::Password,
        private_key_path: None,
        certificate_path: None,
        agent_backend: Default::default(),
        agent_identity: None,
        route: SshRoute::Direct,
    }
}
fn catalog(targets: Vec<SshConnectionRecord>) -> CatalogSnapshot {
    CatalogSnapshot {
        profiles: targets
            .iter()
            .map(|target| ProfileRecord {
                id: target.profile_id,
                name: format!("{} with \"quotes\" and # comment", target.profile_id),
                kind: ProfileKind::Ssh,
                folder_id: None,
                tags: Default::default(),
                favorite: false,
                terminal: Default::default(),
            })
            .collect(),
        ssh_connections: targets,
        ..Default::default()
    }
}
fn fixture(path: &Path) -> CatalogSnapshot {
    let mut key = target(ProfileId::new());
    key.auth_method = SshAuthMethod::PrivateKey;
    key.private_key_path = Some(
        path.join("private % key # with space")
            .to_string_lossy()
            .into_owned(),
    );
    let mut cert = key.clone();
    cert.profile_id = ProfileId::new();
    cert.auth_method = SshAuthMethod::Certificate;
    cert.certificate_path = Some(path.join("cert % name.pub").to_string_lossy().into_owned());
    let mut agent = target(ProfileId::new());
    agent.auth_method = SshAuthMethod::Agent;
    agent.agent_backend = SshAgentBackend::OpenSsh;
    let mut jump = target(ProfileId::new());
    jump.route = SshRoute::Jump {
        profile_id: key.profile_id,
    };
    catalog(vec![target(ProfileId::new()), key, cert, agent, jump])
}
#[test]
fn selection_includes_jump_dependencies_and_generated_authentication_roundtrips()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let snapshot = fixture(temp.path());
    let selected = snapshot.ssh_connections[4].profile_id;
    let bytes = export_openssh_config(&snapshot, &[selected])?;
    let doc = import_openssh_config(&bytes, OpenSshImportOptions::default())?;
    assert_eq!(doc.ssh_connections.len(), 2);
    let gateway = snapshot.ssh_connections[1].profile_id;
    assert!(
        String::from_utf8(bytes)?.contains(&format!("ProxyJump {}", openssh_export_alias(gateway)))
    );
    let all = export_openssh_config(&snapshot, &[])?;
    let doc = import_openssh_config(&all, OpenSshImportOptions::default())?;
    assert_eq!(doc.ssh_connections.len(), 5);
    for original in &snapshot.ssh_connections {
        let imported_id = doc
            .profiles
            .iter()
            .find(|profile| profile.name == openssh_export_alias(original.profile_id))
            .ok_or("missing alias")?
            .id;
        let imported = doc
            .ssh_connections
            .iter()
            .find(|target| target.profile_id == imported_id)
            .ok_or("missing target")?;
        assert_eq!(
            (
                &imported.host,
                imported.port,
                &imported.username,
                imported.auth_method,
                &imported.private_key_path,
                &imported.certificate_path
            ),
            (
                &original.host,
                original.port,
                &original.username,
                original.auth_method,
                &original.private_key_path,
                &original.certificate_path
            )
        );
        if original.auth_method == SshAuthMethod::Agent {
            assert_eq!(imported.agent_backend, SshAgentBackend::OpenSsh);
        }
    }
    let mut reverse = snapshot.clone();
    reverse.profiles.reverse();
    reverse.ssh_connections.reverse();
    assert_eq!(all, export_openssh_config(&reverse, &[])?);
    assert!(std::fs::read_dir(temp.path())?.next().is_none());
    Ok(())
}
#[test]
fn unrepresentable_selected_targets_fail_whole_export_without_affecting_unselected_targets()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let mut snapshot = fixture(temp.path());
    snapshot.ssh_connections[0].route = SshRoute::Socks5 {
        host: "proxy.test".into(),
        port: 1080,
    };
    assert_eq!(
        export_openssh_config(&snapshot, &[]),
        Err(OpenSshExportError::Route)
    );
    assert!(export_openssh_config(&snapshot, &[snapshot.ssh_connections[1].profile_id]).is_ok());
    snapshot.ssh_connections[0].route = SshRoute::Direct;
    snapshot.ssh_connections[3].agent_backend = SshAgentBackend::Pageant;
    assert_eq!(
        export_openssh_config(&snapshot, &[]),
        Err(OpenSshExportError::Agent)
    );
    snapshot.ssh_connections[3].agent_backend = SshAgentBackend::OpenSsh;
    snapshot.ssh_connections[3].agent_identity = Some("SHA256:identity".into());
    assert_eq!(
        export_openssh_config(&snapshot, &[]),
        Err(OpenSshExportError::Agent)
    );
    snapshot.ssh_connections[3].agent_identity = None;
    if cfg!(windows) {
        snapshot.ssh_connections[3].agent_backend = SshAgentBackend::Auto;
        assert_eq!(
            export_openssh_config(&snapshot, &[]),
            Err(OpenSshExportError::Agent)
        );
        snapshot.ssh_connections[3].agent_backend = SshAgentBackend::OpenSsh;
    }
    snapshot.ssh_connections[1].private_key_path = Some(
        temp.path()
            .join("${CSHELL_SENTINEL}")
            .to_string_lossy()
            .into_owned(),
    );
    assert_eq!(
        export_openssh_config(&snapshot, &[]),
        Err(OpenSshExportError::KeyPath)
    );
    snapshot.ssh_connections[1].private_key_path = Some("relative-key".into());
    assert_eq!(
        export_openssh_config(&snapshot, &[]),
        Err(OpenSshExportError::KeyPath)
    );
    snapshot.ssh_connections[1].private_key_path =
        Some(temp.path().join("key").to_string_lossy().into_owned());
    snapshot.ssh_connections[0].host = "host;COMMAND_SENTINEL".into();
    assert_eq!(
        export_openssh_config(&snapshot, &[]),
        Err(OpenSshExportError::Value)
    );
    for error in [
        OpenSshExportError::Value,
        OpenSshExportError::KeyPath,
        OpenSshExportError::Agent,
    ] {
        assert!(!format!("{error} {error:?}").contains("SENTINEL"));
    }
    assert!(std::fs::read_dir(temp.path())?.next().is_none());
    Ok(())
}
#[test]
fn bounds_empty_invalid_selection_and_local_exclusion() -> Result<(), Box<dyn Error>> {
    assert_eq!(
        export_openssh_config(&CatalogSnapshot::default(), &[]),
        Err(OpenSshExportError::Selection)
    );
    let id = ProfileId::new();
    let mut snapshot = catalog(vec![target(id)]);
    assert_eq!(
        export_openssh_config(&snapshot, &[id, id]),
        Err(OpenSshExportError::Selection)
    );
    assert_eq!(
        export_openssh_config(&snapshot, &[ProfileId::new()]),
        Err(OpenSshExportError::Selection)
    );
    let local_id = ProfileId::new();
    let mut local = snapshot.profiles[0].clone();
    local.id = local_id;
    local.kind = ProfileKind::Local;
    local.name = "LOCAL_SENTINEL".into();
    snapshot.profiles.push(local);
    assert!(!String::from_utf8(export_openssh_config(&snapshot, &[])?)?.contains("LOCAL_SENTINEL"));
    let maximum = catalog(
        (0..MAX_OPENSSH_EXPORT_HOSTS)
            .map(|_| target(ProfileId::new()))
            .collect(),
    );
    assert!(export_openssh_config(&maximum, &[])?.len() <= MAX_PROFILE_IMPORT_BYTES);
    let mut excessive = maximum;
    let extra = target(ProfileId::new());
    excessive
        .profiles
        .extend(catalog(vec![extra.clone()]).profiles);
    excessive.ssh_connections.push(extra);
    assert_eq!(
        export_openssh_config(&excessive, &[]),
        Err(OpenSshExportError::Limit)
    );
    Ok(())
}
#[test]
fn actual_openssh_config_oracle_verifies_quotes_paths_algorithms_and_routes()
-> Result<(), Box<dyn Error>> {
    if let Err(error) = Command::new("ssh").arg("-V").output() {
        if error.kind() == std::io::ErrorKind::NotFound
            && std::env::var_os("CSHELL_REQUIRE_OPENSSH_EXPORT_ORACLE").is_none()
        {
            return Ok(());
        }
        return Err(error.into());
    }
    let temp = tempfile::tempdir()?;
    let snapshot = fixture(temp.path());
    let path = temp.path().join("standalone.conf");
    std::fs::write(&path, export_openssh_config(&snapshot, &[])?)?;
    for target in &snapshot.ssh_connections {
        let evaluated = Command::new("ssh")
            .args(["-G", "-F"])
            .arg(&path)
            .arg(openssh_export_alias(target.profile_id))
            .output()?;
        assert!(
            evaluated.status.success(),
            "OpenSSH rejected generated config"
        );
        let text = String::from_utf8(evaluated.stdout)?;
        let value = |name: &str| {
            text.lines().find_map(|line| {
                line.strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix(' '))
            })
        };
        assert_eq!(value("hostname"), Some(target.host.as_str()));
        assert_eq!(value("user"), Some(target.username.as_str()));
        assert_eq!(value("port"), Some("2202"));
        assert_eq!(value("stricthostkeychecking"), Some("true"));
        assert_eq!(value("updatehostkeys"), Some("false"));
        assert_eq!(value("checkhostip"), Some("no"));
        match target.auth_method {
            SshAuthMethod::Password => {
                assert_eq!(value("preferredauthentications"), Some("password"));
                assert_eq!(value("identityagent"), Some("none"));
                assert_eq!(value("identityfile"), Some("none"));
            }
            SshAuthMethod::PrivateKey | SshAuthMethod::Certificate => {
                assert_eq!(value("preferredauthentications"), Some("publickey"));
                assert_eq!(value("identitiesonly"), Some("yes"));
                assert_eq!(value("identityagent"), Some("none"));
                // -G retains %% in filename options; expansion occurs when the client opens them.
                assert_eq!(
                    value("identityfile"),
                    target
                        .private_key_path
                        .as_deref()
                        .map(|path| path.replace('%', "%%"))
                        .as_deref()
                );
                let algorithms = value("pubkeyacceptedalgorithms")
                    .or_else(|| value("pubkeyacceptedkeytypes"))
                    .ok_or("missing algorithms")?;
                assert!(
                    algorithms
                        .split(',')
                        .all(|algorithm| algorithm.contains("cert")
                            == (target.auth_method == SshAuthMethod::Certificate))
                );
            }
            SshAuthMethod::Agent => {
                assert_eq!(value("identityfile"), Some("none"));
                assert_eq!(value("identitiesonly"), Some("no"));
            }
        }
        if let SshRoute::Jump { profile_id } = target.route {
            assert_eq!(
                value("proxyjump"),
                Some(openssh_export_alias(profile_id).as_str())
            );
        }
    }
    Ok(())
}

#[test]
fn importer_only_accepts_representable_auth_and_trust_settings() {
    for directive in [
        "IdentityAgent /tmp/other-agent",
        "UpdateHostKeys yes",
        "CheckHostIP yes",
        "AddKeysToAgent yes",
        "PubkeyAcceptedAlgorithms +ssh-rsa",
        "IdentitiesOnly no\nIdentityFile /absolute/key",
        "IdentityAgent none",
    ] {
        let input = format!("Host srv\nUser ops\n{directive}\n");
        assert!(import_openssh_config(input.as_bytes(), OpenSshImportOptions::default()).is_err());
    }
}

#[test]
fn keyboard_interactive_export_preserves_password_second_factor_and_round_trips(){
    let mut connection=target(ProfileId::new());connection.options.keyboard_interactive=true;
    let snapshot=catalog(vec![connection]);let bytes=export_openssh_config(&snapshot,&[]).unwrap();
    assert!(std::str::from_utf8(&bytes).unwrap().contains("PreferredAuthentications keyboard-interactive,password"));
    let imported=import_openssh_config(&bytes,OpenSshImportOptions::default()).unwrap();assert_eq!(imported.ssh_connections[0].auth_method,SshAuthMethod::Password);assert!(imported.ssh_connections[0].options.keyboard_interactive);
}

#[test]
fn ordered_password_export_preserves_sequence_and_refuses_unrepresentable_combinations() {
    let mut connection = target(ProfileId::new());
    connection.options.authentication_order = vec![SshAuthStep::Password, SshAuthStep::KeyboardInteractive];
    let bytes = export_openssh_config(&catalog(vec![connection.clone()]), &[]).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("PreferredAuthentications password,keyboard-interactive"));
    let imported = import_openssh_config(&bytes, OpenSshImportOptions::default()).unwrap();
    assert_eq!(imported.ssh_connections[0].authentication_steps(), connection.authentication_steps());
    connection.options.authentication_order.push(SshAuthStep::Agent);
    assert_eq!(export_openssh_config(&catalog(vec![connection.clone()]), &[]), Err(OpenSshExportError::AuthenticationOrder));
    connection.options.authentication_order = vec![SshAuthStep::Password];
    connection.username.clear();
    assert_eq!(export_openssh_config(&catalog(vec![connection]), &[]), Err(OpenSshExportError::Value));
}
