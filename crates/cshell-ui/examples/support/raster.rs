use egui::{TextureId, pos2};
use std::{collections::HashMap, io::Write};
pub fn write(
    output: &str,
    width: usize,
    height: usize,
    primitives: Vec<egui::ClippedPrimitive>,
    textures: &HashMap<TextureId, egui::ColorImage>,
) -> Result<(), Box<dyn std::error::Error>> {
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
