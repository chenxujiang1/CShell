//! Local reading styles and metric cards; sampling stays in the parent module.
use super::{ServerResources, memory};
use cshell_ui::theme;
use egui::{FontId, RichText, TextStyle, Ui};

pub(super) fn reading_style(ui: &mut Ui) {
    let style = ui.style_mut();
    for text in [TextStyle::Body, TextStyle::Button] {
        style.text_styles.insert(text, FontId::proportional(15.0));
    }
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(14.0));
    style.visuals.override_text_color = Some(theme::TEXT);
    style.visuals.weak_text_color = Some(theme::MUTED);
    style.spacing.item_spacing = egui::vec2(12.0, 10.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 32.0;
    style.spacing.scroll = egui::style::ScrollStyle::solid();
    style.wrap_mode = Some(egui::TextWrapMode::Wrap);
}

fn card(ui: &mut Ui, title: &str, content: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(theme::BACKGROUND)
        .stroke(egui::Stroke::new(1.0, theme::LINE))
        .corner_radius(6.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(title).size(15.0).color(theme::MUTED));
            content(ui);
        });
}

fn value(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text).size(28.0).strong().color(theme::TEXT));
}

fn bar(ui: &mut Ui, fraction: f32) {
    ui.add(
        egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
            .fill(theme::ACCENT)
            .desired_width(ui.available_width())
            .desired_height(8.0),
    );
}

fn cpu(ui: &mut Ui, sample: &ServerResources) {
    card(ui, "CPU 使用率", |ui| {
        if let Some(points) = sample.cpu_basis_points {
            value(ui, format!("{:.1}%", f32::from(points) / 100.0));
            bar(ui, f32::from(points) / 10_000.0);
            ui.weak("本次采样区间的使用率").on_hover_text(
                "由两次累计计数计算，不重复计入 guest，不将 I/O 等待计作 CPU 使用。",
            );
        } else {
            value(ui, "暂不可用");
            ui.weak("本次无法计算，可刷新重试。");
        }
    });
}

fn memory_card(ui: &mut Ui, sample: &ServerResources) {
    card(ui, "内存", |ui| {
        let used = sample
            .memory_total_kib
            .saturating_sub(sample.memory_available_kib);
        let fraction = used as f64 / sample.memory_total_kib.max(1) as f64;
        value(ui, format!("{:.1}%", fraction * 100.0));
        bar(ui, fraction as f32);
        ui.label(format!(
            "已用 {} / 总量 {}",
            memory(used),
            memory(sample.memory_total_kib)
        ))
        .on_hover_text("已用内存 = 总量 − 可用内存（MemAvailable）。");
    });
}

fn swap(ui: &mut Ui, sample: &ServerResources) {
    card(ui, "Swap", |ui| {
        if sample.swap_total_kib == 0 {
            value(ui, "未启用");
            ui.weak("服务器当前没有可用的 Swap。");
        } else {
            let used = sample.swap_total_kib.saturating_sub(sample.swap_free_kib);
            let fraction = used as f64 / sample.swap_total_kib as f64;
            value(ui, format!("{:.1}%", fraction * 100.0));
            bar(ui, fraction as f32);
            ui.label(format!(
                "已用 {} / 总量 {}",
                memory(used),
                memory(sample.swap_total_kib)
            ));
        }
    });
}

fn load(ui: &mut Ui, sample: &ServerResources) {
    card(ui, "平均负载", |ui| {
        for (minutes, milli) in [1, 5, 15].into_iter().zip(sample.load_milli) {
            ui.horizontal(|ui| {
                ui.label(format!("{minutes} 分钟"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{:.2}", f64::from(milli) / 1000.0))
                            .size(20.0)
                            .monospace()
                            .strong(),
                    );
                });
            });
        }
    });
}

pub(super) fn metrics(ui: &mut Ui, sample: &ServerResources) {
    if ui.available_width() >= 560.0 {
        ui.columns(2, |columns| {
            cpu(&mut columns[0], sample);
            memory_card(&mut columns[1], sample);
        });
        ui.columns(2, |columns| {
            swap(&mut columns[0], sample);
            load(&mut columns[1], sample);
        });
    } else {
        cpu(ui, sample);
        memory_card(ui, sample);
        swap(ui, sample);
        load(ui, sample);
    }
    card(ui, "系统运行时间", |ui| {
        ui.label(
            RichText::new(format!(
                "{} 天 {} 小时 {} 分钟",
                sample.uptime_seconds / 86400,
                sample.uptime_seconds / 3600 % 24,
                sample.uptime_seconds / 60 % 60
            ))
            .size(18.0),
        );
    });
    ui.weak("平均负载不是 CPU 百分比；容器或受限环境的统计范围以服务器提供的数据为准。");
}
