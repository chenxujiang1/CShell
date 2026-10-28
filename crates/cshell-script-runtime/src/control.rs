//! Typed, bounded live-control documents; the daemon owns all actual grants.
use crate::*;
use cshell_domain::{ProfileId, ProfileKind, SessionId, TaskRunId};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptTaskBinding {
    pub name: String,
    pub run: TaskRunId,
    pub revision: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptLiveScope {
    pub session: Option<SessionId>,
    pub secret_parameters: BTreeSet<String>,
    pub tasks: Vec<ScriptTaskBinding>,
    pub trigger_event: Option<u64>,
    #[serde(default)]
    pub trigger_on_next_match: bool,
}
impl Default for ScriptLiveScope {
    fn default() -> Self {
        Self {
            session: None,
            secret_parameters: BTreeSet::new(),
            tasks: Vec::new(),
            trigger_event: None,
            trigger_on_next_match: false,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptProfileSummary {
    pub id: ProfileId,
    pub name: String,
    pub kind: ProfileKind,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptLiveProposal {
    pub document: ScriptDocument,
    pub selected: BTreeSet<ScriptCapability>,
    pub scope: ScriptLiveScope,
}
impl Drop for ScriptLiveProposal {
    fn drop(&mut self) {
        self.document.source.zeroize();
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptLiveReview {
    pub version: u32,
    pub review: ScriptRunId,
    pub plan: cshell_application::ScriptPermissionPlan,
    pub scope: ScriptLiveScope,
    pub profile: Option<ScriptProfileSummary>,
    pub catalog_revision: u64,
    pub authentication_generation: u64,
    pub first_output_sequence: u64,
    pub trigger_run: Option<TaskRunId>,
    pub expires_after_ms: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptLiveState {
    Armed,
    Running,
    Revoked,
    Finished,
    Unknown,
    Expired,
    Cancelled,
}
impl ScriptLiveState {
    pub fn active(self) -> bool {
        matches!(self, Self::Armed | Self::Running | Self::Revoked)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptLiveView {
    pub review: ScriptRunId,
    pub run: ScriptRunId,
    pub document_sha256: [u8; 32],
    pub state: ScriptLiveState,
    pub report: Option<ScriptRunReport>,
    pub failed: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScriptControlCommand {
    Preview {
        proposal: ScriptLiveProposal,
    },
    Start {
        review: ScriptRunId,
        parameters: ScriptParameterValues,
        confirmed: bool,
    },
    Get {
        run: ScriptRunId,
    },
    Revoke {
        run: ScriptRunId,
    },
    List,
    Recover {
        confirmed: bool,
    },
}
impl std::fmt::Debug for ScriptControlCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptControlCommand(..)")
    }
}
impl ScriptControlCommand {
    pub fn operation(&self) -> i32 {
        match self {
            Self::Preview { .. } => 0,
            Self::Start { .. } => 1,
            Self::Get { .. } => 2,
            Self::Revoke { .. } => 3,
            Self::List => 4,
            Self::Recover { .. } => 5,
        }
    }
    pub fn request(&self) -> Result<cshell_ipc::ScriptControlRequest, ScriptWorkerError> {
        let operation = self.operation();
        let document_json = serde_json::to_vec(self).map_err(|_| ScriptWorkerError::Invalid)?;
        if document_json.len() > 256 * 1024 {
            return Err(ScriptWorkerError::Invalid);
        }
        Ok(cshell_ipc::ScriptControlRequest {
            operation,
            document_json,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ScriptControlReply {
    Review(ScriptLiveReview),
    Run(ScriptLiveView),
    Runs(Vec<ScriptLiveView>),
    Recovery(ScriptRecoveryReport),
}
pub fn validate_script_control_reply(
    command: &ScriptControlCommand,
    reply: &ScriptControlReply,
) -> Result<(), ScriptWorkerError> {
    let valid_view = |v: &ScriptLiveView| {
        !v.run.as_uuid().is_nil()
            && !v.review.as_uuid().is_nil()
            && v.report.as_ref().is_none_or(|r| {
                r.version == WORKER_VERSION
                    && r.format == "cshell.script-run-report"
                    && r.run_id == v.run
                    && r.document_sha256 == v.document_sha256
                    && r.operations <= 256
                    && r.notifications <= r.operations
            })
    };
    let valid = match (command, reply) {
        (ScriptControlCommand::Preview { proposal }, ScriptControlReply::Review(review)) => {
            review.version == 1
                && !review.review.as_uuid().is_nil()
                && (review.scope.trigger_on_next_match == review.trigger_run.is_some())
                && review.trigger_run.is_none_or(|r| !r.as_uuid().is_nil())
                && review.expires_after_ms == 30_000
                && review.scope == proposal.scope
                && validate_live_proposal(proposal).is_ok_and(|plan| plan == review.plan)
        }
        (ScriptControlCommand::Start { review, .. }, ScriptControlReply::Run(view)) => {
            view.review == *review && valid_view(view)
        }
        (
            ScriptControlCommand::Get { run } | ScriptControlCommand::Revoke { run },
            ScriptControlReply::Run(view),
        ) => view.run == *run && valid_view(view),
        (ScriptControlCommand::List, ScriptControlReply::Runs(views)) => {
            let mut ids = BTreeSet::new();
            views.len() <= 64 && views.iter().all(|v| valid_view(v) && ids.insert(v.run))
        }
        (ScriptControlCommand::Recover { .. }, ScriptControlReply::Recovery(r)) => {
            r.version == 1
                && u16::from(r.busy) + u16::from(r.recovered) + u16::from(r.unresolved) <= 2
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ScriptWorkerError::Protocol)
    }
}

pub fn validate_live_proposal(
    proposal: &ScriptLiveProposal,
) -> Result<cshell_application::ScriptPermissionPlan, ScriptWorkerError> {
    let plan = cshell_application::plan_script_permissions(&proposal.document, &proposal.selected)
        .map_err(|_| ScriptWorkerError::Invalid)?;
    if !plan.missing.is_empty()
        || proposal.scope.tasks.len() > 8
        || proposal
            .scope
            .session
            .is_some_and(|id| id.as_uuid().is_nil())
        || proposal.scope.trigger_event == Some(0)
    {
        return Err(ScriptWorkerError::Invalid);
    }
    let has = |c| proposal.selected.contains(&c);
    if proposal.scope.trigger_event.is_some() && proposal.scope.session.is_none() {
        return Err(ScriptWorkerError::Invalid);
    }
    if proposal.scope.trigger_on_next_match
        && (proposal.scope.trigger_event.is_some() || proposal.scope.session.is_none())
    {
        return Err(ScriptWorkerError::Invalid);
    }
    if (has(ScriptCapability::TerminalRead)
        || has(ScriptCapability::TerminalWrite)
        || has(ScriptCapability::ProfileRead))
        && proposal.scope.session.is_none()
    {
        return Err(ScriptWorkerError::Capability);
    }
    if !proposal.scope.tasks.is_empty() && !has(ScriptCapability::TaskSubmit)
        || !proposal.scope.secret_parameters.is_empty() && !has(ScriptCapability::TerminalWrite)
    {
        return Err(ScriptWorkerError::Capability);
    }
    if proposal.scope.secret_parameters.iter().any(|name| {
        proposal.document.parameters.get(name) != Some(&cshell_domain::ScriptParameterKind::Secret)
    }) {
        return Err(ScriptWorkerError::Parameters);
    }
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for task in &proposal.scope.tasks {
        if task.name.is_empty()
            || task.name.len() > 64
            || task.revision == 0
            || task.run.as_uuid().is_nil()
            || !names.insert(&task.name)
            || !ids.insert(task.run)
            || !task
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(ScriptWorkerError::Invalid);
        }
    }
    Ok(plan)
}
