//! Render the production chrome without a window, daemon, network or user data.
//! `cargo run -p cshell-ui --example workbench_preview -- preview.ppm [width] [height]`
//! The terminal body is explicitly illustrative; this is not a GPU-terminal capture.
use cshell_domain::{ProfileFolder, ProfileId, ProfileKind, SessionId, TabId};
use cshell_ui::{SavedSshProfileViewModel, WorkbenchViewModel, WorkspaceTabViewModel};
#[path = "support/raster.rs"]
mod raster;
use egui::{Color32, FontId, TextureId, pos2, vec2};
use std::collections::HashMap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let output = args.get(1).ok_or("supply an output .ppm path")?;
    let width: usize = args.get(2).map_or(Ok(1280), |s| s.parse())?;
    let height: usize = args.get(3).map_or(Ok(800), |s| s.parse())?;
    let scale:f32=args.get(5).map_or(Ok(1.0),|s|s.parse())?;
    if !(1.0..=2.0).contains(&scale){return Err("scale must be 1 through 2".into());}
    let context = egui::Context::default();
    context.set_pixels_per_point(scale);
    cshell_ui::install_workbench_style(&context);
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../../apps/desktop/assets/fonts/NotoSansSC-Regular.ttf"
        ))),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let chain=fonts.families.entry(family).or_default();chain.insert(1.min(chain.len()),"cjk".into());
    }
    context.set_fonts(fonts);
    let folder = cshell_domain::FolderId::new();
    let mut model = WorkbenchViewModel {
        daemon_connected: true,
        can_sftp: true,
        ..Default::default()
    };
    model.profile_folders.push(ProfileFolder {
        id: folder,
        name: "开发环境".into(),
        parent_id: None,
        terminal: Default::default(),
    });
    for (i, (name, target)) in [
        ("应用服务器", "deploy@dev-app.example:22"),
        ("数据库", "admin@dev-db.example:22"),
        ("预发布环境", "deploy@staging.example:22"),
    ]
    .into_iter()
    .enumerate()
    {
        model.saved_ssh_profiles.push(SavedSshProfileViewModel {
            id: ProfileId::new(),
            name: name.into(),
            target: target.into(),
            favorite: i == 2,
            folder_id: if i < 2 { Some(folder) } else { None },
            kind: ProfileKind::Ssh,
        });
    }
    model.selected_profile = Some(model.saved_ssh_profiles[0].id);
    for title in ["应用服务器", "数据库", "PowerShell"] {
        model.tabs.push(WorkspaceTabViewModel {
            id: TabId::new(),
            title: title.into(),
            session_id: Some(SessionId::new()),
        });
    }
    model.selected_tab = Some(model.tabs[0].id);
    model.about_open = args.get(4).is_some_and(|value| value == "about");
    if args.get(4).is_some_and(|value| value == "narrow-sidebar") {
        model.sidebar_width = Some(160.0);
        model.profile_folders[0].name = "开发环境 / 长分组名称测试".into();
        model.saved_ssh_profiles[0].name = "应用服务器 / Production SSH".into();
    }
    let show_tabs=args.get(4).is_some_and(|value|value=="tab-search");
    if show_tabs {
        for index in 3..20 {model.tabs.push(WorkspaceTabViewModel{id:TabId::new(),title:format!("维护服务器 {index:02}"),session_id:Some(SessionId::new())});}
    }
    let mut document = cshell_domain::WorkspaceDocument::default();
    for (index, tab) in model.tabs.iter().enumerate() {
        document.tab_groups[0].tabs.push(tab.id);
        document.bindings.push(cshell_domain::WorkspaceBinding {
            tab_id: tab.id,
            profile_id: if index != 2 {Some(model.saved_ssh_profiles[index % model.saved_ssh_profiles.len()].id)} else {None},
            title: tab.title.clone(),
        });
        if index != 2 {model.ssh_tab_states.insert(tab.id,cshell_ui::SshConnectionState::Connected);}
    }
    if args.get(4).is_some_and(|value|value=="environment-split") {
        use cshell_domain::*;
        model.profile_folders[0].name="服务器".into();
        model.saved_ssh_profiles[0].target="deploy@prod-app.example:22".into();
        model.saved_ssh_profiles[1].target="admin@staging-db.example:22".into();
        model.profile_environments.insert(model.saved_ssh_profiles[0].id,cshell_ui::environment::Environment::Production);
        model.profile_environments.insert(model.saved_ssh_profiles[1].id,cshell_ui::environment::Environment::Staging);
        let original=document.tab_groups[0].id;let other=TabGroupId::new();let first=PaneId::new();let second=PaneId::new();
        document.panes[0].content=WorkspacePaneContent::Split{axis:SplitAxis::Horizontal,ratio_permille:500,first,second};
        document.panes.extend([WorkspacePane{id:first,content:WorkspacePaneContent::Tabs{group_id:original}},WorkspacePane{id:second,content:WorkspacePaneContent::Tabs{group_id:other}}]);
        document.tab_groups[0].tabs=vec![model.tabs[0].id,model.tabs[2].id];
        document.tab_groups.push(WorkspaceTabGroup{id:other,tabs:vec![model.tabs[1].id],active_tab:Some(model.tabs[1].id)});
    }
    if args.get(4).is_some_and(|value|value=="focus") {model.sidebar_hidden=true;model.toolbar_hidden=true;}
    document.tab_groups[0].active_tab = model.selected_tab;
    model.workspace_window_id = Some(document.windows[0].id);
    model.workspace_document = Some(document);
    if show_tabs {model.tab_navigator.show(model.selected_tab);}
    model.can_disconnect_ssh = true;
    model.can_restart_ssh = true;
    model.reusable_ssh_sessions = model.tabs.iter().filter_map(|t|t.session_id).collect();
    if args.get(4).is_some_and(|value| value == "disconnected") {
        model.daemon_connected = false;
        model.can_sftp = false;
        model.can_restart_ssh = true;
        model.can_disconnect_ssh = false;
        model.reusable_ssh_sessions.clear();
        model.tabs[0].session_id = None;
        model.ssh_tab_states.insert(model.tabs[0].id, cshell_ui::SshConnectionState::Disconnected);
    }
    let mut textures = HashMap::<TextureId, egui::ColorImage>::new();
    let mut primitives = Vec::new();
    // Let popup fade-in and layout sizing settle before capturing the last frame.
    for frame_index in 0..30 {
        let mut frame = context.run_ui(
            egui::RawInput {
                time: Some(f64::from(frame_index) / 60.0),
                screen_rect: Some(egui::Rect::from_min_size(
                    pos2(0.0, 0.0),
                    vec2(width as f32, height as f32),
                )),
                ..Default::default()
            },
            |ui| {
                cshell_ui::draw_workbench_chrome(ui, &mut model);
                egui::Panel::bottom("preview-send")
                    .exact_size(30.0)
                    .frame(cshell_ui::form::utility_bar())
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let _ = ui.button("同步输入");
                            let _ = ui.selectable_label(false,"查找");
                            let _ = ui.selectable_label(false,"发送");
                            let _ = ui.selectable_label(false,"快捷命令");
                            let mut secure = false;
                            ui.checkbox(&mut secure, "安全输入");
                        });
                    });
                let rect = cshell_ui::draw_workbench_content(ui, &mut model);
                for pane in &model.pane_views {
                    ui.painter().with_clip_rect(pane.rect).rect_filled(pane.rect,0.0,Color32::from_rgb(15,20,29));
                    if pane.tab_id!=model.selected_tab {ui.painter().with_clip_rect(pane.rect).text(pane.rect.min+vec2(18.0,18.0),egui::Align2::LEFT_TOP,"示例会话 · 未连接服务器",FontId::proportional(13.0),Color32::from_rgb(140,158,184));}
                }
                let painter = ui.painter().with_clip_rect(rect);
                painter.rect_filled(rect, 0.0, Color32::from_rgb(15, 20, 29));
                for (i, (text, color)) in [
                    (
                        "CShell  /  工作台界面预览",
                        Color32::from_rgb(140, 158, 184),
                    ),
                    (
                        "示例会话与终端内容 · 未连接服务器",
                        Color32::from_rgb(106, 125, 151),
                    ),
                    ("", Color32::WHITE),
                    (
                        "deploy@dev-app:~$ systemctl status application",
                        Color32::from_rgb(114, 205, 165),
                    ),
                    (
                        "  application.service - Application server",
                        Color32::from_rgb(219, 226, 237),
                    ),
                    (
                        "  Loaded: loaded (/etc/systemd/system/application.service)",
                        Color32::from_rgb(167, 182, 204),
                    ),
                    (
                        "  Active: active (running)",
                        Color32::from_rgb(114, 205, 165),
                    ),
                    ("", Color32::WHITE),
                    ("deploy@dev-app:~$ ", Color32::from_rgb(114, 205, 165)),
                ]
                .into_iter()
                .enumerate()
                {
                    painter.text(
                        rect.min + vec2(24.0, 18.0 + i as f32 * 36.0),
                        egui::Align2::LEFT_TOP,
                        text,
                        if i < 2 {
                            FontId::proportional(13.0)
                        } else {
                            FontId::monospace(cshell_domain::DEFAULT_TERMINAL_FONT_PIXELS as f32)
                        },
                        color,
                    );
                }
            },
        );
        for (id, deltas) in frame.textures_delta.set.drain() {
            for delta in deltas {
                let egui::ImageData::Color(image) = delta.image;
                if let Some([x, y]) = delta.pos {
                    let target = textures
                        .get_mut(&id)
                        .ok_or("texture patch without texture")?;
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
        primitives = context.tessellate(frame.shapes, scale);
    }
    for primitive in &mut primitives {
        primitive.clip_rect=egui::Rect::from_min_max(pos2(primitive.clip_rect.min.x*scale,primitive.clip_rect.min.y*scale),pos2(primitive.clip_rect.max.x*scale,primitive.clip_rect.max.y*scale));
        if let egui::epaint::Primitive::Mesh(mesh)=&mut primitive.primitive {for vertex in &mut mesh.vertices {vertex.pos=pos2(vertex.pos.x*scale,vertex.pos.y*scale);}}
    }
    raster::write(output,(width as f32*scale).round() as usize,(height as f32*scale).round() as usize,primitives,&textures)
}
