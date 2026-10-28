use super::*;
use cshell_domain::{
    ConnectionDiagnostic, ConnectionOutcome as Outcome, ConnectionStage as Stage, ConnectionStep,
    MAX_CONNECTION_STEPS,
};
use cshell_ssh::connection_progress::{self, Event, Observer};
use std::{sync::Mutex, time::Instant};
struct Recorder {
    started: Instant,
    hop: u8,
    endpoint: String,
    value: ConnectionDiagnostic,
}
impl Recorder {
    fn refresh(&mut self) {
        if self.value.outcome != Outcome::Running {
            return;
        }
        self.value.elapsed_ms = self.started.elapsed().as_millis().min(86_400_000) as u64;
        if let Some(last) = self.value.steps.last_mut() {
            last.elapsed_ms = self.value.elapsed_ms.saturating_sub(last.started_ms);
        }
    }
    fn new() -> Self {
        Self {
            started: Instant::now(),
            hop: 0,
            endpoint: String::new(),
            value: ConnectionDiagnostic {
                attempt: cshell_domain::SessionId::new(),
                elapsed_ms: 0,
                outcome: Outcome::Running,
                failure_code: 0,
                truncated: false,
                steps: vec![],
            },
        }
    }
    fn event(&mut self, event: Event) {
        if self.value.outcome != Outcome::Running {
            return;
        }
        let (stage, endpoint) = match event {
            Event::Peer { hop, endpoint } => {
                self.hop = hop.min(16);
                self.endpoint = safe_endpoint(endpoint);
                return;
            }
            Event::Stage(stage) => (stage, self.endpoint.clone()),
            Event::At { stage, endpoint } => (stage, safe_endpoint(endpoint)),
        };
        if self
            .value
            .steps
            .last()
            .is_some_and(|s| s.stage == stage && s.endpoint == endpoint && s.hop == self.hop)
        {
            return;
        }
        let now = self.started.elapsed().as_millis().min(86_400_000) as u64;
        self.value.elapsed_ms = now;
        if let Some(last) = self.value.steps.last_mut() {
            last.elapsed_ms = now.saturating_sub(last.started_ms);
            last.outcome = Outcome::Succeeded;
        }
        let row = ConnectionStep {
            stage,
            hop: self.hop,
            endpoint,
            started_ms: now,
            elapsed_ms: 0,
            outcome: Outcome::Running,
        };
        if self.value.steps.len() == MAX_CONNECTION_STEPS {
            self.value.truncated = true;
            self.value.steps.pop();
        }
        self.value.steps.push(row);
    }
    fn finish(&mut self, outcome: Outcome, code: i32) {
        if outcome == Outcome::Succeeded {
            self.event(Event::Stage(Stage::Ready));
        }
        self.value.elapsed_ms = self.started.elapsed().as_millis().min(86_400_000) as u64;
        self.value.outcome = outcome;
        self.value.failure_code = code;
        if let Some(last) = self.value.steps.last_mut() {
            last.elapsed_ms = self.value.elapsed_ms.saturating_sub(last.started_ms);
            last.outcome = outcome;
        }
    }
}
fn safe_endpoint(value: String) -> String {
    let mut value = value
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    if value.len() > 320 {
        while value.len() > 317 {
            value.pop();
        }
        value.push_str("...");
    }
    value
}

async fn update<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: u64,
    value: &ConnectionDiagnostic,
) -> Result<(), SessionIpcError> {
    let payload = cshell_ipc::ConnectionDiagnosticUpdate {
        document_json: serde_json::to_vec(value)
            .map_err(|_| SessionIpcError::UnsupportedRequest)?,
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        write_envelope(
            writer,
            &Envelope {
                request_id: id,
                payload: Some(envelope::Payload::ConnectionDiagnosticUpdate(payload)),
                ..Default::default()
            },
        ),
    )
    .await
    .map_err(|_| SessionIpcError::UnsupportedRequest)?
    .map_err(SessionIpcError::Transport)
}
impl SessionIpcService {
    pub(super) async fn dispatch_observed<W: AsyncWrite + Unpin>(
        &self,
        mut request: Envelope,
        features: u64,
        writer: &mut W,
    ) -> Result<DispatchResult, SessionIpcError> {
        let enabled = match &mut request.payload {
            Some(envelope::Payload::SessionCreateRequest(r)) => {
                std::mem::take(&mut r.connection_diagnostics)
            }
            Some(envelope::Payload::TerminalContextLaunchRequest(r)) => {
                std::mem::take(&mut r.connection_diagnostics)
            }
            Some(envelope::Payload::SftpRequest(r)) => {
                let enabled = std::mem::take(&mut r.connection_diagnostics);
                if enabled && r.operation != cshell_ipc::SftpOperation::ConnectProfile as i32 {
                    return Err(SessionIpcError::UnsupportedRequest);
                }
                enabled
            }
            _ => false,
        };
        if !enabled {
            return self.dispatch_with_features(request, features).await;
        }
        let id = request.request_id;
        let mut recorder = Recorder::new();
        recorder.event(Event::Stage(Stage::Preparing));
        let (sender, mut changed) = tokio::sync::watch::channel(recorder.value.clone());
        let recorder = Arc::new(Mutex::new(recorder));
        let capture = recorder.clone();
        let observer = Observer::new(move |event| {
            let mut r = capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            r.event(event);
            sender.send_if_modified(|value| {
                if *value == r.value {
                    false
                } else {
                    *value = r.value.clone();
                    true
                }
            });
        });
        let initial = {
            recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .value
                .clone()
        };
        update(writer, id, &initial).await?;
        let work =
            connection_progress::scope(observer, self.dispatch_with_features(request, features));
        tokio::pin!(work);
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(2),
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let result = loop {
            tokio::select! {biased;result=&mut work=>break result,
                result=changed.changed()=>{if result.is_err(){break work.await;}let _=changed.borrow_and_update();let value={recorder.lock().unwrap_or_else(std::sync::PoisonError::into_inner).value.clone()};update(writer,id,&value).await?;}
                _=heartbeat.tick()=>{let value={let mut r=recorder.lock().unwrap_or_else(std::sync::PoisonError::into_inner);r.refresh();r.value.clone()};update(writer,id,&value).await?;}
            }
        };
        let (outcome, code) = match result
            .as_ref()
            .ok()
            .and_then(|r| r.response.payload.as_ref())
        {
            Some(envelope::Payload::SessionCreateResponse(r)) if r.session.is_some() => {
                (Outcome::Succeeded, 0)
            }
            Some(envelope::Payload::SessionCreateResponse(r)) => (
                if r.failure_code == SessionFailureCode::AuthenticationCancelled as i32 {
                    Outcome::Cancelled
                } else {
                    Outcome::Failed
                },
                r.failure_code,
            ),
            Some(envelope::Payload::TerminalContextLaunchResponse(r)) => match &r.result {
                Some(r) if r.session.is_some() => (Outcome::Succeeded, 0),
                Some(r) => (
                    if r.failure_code == SessionFailureCode::AuthenticationCancelled as i32 {
                        Outcome::Cancelled
                    } else {
                        Outcome::Failed
                    },
                    r.failure_code,
                ),
                _ => (Outcome::Uncertain, 0),
            },
            Some(envelope::Payload::SftpResponse(r)) => (
                if r.status == cshell_ipc::SftpStatus::Ok as i32 {
                    Outcome::Succeeded
                } else if r.status == cshell_ipc::SftpStatus::Cancelled as i32 {
                    Outcome::Cancelled
                } else {
                    Outcome::Failed
                },
                r.status,
            ),
            _ => (Outcome::Uncertain, 0),
        };
        let value = {
            let mut r = recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            r.finish(outcome, code);
            r.value.clone()
        };
        update(writer, id, &value).await?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trace_is_bounded_scoped_and_preserves_the_actual_failure_stage() {
        let mut value = Recorder::new();
        value.event(Event::Stage(Stage::Preparing));
        value.event(Event::Peer {
            hop: 2,
            endpoint: "jump.example:22\n".into(),
        });
        for index in 0..MAX_CONNECTION_STEPS + 20 {
            value.event(Event::Stage(if index % 2 == 0 {
                Stage::Authentication
            } else {
                Stage::WaitingForInput
            }));
        }
        value.event(Event::Stage(Stage::JumpChannel));
        value.finish(Outcome::Failed, 17);
        assert_eq!(value.value.steps.len(), MAX_CONNECTION_STEPS);
        assert!(value.value.truncated);
        assert!(value.value.validate());
        let last = value.value.steps.last().unwrap();
        assert_eq!(last.stage, Stage::JumpChannel);
        assert_eq!(last.hop, 2);
        assert_eq!(last.endpoint, "jump.example:22");
        assert_eq!(last.outcome, Outcome::Failed);
        let frozen = value.value.clone();
        value.event(Event::Stage(Stage::Ready));
        assert_eq!(value.value, frozen);
        let mut other = Recorder::new();
        other.event(Event::Stage(Stage::Preparing));
        other.finish(Outcome::Succeeded, 0);
        assert!(other.value.validate());
        assert_ne!(other.value.attempt, value.value.attempt);
    }
}
