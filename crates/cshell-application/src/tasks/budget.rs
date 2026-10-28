use super::*;
use std::{collections::BTreeMap, net::IpAddr, sync::Mutex};

/// Limits shared by all runners using this budget. Endpoint limits apply to
/// destination execution; jump limits apply to intermediate SSH/proxy endpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskResourceLimits {
    pub global: u16,
    pub per_endpoint: u16,
    pub per_jump: u16,
}
impl Default for TaskResourceLimits {
    fn default() -> Self {
        Self {
            global: 50,
            per_endpoint: 1,
            per_jump: 10,
        }
    }
}
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct Endpoint {
    host: String,
    port: u16,
}
impl Endpoint {
    fn new(host: &str, port: u16) -> Self {
        let unbracketed = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let host = unbracketed.parse::<IpAddr>().map_or_else(
            |_| host.trim_end_matches('.').to_ascii_lowercase(),
            |ip| ip.to_string(),
        );
        Self { host, port }
    }
}
#[derive(Clone)]
pub(super) struct ResourceRequest {
    run: TaskRunId,
    concurrency: u16,
    endpoint: Endpoint,
    jumps: BTreeSet<Endpoint>,
}
impl ResourceRequest {
    pub(super) fn new(snapshot: &TaskSnapshot, host: HostId) -> Result<Self, TaskError> {
        Self::frozen(
            snapshot.id(),
            snapshot.targets(),
            snapshot.definition().policy.concurrency,
            host,
        )
    }
    pub(super) fn frozen(
        run: TaskRunId,
        targets: &FrozenHostTargets,
        concurrency: u16,
        host: HostId,
    ) -> Result<Self, TaskError> {
        if concurrency == 0 || concurrency > 50 {
            return Err(TaskError::Invalid);
        }
        let connection = targets
            .resolved_connection(host)
            .ok_or(TaskError::UnknownTarget)?;
        let endpoint = Endpoint::new(&connection.host, connection.port);
        let mut jumps = BTreeSet::new();
        let mut route = &connection.route;
        let mut visited = BTreeSet::new();
        loop {
            match route {
                SshRoute::Direct => break,
                SshRoute::Socks5 { host, port } | SshRoute::HttpConnect { host, port } => {
                    jumps.insert(Endpoint::new(host, *port));
                    break;
                }
                SshRoute::Jump { profile_id } => {
                    if *profile_id == connection.profile_id
                        || !visited.insert(*profile_id)
                        || visited.len() > cshell_domain::MAX_SSH_JUMP_HOPS
                    {
                        return Err(TaskError::Invalid);
                    }
                    let jump = targets
                        .connections()
                        .iter()
                        .find(|c| c.profile_id == *profile_id)
                        .ok_or(TaskError::Invalid)?;
                    jumps.insert(Endpoint::new(&jump.host, jump.port));
                    route = &jump.route;
                }
            }
        }
        Ok(Self {
            run,
            concurrency,
            endpoint,
            jumps,
        })
    }
}
#[derive(Default)]
struct Counts {
    global: u16,
    owners: BTreeSet<TaskRunId>,
    runs: BTreeMap<TaskRunId, u16>,
    endpoints: BTreeMap<Endpoint, u16>,
    jumps: BTreeMap<Endpoint, u16>,
}
struct BudgetInner {
    limits: TaskResourceLimits,
    counts: Mutex<Counts>,
    changed: tokio::sync::Notify,
}
#[derive(Clone)]
pub struct TaskResourceBudget(Arc<BudgetInner>);
impl fmt::Debug for TaskResourceBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskResourceBudget")
            .field("limits", &self.0.limits)
            .finish_non_exhaustive()
    }
}
impl TaskResourceBudget {
    pub fn new(limits: TaskResourceLimits) -> Result<Self, TaskError> {
        if limits.global == 0
            || limits.global > 1000
            || limits.per_endpoint == 0
            || limits.per_endpoint > limits.global
            || limits.per_jump == 0
            || limits.per_jump > limits.global
        {
            return Err(TaskError::Invalid);
        }
        Ok(Self(Arc::new(BudgetInner {
            limits,
            counts: Mutex::new(Counts::default()),
            changed: tokio::sync::Notify::new(),
        })))
    }
    pub fn try_acquire(
        &self,
        snapshot: &TaskSnapshot,
        host: HostId,
    ) -> Result<Option<TaskResourceLease>, TaskError> {
        self.acquire(ResourceRequest::new(snapshot, host)?)
    }
    pub fn try_acquire_frozen(
        &self,
        run: TaskRunId,
        targets: &FrozenHostTargets,
        concurrency: u16,
        host: HostId,
    ) -> Result<Option<TaskResourceLease>, TaskError> {
        self.acquire(ResourceRequest::frozen(run, targets, concurrency, host)?)
    }
    pub(super) fn claim_run(&self, id: TaskRunId) -> Result<RunLease, TaskError> {
        let mut counts = self.0.counts.lock().map_err(|_| TaskError::Invalid)?;
        if counts.owners.len() >= 64 && !counts.owners.contains(&id) {
            return Err(TaskError::Limit);
        }
        if !counts.owners.insert(id) {
            return Err(TaskError::Transition);
        }
        Ok(RunLease {
            budget: self.clone(),
            id,
        })
    }
    pub(super) fn acquire(
        &self,
        request: ResourceRequest,
    ) -> Result<Option<TaskResourceLease>, TaskError> {
        let mut counts = self.0.counts.lock().map_err(|_| TaskError::Invalid)?;
        let limits = self.0.limits;
        if counts.global >= limits.global
            || counts.runs.get(&request.run).copied().unwrap_or(0) >= request.concurrency
            || counts
                .endpoints
                .get(&request.endpoint)
                .copied()
                .unwrap_or(0)
                >= limits.per_endpoint
            || request
                .jumps
                .iter()
                .any(|key| counts.jumps.get(key).copied().unwrap_or(0) >= limits.per_jump)
        {
            return Ok(None);
        }
        counts.global += 1;
        *counts.runs.entry(request.run).or_default() += 1;
        *counts
            .endpoints
            .entry(request.endpoint.clone())
            .or_default() += 1;
        for key in &request.jumps {
            *counts.jumps.entry(key.clone()).or_default() += 1;
        }
        Ok(Some(TaskResourceLease {
            budget: self.clone(),
            request,
        }))
    }
    pub(super) async fn changed(&self) {
        self.0.changed.notified().await;
    }
}
pub(super) struct RunLease {
    budget: TaskResourceBudget,
    id: TaskRunId,
}
impl Drop for RunLease {
    fn drop(&mut self) {
        self.budget
            .0
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .owners
            .remove(&self.id);
        self.budget.0.changed.notify_waiters();
    }
}
/// Non-cloneable all-or-nothing reservation, held until worker cleanup completes.
pub struct TaskResourceLease {
    budget: TaskResourceBudget,
    request: ResourceRequest,
}
impl fmt::Debug for TaskResourceLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskResourceLease")
            .field("run", &self.request.run)
            .finish_non_exhaustive()
    }
}
fn decrement<K: Ord>(map: &mut BTreeMap<K, u16>, key: &K) {
    if let Some(count) = map.get_mut(key) {
        *count -= 1;
        if *count == 0 {
            map.remove(key);
        }
    }
}
impl Drop for TaskResourceLease {
    fn drop(&mut self) {
        // A poisoned budget must never admit more work. Still release existing
        // counters so shutdown does not retain endpoint/configuration strings.
        let mut counts = self
            .budget
            .0
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counts.global -= 1;
        decrement(&mut counts.runs, &self.request.run);
        decrement(&mut counts.endpoints, &self.request.endpoint);
        for key in &self.request.jumps {
            decrement(&mut counts.jumps, key);
        }
        drop(counts);
        self.budget.0.changed.notify_waiters();
    }
}
