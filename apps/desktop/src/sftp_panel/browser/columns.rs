use cshell_ipc::SftpDirectoryEntry;
use std::cmp::Ordering;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Column {
    Name,
    Size,
    Kind,
    Modified,
    Permissions,
    Owner,
}

impl Column {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "名称",
            Self::Size => "大小",
            Self::Kind => "类型",
            Self::Modified => "修改时间",
            Self::Permissions => "属性",
            Self::Owner => "所有者",
        }
    }
    pub fn width(self) -> f32 {
        match self {
            Self::Name => 180.0,
            Self::Size => 80.0,
            Self::Kind => 90.0,
            Self::Modified => 160.0,
            Self::Permissions => 112.0,
            Self::Owner => 76.0,
        }
    }
    pub fn value(self, entry: &SftpDirectoryEntry) -> String {
        match self {
            Self::Name => entry.name.clone(),
            Self::Size => {
                if entry.kind == 2 {
                    "—".into()
                } else {
                    entry.size.map(super::bytes).unwrap_or_else(|| "—".into())
                }
            }
            Self::Kind => kind(entry),
            Self::Modified => modified(entry.modified_unix_seconds),
            Self::Permissions => permissions(entry),
            Self::Owner => entry
                .uid
                .map(|uid| uid.to_string())
                .unwrap_or_else(|| "—".into()),
        }
    }
}

pub(super) fn columns(local: bool) -> &'static [Column] {
    if local {
        &[Column::Name, Column::Size, Column::Kind, Column::Modified]
    } else {
        &[
            Column::Name,
            Column::Size,
            Column::Kind,
            Column::Modified,
            Column::Permissions,
            Column::Owner,
        ]
    }
}

pub(super) fn widths(local: bool, available: f32) -> Vec<f32> {
    let mut widths: Vec<_> = columns(local).iter().map(|column| column.width()).collect();
    let minimum = widths.iter().sum::<f32>() + 8.0 * (widths.len() - 1) as f32;
    widths[0] += (available - minimum).max(0.0);
    widths
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SortState {
    pub column: Column,
    pub ascending: bool,
    pub folders_first: bool,
}
impl SortState {
    pub fn initial(local: bool) -> Self {
        Self {
            column: if local {
                Column::Modified
            } else {
                Column::Name
            },
            ascending: !local,
            folders_first: !local,
        }
    }
    pub fn select(&mut self, column: Column) {
        if self.column == column {
            self.ascending = !self.ascending;
        } else {
            self.column = column;
            self.ascending = true;
        }
    }
    pub fn header(self, column: Column) -> String {
        if self.column == column {
            format!(
                "{} {}",
                column.label(),
                if self.ascending { "▲" } else { "▼" }
            )
        } else {
            column.label().into()
        }
    }
    pub fn sorted<'a>(self, entries: &'a [SftpDirectoryEntry]) -> Vec<&'a SftpDirectoryEntry> {
        let mut entries: Vec<_> = entries.iter().collect();
        entries.sort_by(|a, b| self.compare(a, b));
        entries
    }
    fn compare(self, a: &SftpDirectoryEntry, b: &SftpDirectoryEntry) -> Ordering {
        let grouping = if self.folders_first {
            (a.kind != 2).cmp(&(b.kind != 2))
        } else {
            Ordering::Equal
        };
        if grouping != Ordering::Equal {
            return grouping;
        }
        let key = match self.column {
            Column::Name => direction(
                a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                self.ascending,
            ),
            Column::Kind => direction(kind(a).cmp(&kind(b)), self.ascending),
            Column::Size => optional(
                if a.kind == 2 { None } else { a.size },
                if b.kind == 2 { None } else { b.size },
                self.ascending,
            ),
            Column::Modified => optional(
                a.modified_unix_seconds,
                b.modified_unix_seconds,
                self.ascending,
            ),
            Column::Permissions => optional(
                a.permissions.map(|mode| mode & 0o7777),
                b.permissions.map(|mode| mode & 0o7777),
                self.ascending,
            ),
            Column::Owner => optional(a.uid, b.uid, self.ascending),
        };
        key.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    }
}
fn direction(value: Ordering, ascending: bool) -> Ordering {
    if ascending { value } else { value.reverse() }
}
fn optional<T: Ord>(a: Option<T>, b: Option<T>, ascending: bool) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => direction(a.cmp(&b), ascending),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}
fn kind(entry: &SftpDirectoryEntry) -> String {
    match entry.kind {
        2 => "文件夹".into(),
        3 => "符号链接".into(),
        4 => "其他".into(),
        _ => entry
            .name
            .rsplit_once('.')
            .filter(|(base, extension)| !base.is_empty() && !extension.is_empty())
            .map(|(_, extension)| format!("{} 文件", extension.to_uppercase()))
            .unwrap_or_else(|| "文件".into()),
    }
}
fn permissions(entry: &SftpDirectoryEntry) -> String {
    let Some(mode) = entry.permissions else {
        return "—".into();
    };
    let mut text = String::with_capacity(10);
    text.push(match entry.kind {
        2 => 'd',
        3 => 'l',
        1 => '-',
        _ => '?',
    });
    for shift in [6, 3, 0] {
        text.push(if mode & (4 << shift) != 0 { 'r' } else { '-' });
        text.push(if mode & (2 << shift) != 0 { 'w' } else { '-' });
        let execute = mode & (1 << shift) != 0;
        let special =
            mode & match shift {
                6 => 0o4000,
                3 => 0o2000,
                _ => 0o1000,
            } != 0;
        text.push(match (special, execute, shift) {
            (true, true, 0) => 't',
            (true, false, 0) => 'T',
            (true, true, _) => 's',
            (true, false, _) => 'S',
            (false, true, _) => 'x',
            _ => '-',
        });
    }
    text
}
pub(super) fn modified(seconds: Option<u32>) -> String {
    let Some(parts) = seconds.and_then(local_components) else {
        return "—".into();
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]
    )
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn local_components(seconds: u32) -> Option<[i32; 6]> {
    use windows_sys::Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
    };
    let ticks = (u64::from(seconds) + 11_644_473_600) * 10_000_000;
    let file = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: All structures are initialized and live; a null timezone uses the OS timezone.
    if unsafe {
        FileTimeToSystemTime(&file, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
    } {
        return None;
    }
    Some([
        local.wYear.into(),
        local.wMonth.into(),
        local.wDay.into(),
        local.wHour.into(),
        local.wMinute.into(),
        local.wSecond.into(),
    ])
}
#[cfg(unix)]
#[allow(unsafe_code)]
fn local_components(seconds: u32) -> Option<[i32; 6]> {
    let timestamp = libc::time_t::try_from(seconds).ok()?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: timestamp and output storage are live; successful localtime_r initializes the output.
    let local = unsafe {
        if libc::localtime_r(&timestamp, local.as_mut_ptr()).is_null() {
            return None;
        }
        local.assume_init()
    };
    Some([
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec,
    ])
}

/// Match assistive hit targets to the visible portion of a horizontally scrolled cell.
pub(super) fn clip_accessibility(ui: &egui::Ui, response: &egui::Response) {
    let clip = ui.clip_rect();
    let visible = response.rect.intersect(clip);
    let hidden = !visible.is_positive();
    let visible = if hidden {
        egui::Rect::from_min_size(clip.min, egui::Vec2::ZERO)
    } else {
        visible
    };
    ui.ctx().accesskit_node_builder(response.id, |node| {
        if hidden {
            node.set_hidden();
        } else {
            node.clear_hidden();
        }
        node.set_bounds(egui::accesskit::Rect {
            x0: visible.min.x.into(),
            y0: visible.min.y.into(),
            x1: visible.max.x.into(),
            y1: visible.max.y.into(),
        });
    });
}

pub(super) fn row(
    ui: &mut egui::Ui,
    entry: &SftpDirectoryEntry,
    local: bool,
    widths: &[f32],
    selected: bool,
) -> egui::Response {
    let background = ui.painter().add(egui::Shape::Noop);
    let row = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let name = ui
                .allocate_ui_with_layout(
                    egui::vec2(widths[0], 24.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        super::file_icon(ui, entry.kind == 2);
                        ui.add_sized(
                            [widths[0] - 20.0, 24.0],
                            egui::Button::new(&entry.name)
                                .sense(egui::Sense::click_and_drag())
                                .right_text(())
                                .selected(selected)
                                .frame(false)
                                .truncate(),
                        )
                    },
                )
                .inner;
            clip_accessibility(ui, &name);
            let mut response = name;
            for (&column, &width) in columns(local).iter().zip(widths).skip(1) {
                let value = column.value(entry);
                let cell = ui.add_sized(
                    [width, 24.0],
                    egui::Label::new(&value)
                        .halign(if column == Column::Size {
                            egui::Align::Max
                        } else {
                            egui::Align::Min
                        })
                        .truncate()
                        .sense(egui::Sense::click_and_drag()),
                );
                clip_accessibility(ui, &cell);
                let cell = if column == Column::Owner {
                    cell.on_hover_text(format!(
                        "UID：{}\nGID：{}",
                        value,
                        entry
                            .gid
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "—".into())
                    ))
                } else if column == Column::Permissions {
                    cell.on_hover_text(format!(
                        "{} · {}",
                        value,
                        entry
                            .permissions
                            .map(|mode| format!("{:04o}", mode & 0o7777))
                            .unwrap_or_else(|| "—".into())
                    ))
                } else {
                    cell.on_hover_text(&value)
                };
                response = response.union(cell);
            }
            response
        })
        .inner;
    if selected {
        ui.painter().set(
            background,
            egui::Shape::rect_filled(row.rect, 2.0, cshell_ui::theme::ACCENT.gamma_multiply(0.12)),
        );
    }
    row
}
