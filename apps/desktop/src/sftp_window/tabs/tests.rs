use super::*;
fn cancelled_content(cancelled:bool)->Content {
    let session=SessionId::new();
    Content{connection:Some(DesktopSftpConnection::for_view_test(SftpView{session_id:Some(session),connection_profile:Some(ProfileId::new()),connection_cancelled:cancelled,..Default::default()})),panel:SftpPanel::default(),session:Some(session)}
}
#[test]
fn cancellation_removes_active_and_background_hosts_without_reopening_last_window() {
    for active_cancelled in [false,true] {
        let mut app=DesktopApp::default();
        let mut active=cancelled_content(active_cancelled);
        app.sftp=active.connection.take();app.sftp_session=active.session;app.sftp_panel=active.panel;app.sftp_panel.open=true;
        app.sftp_tabs.active=Some(1);
        app.sftp_tabs.entries=vec![HostTab{id:1,profile:ProfileId::new(),title:"A".into(),host:"A".into(),parked:None},HostTab{id:2,profile:ProfileId::new(),title:"B".into(),host:"B".into(),parked:Some(cancelled_content(true))}];
        app.sync_sftp_tabs();
        if active_cancelled {
            assert!(app.sftp_tabs.entries.is_empty());assert!(app.sftp_tabs.active.is_none());assert!(app.sftp.is_none());assert!(!app.sftp_panel.open);
        } else {
            assert_eq!(app.sftp_tabs.entries.len(),1);assert_eq!(app.sftp_tabs.active,Some(1));assert!(app.sftp.is_some());assert!(app.sftp_panel.open);
        }
        app.sync_sftp_tabs();
    }
}
#[test]
fn cancelling_active_host_returns_to_other_host_and_preserves_its_connection() {
    let mut app=DesktopApp::default();let mut content=cancelled_content(true);app.sftp=content.connection.take();app.sftp_session=content.session;app.sftp_panel=content.panel;
    app.sftp_tabs.active=Some(1);let survivor=cancelled_content(false);let survivor_session=survivor.session;
    app.sftp_tabs.entries=vec![HostTab{id:1,profile:ProfileId::new(),title:"cancel".into(),host:"A".into(),parked:None},HostTab{id:2,profile:ProfileId::new(),title:"keep".into(),host:"B".into(),parked:Some(survivor)}];
    app.sync_sftp_tabs();assert_eq!(app.sftp_tabs.active,Some(2));assert_eq!(app.sftp_session,survivor_session);assert_eq!(app.sftp_tabs.entries.len(),1);assert!(app.sftp_panel.open);
}
#[test]
fn common_local_routes_only_local_commands_and_never_remote_operations() {
    let common = CommonLocal::default();
    let host = SessionId::new();
    assert_eq!(
        common.submit(&SftpClientCommand::BrowseManaged(
            host,
            cshell_application::SftpBrowseOptions {
                local: true,
                path: "/local".into(),
                links: cshell_application::SftpLinkPolicy::NoFollow
            }
        )),
        Some(false)
    );
    assert_eq!(
        common.submit(&SftpClientCommand::BrowseManaged(
            host,
            cshell_application::SftpBrowseOptions {
                local: false,
                path: "/remote".into(),
                links: cshell_application::SftpLinkPolicy::NoFollow
            }
        )),
        None
    );
    assert_eq!(common.submit(&SftpClientCommand::Disconnect(host)), None);
}

use crate::daemon_connection::{DesktopConnectionConfig, ResolvedDesktopEndpoint};
use cshell_ipc::{
    Envelope, HandshakePolicy, SftpOperation, SftpResponse, SftpStatus, envelope, read_envelope,
    server_handshake, transport, write_envelope,
};
use std::collections::BTreeMap;
#[derive(Default)]
struct ServerState {
    hosts: BTreeMap<SessionId, ProfileId>,
    connects: usize,
    disconnects: Vec<SessionId>,
    local_owner: Option<SessionId>,
    local_lists: usize,
    local_pages: usize,
    local_closes: usize,
    status_polls: BTreeMap<SessionId, usize>,
}
fn fixture_peer(profile: ProfileId) -> cshell_domain::ClipboardTrust {
    cshell_domain::ClipboardTrust {
        host: cshell_domain::ClipboardHost::new(&format!("host-{profile}.internal"), 22).unwrap(),
        username: "operator".into(),
        host_key_sha256: "fixture-authenticated-key".into(),
        route: "direct".into(),
    }
}
async fn wait_for(app: &mut DesktopApp, mut condition: impl FnMut(&DesktopApp) -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            app.sync_sftp_tabs();
            if condition(app) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("multi-host SFTP state timed out");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn two_host_tabs_keep_connections_remote_directories_and_one_local_cursor_independent() {
    let root = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let endpoint = format!(r"\\.\pipe\cshell-sftp-tabs-{}", SessionId::new());
    #[cfg(unix)]
    let endpoint = root.path().join("tabs.sock");
    let listener = transport::LocalListener::bind(&endpoint).unwrap();
    let config = DesktopConnectionConfig::for_fixture(ResolvedDesktopEndpoint {
        endpoint,
        instance_token: [62; 32],
        daemon_instance_id: [42; 16],
    });
    let local_path = root.path().to_string_lossy().into_owned();
    let state = Arc::new(std::sync::Mutex::new(ServerState::default()));
    let server_state = state.clone();
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(async move {
        loop {
            let mut stream = tokio::select! {incoming=listener.accept()=>incoming.unwrap(),_=stopped.changed()=>break};
            fn retired(error: &cshell_ipc::IpcError) -> bool {
                matches!(error,cshell_ipc::IpcError::Io(e) if matches!(e.kind(),std::io::ErrorKind::UnexpectedEof|std::io::ErrorKind::BrokenPipe|std::io::ErrorKind::ConnectionReset|std::io::ErrorKind::ConnectionAborted))
            }
            match server_handshake(
                &mut stream,
                &HandshakePolicy::with_instance_id([62; 32], [42; 16], u64::MAX),
            )
            .await
            {
                Ok(_) => {}
                Err(cshell_ipc::HandshakeProtocolError::Ipc(e)) if retired(&e) => continue,
                Err(e) => panic!("invalid mock handshake: {e}"),
            }
            // A tab can retire its polling connection after the authenticated hello.
            let message = match read_envelope(&mut stream).await {
                Ok(message) => message,
                Err(e) if retired(&e) => continue,
                Err(e) => panic!("invalid mock SFTP request: {e}"),
            };
            let Some(envelope::Payload::SftpRequest(query)) = message.payload else {
                panic!("expected SFTP request")
            };
            let operation = SftpOperation::try_from(query.operation).unwrap();
            let id = SessionId::from_bytes(query.session_id.as_slice().try_into().unwrap());
            let mut reply = SftpResponse::default();
            {
                let mut state = server_state.lock().unwrap();
                match operation {
                    SftpOperation::ConnectProfile => {
                        let options:cshell_application::SftpConnectRequest=serde_json::from_slice(&query.document_json).unwrap();
                        assert!(options.discover_directory);
                        let profile=options.profile;
                        assert!(state.hosts.insert(id, profile).is_none());
                        state.connects += 1;
                        reply.document_json =
                            serde_json::to_vec(&cshell_application::SftpConnectionInfo {
                                shared_from: None, initial_directory:Some("/".into()),
                                session: id,
                                profile,
                                title: format!("Host {profile}"),
                                peer: fixture_peer(profile),
                                running: true,
                            })
                            .unwrap();
                    }
                    SftpOperation::Disconnect => {
                        state.hosts.remove(&id);
                        state.disconnects.push(id);
                    }
                    SftpOperation::ConnectionStatus => {
                        if let Some(profile) = state.hosts.get(&id).copied() {
                            *state.status_polls.entry(id).or_default() += 1;
                            reply.document_json =
                                serde_json::to_vec(&cshell_application::SftpConnectionInfo {
                                    shared_from: None, initial_directory:Some("/".into()),
                                    session: id,
                                    profile,
                                    title: "Host".into(),
                                    peer: fixture_peer(profile),
                                    running: true,
                                })
                                .unwrap();
                        } else {
                            reply = SftpResponse::with_status(SftpStatus::Unavailable, "closed");
                        }
                    }
                    SftpOperation::QueueGet => {
                        reply.document_json =
                            serde_json::to_vec(&cshell_application::SftpQueueSnapshot::initial(id))
                                .unwrap()
                    }
                    SftpOperation::PeerGet => {
                        let profile = state.hosts[&id];
                        reply.document_json =
                            serde_json::to_vec(&cshell_application::SftpWorkspaceReply::Peer {
                                session: id,
                                peer: fixture_peer(profile),
                            })
                            .unwrap();
                    }
                    SftpOperation::BrowseManaged => {
                        let options: cshell_application::SftpBrowseOptions =
                            serde_json::from_slice(&query.document_json).unwrap();
                        assert!(
                            !options.local,
                            "host workers must not reload the shared local pane"
                        );
                        assert!(state.hosts.contains_key(&id));
                        reply.browse_local = false;
                        reply.browse_directory = options.path;
                        reply.browse_id = SessionId::new().as_uuid().as_bytes().to_vec();
                        reply.browse_revision = 1;
                        reply.entries = vec![cshell_ipc::SftpDirectoryEntry {
                            name: format!("{}.txt", state.hosts[&id]),
                            path: format!(
                                "{}/{}.txt",
                                reply.browse_directory.trim_end_matches('/'),
                                state.hosts[&id]
                            ),
                            kind: 1,
                            ..Default::default()
                        }];
                    }
                    SftpOperation::LocalBrowse | SftpOperation::BrowseNext => {
                        assert!(
                            !state.hosts.contains_key(&id),
                            "local cursor must use its own scope"
                        );
                        if let Some(owner) = state.local_owner {
                            assert_eq!(owner, id)
                        } else {
                            state.local_owner = Some(id);
                        }
                        let next = operation == SftpOperation::BrowseNext;
                        if next {
                            state.local_pages += 1;
                        } else {
                            state.local_lists += 1;
                        }
                        reply.browse_local = true;
                        reply.browse_directory = if query.local_path.is_empty() {
                            local_path.clone()
                        } else {
                            query.local_path
                        };
                        reply.browse_id = if next {
                            query.transfer_id
                        } else {
                            SessionId::new().as_uuid().as_bytes().to_vec()
                        };
                        reply.browse_revision = if next { query.expected_revision + 1 } else { 1 };
                        reply.truncated = true;
                        reply.entries = vec![cshell_ipc::SftpDirectoryEntry {
                            name: if next { "second.txt" } else { "shared.txt" }.into(),
                            path: format!(
                                "{}/{}",
                                reply.browse_directory,
                                if next { "second.txt" } else { "shared.txt" }
                            ),
                            kind: 1,
                            ..Default::default()
                        }];
                    }
                    SftpOperation::BrowseClose => {
                        assert_eq!(state.local_owner, Some(id));
                        state.local_closes += 1;
                    }
                    _ => panic!("unexpected operation {operation:?}"),
                }
            }
            let _ = write_envelope(
                &mut stream,
                &Envelope {
                    request_id: message.request_id,
                    payload: Some(envelope::Payload::SftpResponse(reply)),
                    ..Default::default()
                },
            )
            .await;
        }
    });
    let mut app = DesktopApp {
        workspace_config: Some(config.clone()),
        sftp: Some(DesktopSftpConnection::start(config).unwrap()),
        ..Default::default()
    };
    let a = ProfileId::new();
    let b = ProfileId::new();
    app.open_sftp_tab(a);
    wait_for(&mut app, |app| {
        app.sftp.as_ref().unwrap().view().connection_phase == ConnectionPhase::Connected
            && app
                .sftp_tabs
                .local
                .connection
                .as_ref()
                .unwrap()
                .view()
                .local_directory
                .is_some()
    })
    .await;
    let a_tab = app.sftp_tabs.active.unwrap();
    let a_session = app.sftp_session.unwrap();
    app.submit_sftp_command(SftpClientCommand::BrowseManaged(
        a_session,
        cshell_application::SftpBrowseOptions {
            local: false,
            path: "/srv/host-a".into(),
            links: cshell_application::SftpLinkPolicy::NoFollow,
        },
    ));
    wait_for(&mut app, |app| {
        app.sftp
            .as_ref()
            .unwrap()
            .view()
            .remote_loaded_directory
            .as_deref()
            == Some("/srv/host-a")
    })
    .await;
    let local_before = app.sftp_tabs.local.connection.as_ref().unwrap().view();
    let initial_local = local_before.local_directory.clone().unwrap();
    let cursor = local_before.local_cursor.clone().unwrap();
    app.submit_sftp_command(SftpClientCommand::BrowseNext(a_session, true, cursor));
    wait_for(&mut app, |app| {
        app.sftp_tabs
            .local
            .connection
            .as_ref()
            .unwrap()
            .view()
            .local_cursor
            .as_ref()
            .is_some_and(|c| c.revision == 2)
    })
    .await;
    let local_page = app.sftp_tabs.local.connection.as_ref().unwrap().view();
    app.open_sftp_tab(b);
    wait_for(&mut app, |app| {
        app.sftp.as_ref().unwrap().view().connection_phase == ConnectionPhase::Connected
    })
    .await;
    let b_tab = app.sftp_tabs.active.unwrap();
    let b_session = app.sftp_session.unwrap();
    assert_ne!(a_session, b_session);
    assert_eq!(app.sftp_tabs.entries.len(), 2);
    let first = app
        .sftp_tabs
        .entries
        .iter()
        .find(|t| t.id == a_tab)
        .unwrap()
        .parked
        .as_ref()
        .unwrap()
        .connection
        .as_ref()
        .unwrap()
        .view();
    assert_eq!(first.session_id, Some(a_session));
    assert_eq!(
        first.remote_loaded_directory.as_deref(),
        Some("/srv/host-a")
    );
    assert_eq!(first.connection_phase, ConnectionPhase::Connected);
    let displayed = app
        .sftp_tabs
        .local
        .display(app.sftp.as_ref().unwrap().view());
    assert_eq!(
        displayed.local_directory.as_deref(),
        Some(initial_local.as_str())
    );
    assert_eq!(displayed.local_entries[0].name, "second.txt");
    assert_eq!(
        displayed.local_cursor.as_ref().unwrap().id,
        local_page.local_cursor.as_ref().unwrap().id
    );
    assert_eq!(displayed.local_cursor.unwrap().revision, 2);
    wait_for(&mut app, |_| {
        state
            .lock()
            .unwrap()
            .status_polls
            .get(&a_session)
            .copied()
            .unwrap_or(0)
            > 0
    })
    .await;
    {
        let state = state.lock().unwrap();
        assert_eq!(state.connects, 2);
        assert!(state.disconnects.is_empty());
        assert_eq!(state.local_lists, 1);
        assert_eq!(state.local_pages, 1);
    }
    app.open_sftp_tab(a);
    assert_eq!(app.sftp_tabs.active, Some(a_tab));
    assert_eq!(app.sftp_session, Some(a_session));
    assert_eq!(
        app.sftp
            .as_ref()
            .unwrap()
            .view()
            .remote_loaded_directory
            .as_deref(),
        Some("/srv/host-a")
    );
    app.apply_sftp_tab_action(TabAction::Close(b_tab));
    wait_for(&mut app, |_| {
        !state.lock().unwrap().hosts.contains_key(&b_session)
    })
    .await;
    assert_eq!(app.sftp_tabs.entries.len(), 1);
    assert_eq!(app.sftp_session, Some(a_session));
    assert_eq!(
        app.sftp.as_ref().unwrap().view().connection_phase,
        ConnectionPhase::Connected
    );
    {
        let state = state.lock().unwrap();
        assert_eq!(state.connects, 2);
        assert!(state.disconnects.iter().all(|id| *id == b_session));
        assert_eq!(state.local_lists, 1);
    }
    app.apply_sftp_tab_action(TabAction::Close(a_tab));
    wait_for(&mut app, |_| {
        let state = state.lock().unwrap();
        state.hosts.is_empty() && state.local_closes >= 1
    })
    .await;
    assert!(app.sftp_tabs.entries.is_empty());
    assert!(app.sftp_tabs.active.is_none());
    assert!(!app.sftp_panel.open);
    let _ = stop.send(true);
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn common_local_browser_reads_and_pages_real_files_without_any_ssh_session() {
    let root = tempfile::tempdir().unwrap();
    // macOS temporary roots may go through /var -> /private/var. The default
    // local browser intentionally rejects directory links, including ancestors.
    let real_root = root.path().canonicalize().unwrap();
    let files = real_root.join("files");
    std::fs::create_dir(&files).unwrap();
    for index in 0..513 {
        std::fs::write(files.join(format!("{index:04}.txt")), b"fixture").unwrap();
    }
    #[cfg(windows)]
    let endpoint = format!(r"\\.\pipe\cshell-local-browser-{}", SessionId::new());
    #[cfg(unix)]
    let endpoint = real_root.join("local.sock");
    let listener = transport::LocalListener::bind(&endpoint).unwrap();
    let config = DesktopConnectionConfig::for_fixture(ResolvedDesktopEndpoint {
        endpoint,
        instance_token: [29; 32],
        daemon_instance_id: [37; 16],
    });
    let service = Arc::new(cshelld::SessionIpcService::new(Arc::new(
        cshelld::LocalSessionRegistry::new(real_root.join("journals"), 16).unwrap(),
    )));
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(async move {
        let mut children = tokio::task::JoinSet::new();
        loop {
            let mut stream = tokio::select! {incoming=listener.accept()=>incoming.unwrap(),_=stopped.changed()=>break};
            let service = service.clone();
            children.spawn(async move {
                server_handshake(
                    &mut stream,
                    &HandshakePolicy::with_instance_id([29; 32], [37; 16], u64::MAX),
                )
                .await
                .unwrap();
                let _ = service.serve_connection(stream).await;
            });
        }
    });
    let session = SessionId::new();
    let connection = DesktopSftpConnection::start(config).unwrap();
    assert!(connection.request(SftpClientCommand::LocalList(
        session,
        files.to_string_lossy().into_owned()
    )));
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let view = connection.view();
            assert!(view.error.is_none(), "local browse failed: {:?}", view.error);
            if view.local_cursor.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let first = connection.view();
    assert_eq!(first.local_entries.len(), 512);
    assert!(first.local_truncated);
    assert_eq!(first.connection_profile, None);
    let common = CommonLocal {
        connection: Some(connection),
        session: Some(session),
        generation: None,
    };
    assert_eq!(
        common.submit(&SftpClientCommand::BrowseNext(
            SessionId::new(),
            true,
            first.local_cursor.unwrap()
        )),
        Some(true)
    );
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let view = common.connection.as_ref().unwrap().view();
            if view.local_directory.is_some() && !view.local_truncated {
                assert_eq!(view.local_entries.len(), 1);
                assert!(view.error.is_none());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(common);
    let _ = stop.send(true);
    server.await.unwrap();
}

#[test]
fn shared_ssh_sftp_source_is_only_the_current_matching_live_terminal() {
    let mut app=DesktopApp::default();let profile=ProfileId::new();let source=SessionId::new();
    let tab=app.workspace.add(Some(profile),"SSH host".into()).unwrap();
    app.current_workspace_tab=Some(tab);app.workspace.sessions.insert(tab,source);app.view_model.reusable_ssh_sessions.push(source);
    assert_eq!(app.sftp_ssh_source(profile),Some(source));assert_eq!(app.sftp_ssh_source(ProfileId::new()),None);
    app.view_model.reusable_ssh_sessions.clear();assert_eq!(app.sftp_ssh_source(profile),None);
    app.current_workspace_tab=None;assert_eq!(app.sftp_ssh_source(profile),None);
}
