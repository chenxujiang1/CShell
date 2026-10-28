//! Session-bound logical input queues. No terminal bytes are copied between targets.

use crate::daemon_connection::DesktopDaemonView;
use cshell_application::{SyncInputDelivery, SyncInputEligibility};
use cshell_domain::{ConnectionId, InputAction, KeyCode, SessionId};
use cshell_ipc::TerminalInputRequest;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use zeroize::Zeroize;

const MAX_QUEUED_INPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DesktopInputSecurity {
    Terminal,
    Secure,
    Locked,
    #[default]
    Unknown,
}

/// Only the armed group's owner keeps this permit alive. Queued inputs hold weak
/// references, so dropping or revoking it prevents writes that have not started.
#[derive(Debug)]
pub(crate) struct DesktopInputPermit {
    alive: Arc<AtomicBool>,
    audit_health: Option<Arc<AtomicBool>>,
}

impl DesktopInputPermit {
    pub(crate) fn new() -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            audit_health: None,
        }
    }

    pub(crate) fn with_audit_health(healthy: Arc<AtomicBool>) -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(true)),
            audit_health: Some(healthy),
        }
    }

    fn valid(&self) -> bool {
        self.alive.load(Ordering::Acquire)
            && self
                .audit_health
                .as_ref()
                .is_none_or(|health| health.load(Ordering::Acquire))
    }

    pub(crate) fn revoke(&self) {
        self.alive.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(
        capacity: usize,
    ) -> (
        DesktopInputSender,
        tokio::sync::mpsc::Receiver<QueuedDesktopInput>,
        SessionId,
    ) {
        let id = SessionId::new();
        let shared = Arc::new(Mutex::new(DesktopDaemonView {
            connected: true,
            session_id: Some(id),
            session_running: true,
            input_security: DesktopInputSecurity::Terminal,
            ..DesktopDaemonView::default()
        }));
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        (DesktopInputSender::new(shared, sender), receiver, id)
    }

    fn take(receiver: &mut tokio::sync::mpsc::Receiver<QueuedDesktopInput>) -> QueuedDesktopInput {
        receiver
            .try_recv()
            .unwrap_or_else(|error| panic!("queued input: {error}"))
    }

    #[test]
    fn queued_input_cannot_cross_session_switch_or_same_session_reconnect() {
        let (sender, mut receiver, id) = queue(2);
        assert!(sender.send_owned(InputAction::Text("old shell secret".into())));
        let stale = take(&mut receiver);
        assert_eq!(stale.validate(&sender.shared, id), Ok(()));
        {
            let mut view = sender
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            view.invalidate_input();
            view.connected = true;
        }
        assert_eq!(
            stale.validate(&sender.shared, id),
            Err(SyncInputDelivery::TargetChanged)
        );
        let other_id = SessionId::new();
        sender
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_id = Some(other_id);
        assert_eq!(
            stale.validate(&sender.shared, other_id),
            Err(SyncInputDelivery::TargetChanged)
        );
        assert!(sender.send_owned(InputAction::Text("new shell input".into())));
        assert_eq!(
            take(&mut receiver).validate(&sender.shared, other_id),
            Ok(())
        );
        assert!(!format!("{stale:?}").contains("old shell secret"));
    }

    #[test]
    fn synchronized_queue_rechecks_source_security_and_revocation_before_write() {
        let (source, _, source_id) = queue(2);
        let (target, mut receiver, target_id) = queue(4);
        let permit = DesktopInputPermit::new();
        let action = InputAction::Text("private terminal input".into());
        assert_eq!(
            target.try_submit(&source, source_id, target_id, &action, &permit),
            SyncInputDelivery::QueueAccepted
        );
        let input = take(&mut receiver);
        assert_eq!(input.validate(&target.shared, target_id), Ok(()));
        source.set_security(DesktopInputSecurity::Secure);
        assert_eq!(
            input.validate(&target.shared, target_id),
            Err(SyncInputDelivery::SourceBlocked)
        );
        assert_eq!(
            target.try_submit(&source, source_id, target_id, &action, &permit),
            SyncInputDelivery::SourceBlocked
        );
        source.set_security(DesktopInputSecurity::Terminal);
        assert_eq!(
            input.validate(&target.shared, target_id),
            Err(SyncInputDelivery::SourceBlocked)
        );
        assert_eq!(
            target.try_submit(&source, source_id, target_id, &action, &permit),
            SyncInputDelivery::QueueAccepted
        );
        let revoked = take(&mut receiver);
        permit.revoke();
        assert_eq!(
            revoked.validate(&target.shared, target_id),
            Err(SyncInputDelivery::SourceBlocked)
        );
        let new_permit = DesktopInputPermit::new();
        assert_eq!(
            target.try_submit(&source, source_id, target_id, &action, &new_permit),
            SyncInputDelivery::QueueAccepted
        );
        let orphaned = take(&mut receiver);
        drop(new_permit);
        assert_eq!(
            orphaned.validate(&target.shared, target_id),
            Err(SyncInputDelivery::SourceBlocked)
        );
    }

    #[test]
    fn independent_bounded_queues_report_slow_closed_and_changed_targets() {
        let (source, _, source_id) = queue(2);
        let (slow, mut slow_receiver, slow_id) = queue(1);
        let (fast, mut fast_receiver, fast_id) = queue(2);
        let permit = DesktopInputPermit::new();
        let action = InputAction::Text("x".into());
        assert_eq!(
            slow.try_submit(&source, source_id, slow_id, &action, &permit),
            SyncInputDelivery::QueueAccepted
        );
        assert_eq!(
            slow.try_submit(&source, source_id, slow_id, &action, &permit),
            SyncInputDelivery::Backpressure
        );
        assert_eq!(
            fast.try_submit(&source, source_id, fast_id, &action, &permit),
            SyncInputDelivery::QueueAccepted
        );
        assert_eq!(
            take(&mut fast_receiver).validate(&fast.shared, fast_id),
            Ok(())
        );
        assert_eq!(
            slow.try_submit(&source, source_id, SessionId::new(), &action, &permit),
            SyncInputDelivery::TargetChanged
        );
        let input = take(&mut slow_receiver);
        slow.set_security(DesktopInputSecurity::Secure);
        assert!(input.validate(&slow.shared, slow_id).is_err());
        assert_eq!(
            slow.try_submit(&source, source_id, slow_id, &action, &permit),
            SyncInputDelivery::SecureInput
        );
        slow.set_security(DesktopInputSecurity::Locked);
        assert_eq!(
            slow.try_submit(&source, source_id, slow_id, &action, &permit),
            SyncInputDelivery::Locked
        );
        slow.set_security(DesktopInputSecurity::Unknown);
        assert_eq!(
            slow.try_submit(&source, source_id, slow_id, &action, &permit),
            SyncInputDelivery::UnknownSecurityState
        );
        slow.set_security(DesktopInputSecurity::Terminal);
        drop(slow_receiver);
        assert_eq!(
            slow.try_submit(&source, source_id, slow_id, &action, &permit),
            SyncInputDelivery::Disconnected
        );
    }

    #[test]
    fn self_target_uses_one_lock_and_single_target_secure_typing_still_works() {
        let (sender, mut receiver, id) = queue(2);
        let permit = DesktopInputPermit::new();
        assert_eq!(
            sender.try_submit(
                &sender,
                id,
                id,
                &InputAction::Text("ordinary".into()),
                &permit
            ),
            SyncInputDelivery::QueueAccepted
        );
        assert_eq!(take(&mut receiver).validate(&sender.shared, id), Ok(()));
        sender.set_security(DesktopInputSecurity::Secure);
        assert!(sender.send_owned(InputAction::Text("password".into())));
        assert_eq!(take(&mut receiver).validate(&sender.shared, id), Ok(()));
        assert!(!sender.send_owned(InputAction::Text(
            "x".repeat(cshell_ipc::MAX_TERMINAL_INPUT_BYTES + 1)
        )));
    }

    #[test]
    fn queue_byte_limit_bounds_large_pastes_and_refunds_failed_and_discarded_items() {
        let (source, _, source_id) = queue(4);
        let (target, mut receiver, target_id) = queue(16);
        let permit = DesktopInputPermit::new();
        let action = InputAction::Paste {
            text: "x".repeat(cshell_ipc::MAX_TERMINAL_INPUT_BYTES),
            bracketed: true,
        };
        for _ in 0..3 {
            assert_eq!(
                target.try_submit(&source, source_id, target_id, &action, &permit),
                SyncInputDelivery::QueueAccepted
            );
        }
        assert_eq!(
            target.try_submit(&source, source_id, target_id, &action, &permit),
            SyncInputDelivery::Backpressure
        );
        assert!(target.queued_bytes.load(Ordering::Acquire) <= MAX_QUEUED_INPUT_BYTES);
        drop(take(&mut receiver));
        assert_eq!(
            target.try_submit(&source, source_id, target_id, &action, &permit),
            SyncInputDelivery::QueueAccepted
        );
        drop(receiver);
        assert_eq!(target.queued_bytes.load(Ordering::Acquire), 0);
        assert_eq!(
            target.try_submit(&source, source_id, target_id, &action, &permit),
            SyncInputDelivery::Disconnected
        );
        assert_eq!(target.queued_bytes.load(Ordering::Acquire), 0);
        let (tiny, mut tiny_receiver, tiny_id) = queue(1);
        assert_eq!(
            tiny.try_submit(&source, source_id, tiny_id, &action, &permit),
            SyncInputDelivery::QueueAccepted
        );
        let bytes = tiny.queued_bytes.load(Ordering::Acquire);
        assert_eq!(
            tiny.try_submit(&source, source_id, tiny_id, &action, &permit),
            SyncInputDelivery::Backpressure
        );
        assert_eq!(tiny.queued_bytes.load(Ordering::Acquire), bytes);
        drop(take(&mut tiny_receiver));
        assert_eq!(tiny.queued_bytes.load(Ordering::Acquire), 0);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DesktopInputSender {
    shared: Arc<Mutex<DesktopDaemonView>>,
    sender: tokio::sync::mpsc::Sender<QueuedDesktopInput>,
    queued_bytes: Arc<AtomicUsize>,
}

impl DesktopInputSender {
    pub(crate) fn new(
        shared: Arc<Mutex<DesktopDaemonView>>,
        sender: tokio::sync::mpsc::Sender<QueuedDesktopInput>,
    ) -> Self {
        Self {
            shared,
            sender,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn reserve(&self, action: &InputAction) -> Option<InputReservation> {
        let body = match action {
            InputAction::Text(text)
            | InputAction::Paste { text, .. }
            | InputAction::Compose { text, .. } => text.len(),
            InputAction::Key(key) => match &key.code {
                KeyCode::Character(text) => text.len(),
                _ => 0,
            },
            _ => 0,
        };
        // Count fixed overhead too; mpsc independently limits the item count.
        let bytes = body.checked_add(64)?;
        self.queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= MAX_QUEUED_INPUT_BYTES)
            })
            .ok()?;
        Some(InputReservation {
            bytes,
            used: Arc::clone(&self.queued_bytes),
        })
    }

    pub(crate) fn send_owned(&self, action: InputAction) -> bool {
        let mut input = QueuedDesktopInput {
            observed_generation: None,
            session_id: None,
            epoch: ConnectionId::new(),
            action,
            synchronized: None,
            reservation: None,
        };
        let view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(session_id) = view.session_id.filter(|_| view.connected) else {
            return false;
        };
        if !valid_action(session_id, &input.action) {
            return false;
        }
        if matches!(input.action, InputAction::Compose { .. }) && !view.compose_supported {
            return false;
        }
        input.session_id = Some(session_id);
        input.observed_generation = view.snapshot.as_ref().map(|snapshot| snapshot.generation);
        input.epoch = view.input_epoch;
        input.reservation = self.reserve(&input.action);
        if input.reservation.is_none() {
            return false;
        }
        // Capture identity and enqueue under the same lock as view transitions.
        self.sender.try_send(input).is_ok()
    }

    pub(crate) fn eligibility(&self, session_id: SessionId) -> SyncInputEligibility {
        eligibility(
            &self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            session_id,
        )
    }

    pub(crate) fn set_security(&self, security: DesktopInputSecurity) {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.input_security != security {
            view.input_security = security;
            view.input_epoch = ConnectionId::new();
        }
    }

    pub(crate) fn try_submit(
        &self,
        source: &Self,
        source_id: SessionId,
        target_id: SessionId,
        action: &InputAction,
        permit: &DesktopInputPermit,
    ) -> SyncInputDelivery {
        with_views(&self.shared, &source.shared, |target, source_view| {
            if eligibility(source_view, source_id) != SyncInputEligibility::Ready || !permit.valid()
            {
                return SyncInputDelivery::SourceBlocked;
            }
            let target_status = eligibility(target, target_id);
            if target.session_id != Some(target_id) {
                return SyncInputDelivery::TargetChanged;
            }
            if target_status != SyncInputEligibility::Ready {
                return delivery(target_status);
            }
            if !valid_action(target_id, action) {
                return SyncInputDelivery::InvalidAction;
            }
            if matches!(action, InputAction::Compose { .. }) && !target.compose_supported {
                return SyncInputDelivery::UnsupportedInput;
            }
            let Some(reservation) = self.reserve(action) else {
                return SyncInputDelivery::Backpressure;
            };
            let input = QueuedDesktopInput {
                session_id: Some(target_id),
                observed_generation: target.snapshot.as_ref().map(|snapshot| snapshot.generation),
                epoch: target.input_epoch,
                action: action.clone(),
                synchronized: Some(SynchronizedInputGuard {
                    source: Arc::downgrade(&source.shared),
                    session_id: source_id,
                    epoch: source_view.input_epoch,
                    permit: Arc::downgrade(&permit.alive),
                    audit_health: permit.audit_health.as_ref().map(Arc::downgrade),
                }),
                reservation: Some(reservation),
            };
            match self.sender.try_send(input) {
                Ok(()) => SyncInputDelivery::QueueAccepted,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    SyncInputDelivery::Backpressure
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    SyncInputDelivery::Disconnected
                }
            }
        })
    }
}

struct SynchronizedInputGuard {
    source: Weak<Mutex<DesktopDaemonView>>,
    session_id: SessionId,
    epoch: ConnectionId,
    permit: Weak<AtomicBool>,
    audit_health: Option<Weak<AtomicBool>>,
}

#[derive(Debug)]
struct InputReservation {
    bytes: usize,
    used: Arc<AtomicUsize>,
}

impl Drop for InputReservation {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

pub(crate) struct QueuedDesktopInput {
    pub(crate) observed_generation: Option<u64>,
    session_id: Option<SessionId>,
    epoch: ConnectionId,
    pub(crate) action: InputAction,
    synchronized: Option<SynchronizedInputGuard>,
    reservation: Option<InputReservation>,
}

impl std::fmt::Debug for QueuedDesktopInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueuedDesktopInput")
            .field("session_id", &self.session_id)
            .field("epoch", &self.epoch)
            .field("synchronized", &self.synchronized.is_some())
            .field("action", &"[REDACTED]")
            .finish()
    }
}

impl QueuedDesktopInput {
    pub(crate) fn validate(
        &self,
        shared: &Arc<Mutex<DesktopDaemonView>>,
        wire_session: SessionId,
    ) -> Result<(), SyncInputDelivery> {
        let check_target = |view: &DesktopDaemonView| {
            if self.session_id != Some(wire_session)
                || view.session_id != self.session_id
                || view.input_epoch != self.epoch
            {
                Err(SyncInputDelivery::TargetChanged)
            } else if !view.connected {
                Err(SyncInputDelivery::Disconnected)
            } else {
                Ok(())
            }
        };
        let Some(guard) = &self.synchronized else {
            return check_target(
                &shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        };
        let source = guard
            .source
            .upgrade()
            .ok_or(SyncInputDelivery::SourceBlocked)?;
        with_views(shared, &source, |target, source_view| {
            check_target(target)?;
            if source_view.input_epoch != guard.epoch
                || eligibility(source_view, guard.session_id) != SyncInputEligibility::Ready
                || !guard
                    .permit
                    .upgrade()
                    .is_some_and(|permit| permit.load(Ordering::Acquire))
                || guard.audit_health.as_ref().is_some_and(|health| {
                    !health
                        .upgrade()
                        .is_some_and(|health| health.load(Ordering::Acquire))
                })
            {
                return Err(SyncInputDelivery::SourceBlocked);
            }
            let target_status = eligibility(target, wire_session);
            if target_status != SyncInputEligibility::Ready {
                return Err(delivery(target_status));
            }
            Ok(())
        })
    }
}

impl Drop for QueuedDesktopInput {
    fn drop(&mut self) {
        match &mut self.action {
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

impl DesktopDaemonView {
    pub(crate) fn invalidate_input(&mut self) {
        self.connected = false;
        self.input_epoch = ConnectionId::new();
    }
}

fn valid_action(session_id: SessionId, action: &InputAction) -> bool {
    TerminalInputRequest::from_action(session_id, action).is_ok()
}

fn eligibility(view: &DesktopDaemonView, expected: SessionId) -> SyncInputEligibility {
    if view.session_id != Some(expected) || !view.connected {
        return if view.session_id == Some(expected)
            && !view.session_running
            && !view.session_opening
        {
            SyncInputEligibility::Closed
        } else {
            SyncInputEligibility::Disconnected
        };
    }
    match view.input_security {
        DesktopInputSecurity::Terminal => SyncInputEligibility::Ready,
        DesktopInputSecurity::Secure => SyncInputEligibility::SecureInput,
        DesktopInputSecurity::Locked => SyncInputEligibility::Locked,
        DesktopInputSecurity::Unknown => SyncInputEligibility::Unknown,
    }
}

fn delivery(eligibility: SyncInputEligibility) -> SyncInputDelivery {
    match eligibility {
        SyncInputEligibility::Ready => SyncInputDelivery::QueueAccepted,
        SyncInputEligibility::Disconnected => SyncInputDelivery::Disconnected,
        SyncInputEligibility::Closed => SyncInputDelivery::Closed,
        SyncInputEligibility::SecureInput => SyncInputDelivery::SecureInput,
        SyncInputEligibility::Locked => SyncInputDelivery::Locked,
        SyncInputEligibility::Unknown => SyncInputDelivery::UnknownSecurityState,
    }
}

/// Stable lock order prevents two source/destination pairs from deadlocking.
/// A source sent to itself takes its lock only once.
fn with_views<R>(
    target: &Arc<Mutex<DesktopDaemonView>>,
    source: &Arc<Mutex<DesktopDaemonView>>,
    action: impl FnOnce(&DesktopDaemonView, &DesktopDaemonView) -> R,
) -> R {
    if Arc::ptr_eq(target, source) {
        let view = target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        action(&view, &view)
    } else if Arc::as_ptr(target) < Arc::as_ptr(source) {
        let target = target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let source = source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        action(&target, &source)
    } else {
        let source = source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let target = target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        action(&target, &source)
    }
}
