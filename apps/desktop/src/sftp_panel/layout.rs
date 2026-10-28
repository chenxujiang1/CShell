use crate::sftp_settings::Layout;
#[derive(Default)]
pub(super) struct LayoutState {
    pub value: Layout,
    pub dirty: bool,
    column_drag: Option<(usize, usize, f32, f32)>,
    pub split_rect: Option<egui::Rect>,
    pub transfer_rect: Option<egui::Rect>,
    #[cfg(test)]
    pub column_handles: Vec<(bool, usize, egui::Rect)>,
}
impl LayoutState {
    pub fn load() -> Self {
        #[cfg(test)]
        {
            Self::default()
        }
        #[cfg(not(test))]
        {
            Self {
                value: crate::sftp_settings::layout(),
                ..Default::default()
            }
        }
    }
    pub fn pane_rects(&self, rect: egui::Rect) -> [egui::Rect; 3] {
        let space = (rect.width() - 8.0).max(0.0);
        let min = 300.0_f32.min(space / 2.0);
        let left = (space * self.value.left_ratio).clamp(min, (space - min).max(min));
        let x = rect.left() + left;
        [
            egui::Rect::from_min_max(rect.min, egui::pos2(x, rect.bottom())),
            egui::Rect::from_min_max(egui::pos2(x + 8.0, rect.top()), rect.max),
            egui::Rect::from_min_max(
                egui::pos2(x, rect.top()),
                egui::pos2(x + 8.0, rect.bottom()),
            ),
        ]
    }
    pub fn transfer_height(&self, available: f32) -> f32 {
        (available * self.value.transfer_ratio).clamp(
            140.0_f32.min(available),
            (available - 260.0).max(140.0).min(available),
        )
    }
    pub fn split_handle(&mut self, ui: &egui::Ui, whole: egui::Rect, handle: egui::Rect) {
        self.split_rect = Some(handle);
        let response = handle_response(ui, handle, "sftp-pane-resize", true);
        if (response.dragged() || response.drag_stopped())
            && let Some(pos) = response.interact_pointer_pos()
        {
            let space = (whole.width() - 8.0).max(1.0);
            let min = 300.0_f32.min(space / 2.0);
            let width = (pos.x - whole.left() - 4.0).clamp(min, (space - min).max(min));
            self.value.left_ratio = (width / space).clamp(0.15, 0.85);
            self.dirty = true;
        }
    }
    pub fn transfer_handle(&mut self, ui: &egui::Ui, whole: egui::Rect, y: f32) {
        let rect = egui::Rect::from_min_max(
            egui::pos2(whole.left(), y - 4.0),
            egui::pos2(whole.right(), y + 4.0),
        );
        self.transfer_rect = Some(rect);
        let response = handle_response(ui, rect, "sftp-transfer-resize", false);
        if (response.dragged() || response.drag_stopped())
            && let Some(pos) = response.interact_pointer_pos()
        {
            let height = (whole.bottom() - pos.y).clamp(140.0, (whole.height() - 260.0).max(140.0));
            self.value.transfer_ratio = (height / whole.height().max(1.0)).clamp(0.15, 0.65);
            self.dirty = true;
        }
    }
    pub fn column_handle(
        &mut self,
        ui: &egui::Ui,
        local: bool,
        index: usize,
        header: egui::Rect,
        widths: &mut [f32],
    ) {
        let edge = if index + 1 == widths.len() {
            header.right() - 8.0
        } else {
            header.right()
        };
        let rect = egui::Rect::from_min_max(
            egui::pos2(edge, header.top()),
            egui::pos2(edge + 8.0, header.bottom()),
        );
        #[cfg(test)]
        self.column_handles
            .push((local, index, rect.intersect(ui.clip_rect())));
        let response = handle_response(ui, rect, ("sftp-column-resize", local, index), true);
        let side = usize::from(!local);
        if response.drag_started() {
            let origin = ui
                .input(|input| input.pointer.press_origin())
                .or_else(|| response.interact_pointer_pos());
            if let Some(pos) = origin {
                self.column_drag = Some((side, index, pos.x, widths[index]));
            }
        }
        if (response.dragged() || response.drag_stopped())
            && let Some((drag_side, drag_index, origin, width)) = self.column_drag
            && side == drag_side
            && index == drag_index
            && let Some(pos) = response.interact_pointer_pos()
        {
            widths[index] =
                (width + pos.x - origin).clamp(if index == 0 { 80.0 } else { 48.0 }, 1500.0);
            self.value.columns[side] = widths.to_vec();
            self.dirty = true;
        }
        if response.drag_stopped() {
            self.column_drag = None;
        }
    }
    pub fn flush(&mut self, ctx: &egui::Context) -> Option<String> {
        if self.dirty && !ctx.input(|input| input.pointer.any_down()) {
            self.dirty = false;
            // Headless UI tests must not alter the operator's preferences.
            #[cfg(not(test))]
            {
                return crate::sftp_settings::save_layout(&self.value).err();
            }
        }
        None
    }
}
fn handle_response(
    ui: &egui::Ui,
    rect: egui::Rect,
    id: impl std::hash::Hash + std::fmt::Debug,
    vertical: bool,
) -> egui::Response {
    let response = ui
        .interact(rect, ui.id().with(id), egui::Sense::click_and_drag())
        .on_hover_cursor(if vertical {
            egui::CursorIcon::ResizeHorizontal
        } else {
            egui::CursorIcon::ResizeVertical
        });
    let color = if response.hovered() || response.dragged() {
        cshell_ui::theme::ACCENT
    } else {
        cshell_ui::theme::LINE
    };
    let points = if vertical {
        [rect.center_top(), rect.center_bottom()]
    } else {
        [rect.left_center(), rect.right_center()]
    };
    ui.painter()
        .line_segment(points, egui::Stroke::new(1.0, color));
    response
}

#[cfg(not(test))]
impl Drop for LayoutState {
    fn drop(&mut self) {
        if self.dirty
            && let Err(error) = crate::sftp_settings::save_layout(&self.value)
        {
            tracing::warn!(%error,"cannot save final SFTP layout");
        }
    }
}
