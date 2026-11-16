//! Daemon-local weak transport registry. Admission never reads authentication secrets.
use crate::{
    session_ipc::SshLaunchFailure,
    shared_ssh::{TransportLease, WeakTransport},
    ssh_route::SshConnectionPlan,
};
use cshell_domain::{
    JumpConnectionOverride, JumpReusePolicy, ProfileId, SessionId, SshConnectionRecord,
    SshProviderId, SshRoute,
};
use cshell_ipc::{
    JumpCandidateData, JumpCandidateSelectionData, JumpCandidatesResponse, JumpCandidatesStatus,
    SessionFailureCode,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
struct Entry {
    title: String,
    transport: WeakTransport,
    record: SshConnectionRecord,
    generation: u64,
}
struct Choice {
    id: SessionId,
    generation: u64,
    plan: [u8; 32],
    expires: Instant,
}
#[derive(Default)]
pub(crate) struct Registry {
    entries: Mutex<BTreeMap<SessionId, Entry>>,
    choices: Mutex<BTreeMap<[u8; 32], Choice>>,
    gates: Mutex<BTreeMap<ProfileId, Weak<tokio::sync::Mutex<()>>>>,
    generation: AtomicU64,
}
impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JumpRegistry").finish_non_exhaustive()
    }
}
pub(crate) struct Prepared {
    pub plan: SshConnectionPlan,
    pub borrowed: Option<TransportLease>,
    _serial: Option<tokio::sync::OwnedMutexGuard<()>>,
    _setup: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl Prepared {
    pub fn credentials(&self) -> SshConnectionPlan {
        let mut plan = self.plan.clone();
        plan.target.options.jump_reuse_policy = JumpReusePolicy::AlwaysNew;
        if self.borrowed.is_some() {
            plan.jumps.clear();
        }
        plan
    }
}
fn error(detail: &'static str) -> SshLaunchFailure {
    SshLaunchFailure::new(SessionFailureCode::ProtocolRejected, detail)
}
fn bound(plan: &SshConnectionPlan) -> Result<[u8; 32], SshLaunchFailure> {
    let mut target = plan.target.clone();
    target.options.jump_reuse_policy = JumpReusePolicy::AlwaysNew;
    let data = serde_json::to_vec(&(target, &plan.jumps)).map_err(|_| error("连接配置无法核对"))?;
    Ok(Sha256::digest(data).into())
}
impl Registry {
    #[cfg(test)]
    pub(crate) fn expire_choices(&self) {
        for choice in self
            .choices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values_mut()
        {
            choice.expires = Instant::now() - Duration::from_secs(1);
        }
    }
    pub(crate) async fn validate_binding(
        plan: &SshConnectionPlan,
        profiles: &crate::ProfileIpcService,
    ) -> Result<(), SshLaunchFailure> {
        let crate::profile_ipc::SavedSessionPlan::Ssh(current) =
            profiles.session_plan(plan.target.profile_id).await?
        else {
            return Err(error("连接配置类型已变化，请重新选择"));
        };
        let mut current = (*current).clone();
        if plan.target.username.is_empty() {
            current.target.username.clear();
        }
        if bound(&current)? != bound(plan)? {
            return Err(error("连接配置在选择后发生变化，请重新连接"));
        }
        Ok(())
    }
    fn register(&self, record: &SshConnectionRecord, lease: &TransportLease, title: &str) {
        if matches!(record.route, SshRoute::Jump { .. })
            || record.options.authentication_on_jump
            || lease.connection().provider_id() != SshProviderId::Russh
            || !lease.running()
        {
            return;
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, entry| entry.transport.live());
        if entries.len() >= 256 {
            return;
        }
        entries.entry(lease.identity()).or_insert_with(|| Entry {
            title: title.chars().take(128).collect(),
            transport: lease.weak(),
            record: {
                let mut record = record.clone();
                if let Some(username) = lease.connection().authenticated_username() {
                    record.username = username.to_owned();
                }
                record
            },
            generation: self
                .generation
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1),
        });
    }
    pub(crate) fn register_route(&self, plan: &SshConnectionPlan, lease: &TransportLease) {
        if plan.jumps.is_empty() {
            self.register(&plan.target, lease, &plan.title);
        }
        if let Some(jumps) = lease.jump() {
            for (record, lease) in plan.jumps.iter().zip(jumps.leases()) {
                self.register(record, lease, "跳板连接");
            }
        }
    }
    async fn matches(
        &self,
        plan: &SshConnectionPlan,
        path: &Path,
    ) -> Result<Vec<(SessionId, Entry)>, SshLaunchFailure> {
        let Some(wanted) = plan.jumps.first().filter(|_| plan.jumps.len() == 1) else {
            return Ok(Vec::new());
        };
        let verifier = crate::host_ca_store::verifier(path, &wanted.host, wanted.port)
            .await
            .map_err(|_| error("跳板信任记录无法读取，请检查指纹配置"))?;
        let entries: Vec<_> = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.retain(|_, entry| entry.transport.live());
            entries
                .iter()
                .map(|(id, entry)| (*id, entry.clone()))
                .collect()
        };
        Ok(entries
            .into_iter()
            .filter(|(_, entry)| {
                entry.record == *wanted
                    && !wanted.username.is_empty()
                    && entry.transport.client().is_some_and(|client| {
                        client.authenticated_username() == Some(wanted.username.as_str())
                            && client.revalidate_host_identity(&verifier)
                    })
            })
            .collect())
    }
    pub(crate) async fn query(
        &self,
        plan: &SshConnectionPlan,
        path: &Path,
    ) -> Result<JumpCandidatesResponse, SshLaunchFailure> {
        let mut response = JumpCandidatesResponse {
            execution_supported: true,
            status: JumpCandidatesStatus::Ok as i32,
            saved_policy: cshell_ipc::encode_jump_reuse_policy(
                plan.target.options.jump_reuse_policy,
            ),
            is_jump: !plan.jumps.is_empty(),
            ..Default::default()
        };
        if plan.jumps.len() > 1 {
            response.execution_supported = false;
            response.detail = "多级跳板仅支持独立连接".into();
            return Ok(response);
        }
        let matching = self.matches(plan, path).await?;
        if matching.len() > 64 {
            return Err(error("同一跳板连接过多，请关闭不再使用的连接后重试"));
        }
        let binding = bound(plan)?;
        let mut choices = self
            .choices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        choices.retain(|_, choice| choice.expires > Instant::now());
        if choices.len().saturating_add(matching.len()) > 512 {
            return Err(error("跳板选择请求过多，请稍后重试"));
        }
        for (id, entry) in matching {
            let token: [u8; 32] = rand::random();
            choices.insert(
                token,
                Choice {
                    id,
                    generation: entry.generation,
                    plan: binding,
                    expires: Instant::now() + Duration::from_secs(60),
                },
            );
            response.candidates.push(JumpCandidateData {
                source_title: entry.title,
                consumers: entry.transport.consumers() as u32,
                host_key_sha256: entry
                    .transport
                    .client()
                    .map(|client| client.verified_host_key_fingerprint().to_owned())
                    .unwrap_or_default(),
                selection: Some(JumpCandidateSelectionData {
                    transport_id: id.as_uuid().as_bytes().to_vec(),
                    generation: entry.generation,
                    token: token.to_vec(),
                }),
                jump_profile_id: entry.record.profile_id.as_uuid().as_bytes().to_vec(),
                username: entry.record.username,
                host: entry.record.host,
                port: u32::from(entry.record.port),
                expires_unix_seconds: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    + 60,
            });
        }
        Ok(response)
    }
    pub(crate) async fn prepare(
        &self,
        plan: &SshConnectionPlan,
        selection: Option<&JumpConnectionOverride>,
        path: &Path,
    ) -> Result<Prepared, SshLaunchFailure> {
        let plan = plan.with_jump_override(selection)?;
        if plan.target.options.jump_reuse_policy.is_always_new() {
            return Ok(Prepared {
                plan,
                borrowed: None,
                _serial: None,
                _setup: None,
            });
        }
        let hop = plan
            .jumps
            .first()
            .filter(|_| plan.jumps.len() == 1)
            .ok_or_else(|| error("当前仅支持单层跳板复用"))?;
        let mut matching = self.matches(&plan, path).await?;
        let serial = if plan.target.options.jump_reuse_policy == JumpReusePolicy::PreferExisting
            && matching.is_empty()
            && selection.is_none_or(|choice| choice.candidate.is_none())
        {
            let gate = {
                let mut gates = self
                    .gates
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                gates.retain(|_, gate| gate.strong_count() > 0);
                if gates.len() >= 256 {
                    return Err(error("跳板连接请求过多"));
                }
                if let Some(gate) = gates.get(&hop.profile_id).and_then(Weak::upgrade) {
                    gate
                } else {
                    let gate = Arc::new(tokio::sync::Mutex::new(()));
                    gates.insert(hop.profile_id, Arc::downgrade(&gate));
                    gate
                }
            };
            let guard = gate.lock_owned().await;
            matching = self.matches(&plan, path).await?;
            Some(guard)
        } else {
            None
        };
        let picked = if let Some(candidate) =
            selection.and_then(|selection| selection.candidate.as_ref())
        {
            let choice = self
                .choices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&candidate.token)
                .ok_or_else(|| error("跳板选择已过期或已使用，请刷新候选"))?;
            if choice.id != candidate.transport_id
                || choice.generation != candidate.generation
                || choice.plan != bound(&plan)?
                || choice.expires <= Instant::now()
            {
                return Err(error("跳板选择与当前配置不符或已过期，请重新选择"));
            }
            Some(
                matching
                    .into_iter()
                    .find(|(id, entry)| *id == choice.id && entry.generation == choice.generation)
                    .ok_or_else(|| error("所选跳板已失效，未切换到其他连接"))?,
            )
        } else {
            if matching.len() > 1 {
                return Err(SshLaunchFailure::new(
                    SessionFailureCode::JumpSelectionRequired,
                    "存在多个可复用跳板，请先选择连接",
                ));
            }
            matching.into_iter().next()
        };
        let borrowed = if let Some((_, entry)) = picked {
            Some(entry.transport.acquire().map_err(error)?)
        } else if plan.target.options.jump_reuse_policy == JumpReusePolicy::OnlyExisting {
            return Err(error(
                "没有符合配置和身份要求的已连接跳板；仅复用模式不会新建连接",
            ));
        } else {
            None
        };
        let setup = if let Some(lease) = &borrowed {
            Some(lease.reserve_jump_setup().await.map_err(error)?)
        } else {
            None
        };
        Ok(Prepared {
            plan,
            borrowed,
            _serial: serial,
            _setup: setup,
        })
    }
}
