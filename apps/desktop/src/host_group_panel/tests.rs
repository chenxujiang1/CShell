use super::*;
use crate::host_group_connection::HostGroupConnection;
use cshell_application::HostManagementSnapshot;
use std::sync::Arc;

#[test]
fn editor_preserves_exact_metadata_and_revision_and_empty_query_is_not_all()
-> Result<(), Box<dyn std::error::Error>> {
    let host = HostRecord {
        id: HostId::new(),
        name: "\u{751f}\u{4ea7} host".into(),
        address: "host.test".into(),
        port: 2222,
        profile_id: ProfileId::new(),
        tags: BTreeSet::from(["has,comma".into(), " spaced tag ".into()]),
        labels: BTreeMap::from([
            ("env".into(), "prod".into()),
            ("version".into(), " x=y ".into()),
        ]),
        platform_hint: HostPlatformHint::Linux,
    };
    let mut panel = HostGroupPanel::default();
    panel.load_host(&host, (7, 12));
    assert_eq!(panel.record()?, host);
    assert_eq!(panel.host_base, Some((7, 12)));
    let Some(HostSelection::Selector { expression }) = panel.selection() else {
        return Err("missing query selection".into());
    };
    assert!(HostSelector::parse(&expression).is_err());
    panel.labels = "env=prod\nenv=stage".into();
    assert!(panel.record().is_err());
    Ok(())
}

#[test]
fn headless_panel_retains_old_edit_revision_when_shared_catalog_is_refreshed() {
    let view = HostGroupView {
        snapshot: Some(Arc::new(HostManagementSnapshot {
            revision: 8,
            profile_revision: 13,
            catalog: HostCatalog::default(),
            profiles: Vec::new(),
        })),
        writable: true,
        ..Default::default()
    };
    let connection = HostGroupConnection::for_panel_fixture(view.clone());
    let mut panel = HostGroupPanel {
        open: true,
        host_base: Some((7, 12)),
        group_base: Some((7, 12)),
        ..Default::default()
    };
    let context = egui::Context::default();
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1024.0, 768.0),
            )),
            ..Default::default()
        },
        |ui| panel.draw(ui.ctx(), &connection, &view),
    );
    assert!(!output.shapes.is_empty());
    output.textures_delta.clear();
    assert_eq!(panel.host_base, Some((7, 12)));
    assert_eq!(panel.group_base, Some((7, 12)));
    assert!(panel.open);
}
