use super::*;
use crate::sftp_connection::BatchAction;
use cshell_application::{SftpQueueItem, SftpQueueSnapshot};
use std::{collections::BTreeMap, time::Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Filter {
    #[default]
    All,
    Active,
    Waiting,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
    Review,
}
impl Filter {
    const ALL: [Self; 8] = [
        Self::All,
        Self::Active,
        Self::Waiting,
        Self::Paused,
        Self::Succeeded,
        Self::Failed,
        Self::Cancelled,
        Self::Review,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Active => "传输中",
            Self::Waiting => "等待中",
            Self::Paused => "已暂停",
            Self::Succeeded => "已完成",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
            Self::Review => "需核对",
        }
    }
}
#[derive(Default)]
pub(super) struct TableState {
    filter: Filter,
    selected: BTreeSet<SftpQueueItemId>,
    rates: BTreeMap<SftpQueueItemId, Sample>,
}
struct Sample {
    transfer: Option<SessionId>,
    bytes: u64,
    at: Instant,
    progress_at: Instant,
    rate: Option<f64>,
    active: bool,
}
impl Sample {
    fn update(&mut self, transfer: Option<SessionId>, done: u64, active: bool, now: Instant) {
        if self.transfer != transfer || done < self.bytes || !active || !self.active {
            *self = Self {
                transfer,
                bytes: done,
                at: now,
                progress_at: now,
                rate: None,
                active,
            };
            return;
        }
        let elapsed = now.duration_since(self.at).as_secs_f64();
        if elapsed >= 0.5 {
            if done > self.bytes {
                let rate = (done - self.bytes) as f64 / elapsed;
                self.rate = Some(self.rate.map_or(rate, |old| old * 0.65 + rate * 0.35));
                self.progress_at = now;
            } else if now.duration_since(self.progress_at).as_secs_f64() >= 3.0 {
                self.rate = None;
            }
            self.bytes = done;
            self.at = now;
        }
    }
}
fn transferred(item: &SftpQueueItem, view: &SftpView) -> (u64, Option<u64>) {
    let transfer = item
        .transfer_id
        .and_then(|id| view.queue_transfers.get(&id));
    let done = if item.state == SftpQueueState::Running {
        transfer.map(|t| t.bytes_transferred)
    } else {
        None
    }
    .or_else(|| item.verified.as_ref().map(|v| v.bytes))
    .or_else(|| item.receipt.as_ref().map(|r| r.bytes))
    .unwrap_or(0);
    let total = transfer
        .and_then(|t| t.total_bytes)
        .or_else(|| {
            item.verified
                .as_ref()
                .and_then(|v| v.content.map(|c| c.size))
        })
        .or_else(|| item.receipt.as_ref().and_then(|r| r.total));
    (done, total)
}
fn category(item: &SftpQueueItem, view: &SftpView) -> Filter {
    if view
        .queue
        .as_ref()
        .is_some_and(|q| q.retry_items.contains(&item.id))
    {
        return Filter::Waiting;
    }
    match item.state {
        SftpQueueState::Pending => Filter::Waiting,
        SftpQueueState::Running => {
            if item
                .transfer_id
                .and_then(|id| view.queue_transfers.get(&id))
                .is_some_and(|t| t.paused)
            {
                Filter::Paused
            } else {
                Filter::Active
            }
        }
        SftpQueueState::Succeeded => Filter::Succeeded,
        SftpQueueState::Cancelled => Filter::Cancelled,
        SftpQueueState::CleanupRequired => Filter::Review,
        SftpQueueState::Unknown => {
            // A cancelled partial transfer keeps its recoverable journal. Uncertain
            // creation/publication must still be reviewed even after cancellation.
            if !item.can_retry_verified() {
                Filter::Review
            } else if view.stopping_items.contains(&item.id)
                || item
                    .transfer_id
                    .and_then(|id| view.queue_transfers.get(&id))
                    .is_some_and(|t| t.state == SftpTransferState::Cancelled as i32)
            {
                Filter::Cancelled
            } else {
                Filter::Failed
            }
        }
    }
}
fn status(item: &SftpQueueItem, view: &SftpView) -> &'static str {
    if item.state==SftpQueueState::Succeeded {
        return if item.draft.verification==Some(cshell_domain::SftpVerification::Standard) {"已传输"}else{"已校验"};
    }
    if view
        .queue
        .as_ref()
        .is_some_and(|q| q.retry_items.contains(&item.id))
    {
        return "等待重试";
    }
    if item.state == SftpQueueState::Running {
        if view.stopping_items.contains(&item.id) {
            return "正在取消";
        }
        if category(item, view) == Filter::Paused {
            return "已暂停";
        }
        if let Some(transfer)=item.transfer_id.and_then(|id|view.queue_transfers.get(&id)) {
            use cshell_ipc::SftpTransferPhase as Phase;
            match Phase::try_from(transfer.phase).unwrap_or(Phase::Unknown) {
                Phase::Preparing=>return "准备中", Phase::Verifying=>return "校验文件",
                Phase::ServerChecksum=>return "服务器校验",
                Phase::Publishing=>return "发布文件", Phase::CheckingPublished=>return if item.draft.verification.is_some(){"核对大小"}else{"核验目标"},
                Phase::Cleaning=>return "清理中", _=>{},
            }
        }
        if item.verified.as_ref().is_some_and(|v| v.content.is_none()) {
            return "准备中";
        }
        if item.verified.as_ref().is_some_and(|v| v.verified) {
            return "提交中";
        }
        let (done, total) = transferred(item, view);
        if total.is_some_and(|total| done >= total) {
            return "校验中";
        }
    }
    if item.state == SftpQueueState::CleanupRequired {
        return "待清理";
    }
    category(item, view).label()
}
fn remaining(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}时{}分", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}分{}秒", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}秒")
    }
}
fn tree_category(row:&cshell_application::SftpTreeSnapshot)->Filter {
    use cshell_application::TerminalUploadState as S;
    match row.state {S::Preparing|S::Ready=>Filter::Waiting,S::Running=>Filter::Active,S::Paused=>Filter::Paused,S::Succeeded=>Filter::Succeeded,S::Failed=>Filter::Failed,S::Cancelled=>Filter::Cancelled}
}
fn cell<R>(ui: &mut egui::Ui, width: f32, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 26.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.set_max_width(width);
            ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()));
            f(ui)
        },
    )
    .inner
}
fn label(ui: &mut egui::Ui, width: f32, text: impl Into<egui::WidgetText>) {
    cell(ui, width, |ui| {
        ui.add(egui::Label::new(text).truncate());
    });
}
fn widths(width: f32) -> [f32; 7] {
    let progress = (width * 0.18).clamp(96.0, 220.0);
    [
        22.0,
        (width - 350.0 - progress).max(40.0),
        60.0,
        progress,
        70.0,
        54.0,
        108.0,
    ]
}
impl SftpPanel {
    fn table_batch(
        &mut self,
        session: SessionId,
        action: BatchAction,
        ids: Vec<SftpQueueItemId>,
        revision: u64,
        command: &mut Option<SftpClientCommand>,
    ) {
        if !ids.is_empty() {
            *command = Some(SftpClientCommand::QueueBatch(
                session, action, ids, revision,
            ));
        }
    }
    pub(super) fn transfer_dock(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        let Some(queue) = view.queue.as_ref() else {
            ui.strong("传输任务");
            ui.weak("正在加载传输队列…");
            egui::ScrollArea::vertical()
                .id_salt("transfer-recovery-warning")
                .max_height(ui.available_height())
                .show(ui, |ui| self.draw_transfer_warning(ui, view));
            return;
        };
        let ready = queue.migrated_to.is_none()
            && view.queue_writable
            && !view.queue_busy
            && !view.workspace_busy;
        let work = !queue.retry_items.is_empty()
            || view.trees.values().any(|v|!v.state.finished())
            || queue.items.iter().any(|item| {
                matches!(
                    item.state,
                    SftpQueueState::Running | SftpQueueState::Pending
                )
            });
        let previous_filter = self.browser.table.filter;
        ui.horizontal_wrapped(|ui| {
            ui.strong("传输任务");
            if visuals::tool(ui, Icon::Refresh, "刷新队列", !view.queue_busy && !view.workspace_busy, None, false)
                .on_hover_text("重新读取后台任务，核对未确认的提交结果，不重复上传")
                .clicked()
            {
                *command = Some(SftpClientCommand::QueueGet(session));
            }
            ui.weak(format!(
                "{} 项 · 并发 {}",
                queue.items.len()+view.trees.len(),
                queue.concurrency.unwrap_or(3)
            ));
            egui::ComboBox::from_id_salt("transfer-filter")
                .width(100.0)
                .selected_text(self.browser.table.filter.label())
                .show_ui(ui, |ui| {
                    for filter in Filter::ALL {
                        let count = queue
                            .items
                            .iter()
                            .filter(|row| filter == Filter::All || category(row, view) == filter)
                            .count()+view.trees.values().filter(|r|filter==Filter::All||tree_category(r)==filter).count();
                        ui.selectable_value(
                            &mut self.browser.table.filter,
                            filter,
                            format!("{} ({count})", filter.label()),
                        );
                    }
                });
            if visuals::tool(ui,Icon::Pause,"暂停队列",ready&&work,None,false)
                .clicked()
            {
                *command = Some(SftpClientCommand::LiveQueue(session, true));
            }
            if visuals::tool(ui,Icon::Play,"继续队列",ready && work && (queue.paused || view.queue_hold || view.queue_transfers.values().any(|t|t.paused)),None,false)
                .clicked()
            {
                *command = Some(SftpClientCommand::LiveQueue(session, false));
            }
            egui::containers::menu::MenuButton::new("更多").ui(ui, |ui| {
                let failed: Vec<_> = queue
                    .items
                    .iter()
                    .filter(|item| BatchAction::Retry.allows(item, view))
                    .map(|item| item.id)
                    .collect();
                if ui
                    .add_enabled(
                        ready && !failed.is_empty(),
                        egui::Button::new(format!("重试失败 ({})", failed.len())),
                    )
                    .clicked()
                {
                    self.table_batch(session, BatchAction::Retry, failed, queue.revision, command);
                    ui.close();
                }
                let finished: Vec<_> = queue
                    .items
                    .iter()
                    .filter(|item| BatchAction::RemoveFinished.allows(item, view))
                    .map(|item| item.id)
                    .collect();
                if ui
                    .add_enabled(
                        ready && !finished.is_empty(),
                        egui::Button::new(format!("清除已完成 ({})", finished.len())),
                    )
                    .on_hover_text("只移除已完成、已取消的任务记录，不删除文件")
                    .clicked()
                {
                    self.table_batch(
                        session,
                        BatchAction::RemoveFinished,
                        finished,
                        queue.revision,
                        command,
                    );
                    ui.close();
                }
                if ui
                    .add_enabled(
                        ready
                            && !self.browser.staged.is_empty()
                            && !self.browser.auto_submit
                            && self.browser.submitted_revision.is_none(),
                        egui::Button::new("重试未加入的文件"),
                    )
                    .clicked()
                {
                    self.browser.auto_submit = true;
                    ui.close();
                }
            });
        });
        if previous_filter != self.browser.table.filter {
            self.browser.table.selected.clear();
        }
        let mut visible: Vec<_> = queue
            .items
            .iter()
            .filter(|item| {
                self.browser.table.filter == Filter::All
                    || category(item, view) == self.browser.table.filter
            })
            .collect();
        // Keep current work visible when many historical completions fill the queue.
        if self.browser.table.filter == Filter::All {
            visible.sort_by_key(|item| match category(item, view) {
                Filter::Active => 0,
                Filter::Paused => 1,
                Filter::Waiting => 2,
                Filter::Failed => 3,
                Filter::Review => 4,
                Filter::Cancelled => 5,
                _ => 6,
            });
        }
        self.browser
            .table
            .selected
            .retain(|id| visible.iter().any(|item| item.id == *id));
        ui.horizontal_wrapped(|ui| {
            let mut all = !visible.is_empty()
                && visible
                    .iter()
                    .all(|item| self.browser.table.selected.contains(&item.id));
            if ui.checkbox(&mut all, "全选当前筛选").changed() {
                if all {
                    self.browser
                        .table
                        .selected
                        .extend(visible.iter().map(|item| item.id));
                } else {
                    self.browser.table.selected.clear();
                }
            }
            ui.weak(format!("已选 {} 项", self.browser.table.selected.len()));
            egui::containers::menu::MenuButton::new("批量操作").ui(ui, |ui| {
                for action in [
                    BatchAction::Pause,
                    BatchAction::Resume,
                    BatchAction::Cancel,
                    BatchAction::Retry,
                    BatchAction::RemoveFinished,
                ] {
                    let ids: Vec<_> = visible
                        .iter()
                        .filter(|item| {
                            self.browser.table.selected.contains(&item.id)
                                && action.allows(item, view)
                        })
                        .map(|item| item.id)
                        .collect();
                    if ui
                        .add_enabled(
                            ready && !ids.is_empty(),
                            egui::Button::new(format!("{}选中 ({})", action.label(), ids.len())),
                        )
                        .on_hover_text("仅操作当前筛选中已选且支持该操作的任务")
                        .clicked()
                    {
                        self.table_batch(session, action, ids, queue.revision, command);
                        ui.close();
                    }
                }
            });
            if view.queue_hold {
                ui.weak("队列已暂停");
            }
            if let Some(message) = &view.batch_status {
                ui.add(egui::Label::new(message).truncate())
                    .on_hover_text(message);
            }
        });
        if let Some(error) = &self.browser.error {
            ui.add(
                egui::Label::new(egui::RichText::new(error).color(cshell_ui::theme::DANGER))
                    .truncate(),
            )
            .on_hover_text(error);
        }
        if let Some(transfer) = view
            .transfer
            .as_ref()
            .filter(|t| t.state == SftpTransferState::Failed as i32)
        {
            let warning = if transfer.destination_committed {
                "目标已提交，清理失败；请核对，不要重复传输"
            } else {
                "传输失败，请核对错误和临时文件"
            };
            ui.add(
                egui::Label::new(egui::RichText::new(warning).color(cshell_ui::theme::DANGER))
                    .truncate(),
            )
            .on_hover_text(format!(
                "{}\n{}",
                transfer.detail,
                transfer.cleanup_path.as_deref().unwrap_or("")
            ));
        }
        // Update every row, including filtered/virtualized rows. Pause/resume/retry resets sampling.
        let now = Instant::now();
        for item in &queue.items {
            let (done, total) = transferred(item, view);
            let active = category(item, view) == Filter::Active
                && status(item, view) == "传输中"
                && total.is_none_or(|total| done < total);
            let sample = self.browser.table.rates.entry(item.id).or_insert(Sample {
                transfer: item.transfer_id,
                bytes: done,
                at: now,
                progress_at: now,
                rate: None,
                active,
            });
            sample.update(item.transfer_id, done, active, now);
        }
        self.browser
            .table
            .rates
            .retain(|id, _| queue.items.iter().any(|item| item.id == *id));
        let columns = widths((ui.available_width() - 12.0).max(620.0));
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.horizontal(|ui| {
            for (text, width) in ["", "文件 / 方向", "状态", "进度", "速度", "剩余", "操作"]
                .into_iter()
                .zip(columns)
            {
                label(ui, width, egui::RichText::new(text).weak());
            }
        });
        ui.separator();
        let staged = if matches!(self.browser.table.filter, Filter::All | Filter::Waiting) {
            self.browser.submitted_drafts.len() + self.browser.staged.len()
        } else {
            0
        };
        if self.browser.tree.is_some() {ui.horizontal_wrapped(|ui|{ui.colored_label(cshell_ui::theme::WARNING,"目录任务尚未加入");if ui.button("重试加入").clicked(){self.browser.tree_auto=true;}});}
        let trees=view.trees.values().filter(|r|self.browser.table.filter==Filter::All||tree_category(r)==self.browser.table.filter).collect::<Vec<_>>();
        let row_height = 26.0;
        egui::ScrollArea::vertical()
            .id_salt(("compact-transfer-rows", self.browser.table.filter as u8))
            .min_scrolled_height(0.0)
            .auto_shrink([false, false])
            .max_height(ui.available_height().max(0.0))
            .show_rows(ui, row_height, visible.len() + trees.len() + staged, |ui, range| {
                for index in range {
                    if let Some(item) = visible.get(index) {
                        ui.push_id(item.id, |ui| {
                            self.task_row(ui, session, view, queue, item, &columns, ready, command)
                        });
                    } else if let Some(tree)=trees.get(index-visible.len()) {
                        ui.push_id(tree.id,|ui|self.tree_row(ui,session,view,tree,&columns,command));
                    } else if let Some(draft) = self
                        .browser
                        .submitted_drafts
                        .iter()
                        .chain(&self.browser.staged)
                        .nth(index - visible.len() - trees.len())
                    {
                        ui.horizontal(|ui| {
                            label(ui, columns[0], "");
                            label(
                                ui,
                                columns[1],
                                format!(
                                    "{} {}",
                                    if draft.direction == SftpQueueDirection::Upload {
                                        "↑"
                                    } else {
                                        "↓"
                                    },
                                    draft
                                        .remote_path
                                        .rsplit('/')
                                        .next()
                                        .unwrap_or(&draft.remote_path)
                                ),
                            );
                            let submitted = index - visible.len() - trees.len() < self.browser.submitted_drafts.len();
                            let (state, detail) = if submitted && view.queue_busy {
                                ("正在加入", "等待队列确认")
                            } else if submitted {
                                ("需核对", "结果未确认，请刷新队列")
                            } else if !view.queue_writable {
                                ("待加入", "队列未就绪，请刷新队列")
                            } else if self.browser.auto_submit {
                                ("待加入", "等待提交")
                            } else {
                                ("未加入", "请在更多菜单重试")
                            };
                            label(ui, columns[2], state);
                            ui.add(egui::Label::new(egui::RichText::new(detail).weak()).truncate())
                                .on_hover_text(detail);
                        });
                    }
                }
                if visible.is_empty() && trees.is_empty() && staged == 0 {
                    ui.weak(if queue.items.is_empty() {
                        "拖入文件即可自动上传或下载"
                    } else {
                        "当前筛选没有任务"
                    });
                }
            });
    }
    fn tree_row(&mut self,ui:&mut egui::Ui,session:SessionId,view:&SftpView,row:&cshell_application::SftpTreeSnapshot,columns:&[f32;7],command:&mut Option<SftpClientCommand>){
        use cshell_application::TerminalUploadState as S;
        let ready=!view.tree_pending.contains(&row.id)&&(view.connection_profile.is_none()||view.connection_phase==crate::sftp_connection::ConnectionPhase::Connected);
        ui.horizontal(|ui|{
            label(ui,columns[0],"▤");
            cell(ui,columns[1],|ui|{ui.add(egui::Label::new(format!("{} {}",if row.direction==SftpQueueDirection::Upload{"→ 上传"}else{"← 下载"},row.label)).truncate()).on_hover_text(&row.detail);});
            label(ui,columns[2],tree_category(row).label());
            cell(ui,columns[3],|ui|{let ratio=if row.state==S::Succeeded{1.0}else if row.total_bytes>0{row.bytes as f32/row.total_bytes as f32}else{0.0};ui.add(egui::ProgressBar::new(ratio.clamp(0.0,1.0)).text(format!("{}/{} 文件",row.completed,row.total_files))).on_hover_text(format!("{} / {}\n{}",bytes(row.bytes),bytes(row.total_bytes),row.detail));});
            label(ui,columns[4],if row.bytes_per_second>0{format!("{}/s",bytes(row.bytes_per_second))}else{"—".into()});
            label(ui,columns[5],if row.bytes_per_second>0{remaining(row.total_bytes.saturating_sub(row.bytes)/row.bytes_per_second)}else{"—".into()});
            cell(ui,columns[6],|ui|{
                let op=match row.state {S::Preparing|S::Running=>Some((SftpOperation::TreePause,Icon::Pause,"暂停")),S::Paused=>Some((SftpOperation::TreeContinue,Icon::Play,"继续")),_ if row.retryable=>Some((SftpOperation::TreeRetry,Icon::Reconnect,"重试失败项")),_=>None};
                if let Some((op,icon,label))=op {if visuals::tool(ui,icon,label,ready,None,true).on_hover_text(label).clicked(){*command=Some(SftpClientCommand::Tree(session,op,row.id,None));}}
                if !row.state.finished()&&visuals::tool(ui,Icon::Close,"取消",ready,None,true).on_hover_text("取消目录任务，已完成文件保留").clicked(){*command=Some(SftpClientCommand::Tree(session,SftpOperation::TreeCancel,row.id,None));}
                if visuals::tool(ui,Icon::Refresh,"刷新进度",ready,None,true).on_hover_text(&row.detail).clicked(){*command=Some(SftpClientCommand::Tree(session,SftpOperation::TreeGet,row.id,None));}
            });
        });
    }
    fn task_row(
        &mut self,
        ui: &mut egui::Ui,
        session: SessionId,
        view: &SftpView,
        queue: &SftpQueueSnapshot,
        item: &SftpQueueItem,
        columns: &[f32; 7],
        ready: bool,
        command: &mut Option<SftpClientCommand>,
    ) {
        let (uploaded, upload_total) = transferred(item, view);
        let phase=item.transfer_id.and_then(|id|view.queue_transfers.get(&id));
        let server_check=phase.is_some_and(|t|t.phase==cshell_ipc::SftpTransferPhase::ServerChecksum as i32);
        let verifying=phase.is_some_and(|t|t.phase==cshell_ipc::SftpTransferPhase::Verifying as i32 || t.phase==cshell_ipc::SftpTransferPhase::CheckingPublished as i32 && item.draft.verification.is_none());
        let moving=phase.is_none_or(|t|matches!(cshell_ipc::SftpTransferPhase::try_from(t.phase),Ok(cshell_ipc::SftpTransferPhase::Unknown|cshell_ipc::SftpTransferPhase::Transferring)));
        let (done, total) = phase.filter(|_| verifying)
            .map_or((uploaded, upload_total), |t| (t.phase_bytes, t.phase_total));
        let detail = format!(
            "本地：{}\n远程：{}\n校验策略：{}{}",
            item.draft.local_path,
            item.draft.remote_path,
            item.draft.verification.map(crate::zmodem_settings::verification_label).unwrap_or("原有严格校验"),
            item.receipt
                .as_ref()
                .and_then(|r| r.detail.as_ref())
                .map(|s| format!("\n{s}"))
                .unwrap_or_default()
        );
        let row_status = status(item, view);
        let rate = self
            .browser
            .table
            .rates
            .get(&item.id)
            .and_then(|sample| sample.rate)
            .filter(|_| category(item, view) == Filter::Active && moving);
        let eta = rate
            .filter(|rate| *rate > 0.0)
            .zip(total)
            .map(
                |(rate, total)| remaining((total.saturating_sub(done) as f64 / rate).ceil() as u64),
            )
            .unwrap_or_else(|| {
                if category(item, view) == Filter::Paused {
                    "已暂停".into()
                } else {
                    "—".into()
                }
            });
        let ratio = if item.state == SftpQueueState::Succeeded {
            1.0
        } else {
            total.filter(|t| *t > 0).map_or(0.0, |total| {
                (done as f64 / total as f64).clamp(0.0, 1.0) as f32
            })
        };
        let selected = self.browser.table.selected.contains(&item.id);
        let row_rect = egui::Rect::from_min_size(
            ui.cursor().min,
            egui::vec2(columns.iter().sum::<f32>() + 36.0, 26.0),
        );
        if selected {
            ui.painter()
                .rect_filled(row_rect, 0.0, cshell_ui::theme::ACCENT.gamma_multiply(0.13));
        }
        ui.horizontal(|ui| {
            cell(ui, columns[0], |ui| {
                let mut checked = selected;
                let response = ui.checkbox(&mut checked, "");
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::Checkbox,
                        true,
                        checked,
                        format!(
                            "选择任务 {}",
                            item.draft
                                .remote_path
                                .rsplit('/')
                                .next()
                                .unwrap_or(&item.draft.remote_path)
                        ),
                    )
                });
                if response.changed() {
                    if checked {
                        self.browser.table.selected.insert(item.id);
                    } else {
                        self.browser.table.selected.remove(&item.id);
                    }
                }
                columns::clip_accessibility(ui, &response);
            });
            cell(ui, columns[1], |ui| {
                let upload=item.draft.direction==SftpQueueDirection::Upload;
                ui.spacing_mut().item_spacing.x=5.0;
                ui.label(egui::RichText::new(if upload {"→ 上传"}else{"← 下载"}).small().strong().color(visuals::direction_color(!upload)))
                    .on_hover_text(if upload {"本地 → 当前主机"}else{"当前主机 → 本地"});
                ui.add(egui::Label::new(item.draft.remote_path.rsplit('/').next().unwrap_or(&item.draft.remote_path)).truncate()).on_hover_text(&detail);
            });
            let color = match category(item, view) {
                Filter::Failed | Filter::Review => cshell_ui::theme::DANGER,
                Filter::Succeeded => cshell_ui::theme::ACCENT,
                _ => ui.visuals().text_color(),
            };
            cell(ui, columns[2], |ui| {
                ui.add(egui::Label::new(egui::RichText::new(row_status).color(color)).truncate())
                    .on_hover_text(&detail);
            });
            cell(ui, columns[3], |ui| {
                ui.add(
                    egui::ProgressBar::new(ratio)
                        .desired_width(columns[3])
                        .text(if server_check {"服务器计算 SHA-256…".into()}else{format!("{}{:.0}% · {}",if verifying {"校验 "} else {""}, ratio * 100.0, bytes(done))}),
                )
                .on_hover_text(format!(
                    "{}{} / {}",
                    if verifying {"当前校验阶段："} else {"已传输："},
                    bytes(done),
                    total.map(bytes).unwrap_or_else(|| "计算中".into())
                ));
            });
            label(
                ui,
                columns[4],
                rate.map(|rate| format!("{}/s", bytes(rate as u64)))
                    .unwrap_or_else(|| "—".into()),
            );
            label(ui, columns[5], eta);
            cell(ui, columns[6], |ui| {
                if item.state == SftpQueueState::Running {
                    let paused = category(item, view) == Filter::Paused;
                    let enabled = ready
                        && crate::sftp_connection::can_pause_transfer(item)
                        && !view.stopping_items.contains(&item.id);
                    let response = ui.add_enabled(
                        enabled,
                        egui::Button::new(if paused { "继续" } else { "暂停" }).small(),
                    );
                    columns::clip_accessibility(ui, &response);
                    if response.clicked() {
                        *command = Some(if paused {
                            SftpClientCommand::ResumeQueueItem(session, item.id, queue.revision)
                        } else {
                            SftpClientCommand::StopQueueItem(session, item.id, queue.revision, true)
                        });
                    }
                    let response = ui.add_enabled(enabled, egui::Button::new("取消").small());
                    columns::clip_accessibility(ui, &response);
                    if response.clicked() {
                        *command = Some(SftpClientCommand::StopQueueItem(
                            session,
                            item.id,
                            queue.revision,
                            false,
                        ));
                    }
                } else if BatchAction::Cancel.allows(item, view) {
                    if ui
                        .add_enabled(ready, egui::Button::new("取消").small())
                        .clicked()
                    {
                        self.table_batch(
                            session,
                            BatchAction::Cancel,
                            vec![item.id],
                            queue.revision,
                            command,
                        );
                    }
                } else if item.can_retry_verified() {
                    if ui
                        .add_enabled(ready, egui::Button::new("重试").small())
                        .on_hover_text("校验源文件及已传前缀后续传；未创建暂存文件的任务从头重试")
                        .clicked()
                    {
                        self.table_batch(
                            session,
                            BatchAction::Retry,
                            vec![item.id],
                            queue.revision,
                            command,
                        );
                    }
                } else if category(item, view) == Filter::Review {
                    if ui.small_button("核对").on_hover_text(&detail).clicked() {
                        self.page = 1;
                    }
                } else {
                    ui.weak("—");
                }
            });
        });
    }
}

#[cfg(test)]
mod tests;
