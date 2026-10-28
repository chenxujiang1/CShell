use super::*;
use cshell_domain::{ControlAction, KeyEvent, Modifiers};
use cshell_terminal::{InputEncoder, TerminalModes};
use std::collections::{BTreeMap, VecDeque};

trait FixtureValue<T> {
    fn checked(self) -> T;
}
impl<T> FixtureValue<T> for Option<T> {
    fn checked(self) -> T {
        match self {
            Some(value) => value,
            None => panic!("required fixture value is missing"),
        }
    }
}
impl<T, E: std::fmt::Debug> FixtureValue<T> for Result<T, E> {
    fn checked(self) -> T {
        match self {
            Ok(value) => value,
            Err(error) => panic!("fixture operation failed: {error:?}"),
        }
    }
}

#[derive(Default)]
struct Port {
    opened: BTreeMap<TabId, OpenedSyncInputTarget>,
    modes: BTreeMap<SessionId, TerminalModes>,
    queues: BTreeMap<SessionId, VecDeque<Vec<u8>>>,
    capacity: usize,
    change_during_submit: Option<TabId>,
    block_source_when_submitting: Option<TabId>,
}
impl SyncInputPort for Port {
    fn opened_target(&self, tab: TabId) -> Option<OpenedSyncInputTarget> {
        self.opened.get(&tab).cloned()
    }
    fn try_submit(
        &mut self,
        source: &SyncInputTarget,
        expected: &SyncInputTarget,
        action: &InputAction,
    ) -> SyncInputDelivery {
        // Exercise the port's final security check after the controller's
        // inspection, as production queue adapters must do under their lock.
        if self.block_source_when_submitting == Some(expected.tab_id) {
            self.opened.get_mut(&source.tab_id).checked().eligibility =
                SyncInputEligibility::SecureInput;
        }
        let Some(current_source) = self.opened.get(&source.tab_id) else {
            return SyncInputDelivery::SourceBlocked;
        };
        if current_source.session_id != source.session_id
            || current_source.eligibility != SyncInputEligibility::Ready
        {
            return SyncInputDelivery::SourceBlocked;
        }
        if self.change_during_submit == Some(expected.tab_id) {
            self.opened.get_mut(&expected.tab_id).checked().eligibility =
                SyncInputEligibility::SecureInput;
        }
        let Some(opened) = self.opened.get(&expected.tab_id) else {
            return SyncInputDelivery::Closed;
        };
        if opened.session_id != expected.session_id {
            return SyncInputDelivery::TargetChanged;
        }
        if opened.eligibility != SyncInputEligibility::Ready {
            return if expected.tab_id == source.tab_id {
                SyncInputDelivery::SourceBlocked
            } else {
                SyncInputDelivery::SecureInput
            };
        }
        let bytes = match InputEncoder::encode(action, self.modes[&expected.session_id]) {
            Ok(bytes) => bytes,
            Err(_) => return SyncInputDelivery::InvalidAction,
        };
        if bytes.is_empty() {
            return SyncInputDelivery::QueueAccepted;
        }
        let queue = self.queues.entry(expected.session_id).or_default();
        if queue.len() >= self.capacity {
            return SyncInputDelivery::Backpressure;
        }
        queue.push_back(bytes);
        SyncInputDelivery::QueueAccepted
    }
}

#[derive(Default)]
struct Audit {
    events: Vec<SyncInputAuditEvent>,
    fail_at: Option<usize>,
    calls: usize,
}
impl SyncInputAuditSink for Audit {
    fn record(&mut self, event: SyncInputAuditEvent) -> Result<(), SyncInputAuditError> {
        let call = self.calls;
        self.calls += 1;
        if self.fail_at == Some(call) {
            return Err(SyncInputAuditError);
        }
        self.events.push(event);
        Ok(())
    }
}
type Controller = SyncInputController<Port, Audit>;

#[test]
fn selected_compose_encodes_each_destination_mode_with_enter_outside_paste() {
    let (mut controller, tabs) = fixture(3, SyncInputTargetKind::Local);
    let first = controller.port().opened[&tabs[0]].session_id;
    let second = controller.port().opened[&tabs[1]].session_id;
    let third = controller.port().opened[&tabs[2]].session_id;
    controller
        .port_mut()
        .modes
        .get_mut(&second)
        .checked()
        .bracketed_paste = true;
    *controller.port_mut().modes.get_mut(&third).checked() = TerminalModes {
        bracketed_paste: true,
        kitty_keyboard_flags: TerminalModes::KITTY_REPORT_ALL_KEYS
            | TerminalModes::KITTY_REPORT_EVENTS,
        ..Default::default()
    };
    let review = controller
        .review_arm(SyncInputSelection::Manual, &tabs)
        .checked();
    controller
        .arm(review, SyncInputApproval::Confirmed)
        .checked();
    let prepared = controller
        .prepare_input(
            tabs[0],
            InputAction::Compose {
                text: "one\n\u{4e8c}".into(),
                append_enter: true,
            },
            SyncInputContext::TerminalCommit,
            false,
        )
        .checked();
    controller
        .dispatch(prepared, SyncInputApproval::Confirmed)
        .checked();
    assert_eq!(
        controller.port().queues[&first][0],
        "one\n\u{4e8c}\r".as_bytes()
    );
    assert_eq!(
        controller.port().queues[&second][0],
        "\x1b[200~one\n\u{4e8c}\x1b[201~\r".as_bytes()
    );
    assert_eq!(
        controller.port().queues[&third][0],
        "\x1b[200~one\n\u{4e8c}\x1b[201~\x1b[13;1u\x1b[13;1:3u".as_bytes()
    );
}

#[test]
fn compose_current_is_reviewed_once_and_does_not_broadcast_to_an_armed_group() {
    let (mut controller, tabs) = fixture(2, SyncInputTargetKind::Local);
    let review = controller
        .review_arm(SyncInputSelection::Manual, &tabs)
        .checked();
    controller
        .arm(review, SyncInputApproval::Confirmed)
        .checked();
    let session = controller.port().opened[&tabs[0]].session_id;
    controller
        .port_mut()
        .modes
        .get_mut(&session)
        .checked()
        .bracketed_paste = true;
    let prepared = controller
        .prepare_current_input(
            tabs[0],
            InputAction::Compose {
                text: "echo unicode \u{4e2d}\nsecond".into(),
                append_enter: true,
            },
            SyncInputContext::TerminalCommit,
        )
        .checked();
    assert!(prepared.risk().requires_confirmation());
    assert!(!format!("{prepared:?}").contains("unicode"));
    assert!(controller.port().queues.is_empty());
    let receipt = controller
        .dispatch_current(prepared, SyncInputApproval::Confirmed)
        .checked();
    assert_eq!(receipt.delivery, SyncInputDelivery::QueueAccepted);
    assert!(receipt.audit_complete);
    assert!(controller.armed_snapshot().is_some());
    assert_eq!(controller.port().queues.len(), 1);
    assert_eq!(
        controller.port().queues[&session][0],
        "\x1b[200~echo unicode \u{4e2d}\nsecond\x1b[201~\r".as_bytes()
    );
    let planned = controller
        .audit()
        .events
        .iter()
        .find(|event| {
            matches!(
                event.operation,
                SyncInputAuditOperation::InputPlanned { .. }
            )
        })
        .checked();
    assert_eq!(planned.group_id, None);
    assert_eq!(planned.targets, vec![(tabs[0], session)]);
}

#[test]
fn current_compose_rejects_security_identity_expiry_and_stale_or_foreign_review() {
    let (mut controller, tabs) = fixture(1, SyncInputTargetKind::Local);
    let action = || InputAction::Compose {
        text: "payload".into(),
        append_enter: false,
    };
    assert!(matches!(
        controller.prepare_current_input(tabs[0], action(), SyncInputContext::SecureInput),
        Err(SyncInputError::InputContextBlocked)
    ));
    let prepared = controller
        .prepare_current_input(tabs[0], action(), SyncInputContext::TerminalCommit)
        .checked();
    controller.disarm(SyncInputDisarmReason::User).checked();
    assert!(matches!(
        controller.dispatch_current(prepared, SyncInputApproval::Confirmed),
        Err(SyncInputError::StaleReview)
    ));
    let mut prepared = controller
        .prepare_current_input(tabs[0], action(), SyncInputContext::TerminalCommit)
        .checked();
    prepared.created = Instant::now() - SYNC_INPUT_REVIEW_LIFETIME;
    assert!(matches!(
        controller.dispatch_current(prepared, SyncInputApproval::Confirmed),
        Err(SyncInputError::ReviewExpired)
    ));
    let prepared = controller
        .prepare_current_input(tabs[0], action(), SyncInputContext::TerminalCommit)
        .checked();
    let (mut other, _) = fixture(1, SyncInputTargetKind::Local);
    assert!(matches!(
        other.dispatch_current(prepared, SyncInputApproval::Confirmed),
        Err(SyncInputError::StaleReview)
    ));
    let prepared = controller
        .prepare_current_input(tabs[0], action(), SyncInputContext::TerminalCommit)
        .checked();
    controller
        .port_mut()
        .opened
        .get_mut(&tabs[0])
        .checked()
        .session_id = SessionId::new();
    assert!(matches!(
        controller.dispatch_current(prepared, SyncInputApproval::Confirmed),
        Err(SyncInputError::TargetIneligible)
    ));
    assert!(controller.port().queues.is_empty());
}

#[test]
fn current_compose_confirmation_and_audit_failures_preserve_real_queue_outcome() {
    let (mut controller, tabs) = fixture(1, SyncInputTargetKind::Local);
    let action = || InputAction::Compose {
        text: "echo test\nwhoami".into(),
        append_enter: true,
    };
    let prepared = controller
        .prepare_current_input(tabs[0], action(), SyncInputContext::TerminalCommit)
        .checked();
    assert!(matches!(
        controller.dispatch_current(prepared, SyncInputApproval::Explicit),
        Err(SyncInputError::ConfirmationRequired)
    ));
    assert!(controller.port().queues.is_empty());
    controller.audit.fail_at = Some(0);
    let prepared = controller
        .prepare_current_input(tabs[0], action(), SyncInputContext::TerminalCommit)
        .checked();
    assert!(
        controller
            .dispatch_current(prepared, SyncInputApproval::Confirmed)
            .is_err()
    );
    assert!(controller.port().queues.is_empty());
    let (mut controller, tabs) = fixture(1, SyncInputTargetKind::Local);
    controller.audit.fail_at = Some(1);
    let prepared = controller
        .prepare_current_input(tabs[0], action(), SyncInputContext::TerminalCommit)
        .checked();
    let receipt = controller
        .dispatch_current(prepared, SyncInputApproval::Confirmed)
        .checked();
    assert_eq!(receipt.delivery, SyncInputDelivery::QueueAccepted);
    assert!(!receipt.audit_complete);
    assert_eq!(controller.port().queues.len(), 1);
}

#[test]
fn explicit_command_enter_is_direct_but_embedded_controls_and_risky_bodies_require_review() {
    let (mut controller,tabs)=fixture(2,SyncInputTargetKind::Ssh);
    let review=controller.review_arm(SyncInputSelection::Manual,&tabs).checked();
    controller.arm(review,SyncInputApproval::Confirmed).checked();
    let prepared=controller.prepare_input(tabs[0],InputAction::Compose{text:"hostname".into(),append_enter:true},SyncInputContext::TerminalCommit,false).checked();
    assert!(!prepared.risk().requires_confirmation());
    let receipt=controller.dispatch(prepared,SyncInputApproval::Explicit).checked();
    assert_eq!(receipt.deliveries.len(),2);
    for text in ["whoami\nhostname","echo x\rwhoami","echo \u{1b}[A","rm -rf /tmp/test","echo \u{202e}hidden","echo \u{200b}hidden","echo one\u{2028}two"] {
        let prepared=controller.prepare_input(tabs[0],InputAction::Compose{text:text.into(),append_enter:true},SyncInputContext::TerminalCommit,false).checked();
        assert!(prepared.risk().requires_confirmation());
        assert!(matches!(controller.dispatch(prepared,SyncInputApproval::Explicit),Err(SyncInputError::ConfirmationRequired)));
    }
}

fn fixture(count: usize, kind: SyncInputTargetKind) -> (Controller, Vec<TabId>) {
    let mut port = Port {
        capacity: 64,
        ..Default::default()
    };
    let mut tabs = Vec::new();
    let window_id = WorkspaceWindowId::new();
    let group_id = TabGroupId::new();
    for index in 0..count {
        let tab_id = TabId::new();
        let session_id = SessionId::new();
        tabs.push(tab_id);
        port.opened.insert(
            tab_id,
            OpenedSyncInputTarget {
                tab_id,
                window_id,
                group_id,
                session_id,
                kind,
                title: format!("target-{index}"),
                eligibility: SyncInputEligibility::Ready,
            },
        );
        port.modes.insert(session_id, TerminalModes::default());
    }
    (
        SyncInputController::new(ConnectionId::new(), port, Audit::default()),
        tabs,
    )
}
fn arm(controller: &mut Controller, tabs: &[TabId]) {
    let review = controller
        .review_arm(SyncInputSelection::Manual, tabs)
        .checked();
    controller
        .arm(review, SyncInputApproval::Confirmed)
        .checked();
}
fn send(
    controller: &mut Controller,
    source: TabId,
    action: InputAction,
) -> SyncInputDispatchReceipt {
    let prepared = controller
        .prepare_input(source, action, SyncInputContext::TerminalCommit, false)
        .checked();
    controller
        .dispatch(prepared, SyncInputApproval::Confirmed)
        .checked()
}
fn queue(controller: &Controller, tab: TabId) -> Vec<Vec<u8>> {
    let session = controller.port.opened[&tab].session_id;
    controller
        .port
        .queues
        .get(&session)
        .map_or_else(Vec::new, |queue| queue.iter().cloned().collect())
}
fn key(code: KeyCode, modifiers: Modifiers) -> InputAction {
    InputAction::Key(KeyEvent {
        code,
        modifiers,
        pressed: true,
        repeated: false,
    })
}

#[test]
fn arm_requires_live_unique_explicit_targets_and_manual_local_selection() {
    let (mut controller, tabs) = fixture(2, SyncInputTargetKind::Local);
    assert!(controller.armed_snapshot().is_none());
    assert!(matches!(
        controller.review_arm(SyncInputSelection::Manual, &[]),
        Err(SyncInputError::TargetCount)
    ));
    assert!(matches!(
        controller.review_arm(
            SyncInputSelection::CurrentWindow(controller.port.opened[&tabs[0]].window_id),
            &tabs
        ),
        Err(SyncInputError::LocalRequiresManualSelection)
    ));
    assert!(matches!(
        controller.review_arm(
            SyncInputSelection::CurrentTabGroup(controller.port.opened[&tabs[0]].group_id),
            &tabs
        ),
        Err(SyncInputError::LocalRequiresManualSelection)
    ));
    assert!(matches!(
        controller.review_arm(SyncInputSelection::Manual, &[tabs[0], tabs[0]]),
        Err(SyncInputError::DuplicateTarget)
    ));
    let first_session = controller.port.opened[&tabs[0]].session_id;
    controller
        .port
        .opened
        .get_mut(&tabs[1])
        .checked()
        .session_id = first_session;
    assert!(matches!(
        controller.review_arm(SyncInputSelection::Manual, &tabs),
        Err(SyncInputError::DuplicateTarget)
    ));
    controller
        .port
        .opened
        .get_mut(&tabs[1])
        .checked()
        .session_id = SessionId::new();
    controller.port.opened.get_mut(&tabs[1]).checked().kind = SyncInputTargetKind::Ssh;
    assert!(matches!(
        controller.review_arm(SyncInputSelection::Manual, &tabs),
        Err(SyncInputError::MixedTargetKinds)
    ));
    for eligibility in [
        SyncInputEligibility::Closed,
        SyncInputEligibility::Disconnected,
        SyncInputEligibility::SecureInput,
        SyncInputEligibility::Locked,
        SyncInputEligibility::Unknown,
    ] {
        controller
            .port
            .opened
            .get_mut(&tabs[0])
            .checked()
            .eligibility = eligibility;
        assert!(matches!(
            controller.review_arm(SyncInputSelection::Manual, &[tabs[0]]),
            Err(SyncInputError::TargetIneligible)
        ));
    }
    assert!(matches!(
        controller.review_arm(SyncInputSelection::Manual, &[TabId::new()]),
        Err(SyncInputError::TargetIneligible)
    ));
    assert!(matches!(
        controller.review_arm(
            SyncInputSelection::Manual,
            &vec![tabs[0]; MAX_SYNC_INPUT_TARGETS + 1]
        ),
        Err(SyncInputError::TargetCount)
    ));
}

#[test]
fn large_group_requires_second_arm_approval_and_snapshot_is_immutable() {
    let (mut controller, tabs) = fixture(100, SyncInputTargetKind::Ssh);
    let review = controller
        .review_arm(
            SyncInputSelection::CurrentWindow(controller.port.opened[&tabs[0]].window_id),
            &tabs,
        )
        .checked();
    assert!(review.requires_confirmation());
    assert_eq!(
        controller.arm(review, SyncInputApproval::Explicit),
        Err(SyncInputError::ConfirmationRequired)
    );
    assert!(controller.audit.events.is_empty());
    arm(&mut controller, &tabs);
    let snapshot = controller.armed_snapshot().checked().clone();
    let extra = TabId::new();
    controller.port.opened.insert(
        extra,
        OpenedSyncInputTarget {
            tab_id: extra,
            window_id: WorkspaceWindowId::new(),
            group_id: TabGroupId::new(),
            session_id: SessionId::new(),
            kind: SyncInputTargetKind::Ssh,
            title: "newly opened".into(),
            eligibility: SyncInputEligibility::Ready,
        },
    );
    controller.port.opened.remove(&tabs[99]);
    let receipt = send(
        &mut controller,
        tabs[0],
        InputAction::Text("committed".into()),
    );
    assert_eq!(receipt.snapshot.id(), snapshot.id());
    assert_eq!(receipt.deliveries.len(), 100);
    assert_eq!(receipt.deliveries[99].delivery, SyncInputDelivery::Closed);
    assert!(
        receipt.deliveries[..99]
            .iter()
            .all(|target| target.delivery == SyncInputDelivery::QueueAccepted)
    );
    assert!(
        !receipt
            .snapshot
            .targets()
            .iter()
            .any(|target| target.tab_id == extra)
    );
    assert_eq!(controller.audit.events[0].targets.len(), 100);
}

#[test]
fn window_and_group_selection_membership_is_checked_again_when_arming() {
    let (mut controller, tabs) = fixture(2, SyncInputTargetKind::Ssh);
    let window = controller.port.opened[&tabs[0]].window_id;
    let group = controller.port.opened[&tabs[0]].group_id;
    assert!(matches!(
        controller.review_arm(
            SyncInputSelection::CurrentWindow(WorkspaceWindowId::new()),
            &tabs
        ),
        Err(SyncInputError::TargetOutsideSelection)
    ));
    assert!(matches!(
        controller.review_arm(
            SyncInputSelection::CurrentTabGroup(TabGroupId::new()),
            &tabs
        ),
        Err(SyncInputError::TargetOutsideSelection)
    ));
    let review = controller
        .review_arm(SyncInputSelection::CurrentWindow(window), &tabs)
        .checked();
    controller.port.opened.get_mut(&tabs[1]).checked().window_id = WorkspaceWindowId::new();
    assert_eq!(
        controller.arm(review, SyncInputApproval::Explicit),
        Err(SyncInputError::StaleReview)
    );
    let review = controller
        .review_arm(SyncInputSelection::CurrentTabGroup(group), &tabs)
        .checked();
    controller.port.opened.get_mut(&tabs[1]).checked().group_id = TabGroupId::new();
    assert_eq!(
        controller.arm(review, SyncInputApproval::Explicit),
        Err(SyncInputError::StaleReview)
    );
    arm(&mut controller, &tabs); // Individually selected tabs may span windows.
    assert_eq!(
        controller.armed_snapshot().checked().selection(),
        SyncInputSelection::Manual
    );
}

#[test]
fn arm_review_rechecks_identity_owner_expiry_and_disarm_revision() {
    let (mut controller, tabs) = fixture(1, SyncInputTargetKind::Ssh);
    let review = controller
        .review_arm(SyncInputSelection::Manual, &tabs)
        .checked();
    controller
        .port
        .opened
        .get_mut(&tabs[0])
        .checked()
        .session_id = SessionId::new();
    assert_eq!(
        controller.arm(review, SyncInputApproval::Explicit),
        Err(SyncInputError::StaleReview)
    );
    let mut expired = controller
        .review_arm(SyncInputSelection::Manual, &tabs)
        .checked();
    expired.created = Instant::now() - SYNC_INPUT_REVIEW_LIFETIME;
    assert_eq!(
        controller.arm(expired, SyncInputApproval::Explicit),
        Err(SyncInputError::ReviewExpired)
    );
    let stale = controller
        .review_arm(SyncInputSelection::Manual, &tabs)
        .checked();
    controller.disarm(SyncInputDisarmReason::User).checked();
    assert_eq!(
        controller.arm(stale, SyncInputApproval::Explicit),
        Err(SyncInputError::StaleReview)
    );
    let mut foreign = controller
        .review_arm(SyncInputSelection::Manual, &tabs)
        .checked();
    foreign.owner = ConnectionId::new();
    assert_eq!(
        controller.arm(foreign, SyncInputApproval::Explicit),
        Err(SyncInputError::StaleReview)
    );
    assert!(controller.audit.events.is_empty());
}

#[test]
fn exact_prepared_text_requires_confirmation_without_logging_payload() {
    let (mut controller, tabs) = fixture(2, SyncInputTargetKind::Ssh);
    arm(&mut controller, &tabs);
    let text = "PRIVATE-NONCE\nrm -rf private-path";
    let prepared = controller
        .prepare_input(
            tabs[0],
            InputAction::Paste {
                text: text.into(),
                bracketed: true,
            },
            SyncInputContext::TerminalCommit,
            false,
        )
        .checked();
    assert!(prepared.risk().multiline);
    assert!(prepared.risk().control_characters);
    assert!(prepared.risk().destructive_command);
    assert!(!format!("{prepared:?}").contains("PRIVATE-NONCE"));
    assert!(matches!(
        controller.dispatch(prepared, SyncInputApproval::Explicit),
        Err(SyncInputError::ConfirmationRequired)
    ));
    assert!(queue(&controller, tabs[0]).is_empty());
    let receipt = send(
        &mut controller,
        tabs[0],
        InputAction::Paste {
            text: text.into(),
            bracketed: true,
        },
    );
    assert!(receipt.audit_complete);
    assert_eq!(queue(&controller, tabs[0]), [text.as_bytes()]);
    assert_eq!(queue(&controller, tabs[1]), [text.as_bytes()]);
    let audit = format!("{:?}", controller.audit.events);
    for secret in ["PRIVATE-NONCE", "private-path", "rm -rf"] {
        assert!(!audit.contains(secret));
    }
    assert!(
        matches!(controller.audit.events[1].operation, SyncInputAuditOperation::InputPlanned { kind: SyncInputKind::Paste, bytes, .. } if bytes == text.len())
    );
}

#[test]
fn secure_source_and_noncommit_contexts_never_broadcast() {
    let (mut controller, tabs) = fixture(2, SyncInputTargetKind::Ssh);
    arm(&mut controller, &tabs);
    for context in [
        SyncInputContext::ImePreedit,
        SyncInputContext::ApplicationShortcut,
    ] {
        assert!(matches!(
            controller.prepare_input(tabs[0], InputAction::Text("SECRET".into()), context, false),
            Err(SyncInputError::InputContextBlocked)
        ));
        assert!(controller.armed_snapshot().is_some());
    }
    assert!(matches!(
        controller.prepare_input(
            TabId::new(),
            InputAction::Text("SECRET".into()),
            SyncInputContext::TerminalCommit,
            false
        ),
        Err(SyncInputError::SourceNotArmed)
    ));
    for eligibility in [
        SyncInputEligibility::SecureInput,
        SyncInputEligibility::Locked,
        SyncInputEligibility::Unknown,
    ] {
        controller
            .port
            .opened
            .get_mut(&tabs[0])
            .checked()
            .eligibility = SyncInputEligibility::Ready;
        if controller.armed_snapshot().is_none() {
            arm(&mut controller, &tabs);
        }
        controller
            .port
            .opened
            .get_mut(&tabs[0])
            .checked()
            .eligibility = eligibility;
        assert!(matches!(
            controller.prepare_input(
                tabs[0],
                InputAction::Text("SECRET".into()),
                SyncInputContext::TerminalCommit,
                false
            ),
            Err(SyncInputError::InputContextBlocked)
        ));
        assert!(controller.armed_snapshot().is_none());
    }
    controller
        .port
        .opened
        .get_mut(&tabs[0])
        .checked()
        .eligibility = SyncInputEligibility::Ready;
    arm(&mut controller, &tabs);
    assert!(matches!(
        controller.prepare_input(
            tabs[0],
            InputAction::Text("SECRET".into()),
            SyncInputContext::SecureInput,
            false
        ),
        Err(SyncInputError::InputContextBlocked)
    ));
    assert!(controller.armed_snapshot().is_none());
    assert!(controller.port.queues.is_empty());
    assert!(!format!("{:?}", controller.audit.events).contains("SECRET"));
}

#[test]
fn prepared_input_cannot_follow_rearm_expiry_or_secure_source_transition() {
    let (mut controller, tabs) = fixture(2, SyncInputTargetKind::Ssh);
    arm(&mut controller, &tabs);
    let old = controller
        .prepare_input(
            tabs[0],
            InputAction::Text("old".into()),
            SyncInputContext::TerminalCommit,
            false,
        )
        .checked();
    controller
        .disarm(SyncInputDisarmReason::FocusLost)
        .checked();
    arm(&mut controller, &tabs);
    assert!(matches!(
        controller.dispatch(old, SyncInputApproval::Confirmed),
        Err(SyncInputError::StaleReview)
    ));
    let mut expired = controller
        .prepare_input(
            tabs[0],
            InputAction::Text("old".into()),
            SyncInputContext::TerminalCommit,
            false,
        )
        .checked();
    expired.created = Instant::now() - SYNC_INPUT_REVIEW_LIFETIME;
    assert!(matches!(
        controller.dispatch(expired, SyncInputApproval::Confirmed),
        Err(SyncInputError::ReviewExpired)
    ));
    let prepared = controller
        .prepare_input(
            tabs[0],
            InputAction::Text("SECRET".into()),
            SyncInputContext::TerminalCommit,
            false,
        )
        .checked();
    controller
        .port
        .opened
        .get_mut(&tabs[0])
        .checked()
        .eligibility = SyncInputEligibility::SecureInput;
    assert!(matches!(
        controller.dispatch(prepared, SyncInputApproval::Confirmed),
        Err(SyncInputError::InputContextBlocked)
    ));
    assert!(controller.armed_snapshot().is_none());
    assert!(controller.port.queues.is_empty());
}

#[test]
fn each_destination_checks_current_state_and_reports_its_own_queue_result() {
    let (mut controller, tabs) = fixture(8, SyncInputTargetKind::Ssh);
    arm(&mut controller, &tabs);
    controller.port.capacity = 1;
    let full_session = controller.port.opened[&tabs[1]].session_id;
    controller
        .port
        .queues
        .insert(full_session, VecDeque::from([b"earlier".to_vec()]));
    controller
        .port
        .opened
        .get_mut(&tabs[2])
        .checked()
        .eligibility = SyncInputEligibility::Disconnected;
    controller
        .port
        .opened
        .get_mut(&tabs[3])
        .checked()
        .eligibility = SyncInputEligibility::Locked;
    controller
        .port
        .opened
        .get_mut(&tabs[4])
        .checked()
        .eligibility = SyncInputEligibility::Unknown;
    controller
        .port
        .opened
        .get_mut(&tabs[5])
        .checked()
        .session_id = SessionId::new();
    controller.port.opened.remove(&tabs[6]);
    controller.port.change_during_submit = Some(tabs[7]);
    let receipt = send(
        &mut controller,
        tabs[0],
        InputAction::Text("isolated".into()),
    );
    let statuses: Vec<_> = receipt
        .deliveries
        .iter()
        .map(|delivery| delivery.delivery)
        .collect();
    assert_eq!(
        statuses,
        [
            SyncInputDelivery::QueueAccepted,
            SyncInputDelivery::Backpressure,
            SyncInputDelivery::Disconnected,
            SyncInputDelivery::Locked,
            SyncInputDelivery::UnknownSecurityState,
            SyncInputDelivery::TargetChanged,
            SyncInputDelivery::Closed,
            SyncInputDelivery::SecureInput
        ]
    );
    assert_eq!(queue(&controller, tabs[0]), [b"isolated"]);
    assert_eq!(queue(&controller, tabs[1]), [b"earlier"]);
    assert_eq!(controller.port.queues.len(), 2);
}

#[test]
fn logical_actions_encode_against_each_targets_modes_including_paste() {
    let (mut controller, tabs) = fixture(3, SyncInputTargetKind::Ssh);
    let application = controller.port.opened[&tabs[1]].session_id;
    controller.port.modes.insert(
        application,
        TerminalModes {
            application_cursor: true,
            bracketed_paste: true,
            ..Default::default()
        },
    );
    let kitty = controller.port.opened[&tabs[2]].session_id;
    controller.port.modes.insert(
        kitty,
        TerminalModes {
            kitty_keyboard_flags: TerminalModes::KITTY_REPORT_ALL_KEYS
                | TerminalModes::KITTY_REPORT_EVENTS
                | TerminalModes::KITTY_REPORT_ASSOCIATED_TEXT,
            ..Default::default()
        },
    );
    arm(&mut controller, &tabs);
    let actions = [
        key(KeyCode::ArrowUp, Modifiers::default()),
        key(
            KeyCode::Function(5),
            Modifiers {
                shift: true,
                ..Default::default()
            },
        ),
        key(
            KeyCode::Character("c".into()),
            Modifiers {
                ctrl: true,
                ..Default::default()
            },
        ),
        InputAction::Text("\u{4e2d}\u{6587}\u{1f642}".into()),
        InputAction::Paste {
            text: "paste".into(),
            bracketed: true,
        },
        key(KeyCode::Character("e\u{301}".into()), Modifiers::default()),
        key(KeyCode::Home, Modifiers::default()),
        key(KeyCode::End, Modifiers::default()),
        key(KeyCode::Insert, Modifiers::default()),
        key(KeyCode::Delete, Modifiers::default()),
        key(KeyCode::PageUp, Modifiers::default()),
        key(KeyCode::PageDown, Modifiers::default()),
        key(KeyCode::Character(" ".into()), Modifiers::default()),
        InputAction::Control(ControlAction::Interrupt),
        InputAction::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: Modifiers::default(),
            pressed: false,
            repeated: false,
        }),
    ];
    for action in &actions {
        send(&mut controller, tabs[0], action.clone());
    }
    for tab in &tabs {
        let session = controller.port.opened[tab].session_id;
        let expected: Vec<_> = actions
            .iter()
            .map(|action| InputEncoder::encode(action, controller.port.modes[&session]).checked())
            .filter(|bytes| !bytes.is_empty())
            .collect();
        assert_eq!(queue(&controller, *tab), expected);
    }
    assert_eq!(queue(&controller, tabs[0])[0], b"\x1b[A");
    assert_eq!(queue(&controller, tabs[1])[0], b"\x1bOA");
    assert_eq!(queue(&controller, tabs[0])[4], b"paste");
    assert_eq!(queue(&controller, tabs[1])[4], b"\x1b[200~paste\x1b[201~");
    assert_ne!(queue(&controller, tabs[2]), queue(&controller, tabs[0]));
}

#[test]
fn audit_failure_before_send_sends_nothing_and_after_send_preserves_outcomes() {
    let (mut controller, tabs) = fixture(2, SyncInputTargetKind::Ssh);
    controller.audit.fail_at = Some(0);
    let review = controller
        .review_arm(SyncInputSelection::Manual, &tabs)
        .checked();
    assert_eq!(
        controller.arm(review, SyncInputApproval::Explicit),
        Err(SyncInputError::Audit(SyncInputAuditError))
    );
    assert!(controller.armed_snapshot().is_none());
    controller.audit.fail_at = None;
    arm(&mut controller, &tabs);
    controller.audit.fail_at = Some(controller.audit.calls);
    let prepared = controller
        .prepare_input(
            tabs[0],
            InputAction::Text("not-sent".into()),
            SyncInputContext::TerminalCommit,
            false,
        )
        .checked();
    assert!(matches!(
        controller.dispatch(prepared, SyncInputApproval::Explicit),
        Err(SyncInputError::Audit(_))
    ));
    assert!(controller.port.queues.is_empty());
    assert!(controller.armed_snapshot().is_none());
    controller.audit.fail_at = None;
    arm(&mut controller, &tabs);
    controller.audit.fail_at = Some(controller.audit.calls + 1);
    let receipt = send(
        &mut controller,
        tabs[0],
        InputAction::Text("enqueued".into()),
    );
    assert!(!receipt.audit_complete);
    assert!(
        receipt
            .deliveries
            .iter()
            .all(|result| result.delivery == SyncInputDelivery::QueueAccepted)
    );
    assert_eq!(queue(&controller, tabs[0]), [b"enqueued"]);
    assert!(controller.armed_snapshot().is_none());
}

#[test]
fn source_security_transition_at_enqueue_stops_later_destinations() {
    let (mut controller, tabs) = fixture(3, SyncInputTargetKind::Ssh);
    arm(&mut controller, &tabs);
    // Source is last in the user's selection. It must still be checked first.
    controller.port.block_source_when_submitting = Some(tabs[2]);
    let receipt = send(
        &mut controller,
        tabs[2],
        InputAction::Text("never-forward".into()),
    );
    assert!(
        receipt
            .deliveries
            .iter()
            .all(|target| target.delivery == SyncInputDelivery::SourceBlocked)
    );
    assert!(controller.port.queues.is_empty());
    assert!(controller.armed_snapshot().is_none());

    controller
        .port
        .opened
        .get_mut(&tabs[2])
        .checked()
        .eligibility = SyncInputEligibility::Ready;
    arm(&mut controller, &tabs);
    controller.port.block_source_when_submitting = Some(tabs[0]);
    let receipt = send(
        &mut controller,
        tabs[2],
        InputAction::Text("previously-eligible".into()),
    );
    assert_eq!(
        receipt.deliveries[2].delivery,
        SyncInputDelivery::QueueAccepted
    );
    assert_eq!(
        receipt.deliveries[0].delivery,
        SyncInputDelivery::SourceBlocked
    );
    assert_eq!(
        receipt.deliveries[1].delivery,
        SyncInputDelivery::SourceBlocked
    );
    assert_eq!(queue(&controller, tabs[2]), [b"previously-eligible"]);
    assert!(queue(&controller, tabs[0]).is_empty());
    assert!(queue(&controller, tabs[1]).is_empty());
    assert!(controller.armed_snapshot().is_none());
}

#[test]
fn limits_destructive_hint_and_payload_drop_are_explicit() {
    let (mut controller, tabs) = fixture(1, SyncInputTargetKind::Ssh);
    arm(&mut controller, &tabs);
    for action in [
        InputAction::Text(String::new()),
        InputAction::Text("x".repeat(MAX_SYNC_INPUT_BYTES + 1)),
        key(KeyCode::Function(0), Modifiers::default()),
        key(KeyCode::Function(25), Modifiers::default()),
        key(KeyCode::Character("\x1b".into()), Modifiers::default()),
    ] {
        assert!(matches!(
            controller.prepare_input(tabs[0], action, SyncInputContext::TerminalCommit, false),
            Err(SyncInputError::InvalidAction)
        ));
    }
    let prepared = controller
        .prepare_input(
            tabs[0],
            InputAction::Text("ambiguous-tool".into()),
            SyncInputContext::TerminalCommit,
            true,
        )
        .checked();
    assert!(prepared.risk().destructive_command);
    assert!(matches!(
        controller.dispatch(prepared, SyncInputApproval::Explicit),
        Err(SyncInputError::ConfirmationRequired)
    ));
    let at_limit = controller
        .prepare_input(
            tabs[0],
            InputAction::Text("x".repeat(MAX_SYNC_INPUT_BYTES)),
            SyncInputContext::TerminalCommit,
            false,
        )
        .checked();
    assert_eq!(at_limit.bytes(), MAX_SYNC_INPUT_BYTES);
    drop(at_limit);
    for text in [
        "echo a\nb",
        "echo a\rb",
        "echo \x1b",
        "sudo rm -rf /path",
        "mkfs.ext4 /dev/example",
        "Remove-Item example",
    ] {
        let prepared = controller
            .prepare_input(
                tabs[0],
                InputAction::Text(text.into()),
                SyncInputContext::TerminalCommit,
                false,
            )
            .checked();
        assert!(prepared.risk().requires_confirmation(), "{text}");
    }
    assert!(controller.port.queues.is_empty());
}
