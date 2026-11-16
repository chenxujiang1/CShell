#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn opening_samples_once_and_zero_never_schedules_polling() {
    let mut panel = Panel::default();
    let now = Instant::now();
    assert!(!panel.due(now));
    panel.show();
    panel.visible = true;
    assert_eq!(panel.refresh_seconds, 0);
    assert!(panel.due(now));
    panel.initial = false; // initial request dispatched
    assert!(!panel.due(now + Duration::from_secs(86400)));
    panel.interval(0, now);
    assert!(panel.next_due.is_none());
}

#[test]
fn automatic_refresh_has_a_minimum_and_hiding_stops_it() {
    let mut panel = Panel::default();
    panel.show();
    panel.visible = true;
    panel.initial = false;
    let now = Instant::now();
    panel.interval(1, now);
    assert_eq!(panel.refresh_seconds, 5);
    assert!(!panel.due(now + Duration::from_secs(4)));
    assert!(panel.due(now + Duration::from_secs(5)));
    panel.suspend();
    assert!(!panel.due(now + Duration::from_secs(100)));
    panel.interval(0, now);
    assert!(!panel.due(now + Duration::from_secs(100)));
    panel.open = false;
    panel.show();
    assert_eq!(panel.refresh_seconds, 0);
}

#[test]
fn failed_or_stale_reply_disables_automatic_refresh() {
    let mut panel = Panel::default();
    panel.show();
    panel.session = Some(SessionId::new());
    let now = Instant::now();
    panel.interval(10, now);
    let (send, result) = mpsc::sync_channel(1);
    send.send(Err("probe failed".into())).unwrap();
    panel.worker = Some(Worker {
        result,
        cancel: None,
    });
    panel.receive(now);
    assert_eq!(panel.refresh_seconds, 0);
    assert!(panel.next_due.is_none());
    assert_eq!(panel.error.as_deref(), Some("probe failed"));
}

#[test]
fn small_window_without_an_ssh_target_never_starts_sampling() {
    let ctx = egui::Context::default();
    crate::ui_fonts::install(&ctx);
    let mut panel = Panel::default();
    panel.show();
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(480.0, 320.0),
            )),
            ..Default::default()
        },
        |ui| {
            panel.draw(ui.ctx(), None, None, None, true);
        },
    );
    output.textures_delta.clear();
    assert!(panel.worker.is_none());
    assert_eq!(panel.refresh_seconds, 0);
    assert!(!panel.initial);
}

fn sample() -> ServerResources {
    ServerResources {
        session: SessionId::new(),
        transport_generation: 1,
        sampled_unix_ms: 1_720_000_000_000,
        cpu_basis_points: Some(1234),
        memory_total_kib: 8 * 1024 * 1024,
        memory_available_kib: 2 * 1024 * 1024,
        swap_total_kib: 1024 * 1024,
        swap_free_kib: 512 * 1024,
        load_milli: [1250, 750, 250],
        uptime_seconds: 176_520,
    }
}

fn fixture_panel(sample: ServerResources) -> (Panel, DesktopConnectionConfig) {
    let panel = Panel {
        open: true,
        session: Some(sample.session),
        sample: Some(sample),
        visible: true,
        ..Default::default()
    };
    let config =
        DesktopConnectionConfig::for_fixture(crate::daemon_connection::ResolvedDesktopEndpoint {
            endpoint: "resource-ui-offline-no-service".into(),
            instance_token: [31; 32],
            daemon_instance_id: [32; 16],
        });
    (panel, config)
}

#[test]
fn populated_panel_keeps_refresh_and_close_visible_at_small_sizes_and_dpi() {
    for (size, scale) in [
        (egui::vec2(1000.0, 760.0), 1.0),
        (egui::vec2(720.0, 480.0), 1.5),
        (egui::vec2(480.0, 320.0), 2.0),
    ] {
        for extreme in [false, true] {
            let ctx = crate::ui_layout_gate::context();
            ctx.set_pixels_per_point(scale);
            ctx.all_styles_mut(|s| s.animation_time = 0.0);
            let original = ctx.style_of(egui::Theme::Dark).text_styles.clone();
            let mut data = sample();
            if extreme {
                data.cpu_basis_points = None;
                data.swap_total_kib = 0;
                data.swap_free_kib = 0;
                data.memory_total_kib = 1 << 50;
                data.load_milli = [u32::MAX; 3];
                data.uptime_seconds = u64::MAX;
            }
            let (mut panel, config) = fixture_panel(data);
            let session = panel.session;
            let target = "生产服务器 · developer@[2001:db8::1]:2222";
            let mut tree = None;
            for _ in 0..4 {
                tree = Some(crate::ui_layout_gate::frame(&ctx, size, |c| {
                    panel.draw(c, Some(&config), session, Some(target), true);
                }));
            }
            let tree = tree.unwrap();
            let window = ctx
                .memory(|m| m.area_rect(egui::Id::new("server-resources-panel")))
                .unwrap();
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size).expand(1.0);
            assert!(
                screen.contains_rect(window),
                "{size:?} at {scale}: {window:?}"
            );
            for label in ["刷新一次", "自动刷新", "关闭"] {
                let node = tree
                    .nodes
                    .iter()
                    .find(|(_, n)| n.label() == Some(label))
                    .unwrap();
                let rect = crate::ui_layout_gate::bounds(&node.1);
                assert!(
                    screen.contains_rect(rect),
                    "{label}: {rect:?} outside {size:?}"
                );
            }
            assert!(
                panel.worker.is_none(),
                "rendering the cached sample must not collect again"
            );
            assert_eq!(panel.refresh_seconds, 0);
            assert_eq!(ctx.style_of(egui::Theme::Dark).text_styles, original);
        }
    }
}

#[test]
fn metric_cards_show_values_units_and_unknown_or_disabled_states() {
    for unavailable in [false, true] {
        let ctx = crate::ui_layout_gate::context();
        let mut data = sample();
        if unavailable {
            data.cpu_basis_points = None;
            data.swap_total_kib = 0;
            data.swap_free_kib = 0;
        }
        let mut tree = None;
        // Draw cards directly into a large viewport so all metrics are visible.
        for _ in 0..3 {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 1600.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    view::reading_style(ui);
                    view::metrics(ui, &data);
                },
            );
            tree = out.platform_output.accesskit_update.take();
            out.textures_delta.clear();
        }
        let text = tree
            .unwrap()
            .nodes
            .iter()
            .map(|(_, n)| {
                format!(
                    "{} {}",
                    n.label().unwrap_or_default(),
                    n.value().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "CPU 使用率",
            "75.0%",
            "6.00 GiB",
            "8.00 GiB",
            "1.25",
            "0.75",
            "0.25",
            "2 天 1 小时 2 分钟",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        for expected in if unavailable {
            ["暂不可用", "未启用"]
        } else {
            ["12.3%", "50.0%"]
        } {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("NaN"));
    }
}

#[test]
fn failed_refresh_marks_the_previous_sample_and_keeps_automatic_refresh_off() {
    let ctx = crate::ui_layout_gate::context();
    let (mut panel, config) = fixture_panel(sample());
    panel.error = Some("采集超时，请手动重试".into());
    let session = panel.session;
    let mut last = None;
    for _ in 0..3 {
        last = Some(crate::ui_layout_gate::frame(
            &ctx,
            egui::vec2(1000.0, 760.0),
            |c| {
                panel.draw(c, Some(&config), session, Some("测试服务器"), true);
            },
        ));
    }
    let text = last
        .unwrap()
        .nodes
        .iter()
        .map(|(_, n)| {
            format!(
                "{} {}",
                n.label().unwrap_or_default(),
                n.value().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("上次成功快照"));
    assert!(text.contains("上次快照"));
    assert!(panel.sample.is_some());
    assert!(panel.worker.is_none());
    assert_eq!(panel.refresh_seconds, 0);
}

#[test]
#[ignore = "explicit fixed offline resource panel preview with production fonts"]
fn render_resource_readability_preview() {
    mod raster {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/cshell-ui/examples/support/raster.rs"
        ));
    }
    for (name, size, failed) in [
        ("resources-preview", egui::vec2(1100.0, 800.0), false),
        ("resources-small", egui::vec2(720.0, 480.0), false),
        ("resources-error", egui::vec2(720.0, 480.0), true),
        ("resources-toolbar", egui::vec2(1100.0, 500.0), false),
    ] {
        let ctx = crate::ui_layout_gate::context();
        ctx.all_styles_mut(|s| s.animation_time = 0.0);
        let (mut panel, config) = fixture_panel(sample());
        if failed {
            panel.error = Some("采集超时，请检查连接后手动刷新。".into());
        }
        let session = panel.session;
        let tab = cshell_domain::TabId::new();
        let mut model = cshell_ui::WorkbenchViewModel {
            can_sftp: true,
            can_disconnect_ssh: true,
            can_restart_ssh: true,
            selected_tab: Some(tab),
            tabs: vec![cshell_ui::WorkspaceTabViewModel {
                id: tab,
                title: "生产服务器".into(),
                session_id: session,
            }],
            reusable_ssh_sessions: vec![session.unwrap()],
            ..Default::default()
        };
        let mut textures = std::collections::HashMap::new();
        let mut primitives = Vec::new();
        for frame in 0..4 {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    time: Some(f64::from(frame)),
                    ..Default::default()
                },
                |ui| {
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(egui::Pos2::ZERO, size),
                        0.0,
                        cshell_ui::theme::BACKGROUND,
                    );
                    cshell_ui::draw_workbench_chrome(ui, &mut model);
                    let _ = cshell_ui::draw_workbench_content(ui, &mut model);
                    if name != "resources-toolbar" {
                        panel.draw(
                            ui.ctx(),
                            Some(&config),
                            session,
                            Some("配置：生产服务器 · developer@server.example:22"),
                            true,
                        );
                    }
                },
            );
            for (id, deltas) in out.textures_delta.set.drain() {
                for delta in deltas {
                    let egui::ImageData::Color(image) = delta.image;
                    if let Some([x, y]) = delta.pos {
                        let target: &mut egui::ColorImage = textures.get_mut(&id).unwrap();
                        for row in 0..image.size[1] {
                            let start = (y + row) * target.size[0] + x;
                            target.pixels[start..start + image.size[0]].copy_from_slice(
                                &image.pixels[row * image.size[0]..(row + 1) * image.size[0]],
                            );
                        }
                    } else {
                        textures.insert(id, (*image).clone());
                    }
                }
            }
            primitives = ctx.tessellate(out.shapes, 1.0);
        }
        raster::write(
            &format!(
                "{}/../../artifacts/ui-refresh-20261015/{name}.ppm",
                env!("CARGO_MANIFEST_DIR")
            ),
            size.x as usize,
            size.y as usize,
            primitives,
            &textures,
        )
        .unwrap();
        assert!(panel.worker.is_none());
    }
}
