//! One-shot reviewed input to the current terminal, independent of an armed group.
use super::*;

pub struct PreparedCurrentInput {
    owner: ConnectionId,
    revision: SyncInputSnapshotId,
    id: SyncInputSnapshotId,
    target: SyncInputTarget,
    action: PendingAction,
    kind: SyncInputKind,
    bytes: usize,
    risk: SyncInputRisk,
    pub(super) created: Instant,
}
impl fmt::Debug for PreparedCurrentInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedCurrentInput")
            .field("id", &self.id)
            .field("target", &self.target.tab_id)
            .field("action", &"[REDACTED]")
            .finish()
    }
}
impl PreparedCurrentInput {
    #[must_use]
    pub fn target(&self) -> &SyncInputTarget {
        &self.target
    }
    #[must_use]
    pub fn action_for_review(&self) -> &InputAction {
        &self.action.0
    }
    #[must_use]
    pub const fn risk(&self) -> SyncInputRisk {
        self.risk
    }
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
    #[must_use]
    pub const fn kind(&self) -> SyncInputKind {
        self.kind
    }
}

#[derive(Debug)]
pub struct CurrentInputDispatchReceipt {
    pub send_id: SyncInputSendId,
    pub snapshot_id: SyncInputSnapshotId,
    pub target: SyncInputTarget,
    pub delivery: SyncInputDelivery,
    pub audit_complete: bool,
}

impl<P: SyncInputPort, A: SyncInputAuditSink> SyncInputController<P, A> {
    /// Freezes only this current tab. It never changes or adds an armed target.
    pub fn prepare_current_input(
        &mut self,
        source: TabId,
        action: InputAction,
        context: SyncInputContext,
    ) -> Result<PreparedCurrentInput, SyncInputError> {
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
        let opened = self
            .port
            .opened_target(source)
            .filter(|target| {
                target.tab_id == source
                    && target.eligibility == SyncInputEligibility::Ready
                    && target.title.len() <= 4096
            })
            .ok_or(SyncInputError::TargetIneligible)?;
        let (kind, bytes, risk) = inspect_action(&action.0)?;
        Ok(PreparedCurrentInput {
            owner: self.operator,
            revision: self.revision,
            id: SyncInputSnapshotId::new(),
            target: SyncInputTarget {
                tab_id: source,
                session_id: opened.session_id,
                kind: opened.kind,
                title: opened.title,
            },
            action,
            kind,
            bytes,
            risk,
            created: Instant::now(),
        })
    }

    pub fn dispatch_current(
        &mut self,
        prepared: PreparedCurrentInput,
        approval: SyncInputApproval,
    ) -> Result<CurrentInputDispatchReceipt, SyncInputError> {
        if prepared.owner != self.operator || prepared.revision != self.revision {
            return Err(SyncInputError::StaleReview);
        }
        if prepared.created.elapsed() >= SYNC_INPUT_REVIEW_LIFETIME {
            return Err(SyncInputError::ReviewExpired);
        }
        if prepared.risk.requires_confirmation() && approval != SyncInputApproval::Confirmed {
            return Err(SyncInputError::ConfirmationRequired);
        }
        let source_ready = self
            .port
            .opened_target(prepared.target.tab_id)
            .is_some_and(|target| {
                target.session_id == prepared.target.session_id
                    && target.kind == prepared.target.kind
                    && target.eligibility == SyncInputEligibility::Ready
            });
        if !source_ready {
            self.disarm(SyncInputDisarmReason::SourceUnavailable)?;
            return Err(SyncInputError::TargetIneligible);
        }
        let send_id = SyncInputSendId::new();
        if let Err(error) = self.record_current(
            &prepared,
            SyncInputAuditOperation::InputPlanned {
                send_id,
                kind: prepared.kind,
                bytes: prepared.bytes,
            },
        ) {
            let _cleared = self.disarm(SyncInputDisarmReason::AuditFailure);
            return Err(error);
        }
        let delivery = self
            .port
            .try_submit(&prepared.target, &prepared.target, &prepared.action.0);
        let audit_complete = self
            .record_current(
                &prepared,
                SyncInputAuditOperation::InputDispatched {
                    send_id,
                    deliveries: vec![SyncInputTargetDelivery {
                        tab_id: prepared.target.tab_id,
                        session_id: prepared.target.session_id,
                        delivery,
                    }],
                },
            )
            .is_ok();
        if !audit_complete {
            let _cleared = self.disarm(SyncInputDisarmReason::AuditFailure);
        } else if !matches!(
            delivery,
            SyncInputDelivery::QueueAccepted
                | SyncInputDelivery::Backpressure
                | SyncInputDelivery::UnsupportedInput
        ) {
            let _cleared = self.disarm(SyncInputDisarmReason::SourceUnavailable);
        }
        Ok(CurrentInputDispatchReceipt {
            send_id,
            snapshot_id: prepared.id,
            target: prepared.target.clone(),
            delivery,
            audit_complete,
        })
    }

    fn record_current(
        &mut self,
        prepared: &PreparedCurrentInput,
        operation: SyncInputAuditOperation,
    ) -> Result<(), SyncInputError> {
        self.audit.record(SyncInputAuditEvent {
            operator: self.operator,
            time: SystemTime::now(),
            group_id: None,
            snapshot_id: Some(prepared.id),
            selection: Some(SyncInputSelection::Manual),
            targets: vec![(prepared.target.tab_id, prepared.target.session_id)],
            operation,
        })?;
        Ok(())
    }
}
