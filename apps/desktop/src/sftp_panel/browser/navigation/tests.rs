use super::super::tests::{click, draw, fixture};
use super::*;
use crate::ui_layout_gate::{check_window, context};

fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

#[test]
fn history_commits_only_successful_loads_and_truncates_forward_branch() {
    let mut history = History::default();
    for path in ["/", "/home", "/var", "/var"] {
        history.loaded(path);
    }
    assert_eq!(history.paths, ["/", "/home", "/var"]);
    history.pending = history.target(false);
    // A failed request never calls loaded: the current directory and forward branch stay put.
    assert_eq!(history.current, 2);
    history.loaded("/home");
    assert_eq!(history.current, 1);
    history.pending = history.target(true);
    history.loaded("/var");
    assert_eq!(history.current, 2);
    history.pending = history.target(false);
    history.loaded("/home");
    history.loaded("/etc");
    assert_eq!(history.paths, ["/", "/home", "/etc"]);
    assert!(history.target(true).is_none());
    for index in 0..100 {
        history.loaded(&format!("/dir{index}"));
    }
    assert_eq!(history.paths.len(), 64);
    assert_eq!(history.current, 63);
}

#[test]
fn breadcrumbs_preserve_posix_drive_and_unc_roots() {
    assert_eq!(
        breadcrumbs(false, "/home/中文目录"),
        vec![
            ("/".into(), "/".into()),
            ("home".into(), "/home".into()),
            ("中文目录".into(), "/home/中文目录".into())
        ]
    );
    assert_eq!(breadcrumbs(false, "/").len(), 1);
    assert!(breadcrumbs(false, "").is_empty());
    #[cfg(windows)]
    {
        let drive = breadcrumbs(true, "C:/Users/operator");
        assert_eq!(drive[0].1, "C:/");
        assert_eq!(drive[2], ("operator".into(), "C:/Users/operator".into()));
        let unc = breadcrumbs(true, r"\\server\share\folder");
        assert_eq!(unc[0].1, r"\\server\share\");
        assert_eq!(unc.last().unwrap().0, "folder");
    }
}

#[test]
fn editable_path_selects_replaces_submits_and_escape_cancels() {
    let context = context();
    let (mut panel, view, session) = fixture();
    let size = egui::vec2(900.0, 600.0);
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    assert!(
        draw(
            &context,
            &mut panel,
            &view,
            size,
            vec![key(egui::Key::L, egui::Modifiers::CTRL)]
        )
        .1
        .is_none()
    );
    assert!(panel.browser.navigation.editing[1]);
    draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![egui::Event::Text("/var/log".into())],
    );
    assert_eq!(panel.remote_directory, "/var/log");
    let command = draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
    )
    .1;
    assert!(
        matches!(command, Some(SftpClientCommand::BrowseManaged(id, options)) if id == session && !options.local && options.path == "/var/log")
    );
    assert!(!panel.browser.navigation.editing[1]);
    draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![key(egui::Key::D, egui::Modifiers::ALT)],
    );
    draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![egui::Event::Text("/not-sent".into())],
    );
    assert!(
        draw(
            &context,
            &mut panel,
            &view,
            size,
            vec![key(egui::Key::Escape, egui::Modifiers::NONE)]
        )
        .1
        .is_none()
    );
    assert!(!panel.browser.navigation.editing[1]);
    assert_eq!(
        panel.remote_directory, "/home/operator",
        "Esc restores the loaded path for other SFTP pages too"
    );
    let command = draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![key(egui::Key::F5, egui::Modifiers::NONE)],
    )
    .1;
    assert!(
        matches!(command, Some(SftpClientCommand::BrowseManaged(_, options)) if options.path == "/home/operator")
    );
}

#[test]
fn shortcuts_and_buttons_keep_pane_histories_separate() {
    let context = context();
    let (mut panel, mut view, session) = fixture();
    let size = egui::vec2(900.0, 600.0);
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    view.remote_loaded_directory = Some("/var/log".into());
    draw(&context, &mut panel, &view, size, vec![]);
    let command = draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![key(egui::Key::ArrowLeft, egui::Modifiers::ALT)],
    )
    .1;
    assert!(
        matches!(command, Some(SftpClientCommand::BrowseManaged(id, options)) if id == session && !options.local && options.path == "/home/operator")
    );
    assert_eq!(panel.browser.navigation.history[1].current, 1);
    view.error = Some("permission denied".into());
    draw(&context, &mut panel, &view, size, vec![]);
    assert_eq!(panel.browser.navigation.history[1].current, 1);
    view.error = None;
    view.remote_loaded_directory = Some("/home/operator".into());
    draw(&context, &mut panel, &view, size, vec![]);
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    let pos = crate::ui_layout_gate::bounds(
        &tree
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.role() == egui::accesskit::Role::Button && node.label() == Some("→")
            })
            .max_by(|(_, a), (_, b)| {
                crate::ui_layout_gate::bounds(a)
                    .min
                    .x
                    .total_cmp(&crate::ui_layout_gate::bounds(b).min.x)
            })
            .unwrap()
            .1,
    )
    .center();
    let pointer = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![egui::Event::PointerMoved(pos), pointer(true)],
    );
    let command = draw(&context, &mut panel, &view, size, vec![pointer(false)]).1;
    assert!(
        matches!(command, Some(SftpClientCommand::BrowseManaged(_, options)) if !options.local && options.path == "/var/log")
    );
    panel.browser.navigation.active = 0;
    let command = draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![key(egui::Key::ArrowUp, egui::Modifiers::ALT)],
    )
    .1;
    assert!(
        matches!(command, Some(SftpClientCommand::BrowseManaged(_, options)) if options.local && options.path == parent_path(true, view.local_directory.as_deref().unwrap()))
    );
    assert_eq!(panel.browser.navigation.history[0].paths.len(), 1);
    assert_eq!(panel.browser.navigation.history[1].paths.len(), 2);
    // Default directory editing owns its keystrokes while the menu is open.
    click(&context, &mut panel, &view, size, "操作");
    click(&context, &mut panel, &view, size, "默认目录");
    assert!(
        draw(
            &context,
            &mut panel,
            &view,
            size,
            vec![key(egui::Key::F5, egui::Modifiers::NONE)]
        )
        .1
        .is_none()
    );
}

#[test]
fn long_breadcrumbs_and_path_editor_fit_small_window() {
    let context = context();
    let (mut panel, mut view, _) = fixture();
    view.remote_loaded_directory = Some(format!("/home/{}", "long-directory/".repeat(24)));
    let size = egui::vec2(720.0, 480.0);
    for _ in 0..4 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    check_window(&context, size, "SFTP 文件", &tree);
    draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![key(egui::Key::L, egui::Modifiers::CTRL)],
    );
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    check_window(&context, size, "SFTP 文件", &tree);
}

#[test]
fn favorites_are_scoped_to_verified_peer_and_local_pane() {
    let (_, mut view, _) = fixture();
    assert!(favorite_for(&view, false, "/root").is_none());
    view.peer = Some(cshell_domain::ClipboardTrust {
        host: cshell_domain::ClipboardHost::new("example.test", 22).unwrap(),
        username: "root".into(),
        host_key_sha256: "key".into(),
        route: "direct".into(),
    });
    let remote = favorite_for(&view, false, "/root").unwrap();
    assert!(remote.peer == view.peer);
    assert!(
        favorite_for(&view, true, "C:/Users")
            .unwrap()
            .peer
            .is_none()
    );
    view.peer.as_mut().unwrap().username = "other".into();
    assert!(remote.peer != view.peer);
}

#[test]
fn mouse_go_button_submits_an_edited_path() {
    let context = context();
    let (mut panel, view, _) = fixture();
    let size = egui::vec2(900.0, 600.0);
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![key(egui::Key::L, egui::Modifiers::CTRL)],
    );
    draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![egui::Event::Text("/srv".into())],
    );
    assert!(
        matches!(click(&context, &mut panel, &view, size, "转到"), Some(SftpClientCommand::BrowseManaged(_, options)) if options.path == "/srv")
    );
}

#[test]
fn favorite_menus_filter_by_pane_and_navigate_without_confirmation() {
    let context = context();
    let (mut panel, mut view, _) = fixture();
    let peer = cshell_domain::ClipboardTrust {
        host: cshell_domain::ClipboardHost::new("server.test", 22).unwrap(),
        username: "root".into(),
        host_key_sha256: "key".into(),
        route: "direct".into(),
    };
    view.peer = Some(peer.clone());
    let mut other = peer.clone();
    other.username = "someone-else".into();
    let local_path = if cfg!(windows) {
        "C:/favorite-local"
    } else {
        "/favorite-local"
    };
    panel.browser.navigation.favorites = vec![
        Favorite {
            peer: None,
            path: local_path.into(),
        },
        Favorite {
            peer: Some(peer),
            path: "/srv/favorite".into(),
        },
        Favorite {
            peer: Some(other),
            path: "/other-account".into(),
        },
    ];
    let size = egui::vec2(1280.0, 800.0);
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    click(&context, &mut panel, &view, size, "☆");
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, n)| n.label() == Some("/srv/favorite"))
    );
    assert!(
        matches!(click(&context, &mut panel, &view, size, local_path), Some(SftpClientCommand::BrowseManaged(_, options)) if options.local && options.path == local_path)
    );
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    let pos = crate::ui_layout_gate::bounds(
        &tree
            .nodes
            .iter()
            .filter(|(_, n)| n.role() == egui::accesskit::Role::Button && n.label() == Some("☆"))
            .max_by(|(_, a), (_, b)| {
                crate::ui_layout_gate::bounds(a)
                    .min
                    .x
                    .total_cmp(&crate::ui_layout_gate::bounds(b).min.x)
            })
            .unwrap()
            .1,
    )
    .center();
    let pointer = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![egui::Event::PointerMoved(pos), pointer(true)],
    );
    draw(&context, &mut panel, &view, size, vec![pointer(false)]);
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, n)| n.label() == Some("/other-account"))
    );
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, n)| n.label() == Some(local_path))
    );
    assert!(
        matches!(click(&context, &mut panel, &view, size, "/srv/favorite"), Some(SftpClientCommand::BrowseManaged(_, options)) if !options.local && options.path == "/srv/favorite")
    );
    assert!(
        matches!(click(&context, &mut panel, &view, size, "/"), Some(SftpClientCommand::BrowseManaged(_, options)) if !options.local && options.path == "/")
    );
}
