//! Offline production-egui preview, with synthetic files and no daemon/network.
use super::*;
use egui::{TextureId, pos2};
use std::{collections::HashMap, io::Write};

#[test]
#[ignore = "writes the fixed SFTP visual review artifact"]
fn render_sftp_preview() -> Result<(), Box<dyn std::error::Error>> {
    let width = 1600usize;
    let height = 900usize;
    let context = context();
    let (mut panel, mut view, _) = fixture();
    view.peer=Some(cshell_domain::ClipboardTrust {host:cshell_domain::ClipboardHost::new("linux-build.internal",22).unwrap(),username:"operator".into(),host_key_sha256:"synthetic-preview".into(),route:"direct".into()});
    view.connection_profile=Some(cshell_domain::ProfileId::new());
    view.connection_phase=if std::env::var_os("CSHELL_SFTP_PREVIEW_DISCONNECTED").is_some() {crate::sftp_connection::ConnectionPhase::Disconnected} else {crate::sftp_connection::ConnectionPhase::Connected};
    if view.connection_phase==crate::sftp_connection::ConnectionPhase::Disconnected {view.status="SFTP 已断开；可点击重新连接".into();}
    panel.remote_tabs=vec![
        crate::sftp_panel::RemoteTab{id:1,title:"Linux 构建机".into(),host:"operator@linux-build.internal:22".into(),phase:crate::sftp_connection::ConnectionPhase::Connected,active:true,running:1},
        crate::sftp_panel::RemoteTab{id:2,title:"生产服务器".into(),host:"deploy@prod.internal:22".into(),phase:crate::sftp_connection::ConnectionPhase::Connected,active:false,running:2},
        crate::sftp_panel::RemoteTab{id:3,title:"测试环境".into(),host:"operator@test.internal:22".into(),phase:crate::sftp_connection::ConnectionPhase::Disconnected,active:false,running:0},
    ];
    for i in 0..12 {
        let name = format!("application-{i:02}.tar.gz");
        view.entries.push(cshell_ipc::SftpDirectoryEntry {
            path: format!("/home/operator/{name}"),
            name: name.clone(),
            kind: 1,
            size: Some(32 * 1024 * 1024),
            modified_unix_seconds: Some(1_792_000_000 - i * 3600),
            permissions: Some(0o100644),
            uid: Some(1000),
            gid: Some(1000),
            ..Default::default()
        });
        view.local_entries.push(cshell_ipc::SftpDirectoryEntry {
            path: format!("C:/Users/operator/{name}"),
            name,
            kind: 1,
            size: Some(32 * 1024 * 1024),
            modified_unix_seconds: Some(1_792_000_000 - i * 3600),
            ..Default::default()
        });
    }
    for local in [true, false] {
        let entry = cshell_ipc::SftpDirectoryEntry {
            name: "releases".into(),
            path: if local {
                "C:/Users/operator/releases"
            } else {
                "/home/operator/releases"
            }
            .into(),
            kind: 2,
            modified_unix_seconds: Some(1_790_000_000),
            permissions: Some(0o40755),
            uid: Some(1000),
            gid: Some(1000),
            ..Default::default()
        };
        if local {
            view.local_entries.push(entry);
        } else {
            view.entries.push(entry);
        }
    }
    let mut queue = (**view.queue.as_ref().unwrap()).clone();
    queue.paused = false;
    for (i, state) in [
        SftpQueueState::Running,
        SftpQueueState::Pending,
        SftpQueueState::Succeeded,
    ]
    .into_iter()
    .enumerate()
    {
        let transfer = SessionId::new();
        queue.items.push(SftpQueueItem {
            id: SftpQueueItemId::new(),
            state,
            transfer_id: Some(transfer),
            receipt: None,
            draft: SftpQueueDraft {
                verified: true,
                direction:if i==1 {SftpQueueDirection::Download}else{SftpQueueDirection::Upload},
                local_path: format!("C:/Users/operator/application-{i:02}.tar.gz"),
                remote_path: format!("/home/operator/application-{i:02}.tar.gz"),
                ..Default::default()
            },
            verified: if state == SftpQueueState::Pending {
                None
            } else {
                Some(SftpVerifiedTransfer {
                    temporary_owned: true,
                    bytes: if state == SftpQueueState::Succeeded { 32 } else { 12 } * 1024 * 1024,
                    content: Some(SftpVerifiedContent {
                        size: 32 * 1024 * 1024,
                        sha256: [1; 32],
                    }),
                    ..Default::default()
                })
            },
        });
    }
    view.queue = Some(std::sync::Arc::new(queue));
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../artifacts/ui-refresh-20261015");
    let output = root.join("sftp.ppm");
    let mut textures = HashMap::<TextureId, egui::ColorImage>::new();
    let mut primitives = Vec::new();
    for frame_index in 0..30 {
        if frame_index == 1 {
            if std::env::var_os("CSHELL_SFTP_PREVIEW_LAYOUT").is_some() {
                panel.layout.value.left_ratio=0.60;
                panel.layout.value.transfer_ratio=0.34;
                panel.layout.value.columns[0]=vec![310.0,100.0,110.0,190.0];
            }
            panel.browser.selection[1].paths = view.entries.iter().filter(|entry| entry.kind == 1 && entry.permissions.is_some()).take(3).map(|entry|entry.path.clone()).collect();
            if std::env::var_os("CSHELL_SFTP_PREVIEW_FILE_OPERATION").is_some() {
                panel.open_file_dialog(&view,false,cshell_application::SftpFileAction::Inspect);
            }
        }
        let mut frame = context.run_ui(
            egui::RawInput {
                time: Some(f64::from(frame_index) / 60.0),
                screen_rect: Some(egui::Rect::from_min_size(
                    pos2(0.0, 0.0),
                    egui::vec2(width as f32, height as f32),
                )),
                events: if frame_index>1 && std::env::var_os("CSHELL_SFTP_PREVIEW_DROP").is_some() {
                    vec![Event::PointerMoved(panel.browser.remote_rect.unwrap().center())]
                }else{vec![]},
                hovered_files:if frame_index>1 && std::env::var_os("CSHELL_SFTP_PREVIEW_DROP").is_some() {
                    vec![egui::HoveredFile {path:Some("C:/preview/application.tar.gz".into()),..Default::default()}]
                }else{vec![]},
                ..Default::default()
            },
            |ui| {
                ui.painter()
                    .rect_filled(ui.max_rect(), 0.0, cshell_ui::theme::BACKGROUND);
                egui::Frame::NONE.inner_margin(10).show(ui, |ui| {
                    panel.draw_contents(ui, view.session_id, Some(&view));
                });
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
        frame.textures_delta.clear();
        primitives = context.tessellate(frame.shapes, 1.0);
    }
    let mut pixels = vec![[255_u8; 3]; width * height];
    for primitive in primitives {
        let egui::epaint::Primitive::Mesh(mesh) = primitive.primitive else {
            continue;
        };
        let texture = textures.get(&mesh.texture_id).ok_or("missing texture")?;
        for indices in mesh.indices.chunks_exact(3) {
            let v = [
                mesh.vertices[indices[0] as usize],
                mesh.vertices[indices[1] as usize],
                mesh.vertices[indices[2] as usize],
            ];
            let edge = |a: egui::Pos2, b: egui::Pos2, p: egui::Pos2| {
                (p.x - a.x) * (b.y - a.y) - (p.y - a.y) * (b.x - a.x)
            };
            let area = edge(v[0].pos, v[1].pos, v[2].pos);
            if area.abs() < 0.0001 {
                continue;
            }
            let min_x = v
                .iter()
                .map(|v| v.pos.x)
                .fold(f32::INFINITY, f32::min)
                .max(primitive.clip_rect.min.x)
                .max(0.0)
                .floor() as usize;
            let min_y = v
                .iter()
                .map(|v| v.pos.y)
                .fold(f32::INFINITY, f32::min)
                .max(primitive.clip_rect.min.y)
                .max(0.0)
                .floor() as usize;
            let max_x = v
                .iter()
                .map(|v| v.pos.x)
                .fold(0.0, f32::max)
                .min(primitive.clip_rect.max.x)
                .min(width as f32)
                .ceil() as usize;
            let max_y = v
                .iter()
                .map(|v| v.pos.y)
                .fold(0.0, f32::max)
                .min(primitive.clip_rect.max.y)
                .min(height as f32)
                .ceil() as usize;
            for y in min_y..max_y {
                for x in min_x..max_x {
                    let p = pos2(x as f32 + 0.5, y as f32 + 0.5);
                    let w = [
                        edge(v[1].pos, v[2].pos, p) / area,
                        edge(v[2].pos, v[0].pos, p) / area,
                        edge(v[0].pos, v[1].pos, p) / area,
                    ];
                    if w.iter().any(|w| *w < 0.0) {
                        continue;
                    }
                    let u = (0..3).map(|i| w[i] * v[i].uv.x).sum::<f32>() * texture.size[0] as f32
                        - 0.5;
                    let t = (0..3).map(|i| w[i] * v[i].uv.y).sum::<f32>() * texture.size[1] as f32
                        - 0.5;
                    let mut sample = [0.0; 4];
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let tx = (u.floor() as isize + dx)
                                .clamp(0, texture.size[0] as isize - 1)
                                as usize;
                            let ty = (t.floor() as isize + dy)
                                .clamp(0, texture.size[1] as isize - 1)
                                as usize;
                            let weight = if dx == 0 { 1.0 - u.fract() } else { u.fract() }
                                * if dy == 0 { 1.0 - t.fract() } else { t.fract() };
                            for (channel, value) in texture.pixels[ty * texture.size[0] + tx]
                                .to_array()
                                .into_iter()
                                .enumerate()
                            {
                                sample[channel] += weight * f32::from(value) / 255.0;
                            }
                        }
                    }
                    let color: [f32; 4] = std::array::from_fn(|c| {
                        (0..3)
                            .map(|i| w[i] * f32::from(v[i].color[c]) / 255.0)
                            .sum::<f32>()
                            * sample[c]
                    });
                    let dst = &mut pixels[y * width + x];
                    for c in 0..3 {
                        dst[c] = (color[c] * 255.0 + f32::from(dst[c]) * (1.0 - color[3]))
                            .clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    }
    let mut file = std::io::BufWriter::new(std::fs::File::create(output)?);
    write!(file, "P6\n{width} {height}\n255\n")?;
    for pixel in pixels {
        file.write_all(&pixel)?;
    }
    Ok(())
}
