//! Explicit, ephemeral synchronized input. Ports never wait for remote writers.

use cshell_domain::{
    ConnectionId, InputAction, KeyCode, SessionId, SyncInputGroupId, SyncInputSendId,
    SyncInputSnapshotId, TabGroupId, TabId, WorkspaceWindowId,
};
use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};
mod current;
pub use current::*;

pub const MAX_SYNC_INPUT_TARGETS: usize = 256;
pub const MAX_SYNC_INPUT_BYTES: usize = 1024 * 1024;
pub const SYNC_INPUT_CONFIRM_TARGETS: usize = 10;
pub const SYNC_INPUT_REVIEW_LIFETIME: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncInputTargetKind {
    Ssh,
    Local,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncInputEligibility {
    Ready,
    Disconnected,
    Closed,
    SecureInput,
    Locked,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenedSyncInputTarget {
    pub tab_id: TabId,
    pub window_id: WorkspaceWindowId,
    pub group_id: TabGroupId,
    pub session_id: SessionId,
    pub kind: SyncInputTargetKind,
    pub title: String,
    pub eligibility: SyncInputEligibility,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncInputTarget {
    pub tab_id: TabId,
    pub session_id: SessionId,
    pub kind: SyncInputTargetKind,
    pub title: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncInputSelection {
    Manual,
    CurrentWindow(WorkspaceWindowId),
    CurrentTabGroup(TabGroupId),
}

#[derive(Clone, Debug)]
pub struct SyncInputSnapshot {
    group_id: SyncInputGroupId,
    id: SyncInputSnapshotId,
    targets: Arc<[SyncInputTarget]>,
    selection: SyncInputSelection,
}
impl SyncInputSnapshot {
    #[must_use]
    pub const fn group_id(&self) -> SyncInputGroupId {
        self.group_id
    }
    #[must_use]
    pub const fn id(&self) -> SyncInputSnapshotId {
        self.id
    }
    #[must_use]
    pub fn targets(&self) -> &[SyncInputTarget] {
        &self.targets
    }
    #[must_use]
    pub const fn selection(&self) -> SyncInputSelection {
        self.selection
    }
}

#[derive(Debug)]
pub struct SyncInputArmReview {
    owner: ConnectionId,
    revision: SyncInputSnapshotId,
    snapshot: SyncInputSnapshot,
    created: Instant,
}
impl SyncInputArmReview {
    #[must_use]
    pub fn targets(&self) -> &[SyncInputTarget] {
        self.snapshot.targets()
    }
    #[must_use]
    pub fn requires_confirmation(&self) -> bool {
        self.targets().len() > SYNC_INPUT_CONFIRM_TARGETS
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncInputApproval {
    Explicit,
    Confirmed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncInputContext {
    TerminalCommit,
    ImePreedit,
    ApplicationShortcut,
    SecureInput,
    Locked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncInputKind {
    Text,
    Key,
    Paste,
    Compose,
    Control,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncInputRisk {
    pub multiline: bool,
    pub control_characters: bool,
    pub destructive_command: bool,
}
impl SyncInputRisk {
    #[must_use]
    pub const fn requires_confirmation(self) -> bool {
        self.multiline || self.control_characters || self.destructive_command
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncInputDelivery {
    QueueAccepted,
    Backpressure,
    Disconnected,
    Closed,
    SecureInput,
    Locked,
    UnknownSecurityState,
    TargetChanged,
    InvalidAction,
    Failed,
    SourceBlocked,
    UnsupportedInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncInputTargetDelivery {
    pub tab_id: TabId,
    pub session_id: SessionId,
    pub delivery: SyncInputDelivery,
}

#[derive(Debug)]
pub struct SyncInputDispatchReceipt {
    pub send_id: SyncInputSendId,
    pub snapshot: SyncInputSnapshot,
    pub deliveries: Vec<SyncInputTargetDelivery>,
    pub audit_complete: bool,
}

/// Implementations expose only already opened interactive tabs. `try_submit`
/// must atomically check source and target identity/security state and use a bounded,
/// nonblocking target queue. It must encode against the target's own modes.
pub trait SyncInputPort {
    fn opened_target(&self, tab_id: TabId) -> Option<OpenedSyncInputTarget>;
    fn try_submit(
        &mut self,
        source: &SyncInputTarget,
        expected: &SyncInputTarget,
        action: &InputAction,
    ) -> SyncInputDelivery;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncInputDisarmReason {
    User,
    FocusLost,
    SecureInput,
    SourceUnavailable,
    AuditFailure,
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncInputAuditOperation {
    Armed,
    Disarmed(SyncInputDisarmReason),
    InputPlanned {
        send_id: SyncInputSendId,
        kind: SyncInputKind,
        bytes: usize,
    },
    InputDispatched {
        send_id: SyncInputSendId,
        deliveries: Vec<SyncInputTargetDelivery>,
    },
}

/// Deliberately contains no title, input text, key value, encoded bytes or hash.
#[derive(Clone, Debug)]
pub struct SyncInputAuditEvent {
    pub operator: ConnectionId,
    pub time: SystemTime,
    pub group_id: Option<SyncInputGroupId>,
    pub snapshot_id: Option<SyncInputSnapshotId>,
    pub selection: Option<SyncInputSelection>,
    pub targets: Vec<(TabId, SessionId)>,
    pub operation: SyncInputAuditOperation,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("synchronized input metadata audit is unavailable")]
pub struct SyncInputAuditError;

pub trait SyncInputAuditSink {
    fn record(&mut self, event: SyncInputAuditEvent) -> Result<(), SyncInputAuditError>;
}

struct PendingAction(InputAction);
impl Drop for PendingAction {
    fn drop(&mut self) {
        match &mut self.0 {
            InputAction::Text(text)
            | InputAction::Paste { text, .. }
            | InputAction::Compose { text, .. } => text.zeroize(),
            InputAction::Key(key) => {
                if let KeyCode::Character(text) = &mut key.code {
                    text.zeroize();
                }
            }
            _ => {}
        }
    }
}

/// Exact one-use payload capability, intentionally neither Clone nor Serialize.
pub struct PreparedSyncInput {
    owner: ConnectionId,
    source: TabId,
    snapshot: SyncInputSnapshot,
    action: PendingAction,
    kind: SyncInputKind,
    bytes: usize,
    risk: SyncInputRisk,
    created: Instant,
}
impl fmt::Debug for PreparedSyncInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedSyncInput")
            .field("snapshot", &self.snapshot.id)
            .field("kind", &self.kind)
            .field("bytes", &self.bytes)
            .field("risk", &self.risk)
            .field("action", &"[REDACTED]")
            .finish()
    }
}
impl PreparedSyncInput {
    #[must_use]
    pub fn snapshot(&self) -> &SyncInputSnapshot {
        &self.snapshot
    }
    /// Only for the transient confirmation UI. Never send this to diagnostics.
    #[must_use]
    pub fn action_for_review(&self) -> &InputAction {
        &self.action.0
    }
    #[must_use]
    pub const fn risk(&self) -> SyncInputRisk {
        self.risk
    }
    #[must_use]
    pub const fn kind(&self) -> SyncInputKind {
        self.kind
    }
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SyncInputError {
    #[error("synchronized input is already armed")]
    AlreadyArmed,
    #[error("synchronized input is disarmed")]
    Disarmed,
    #[error("select between one and 256 opened terminal tabs")]
    TargetCount,
    #[error("a tab or runtime session was selected more than once")]
    DuplicateTarget,
    #[error("local terminals require individual manual selection")]
    LocalRequiresManualSelection,
    #[error("SSH and local terminals cannot share a synchronized input group")]
    MixedTargetKinds,
    #[error("an opened terminal is unavailable or ineligible")]
    TargetIneligible,
    #[error("a selected tab is outside the chosen window or tab group")]
    TargetOutsideSelection,
    #[error("the review no longer identifies the same controller or target sessions")]
    StaleReview,
    #[error("the synchronized input review expired")]
    ReviewExpired,
    #[error("explicit secondary confirmation is required")]
    ConfirmationRequired,
    #[error("input must originate in an armed terminal tab")]
    SourceNotArmed,
    #[error("only committed, unlocked, nonsecure terminal input may be broadcast")]
    InputContextBlocked,
    #[error("the logical input action is invalid or exceeds the one MiB limit")]
    InvalidAction,
    #[error(transparent)]
    Audit(#[from] SyncInputAuditError),
}

/// Own one per GUI/CLI attachment. Drop/disarm clears all live capabilities.
pub struct SyncInputController<P: SyncInputPort, A: SyncInputAuditSink> {
    operator: ConnectionId,
    revision: SyncInputSnapshotId,
    armed: Option<SyncInputSnapshot>,
    port: P,
    audit: A,
}
impl<P: SyncInputPort, A: SyncInputAuditSink> fmt::Debug for SyncInputController<P, A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SyncInputController")
            .field("operator", &self.operator)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}
impl<P: SyncInputPort, A: SyncInputAuditSink> SyncInputController<P, A> {
    #[must_use]
    pub fn new(operator: ConnectionId, port: P, audit: A) -> Self {
        Self {
            operator,
            revision: SyncInputSnapshotId::new(),
            armed: None,
            port,
            audit,
        }
    }
    #[must_use]
    pub fn armed_snapshot(&self) -> Option<&SyncInputSnapshot> {
        self.armed.as_ref()
    }
    #[must_use]
    pub fn port(&self) -> &P {
        &self.port
    }
    pub fn port_mut(&mut self) -> &mut P {
        &mut self.port
    }
    #[must_use]
    pub fn audit(&self) -> &A {
        &self.audit
    }

    pub fn review_arm(
        &self,
        selection: SyncInputSelection,
        tabs: &[TabId],
    ) -> Result<SyncInputArmReview, SyncInputError> {
        if self.armed.is_some() {
            return Err(SyncInputError::AlreadyArmed);
        }
        if tabs.is_empty() || tabs.len() > MAX_SYNC_INPUT_TARGETS {
            return Err(SyncInputError::TargetCount);
        }
        let mut targets = Vec::with_capacity(tabs.len());
        let mut seen_tabs = std::collections::BTreeSet::new();
        let mut seen_sessions = std::collections::BTreeSet::new();
        for tab in tabs {
            let opened = self
                .port
                .opened_target(*tab)
                .ok_or(SyncInputError::TargetIneligible)?;
            if opened.tab_id != *tab
                || opened.eligibility != SyncInputEligibility::Ready
                || opened.title.len() > 4096
            {
                return Err(SyncInputError::TargetIneligible);
            }
            if !selection_contains(selection, &opened) {
                return Err(SyncInputError::TargetOutsideSelection);
            }
            if !seen_tabs.insert(*tab) || !seen_sessions.insert(opened.session_id) {
                return Err(SyncInputError::DuplicateTarget);
            }
            if opened.kind == SyncInputTargetKind::Local && selection != SyncInputSelection::Manual
            {
                return Err(SyncInputError::LocalRequiresManualSelection);
            }
            if targets
                .first()
                .is_some_and(|first: &SyncInputTarget| first.kind != opened.kind)
            {
                return Err(SyncInputError::MixedTargetKinds);
            }
            targets.push(SyncInputTarget {
                tab_id: *tab,
                session_id: opened.session_id,
                kind: opened.kind,
                title: opened.title,
            });
        }
        Ok(SyncInputArmReview {
            owner: self.operator,
            revision: self.revision,
            snapshot: SyncInputSnapshot {
                group_id: SyncInputGroupId::new(),
                id: SyncInputSnapshotId::new(),
                targets: targets.into(),
                selection,
            },
            created: Instant::now(),
        })
    }

    pub fn arm(
        &mut self,
        review: SyncInputArmReview,
        approval: SyncInputApproval,
    ) -> Result<(), SyncInputError> {
        if self.armed.is_some() {
            return Err(SyncInputError::AlreadyArmed);
        }
        if review.owner != self.operator || review.revision != self.revision {
            return Err(SyncInputError::StaleReview);
        }
        if review.created.elapsed() >= SYNC_INPUT_REVIEW_LIFETIME {
            return Err(SyncInputError::ReviewExpired);
        }
        if review.requires_confirmation() && approval != SyncInputApproval::Confirmed {
            return Err(SyncInputError::ConfirmationRequired);
        }
        for target in review.targets() {
            let opened = self
                .port
                .opened_target(target.tab_id)
                .ok_or(SyncInputError::StaleReview)?;
            if opened.tab_id != target.tab_id
                || opened.session_id != target.session_id
                || opened.kind != target.kind
                || opened.eligibility != SyncInputEligibility::Ready
                || !selection_contains(review.snapshot.selection, &opened)
            {
                return Err(SyncInputError::StaleReview);
            }
        }
        self.record(Some(&review.snapshot), SyncInputAuditOperation::Armed)?;
        self.armed = Some(review.snapshot);
        self.revision = SyncInputSnapshotId::new();
        Ok(())
    }

    pub fn disarm(&mut self, reason: SyncInputDisarmReason) -> Result<(), SyncInputError> {
        self.revision = SyncInputSnapshotId::new();
        let snapshot = self.armed.take(); // Safety state clears even if the audit fails.
        if let Some(snapshot) = snapshot {
            self.record(Some(&snapshot), SyncInputAuditOperation::Disarmed(reason))?;
        }
        Ok(())
    }

    pub fn prepare_input(
        &mut self,
        source: TabId,
        action: InputAction,
        context: SyncInputContext,
        destructive_hint: bool,
    ) -> Result<PreparedSyncInput, SyncInputError> {
        let action = PendingAction(action);
        if context != SyncInputContext::TerminalCommit {
            if matches!(
                context,
                SyncInputContext::SecureInput | SyncInputContext::Locked
            ) {
                self.disarm(SyncInputDisarmReason::SecureInput)?;
            }
            return Err(SyncInputError::InputContextBlocked);
        }
        let snapshot = self.armed.as_ref().ok_or(SyncInputError::Disarmed)?.clone();
        self.validate_source(source, &snapshot)?;
        let (kind, bytes, mut risk) = inspect_action(&action.0)?;
        risk.destructive_command |= destructive_hint;
        Ok(PreparedSyncInput {
            owner: self.operator,
            source,
            snapshot,
            action,
            kind,
            bytes,
            risk,
            created: Instant::now(),
        })
    }

    pub fn dispatch(
        &mut self,
        prepared: PreparedSyncInput,
        approval: SyncInputApproval,
    ) -> Result<SyncInputDispatchReceipt, SyncInputError> {
        let current = self.armed.as_ref().ok_or(SyncInputError::Disarmed)?;
        if prepared.owner != self.operator || current.id != prepared.snapshot.id {
            return Err(SyncInputError::StaleReview);
        }
        if prepared.created.elapsed() >= SYNC_INPUT_REVIEW_LIFETIME {
            return Err(SyncInputError::ReviewExpired);
        }
        if prepared.risk.requires_confirmation() && approval != SyncInputApproval::Confirmed {
            return Err(SyncInputError::ConfirmationRequired);
        }
        self.validate_source(prepared.source, &prepared.snapshot)?;
        let send_id = SyncInputSendId::new();
        if let Err(error) = self.record(
            Some(&prepared.snapshot),
            SyncInputAuditOperation::InputPlanned {
                send_id,
                kind: prepared.kind,
                bytes: prepared.bytes,
            },
        ) {
            let _ = self.disarm(SyncInputDisarmReason::AuditFailure);
            return Err(error);
        }
        let targets = prepared.snapshot.targets();
        let source_index = targets
            .iter()
            .position(|target| target.tab_id == prepared.source)
            .ok_or(SyncInputError::SourceNotArmed)?;
        let source = &targets[source_index];
        let mut statuses = vec![SyncInputDelivery::SourceBlocked; targets.len()];
        let mut source_blocked = false;
        // Check and submit the source first, regardless of selection order.
        // A security transition stops remaining targets without undoing input
        // already queued while the source was eligible.
        for index in std::iter::once(source_index)
            .chain((0..targets.len()).filter(|index| *index != source_index))
        {
            let target = &targets[index];
            if source_blocked || self.current_delivery(source) != SyncInputDelivery::QueueAccepted {
                source_blocked = true;
                continue;
            }
            let checked = self.current_delivery(target);
            let delivery = if checked == SyncInputDelivery::QueueAccepted {
                self.port.try_submit(source, target, &prepared.action.0)
            } else {
                checked
            };
            statuses[index] = delivery;
            source_blocked |= delivery == SyncInputDelivery::SourceBlocked
                || (index == source_index
                    && !matches!(
                        delivery,
                        SyncInputDelivery::QueueAccepted | SyncInputDelivery::Backpressure
                    ));
        }
        let deliveries = targets
            .iter()
            .zip(statuses)
            .map(|(target, delivery)| SyncInputTargetDelivery {
                tab_id: target.tab_id,
                session_id: target.session_id,
                delivery,
            })
            .collect::<Vec<_>>();
        let mut audit_complete = self
            .record(
                Some(&prepared.snapshot),
                SyncInputAuditOperation::InputDispatched {
                    send_id,
                    deliveries: deliveries.clone(),
                },
            )
            .is_ok();
        if !audit_complete {
            let _ = self.disarm(SyncInputDisarmReason::AuditFailure);
        } else if source_blocked {
            audit_complete = self
                .disarm(SyncInputDisarmReason::SourceUnavailable)
                .is_ok();
        }
        Ok(SyncInputDispatchReceipt {
            send_id,
            snapshot: prepared.snapshot.clone(),
            deliveries,
            audit_complete,
        })
    }

    fn validate_source(
        &mut self,
        source: TabId,
        snapshot: &SyncInputSnapshot,
    ) -> Result<(), SyncInputError> {
        let target = snapshot
            .targets()
            .iter()
            .find(|target| target.tab_id == source)
            .ok_or(SyncInputError::SourceNotArmed)?;
        match self.current_delivery(target) {
            SyncInputDelivery::QueueAccepted => Ok(()),
            SyncInputDelivery::SecureInput
            | SyncInputDelivery::Locked
            | SyncInputDelivery::UnknownSecurityState => {
                self.disarm(SyncInputDisarmReason::SecureInput)?;
                Err(SyncInputError::InputContextBlocked)
            }
            _ => {
                self.disarm(SyncInputDisarmReason::SourceUnavailable)?;
                Err(SyncInputError::StaleReview)
            }
        }
    }

    fn current_delivery(&self, expected: &SyncInputTarget) -> SyncInputDelivery {
        let Some(current) = self.port.opened_target(expected.tab_id) else {
            return SyncInputDelivery::Closed;
        };
        if current.tab_id != expected.tab_id
            || current.session_id != expected.session_id
            || current.kind != expected.kind
        {
            return SyncInputDelivery::TargetChanged;
        }
        match current.eligibility {
            SyncInputEligibility::Ready => SyncInputDelivery::QueueAccepted,
            SyncInputEligibility::Disconnected => SyncInputDelivery::Disconnected,
            SyncInputEligibility::Closed => SyncInputDelivery::Closed,
            SyncInputEligibility::SecureInput => SyncInputDelivery::SecureInput,
            SyncInputEligibility::Locked => SyncInputDelivery::Locked,
            SyncInputEligibility::Unknown => SyncInputDelivery::UnknownSecurityState,
        }
    }

    fn record(
        &mut self,
        snapshot: Option<&SyncInputSnapshot>,
        operation: SyncInputAuditOperation,
    ) -> Result<(), SyncInputError> {
        self.audit.record(SyncInputAuditEvent {
            operator: self.operator,
            time: SystemTime::now(),
            group_id: snapshot.map(SyncInputSnapshot::group_id),
            snapshot_id: snapshot.map(SyncInputSnapshot::id),
            selection: snapshot.map(SyncInputSnapshot::selection),
            targets: snapshot.map_or_else(Vec::new, |snapshot| {
                snapshot
                    .targets()
                    .iter()
                    .map(|target| (target.tab_id, target.session_id))
                    .collect()
            }),
            operation,
        })?;
        Ok(())
    }
}
impl<P: SyncInputPort, A: SyncInputAuditSink> Drop for SyncInputController<P, A> {
    fn drop(&mut self) {
        let _ = self.disarm(SyncInputDisarmReason::Shutdown);
    }
}

fn selection_contains(selection: SyncInputSelection, opened: &OpenedSyncInputTarget) -> bool {
    match selection {
        SyncInputSelection::Manual => true,
        SyncInputSelection::CurrentWindow(window) => window == opened.window_id,
        SyncInputSelection::CurrentTabGroup(group) => group == opened.group_id,
    }
}

fn inspect_action(
    action: &InputAction,
) -> Result<(SyncInputKind, usize, SyncInputRisk), SyncInputError> {
    let (kind, text) = match action {
        InputAction::Text(text) => (SyncInputKind::Text, Some(text.as_str())),
        InputAction::Paste { text, .. } => (SyncInputKind::Paste, Some(text.as_str())),
        InputAction::Compose { text, .. } => (SyncInputKind::Compose, Some(text.as_str())),
        InputAction::Key(key) => {
            match &key.code {
                KeyCode::Character(text)
                    if !text.is_empty()
                        && text.len() <= MAX_SYNC_INPUT_BYTES
                        && !text.chars().any(char::is_control) => {}
                KeyCode::Character(_) => return Err(SyncInputError::InvalidAction),
                KeyCode::Function(key) if !(1..=24).contains(key) => {
                    return Err(SyncInputError::InvalidAction);
                }
                KeyCode::Enter
                | KeyCode::Tab
                | KeyCode::Backspace
                | KeyCode::Escape
                | KeyCode::ArrowUp
                | KeyCode::ArrowDown
                | KeyCode::ArrowLeft
                | KeyCode::ArrowRight
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Insert
                | KeyCode::Delete
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Function(_) => {}
                _ => return Err(SyncInputError::InvalidAction),
            }
            let bytes = match &key.code {
                KeyCode::Character(text) => text.len(),
                _ => 0,
            };
            return Ok((SyncInputKind::Key, bytes, SyncInputRisk::default()));
        }
        InputAction::Control(_) => {
            return Ok((SyncInputKind::Control, 0, SyncInputRisk::default()));
        }
        _ => return Err(SyncInputError::InvalidAction),
    };
    let text = text.ok_or(SyncInputError::InvalidAction)?;
    if text.is_empty() || text.len() > MAX_SYNC_INPUT_BYTES {
        return Err(SyncInputError::InvalidAction);
    }
    let lower = Zeroizing::new(text.to_ascii_lowercase());
    let destructive_command = lower
        .split(|c: char| c.is_whitespace() || ";|&()".contains(c))
        .any(|word| {
            matches!(
                word,
                "rm" | "mkfs"
                    | "dd"
                    | "shutdown"
                    | "reboot"
                    | "poweroff"
                    | "format"
                    | "del"
                    | "rd"
                    | "remove-item"
                    | "invoke-expression"
            ) || word.starts_with("mkfs.")
        });
    Ok((
        kind,
        text.len()
            + usize::from(matches!(
                action,
                InputAction::Compose {
                    append_enter: true,
                    ..
                }
            )),
        SyncInputRisk {
            multiline: text.contains(['\n', '\r']),
            // append_enter is an explicit send option, not hidden text content.
            // Embedded controls, multiline bodies and destructive commands still
            // require Confirmed approval in dispatch/dispatch_current.
            control_characters: text.chars().any(|c|c.is_control() || matches!(c,
                '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}'
            )),
            destructive_command,
        },
    ))
}

#[cfg(test)]
mod tests;
