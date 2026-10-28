//! Shared typed task drafts. Invalid rows stay editable and block saving.
use cshell_application::{
    BuiltinTaskTemplate, MAX_TASK_COMMAND_BYTES, MAX_TASK_STEPS, validate_task_definition,
};
use cshell_domain::*;
use std::collections::{BTreeMap, BTreeSet};
use zeroize::Zeroizing;

// Local widget identity follows rows across reorder; it never enters a saved task.
static NEXT_ROW_KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn row_key() -> u64 {
    NEXT_ROW_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
const MAX_DEFINITION_BYTES: usize = 1024 * 1024;
const MAX_VALUE_BYTES: usize = 8192;
const MAX_PAIRS: usize = 64;
const MAX_ARGUMENTS: usize = 256;
pub(crate) fn show_publication_receipt(
    ui: &mut egui::Ui,
    receipt: Option<cshell_application::FilePublicationReceipt>,
) {
    if let Some(receipt) = receipt {
        ui.label(format!(
            "发布回执：已确认相同内容复用 {} / 已确认独立备份 {}（未确认不表示文件不存在）",
            receipt.skipped_identical, receipt.backup_verified
        ));
        if let Some(previous) = receipt.previous {
            let digest = previous
                .sha256
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            ui.label(format!("原目标：{} 字节 / SHA-256 {digest}", previous.size));
        }
    }
}
pub(crate) fn draw_publication_policy(
    ui: &mut egui::Ui,
    policy: &mut Option<cshell_application::FilePublicationPolicy>,
) {
    use cshell_application::FilePublicationPolicy;
    egui::ComboBox::from_id_salt("file-publication-policy")
        .truncate()
        .selected_text(match policy {
            None => "目标存在则失败",
            Some(FilePublicationPolicy::SkipIdentical) => "内容相同则复用",
            Some(FilePublicationPolicy::BackupReplace) => "保留备份后替换",
        })
        .show_ui(ui, |ui| {
            cshell_ui::form::menu(ui);
            ui.selectable_value(policy, None, "目标存在则失败");
            ui.selectable_value(
                policy,
                Some(FilePublicationPolicy::SkipIdentical),
                "内容相同则复用",
            );
            ui.selectable_value(
                policy,
                Some(FilePublicationPolicy::BackupReplace),
                "保留备份后替换",
            );
        });
    if policy.is_some() {
        ui.label("发布前核对原目标摘要；复用仍会验证源与暂存文件。备份使用独立副本，不自动清理。");
    }
    if *policy == Some(FilePublicationPolicy::BackupReplace) {
        ui.colored_label(
            cshell_ui::theme::WARNING,
            "替换前须保证目标没有其他写入者；文件协议无条件交换。保存并启动此策略即确认替换。",
        );
    }
}

struct ValueDraft {
    key: u64,
    name: String,
    value: Zeroizing<String>,
}
struct ArgumentDraft {
    key: u64,
    variable: bool,
    literal: Zeroizing<String>,
    name: String,
}
impl ArgumentDraft {
    fn from_argument(argument: &TaskArgument) -> Self {
        match argument {
            TaskArgument::Literal { text } => Self {
                key: row_key(),
                variable: false,
                literal: Zeroizing::new(text.clone()),
                name: String::new(),
            },
            TaskArgument::Variable { name } => Self {
                key: row_key(),
                variable: true,
                literal: Zeroizing::new(String::new()),
                name: name.clone(),
            },
        }
    }
    fn argument(&self) -> TaskArgument {
        if self.variable {
            TaskArgument::Variable {
                name: self.name.clone(),
            }
        } else {
            TaskArgument::Literal {
                text: self.literal.to_string(),
            }
        }
    }
}
struct StepDraft {
    key: u64,
    name: String,
    invocation: bool,
    script: Zeroizing<String>,
    program: Zeroizing<String>,
    arguments: Vec<ArgumentDraft>,
    codes: String,
    idempotent: bool,
}
impl StepDraft {
    fn from_step(step: &TaskStep) -> Self {
        let mut draft = Self {
            key: row_key(),
            name: step.name.clone(),
            invocation: false,
            script: Zeroizing::new(String::new()),
            program: Zeroizing::new(String::new()),
            arguments: Vec::new(),
            codes: step
                .success_codes
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
            idempotent: step.idempotent,
        };
        match &step.command {
            TaskCommand::Script { text } => draft.script.push_str(text),
            TaskCommand::Invocation { program, arguments } => {
                draft.invocation = true;
                draft.program.push_str(program);
                draft.arguments = arguments.iter().map(ArgumentDraft::from_argument).collect();
            }
        }
        draft
    }
    fn empty() -> Self {
        Self::from_step(&TaskStep {
            name: "执行命令".into(),
            command: TaskCommand::Script {
                text: String::new(),
            },
            success_codes: BTreeSet::from([0]),
            idempotent: false,
        })
    }
    fn step(&self) -> Result<TaskStep, String> {
        let codes = self
            .codes
            .split(',')
            .map(|value| {
                value
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| "成功码须为逗号分隔的非负整数".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(TaskStep {
            name: self.name.clone(),
            command: if self.invocation {
                TaskCommand::Invocation {
                    program: self.program.to_string(),
                    arguments: self.arguments.iter().map(ArgumentDraft::argument).collect(),
                }
            } else {
                TaskCommand::Script {
                    text: self.script.to_string(),
                }
            },
            success_codes: codes.into_iter().collect(),
            idempotent: self.idempotent,
        })
    }
}
pub(crate) struct TaskDefinitionEditor {
    id: TaskId,
    name: String,
    shell_kind: u8,
    launcher: Zeroizing<String>,
    dialect: TaskShellDialect,
    steps: Vec<StepDraft>,
    variables: Vec<ValueDraft>,
    environment: Vec<ValueDraft>,
    policy: TaskPolicy,
    revision: u64,
    template: BuiltinTaskTemplate,
    template_append: bool,
    replace_ack: bool,
    json: Zeroizing<String>,
    json_open: bool,
    import_ack: bool,
    status: String,
    validation_revision: Option<(u64, bool, bool)>,
    validation_error: String,
}
impl Default for TaskDefinitionEditor {
    fn default() -> Self {
        Self::from_definition(&TaskDefinition {
            id: TaskId::new(),
            name: "批量任务".into(),
            shell: RemoteShellProfile::Posix,
            steps: vec![TaskStep {
                name: "执行命令".into(),
                command: TaskCommand::Script {
                    text: String::new(),
                },
                success_codes: BTreeSet::from([0]),
                idempotent: false,
            }],
            variables: BTreeMap::new(),
            environment: BTreeMap::new(),
            policy: TaskPolicy::default(),
        })
    }
}
impl std::fmt::Debug for TaskDefinitionEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskDefinitionEditor")
            .field("steps", &self.steps.len())
            .field("variables", &self.variables.len())
            .field("environment", &self.environment.len())
            .field("revision", &self.revision)
            .field("body", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}
impl TaskDefinitionEditor {
    pub(crate) fn from_definition(definition: &TaskDefinition) -> Self {
        let (shell_kind, launcher, dialect) = match &definition.shell {
            RemoteShellProfile::Posix => (0, String::new(), TaskShellDialect::Posix),
            RemoteShellProfile::PowerShell => (1, String::new(), TaskShellDialect::PowerShell),
            RemoteShellProfile::Cmd => (2, String::new(), TaskShellDialect::Cmd),
            RemoteShellProfile::Custom { launcher, dialect } => (3, launcher.clone(), *dialect),
        };
        let pairs = |values: &BTreeMap<String, String>| {
            values
                .iter()
                .map(|(name, value)| ValueDraft {
                    key: row_key(),
                    name: name.clone(),
                    value: Zeroizing::new(value.clone()),
                })
                .collect()
        };
        Self {
            id: definition.id,
            name: definition.name.clone(),
            shell_kind,
            launcher: Zeroizing::new(launcher),
            dialect,
            steps: definition.steps.iter().map(StepDraft::from_step).collect(),
            variables: pairs(&definition.variables),
            environment: pairs(&definition.environment),
            policy: definition.policy.clone(),
            revision: 1,
            template: BuiltinTaskTemplate::PosixFileExists,
            template_append: false,
            replace_ack: false,
            json: Zeroizing::new(String::new()),
            json_open: false,
            import_ack: false,
            status: String::new(),
            validation_revision: None,
            validation_error: String::new(),
        }
    }
    fn append_template(&self, template: &TaskDefinition) -> Result<TaskDefinition, String> {
        let mut definition = self.definition(false)?;
        if definition.shell != template.shell {
            return Err("模板与当前 Shell 不同，请另建任务或明确替换整个草稿".into());
        }
        if definition.steps.len() + template.steps.len() > MAX_TASK_STEPS {
            return Err("追加模板将超过步骤数限制".into());
        }
        for (destination, source) in [
            (&mut definition.variables, &template.variables),
            (&mut definition.environment, &template.environment),
        ] {
            for (name, value) in source {
                if destination
                    .get(name)
                    .is_some_and(|existing| existing != value)
                {
                    return Err("模板变量或环境值与当前定义冲突，请先改名或修改值".into());
                }
                destination.insert(name.clone(), value.clone());
            }
        }
        definition.steps.extend(template.steps.iter().cloned());
        validate_task_definition(&definition).map_err(|error| error.to_string())?;
        Ok(definition)
    }
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
    pub(crate) fn definition(&self, single: bool) -> Result<TaskDefinition, String> {
        let definition = TaskDefinition {
            id: self.id,
            name: self.name.clone(),
            shell: match self.shell_kind {
                0 => RemoteShellProfile::Posix,
                1 => RemoteShellProfile::PowerShell,
                2 => RemoteShellProfile::Cmd,
                _ => RemoteShellProfile::Custom {
                    launcher: self.launcher.to_string(),
                    dialect: self.dialect,
                },
            },
            steps: self
                .steps
                .iter()
                .map(StepDraft::step)
                .collect::<Result<_, _>>()?,
            variables: value_map(&self.variables)?,
            environment: value_map(&self.environment)?,
            policy: self.policy.clone(),
        };
        validate_task_definition(&definition).map_err(|error| error.to_string())?;
        if single {
            let p = &definition.policy;
            if definition.steps.len() != 1
                || p.concurrency != 1
                || p.batch_size.is_some()
                || p.canary_count != 0
                || p.approve_after_canary
                || p.pause_between_batches
                || p.batch_delay_ms != 0
                || p.failure_limit.is_some()
            {
                return Err("混合命令须为单步骤、并发 1，分批策略由父任务控制".into());
            }
        }
        Ok(definition)
    }
    pub(crate) fn draw(&mut self, ui: &mut egui::Ui, single: bool, script_only: bool) {
        if (single || script_only) && self.template_append {
            self.template_append = false;
            self.replace_ack = false;
        }
        let mut changed = false;
        ui.collapsing("检查与报告模板", |ui| {
            egui::ComboBox::from_id_salt("builtin-task-template").truncate().selected_text(self.template.label()).show_ui(ui, |ui| { cshell_ui::form::menu(ui);
                for template in BuiltinTaskTemplate::ALL {
                    if ui.selectable_value(&mut self.template, template, template.label()).changed() { self.replace_ack = false; }
                }
            });
            ui.label("模板生成草稿；请修改示例路径或服务名称，核对远端工具和权限。检查按退出码决定是否进入下一步。");
            ui.horizontal_wrapped(|ui| {
                if ui.radio_value(&mut self.template_append, false, "替换全部定义").changed() { self.replace_ack = false; }
                ui.add_enabled_ui(!single && !script_only, |ui| {
                    if ui.radio_value(&mut self.template_append, true, "追加模板步骤").changed() { self.replace_ack = false; }
                });
            });
            ui.checkbox(&mut self.replace_ack, if self.template_append { "确认追加模板步骤及变量、环境" } else { "确认用模板替换此任务草稿的全部定义" });
            if ui.add_enabled(self.replace_ack, egui::Button::new("采用模板")).clicked() {
                let mut definition = self.template.definition();
                if single { definition.policy.concurrency = 1; }
                if script_only && !matches!(definition.steps[0].command, TaskCommand::Script { .. }) {
                    self.status = "单步脚本快捷命令仅可采用 Script 模板".into();
                } else {
                    let result = if self.template_append { self.append_template(&definition) } else { Ok(definition) };
                    match result {
                        Ok(definition) => {
                            let revision = self.revision.saturating_add(1);
                            *self = Self::from_definition(&definition);
                            self.revision = revision;
                            self.status = "模板已载入草稿，尚未保存或启动".into();
                        }
                        Err(error) => { self.status = error; self.replace_ack = false; }
                    }
                }
            }
        });
        changed |= ui
            .add(
                egui::TextEdit::singleline(&mut self.name)
                    .hint_text("任务名称")
                    .char_limit(128),
            )
            .changed();
        ui.horizontal_wrapped(|ui| {
            for (kind, label) in [
                (0, "POSIX sh"),
                (1, "PowerShell"),
                (2, "CMD"),
                (3, "自定义启动器"),
            ] {
                changed |= ui
                    .selectable_value(&mut self.shell_kind, kind, label)
                    .changed();
            }
        });
        if self.shell_kind == 3 {
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut *self.launcher)
                        .hint_text("启动器，必须含一个 {script}")
                        .char_limit(4096),
                )
                .changed();
            ui.horizontal_wrapped(|ui| {
                for (dialect, label) in [
                    (TaskShellDialect::Posix, "POSIX 引号"),
                    (TaskShellDialect::PowerShell, "PowerShell 引号"),
                    (TaskShellDialect::Cmd, "CMD 引号"),
                ] {
                    changed |= ui
                        .selectable_value(&mut self.dialect, dialect, label)
                        .changed();
                }
            });
        }
        ui.label("脚本是静态正文；变量仅作为 Invocation 参数引用。Shell 明确选择，平台标签不自动决定解释器。");
        ui.collapsing("变量", |ui| {
            changed |= draw_pairs(ui, &mut self.variables, false);
        });
        ui.collapsing("环境", |ui| {
            changed |= draw_pairs(ui, &mut self.environment, true);
        });
        let mut remove = None;
        let mut move_step = None;
        let count = self.steps.len();
        for (index, step) in self.steps.iter_mut().enumerate() {
            ui.push_id(("definition-step", step.key), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("步骤 {}", index + 1));
                    if ui
                        .add_enabled(
                            !single && !script_only && index > 0,
                            egui::Button::new("上移"),
                        )
                        .clicked()
                    {
                        move_step = Some((index, index - 1));
                    }
                    if ui
                        .add_enabled(
                            !single && !script_only && index + 1 < count,
                            egui::Button::new("下移"),
                        )
                        .clicked()
                    {
                        move_step = Some((index, index + 1));
                    }
                    if ui
                        .add_enabled(!single && !script_only, egui::Button::new("删除"))
                        .clicked()
                    {
                        remove = Some(index);
                    }
                });
                changed |= draw_step(ui, step, &self.variables, script_only);
            });
        }
        if let Some(index) = remove {
            self.steps.remove(index);
            changed = true;
        }
        if let Some((from, to)) = move_step {
            self.steps.swap(from, to);
            changed = true;
        }
        if ui
            .add_enabled(
                !single && !script_only && self.steps.len() < MAX_TASK_STEPS,
                egui::Button::new("添加步骤"),
            )
            .clicked()
        {
            self.steps.push(StepDraft::empty());
            changed = true;
        }
        ui.collapsing("执行策略", |ui| {
            changed |= draw_policy(ui, &mut self.policy, single);
        });
        if changed {
            self.revision = self.revision.saturating_add(1);
            self.replace_ack = false;
            self.import_ack = false;
            self.status.clear();
        }
        self.draw_json(ui, single, script_only);
        let stamp = (self.revision, single, script_only);
        if self.validation_revision != Some(stamp) {
            self.validation_error = match self.definition(single) {
                Err(error) => error,
                Ok(definition)
                    if script_only
                        && (definition.steps.len() != 1
                            || !matches!(
                                definition.steps[0].command,
                                TaskCommand::Script { .. }
                            )) =>
                {
                    "单步脚本快捷命令须保留一个 Script 步骤".into()
                }
                Ok(_) => String::new(),
            };
            self.validation_revision = Some(stamp);
        }
        if !self.validation_error.is_empty() {
            ui.colored_label(cshell_ui::theme::WARNING, &self.validation_error);
        }
        if !self.status.is_empty() {
            ui.colored_label(cshell_ui::theme::WARNING, &self.status);
        }
    }
    fn draw_json(&mut self, ui: &mut egui::Ui, single: bool, script_only: bool) {
        if ui
            .checkbox(&mut self.json_open, "完整 JSON 导入 / 导出")
            .changed()
        {
            self.import_ack = false;
            self.replace_ack = false;
        }
        if !self.json_open {
            return;
        }
        ui.label("导入需确认替换草稿；无效输入保留，已有表单不变。");
        if ui.button("从当前表单生成 JSON").clicked() {
            match self.definition(single).and_then(|definition| {
                serde_json::to_string_pretty(&definition).map_err(|_| "无法编码任务定义".into())
            }) {
                Ok(json) => {
                    self.json = Zeroizing::new(json);
                    self.import_ack = false;
                    self.status.clear();
                }
                Err(error) => self.status = error,
            }
        }
        if ui
            .add(
                egui::TextEdit::multiline(&mut *self.json)
                    .desired_rows(10)
                    .code_editor()
                    .char_limit(MAX_DEFINITION_BYTES),
            )
            .changed()
        {
            self.import_ack = false;
        }
        ui.checkbox(&mut self.import_ack, "确认用 JSON 替换当前表单的全部定义");
        if ui
            .add_enabled(self.import_ack, egui::Button::new("导入到表单"))
            .clicked()
        {
            let result = (|| {
                if self.json.len() > MAX_DEFINITION_BYTES {
                    return Err("任务定义超过 1 MiB".to_owned());
                }
                let definition: TaskDefinition = serde_json::from_str(&self.json)
                    .map_err(|_| "任务定义 JSON 无效".to_owned())?;
                validate_task_definition(&definition).map_err(|error| error.to_string())?;
                let editor = Self::from_definition(&definition);
                let definition = editor.definition(single)?;
                if script_only
                    && (definition.steps.len() != 1
                        || !matches!(definition.steps[0].command, TaskCommand::Script { .. }))
                {
                    return Err("单步脚本快捷命令须保留一个 Script 步骤".into());
                }
                Ok(editor)
            })();
            match result {
                Ok(mut editor) => {
                    editor.revision = self.revision.saturating_add(1);
                    *self = editor;
                    self.status = "JSON 已载入表单，尚未保存或启动".into();
                }
                Err(error) => {
                    self.status = error;
                    self.import_ack = false;
                }
            }
        }
    }
}
fn value_map(rows: &[ValueDraft]) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    for row in rows {
        if row.name.is_empty()
            || values
                .insert(row.name.clone(), row.value.to_string())
                .is_some()
        {
            return Err("变量或环境名称为空或重复，请修改后保存".into());
        }
    }
    Ok(values)
}
fn draw_pairs(ui: &mut egui::Ui, rows: &mut Vec<ValueDraft>, environment: bool) -> bool {
    let mut changed = false;
    let mut remove = None;
    ui.label(if environment {
        "环境值由所选 Shell 设置；按普通配置保存。"
    } else {
        "变量引用必须指向这里的名称；删除或改名后请核对参数引用。"
    });
    for (index, row) in rows.iter_mut().enumerate() {
        ui.push_id(row.key, |ui| {
            ui.horizontal_wrapped(|ui| {
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut row.name)
                            .hint_text("名称")
                            .char_limit(64),
                    )
                    .changed();
                if ui.button("删除").clicked() {
                    remove = Some(index);
                }
            });
            changed |= ui
                .add(
                    egui::TextEdit::multiline(&mut *row.value)
                        .desired_rows(1)
                        .hint_text("值")
                        .char_limit(MAX_VALUE_BYTES),
                )
                .changed();
        });
    }
    if let Some(index) = remove {
        rows.remove(index);
        changed = true;
    }
    if ui
        .add_enabled(rows.len() < MAX_PAIRS, egui::Button::new("添加"))
        .clicked()
    {
        rows.push(ValueDraft {
            key: row_key(),
            name: String::new(),
            value: Zeroizing::new(String::new()),
        });
        changed = true;
    }
    changed
}
fn draw_step(
    ui: &mut egui::Ui,
    step: &mut StepDraft,
    variables: &[ValueDraft],
    script_only: bool,
) -> bool {
    let mut changed = ui
        .add(
            egui::TextEdit::singleline(&mut step.name)
                .hint_text("步骤名称")
                .char_limit(128),
        )
        .changed();
    ui.horizontal_wrapped(|ui| {
        changed |= ui
            .radio_value(&mut step.invocation, false, "静态 Script")
            .changed();
        ui.add_enabled_ui(!script_only, |ui| {
            changed |= ui
                .radio_value(&mut step.invocation, true, "程序与参数 Invocation")
                .changed();
        });
    });
    if !step.invocation {
        changed |= ui
            .add(
                egui::TextEdit::multiline(&mut *step.script)
                    .desired_rows(4)
                    .char_limit(MAX_TASK_COMMAND_BYTES)
                    .code_editor(),
            )
            .changed();
    } else {
        changed |= ui
            .add(
                egui::TextEdit::singleline(&mut *step.program)
                    .hint_text("程序名或路径")
                    .char_limit(4096),
            )
            .changed();
        ui.label("每一行是一个参数，保持顺序；字面值按 Shell 引号编码，变量不展开成多个参数。空字面参数保留。");
        let mut remove = None;
        let mut move_arg = None;
        let count = step.arguments.len();
        for (index, argument) in step.arguments.iter_mut().enumerate() {
            ui.push_id(("argument", argument.key), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{}", index + 1));
                    changed |= ui
                        .radio_value(&mut argument.variable, false, "字面值")
                        .changed();
                    changed |= ui
                        .radio_value(&mut argument.variable, true, "变量")
                        .changed();
                    if ui.add_enabled(index > 0, egui::Button::new("↑")).clicked() {
                        move_arg = Some((index, index - 1));
                    }
                    if ui
                        .add_enabled(index + 1 < count, egui::Button::new("↓"))
                        .clicked()
                    {
                        move_arg = Some((index, index + 1));
                    }
                    if ui.button("删除").clicked() {
                        remove = Some(index);
                    }
                });
                if argument.variable {
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut argument.name)
                                .hint_text("变量名称")
                                .char_limit(64),
                        )
                        .changed();
                    egui::ComboBox::from_id_salt("argument-variable")
                        .truncate()
                        .selected_text("选用已有变量")
                        .show_ui(ui, |ui| {
                            cshell_ui::form::menu(ui);
                            for row in variables {
                                if !row.name.is_empty() {
                                    changed |= ui
                                        .selectable_value(
                                            &mut argument.name,
                                            row.name.clone(),
                                            &row.name,
                                        )
                                        .changed();
                                }
                            }
                        });
                    if !variables
                        .iter()
                        .any(|row| row.name == argument.name && !row.name.is_empty())
                    {
                        ui.colored_label(cshell_ui::theme::WARNING, "引用的变量不存在");
                    }
                } else {
                    changed |= ui
                        .add(
                            egui::TextEdit::multiline(&mut *argument.literal)
                                .desired_rows(1)
                                .char_limit(MAX_VALUE_BYTES),
                        )
                        .changed();
                }
            });
        }
        if let Some(index) = remove {
            step.arguments.remove(index);
            changed = true;
        }
        if let Some((from, to)) = move_arg {
            step.arguments.swap(from, to);
            changed = true;
        }
        if ui
            .add_enabled(
                step.arguments.len() < MAX_ARGUMENTS,
                egui::Button::new("添加参数"),
            )
            .clicked()
        {
            step.arguments.push(ArgumentDraft {
                key: row_key(),
                variable: false,
                literal: Zeroizing::new(String::new()),
                name: String::new(),
            });
            changed = true;
        }
    }
    changed |= ui
        .add(
            egui::TextEdit::singleline(&mut step.codes)
                .hint_text("成功码，如 0,2")
                .char_limit(160),
        )
        .changed();
    changed |= ui
        .checkbox(&mut step.idempotent, "此步骤重复执行是安全的（幂等）")
        .changed();
    changed
}
fn draw_policy(ui: &mut egui::Ui, policy: &mut TaskPolicy, single: bool) -> bool {
    let mut changed = false;
    if single {
        ui.label("一个混合命令步骤，分批与并发由父计划控制。");
    } else {
        changed |= number(ui, "并发", &mut policy.concurrency, 1..=50);
        let mut batches = policy.batch_size.is_some();
        if ui.checkbox(&mut batches, "按批执行").changed() {
            policy.batch_size = batches.then_some(20);
            changed = true;
        }
        if let Some(size) = &mut policy.batch_size {
            changed |= number(ui, "每批目标数", size, 1..=1000);
        }
        changed |= number(ui, "Canary 目标数", &mut policy.canary_count, 0..=1000);
        changed |= ui
            .add_enabled(
                policy.canary_count > 0,
                egui::Checkbox::new(&mut policy.approve_after_canary, "Canary 后等待批准"),
            )
            .changed();
        if policy.canary_count == 0 && policy.approve_after_canary {
            policy.approve_after_canary = false;
            changed = true;
        }
        changed |= ui
            .checkbox(&mut policy.pause_between_batches, "每批后等待批准")
            .changed();
        let mut failures = policy.failure_limit.is_some();
        if ui.checkbox(&mut failures, "达到失败阈值停止").changed() {
            policy.failure_limit = failures.then_some(1);
            changed = true;
        }
        if let Some(limit) = &mut policy.failure_limit {
            changed |= number(ui, "失败目标数", limit, 1..=1000);
        }
        changed |= milliseconds(ui, "批次间隔 ms", &mut policy.batch_delay_ms, 0);
    }
    changed |= milliseconds(ui, "排队超时 ms", &mut policy.queue_timeout_ms, 1);
    changed |= milliseconds(ui, "连接超时 ms", &mut policy.connect_timeout_ms, 1);
    changed |= milliseconds(ui, "单步超时 ms", &mut policy.step_timeout_ms, 1);
    changed |= milliseconds(ui, "总超时 ms", &mut policy.total_timeout_ms, 1);
    changed
}
fn number(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut u16,
    range: std::ops::RangeInclusive<u16>,
) -> bool {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(value).range(range)).changed()
    })
    .inner
}
fn milliseconds(ui: &mut egui::Ui, label: &str, value: &mut u64, minimum: u64) -> bool {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(value).range(minimum..=604_800_000))
            .changed()
    })
    .inner
}
