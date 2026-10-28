use crate::SessionIpcService;
use cshell_application::{
    ClipboardPolicyDocument, ClipboardPolicyError, ClipboardPolicySnapshot, decode_clipboard_policy,
};
use cshell_domain::SessionId;
use cshell_ipc::{
    ClipboardOperation, ClipboardPolicyOperation, ClipboardPolicyRequest, ClipboardPolicyResponse,
    ClipboardPolicyStatus, ClipboardResponse, ClipboardStatus,
};

impl SessionIpcService {
    pub(crate) async fn handle_clipboard_policy(
        &self,
        request: &ClipboardPolicyRequest,
    ) -> ClipboardPolicyResponse {
        let _guard = self.clipboard_policy_gate.lock().await;
        let Some(profiles) = &self.profiles else {
            return ClipboardPolicyResponse::with_status(ClipboardPolicyStatus::Unavailable);
        };
        if !request.selected_session_id.is_empty()
            && cshell_ipc::decode_id(&request.selected_session_id).is_err()
        {
            return ClipboardPolicyResponse::with_status(ClipboardPolicyStatus::Invalid);
        }
        let result = match ClipboardPolicyOperation::try_from(request.operation) {
            Ok(ClipboardPolicyOperation::Load)
                if request.document_json.is_empty() && request.expected_revision == 0 =>
            {
                profiles.load_clipboard_policy().await
            }
            Ok(ClipboardPolicyOperation::Save) => {
                let document = match decode_clipboard_policy(&request.document_json) {
                    Ok(document) => document,
                    Err(error) => return policy_error(error),
                };
                let previous = match profiles.load_clipboard_policy().await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        self.restrict_clipboard_sessions();
                        return policy_error(error);
                    }
                };
                let selected = cshell_ipc::decode_id(&request.selected_session_id)
                    .ok()
                    .map(SessionId::from_bytes)
                    .and_then(|id| self.ssh_sessions.get(id).ok())
                    .filter(|session| session.running());
                if document.automatic.iter().any(|grant| {
                    !previous.document.automatic.contains(grant)
                        && selected
                            .as_ref()
                            .and_then(|session| session.clipboard_trust())
                            != Some(grant)
                }) {
                    return ClipboardPolicyResponse::with_status(ClipboardPolicyStatus::Invalid);
                }
                profiles
                    .save_clipboard_policy(request.expected_revision, &document)
                    .await
            }
            _ => return ClipboardPolicyResponse::with_status(ClipboardPolicyStatus::Invalid),
        };
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.restrict_clipboard_sessions();
                return policy_error(error);
            }
        };
        self.apply_clipboard_policy(&snapshot.document);
        if request.operation == ClipboardPolicyOperation::Save as i32 {
            tracing::info!(
                revision = snapshot.revision,
                blocked_host_count = snapshot.document.blocked_hosts.len(),
                "host clipboard restrictions saved"
            );
        }
        let selected_session = cshell_ipc::decode_id(&request.selected_session_id)
            .ok()
            .map(SessionId::from_bytes)
            .and_then(|id| self.ssh_sessions.get(id).ok())
            .filter(|session| session.running());
        let selected = selected_session
            .as_ref()
            .and_then(|session| session.clipboard_host().cloned());
        let document_json = match serde_json::to_vec(&snapshot.document) {
            Ok(json) => json,
            Err(_) => {
                return ClipboardPolicyResponse::with_status(ClipboardPolicyStatus::Unavailable);
            }
        };
        ClipboardPolicyResponse {
            status: ClipboardPolicyStatus::Ok as i32,
            revision: snapshot.revision,
            document_json,
            selected_host: selected
                .as_ref()
                .map_or_else(String::new, |host| host.host.clone()),
            selected_port: selected.map_or(0, |host| u32::from(host.port)),
            selected_trust_json: selected_session
                .as_ref()
                .and_then(|session| session.clipboard_trust())
                .and_then(|trust| serde_json::to_vec(trust).ok())
                .unwrap_or_default(),
        }
    }

    pub(crate) async fn handle_clipboard_decision(
        &self,
        id: SessionId,
        operation: ClipboardOperation,
        token: &[u8],
    ) -> ClipboardResponse {
        let _guard = self.clipboard_policy_gate.lock().await;
        if self
            .clipboard_policy_restricted
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return ClipboardResponse::with_status(ClipboardStatus::Unavailable);
        }
        let Ok(session) = self.ssh_sessions.get(id) else {
            return ClipboardResponse::with_status(ClipboardStatus::Unavailable);
        };
        if session.clipboard_host().is_some() {
            let snapshot = match &self.profiles {
                Some(profiles) => profiles.load_clipboard_policy().await,
                None => Err(ClipboardPolicyError::Unavailable),
            };
            match snapshot {
                Ok(snapshot) => session.set_clipboard_policy(&snapshot.document),
                Err(_) => {
                    self.restrict_clipboard_sessions();
                    return ClipboardResponse::with_status(ClipboardStatus::Unavailable);
                }
            }
        }
        session.clipboard_request(operation, token)
    }
    pub(crate) fn apply_clipboard_policy(&self, document: &ClipboardPolicyDocument) {
        self.clipboard_policy_restricted
            .store(false, std::sync::atomic::Ordering::Release);
        for session in self.ssh_sessions.list() {
            if session.clipboard_host().is_some() {
                session.set_clipboard_policy(document);
            }
        }
    }
    fn restrict_clipboard_sessions(&self) {
        self.clipboard_policy_restricted
            .store(true, std::sync::atomic::Ordering::Release);
        for session in self.ssh_sessions.list() {
            if session.clipboard_host().is_some() {
                session.set_host_clipboard_blocked(true);
            }
        }
    }
    pub(crate) async fn load_launch_clipboard_policy(
        &self,
    ) -> Result<ClipboardPolicySnapshot, ClipboardPolicyError> {
        self.profiles
            .as_ref()
            .ok_or(ClipboardPolicyError::Unavailable)?
            .load_clipboard_policy()
            .await
    }
}
fn policy_error(error: ClipboardPolicyError) -> ClipboardPolicyResponse {
    ClipboardPolicyResponse::with_status(match error {
        ClipboardPolicyError::Unavailable => ClipboardPolicyStatus::Unavailable,
        ClipboardPolicyError::Invalid => ClipboardPolicyStatus::Invalid,
        ClipboardPolicyError::Conflict => ClipboardPolicyStatus::Conflict,
    })
}
