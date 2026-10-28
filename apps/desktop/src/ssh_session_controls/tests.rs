use super::*;
use cshell_ipc::{
    Envelope, HandshakePolicy, SessionCreateResponse, SessionListResponse, SessionSummary,
    envelope, features, read_envelope, server_handshake, transport, write_envelope,
};
use cshell_terminal::{ProbeTerminalEngine, TerminalEngine};
use daemon_connection::{DesktopConnectionConfig, ResolvedDesktopEndpoint};
use std::{sync::Mutex, time::Duration};

#[derive(Default)]
struct ServerState {
    sessions: Vec<SessionSummary>,
    events: Vec<&'static str>,
}

fn summary(id: SessionId, profile: ProfileId) -> SessionSummary {
    SessionSummary {
        reconnecting: false,
        session_id: id.as_uuid().as_bytes().to_vec(),
        profile_id: Some(profile.as_uuid().as_bytes().to_vec()),
        title: "SSH toolbar test".into(),
        running: true,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_controls_close_confirm_reconnect_once_and_keep_the_same_tab() {
    for (immediate_reconnect, uncertain_close, missing_old) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let profile = ProfileId::new();
        let old = SessionId::new();
        let new = SessionId::new();
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\cshell-ssh-toolbar-{old}");
        #[cfg(unix)]
        let endpoint = directory.path().join("controls.sock");
        let listener = transport::LocalListener::bind(&endpoint).unwrap();
        let shared = Arc::new(Mutex::new(ServerState {
            sessions: vec![summary(old, profile)],
            ..Default::default()
        }));
        let state = shared.clone();
        let server = tokio::spawn(async move {
            let policy = HandshakePolicy::with_instance_id(
                [71; 32],
                [17; 16],
                features::TERMINAL_CONTROL
                    | features::FULL_FRAME_RECOVERY
                    | features::SSH_PROFILE_SESSION
                    | features::LOCAL_PROFILE,
            );
            let mut clients = tokio::task::JoinSet::new();
            // Match the real server: a completed client must not cancel and
            // drop a named-pipe accept that another client has already opened.
            let pending_accept=listener.accept();
            tokio::pin!(pending_accept);
            loop {
                tokio::select! {
                    accepted=&mut pending_accept=>{
                        let mut stream=accepted.unwrap(); let state=state.clone(); let policy=policy.clone();
                        pending_accept.set(listener.accept());
                        clients.spawn(async move {
                            if server_handshake(&mut stream,&policy).await.is_err(){return;}
                            while let Ok(request)=read_envelope(&mut stream).await {
                                let payload={
                                    let mut state=state.lock().unwrap();
                                    match request.payload {
                                        Some(envelope::Payload::SessionListRequest(_))=>envelope::Payload::SessionListResponse(SessionListResponse{sessions:state.sessions.clone()}),
                                        Some(envelope::Payload::SessionCloseRequest(close))=>{
                                            assert!(!close.apply_view_policy);
                                            assert_eq!(close.session_id,old.as_uuid().as_bytes());
                                            state.events.push("close");
                                            if !uncertain_close {state.sessions.retain(|s|s.session_id!=close.session_id);}
                                            envelope::Payload::SessionCloseResponse(cshell_ipc::SessionCloseResponse{session_id:close.session_id})
                                        }
                                        Some(envelope::Payload::SessionCreateRequest(create))=>{
                                            assert_eq!(create.profile_id.as_deref(),Some(profile.as_uuid().as_bytes().as_slice()));
                                            assert!(state.sessions.is_empty(),"reconnect must not leave the old server session running");
                                            state.events.push("create"); state.sessions.push(summary(new,profile));
                                            envelope::Payload::SessionCreateResponse(SessionCreateResponse{session:Some(summary(new,profile)),..Default::default()})
                                        }
                                        Some(envelope::Payload::SnapshotRequest(snapshot))=>{
                                            let id=SessionId::from_bytes(snapshot.session_id.try_into().unwrap());
                                            let mut terminal=ProbeTerminalEngine::new(cshell_domain::TerminalSize::cells(24,80));
                                            terminal.feed(b"SSH TOOLBAR READY");
                                            envelope::Payload::FullFrame(cshell_ipc::FullFrame::from_terminal_snapshot(id,&terminal.snapshot()))
                                        }
                                        other=>panic!("unexpected control request: {other:?}"),
                                    }
                                };
                                if write_envelope(&mut stream,&Envelope{request_id:request.request_id,payload:Some(payload),..Default::default()}).await.is_err(){break;}
                            }
                        });
                    }
                    joined=clients.join_next(), if !clients.is_empty()=>{joined.unwrap().unwrap();}
                }
            }
        });
        let config = DesktopConnectionConfig::for_fixture(ResolvedDesktopEndpoint {
            endpoint,
            instance_token: [71; 32],
            daemon_instance_id: [17; 16],
        });
        let mut app = DesktopApp::default();
        app.workspace.writable = true;
        app.workspace_loaded = true;
        let tab = app
            .workspace
            .add(Some(profile), "SSH toolbar test".into())
            .unwrap();
        app.workspace.sessions.insert(tab, old);
        app.current_workspace_tab = Some(tab);
        app.view_model
            .saved_ssh_profiles
            .push(cshell_ui::SavedSshProfileViewModel {
                id: profile,
                name: "SSH".into(),
                target: "operator@test".into(),
                favorite: false,
                folder_id: None,
                kind: ProfileKind::Ssh,
            });
        // Sidebar selection must never change which server the toolbar reconnects.
        app.view_model.selected_profile = Some(ProfileId::new());
        let connection = DesktopDaemonConnection::start_idle(config.clone()).unwrap();
        assert!(connection.attach_session(old));
        app.daemon = Some(connection);
        app.daemon_config = Some(config);
        tokio::time::timeout(Duration::from_secs(8), async {
            while !app.daemon.as_ref().unwrap().view().connected
                || app.daemon.as_ref().unwrap().view().snapshot.is_none()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        app.view_model.selected = Some(old);
        app.poll_primary_terminal(std::time::Instant::now());
        assert!(
            app.terminal_surface.latest_snapshot().is_some(),
            "initial terminal must be visible before disconnect"
        );
        let original_document = app.workspace.document.clone();
        if missing_old {
            shared.lock().unwrap().sessions.clear();
        }
        app.request_ssh_control(immediate_reconnect);
        assert!(app.ssh_control.is_some());
        assert_eq!(
            app.view_model.ssh_tab_states.get(&tab),
            Some(&if immediate_reconnect {
                SshConnectionState::Reconnecting
            } else {
                SshConnectionState::Disconnecting
            })
        );
        app.request_ssh_control(immediate_reconnect); // repeated click must not overwrite the close
        assert_eq!(app.workspace.pending_launch, Some(tab));
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                app.sync_workspace();
                if uncertain_close {
                    if app.ssh_control.is_none() {
                        break;
                    }
                } else if immediate_reconnect {
                    if app.workspace.sessions.get(&tab) == Some(&new) {
                        break;
                    }
                } else if app.ssh_control.is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(app.current_workspace_tab, Some(tab));
        assert_eq!(app.workspace.document, original_document);
        assert!(app.closing_workspace_tab.is_none());
        if uncertain_close {
            assert_eq!(app.workspace.sessions.get(&tab), Some(&old));
            assert!(
                app.view_model
                    .input_warning
                    .as_deref()
                    .is_some_and(|w| w.contains("未创建新连接"))
            );
            assert_ne!(
                app.view_model.ssh_tab_states.get(&tab),
                Some(&SshConnectionState::Disconnected)
            );
            assert_eq!(shared.lock().unwrap().events, vec!["close"]);
        } else {
            if !immediate_reconnect {
                assert!(!app.workspace.sessions.contains_key(&tab));
                assert!(
                    app.terminal_surface.latest_snapshot().is_some(),
                    "disconnect preserves terminal history"
                );
                for _ in 0..3 {
                    app.sync_workspace();
                    app.poll_primary_terminal(std::time::Instant::now());
                    app.refresh_ssh_controls();
                }
                assert_eq!(
                    app.view_model.ssh_tab_states.get(&tab),
                    Some(&SshConnectionState::Disconnected)
                );
                assert!(app.cursor_blink.next_toggle().is_none());
                let other = app.workspace.add(None, "Other".into()).unwrap();
                app.activate_workspace_tab(other);
                app.refresh_ssh_controls();
                assert_eq!(
                    app.view_model.ssh_tab_states.get(&tab),
                    Some(&SshConnectionState::Disconnected)
                );
                app.activate_workspace_tab(tab);
                app.workspace.remove(other);
                app.refresh_workspace_tabs();
                app.refresh_ssh_controls();
                assert_eq!(
                    app.view_model.ssh_tab_states.get(&tab),
                    Some(&SshConnectionState::Disconnected)
                );
                assert!(!app.view_model.can_disconnect_ssh);
                assert!(app.view_model.can_restart_ssh);
                app.request_ssh_control(true);
                app.request_ssh_control(true);
                tokio::time::timeout(Duration::from_secs(8), async {
                    while app.workspace.sessions.get(&tab) != Some(&new) {
                        app.sync_workspace();
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
            }
            app.refresh_ssh_controls();
            assert_eq!(
                app.view_model.ssh_tab_states.get(&tab),
                Some(&SshConnectionState::Connected)
            );
            assert_eq!(app.workspace.sessions.get(&tab), Some(&new));
            assert_eq!(app.workspace.document.bindings.len(), 1);
            assert_eq!(
                shared.lock().unwrap().events,
                if missing_old {
                    vec!["create"]
                } else {
                    vec!["close", "create"]
                }
            );
            assert_eq!(shared.lock().unwrap().sessions.len(), 1);
        }
        drop(app);
        server.abort();
        let _ = server.await;
        drop(directory);
    }
}

#[test]
fn local_or_empty_tabs_never_offer_ssh_controls() {
    let mut app = DesktopApp::default();
    app.workspace.writable = true;
    app.refresh_ssh_controls();
    assert!(!app.view_model.can_restart_ssh);
    assert!(!app.view_model.can_disconnect_ssh);
    let profile = ProfileId::new();
    let tab = app.workspace.add(Some(profile), "Local".into()).unwrap();
    app.current_workspace_tab = Some(tab);
    app.view_model
        .saved_ssh_profiles
        .push(cshell_ui::SavedSshProfileViewModel {
            id: profile,
            name: "Local".into(),
            target: "pwsh".into(),
            favorite: false,
            folder_id: None,
            kind: ProfileKind::Local,
        });
    app.request_ssh_control(true);
    assert!(app.workspace.pending_launch.is_none());
    assert!(app.ssh_control.is_none());
}


#[test]
fn configured_target_omits_missing_username_and_brackets_ipv6() {
    let mut target=cshell_domain::SshConnectionRecord{
        profile_id:ProfileId::new(),host:"2001:db8::1".into(),port:2222,username:String::new(),
        auth_method:cshell_domain::SshAuthMethod::Password,private_key_path:None,certificate_path:None,
        agent_backend:Default::default(),agent_identity:None,route:Default::default(),options:Default::default(),
    };
    assert_eq!(configured_target(&target),"[2001:db8::1]:2222");
    target.username="operator".into();assert_eq!(configured_target(&target),"operator@[2001:db8::1]:2222");
    target.host="server.example".into();assert_eq!(configured_target(&target),"operator@server.example:2222");
}
