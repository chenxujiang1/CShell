//! Runtime grants stay in the host; JS receives no grant or secret values.
use crate::*;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub enum ScriptHostOperation {
    Send {
        text: Zeroizing<String>,
        append_newline: bool,
    },
    SendParameter {
        name: String,
        value: Zeroizing<String>,
        append_newline: bool,
    },
    Wait {
        text: Zeroizing<String>,
        timeout_ms: u32,
    },
    ProfileRead,
    TaskSubmit {
        binding: String,
    },
    Notify {
        message: Zeroizing<String>,
    },
}
impl std::fmt::Debug for ScriptHostOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptHostOperation(..)")
    }
}
pub type ScriptHostFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HostResult, ScriptWorkerError>> + Send + 'a>>;
pub trait ScriptHostBroker: Send + Sync + 'static {
    fn authorized(&self) -> bool;
    fn invoke(&self, operation: ScriptHostOperation) -> ScriptHostFuture<'_>;
    fn effects_confirmed(&self) -> bool;
}
pub struct ScriptGrant {
    revoked: Arc<AtomicBool>,
    expires: Instant,
    digest: [u8; 32],
    selected: BTreeSet<ScriptCapability>,
    pub(crate) cancel: ScriptCancellation,
}
impl std::fmt::Debug for ScriptGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScriptGrant(..)")
    }
}
impl ScriptGrant {
    /// Explicit confirmation of document, plan and concrete target is required
    /// at the trusted entry point. An inert plan never creates authority.
    pub fn issue(
        plan: &cshell_application::ScriptPermissionPlan,
        confirmed: bool,
    ) -> Result<Self, ScriptWorkerError> {
        if !confirmed || !plan.missing.is_empty() || plan.selected != plan.requested {
            return Err(ScriptWorkerError::Capability);
        }
        Ok(Self {
            revoked: Arc::new(AtomicBool::new(false)),
            expires: Instant::now()
                + Duration::from_millis(u64::from(plan.limits.timeout_ms) + 10_000),
            digest: plan.document_sha256,
            selected: plan.selected.clone(),
            cancel: ScriptCancellation::default(),
        })
    }
    pub fn cancellation(&self) -> ScriptCancellation {
        self.cancel.clone()
    }
    pub fn issue_armed(
        plan: &cshell_application::ScriptPermissionPlan,
        confirmed: bool,
    ) -> Result<Self, ScriptWorkerError> {
        let mut grant = Self::issue(plan, confirmed)?;
        grant.expires += Duration::from_secs(30);
        Ok(grant)
    }
    pub fn revocation(&self) -> ScriptRevocation {
        ScriptRevocation {
            revoked: self.revoked.clone(),
            cancel: self.cancel.clone(),
        }
    }
    pub(crate) fn valid(&self, start: &WorkerStart) -> bool {
        !self.revoked.load(Ordering::Acquire)
            && Instant::now() < self.expires
            && start.document_sha256 == self.digest
            && start.selected == self.selected
    }
}
impl Drop for ScriptGrant {
    fn drop(&mut self) {
        self.revoked.store(true, Ordering::Release);
        self.cancel.cancel();
    }
}
#[derive(Clone)]
pub struct ScriptRevocation {
    revoked: Arc<AtomicBool>,
    cancel: ScriptCancellation,
}
impl ScriptRevocation {
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        self.cancel.cancel();
    }
}
pub(crate) struct LiveAuthority {
    pub grant: ScriptGrant,
    pub broker: Arc<dyn ScriptHostBroker>,
}
impl std::fmt::Debug for LiveAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LiveAuthority(..)")
    }
}
pub struct LiveScriptRun(pub(crate) crate::OfflineScriptRun);
impl LiveScriptRun {
    pub fn authorized(&self) -> bool {
        self.0
            .live
            .as_ref()
            .is_some_and(|a| a.grant.valid(&self.0.start) && a.broker.authorized())
    }
    pub fn review(
        document: ScriptDocument,
        plan: cshell_application::ScriptPermissionPlan,
        parameters: ScriptParameterValues,
        grant: ScriptGrant,
        broker: Arc<dyn ScriptHostBroker>,
    ) -> Result<Self, ScriptWorkerError> {
        if cshell_application::plan_script_permissions(&document, &plan.selected)
            .map_err(|_| ScriptWorkerError::Invalid)?
            != plan
            || !plan.missing.is_empty()
        {
            return Err(ScriptWorkerError::Invalid);
        }
        parameters.validate(&document)?;
        let start = WorkerStart {
            version: WORKER_VERSION,
            run_id: ScriptRunId::new(),
            document,
            document_sha256: plan.document_sha256,
            selected: plan.selected,
            live: true,
        };
        start.validate()?;
        if !grant.valid(&start) || !broker.authorized() {
            return Err(ScriptWorkerError::Capability);
        }
        Ok(Self(crate::OfflineScriptRun {
            start,
            parameters,
            live: Some(LiveAuthority { grant, broker }),
        }))
    }
    pub fn run_id(&self) -> ScriptRunId {
        self.0.start.run_id
    }
}
pub async fn run_live(run: LiveScriptRun) -> Result<ScriptRunReport, ScriptWorkerError> {
    let cancel = run
        .0
        .live
        .as_ref()
        .ok_or(ScriptWorkerError::Capability)?
        .grant
        .cancellation();
    crate::supervisor::run_supervised(run.0, cancel).await
}
