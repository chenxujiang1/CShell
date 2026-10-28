//! Durable definitions and native intent/acknowledgment facts for session tunnels.
use cshell_domain::{ClipboardTrust, ProfileId, SessionId, SshConnectionRecord, TunnelId};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
};
pub const MAX_TUNNEL_DOCUMENT: usize = 1024 * 1024;
pub const MAX_TUNNELS: usize = 128;
#[derive(Debug)]
pub enum TunnelReplyExpectation<'a> {
    Saved(TunnelId, &'a TunnelDefinition),
    Starting(TunnelId, SessionId),
    Stopping(TunnelId),
    Removed(TunnelId),
    DedicatedConfigured(TunnelId, &'a TunnelDedicatedConfiguration),
    DedicatedStarting(TunnelId),
    DedicatedCleared(TunnelId),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelReconnectPolicy {
    pub max_retries: u32,
    pub delay_ms: u32,
}
impl TunnelReconnectPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_retries > 5 || !(1000..=60000).contains(&self.delay_ms) {
            return Err("invalid tunnel retry bound");
        }
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelDedicatedConfiguration {
    pub profile_id: ProfileId,
    pub profile_revision: u64,
    pub reconnect: Option<TunnelReconnectPolicy>,
}
impl TunnelDedicatedConfiguration {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.profile_id.as_uuid().is_nil()
            || self.profile_revision == 0
            || self.profile_revision > i64::MAX as u64
        {
            return Err("invalid dedicated Profile binding");
        }
        if let Some(policy) = &self.reconnect {
            policy.validate()?;
        }
        Ok(())
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelDedicatedPlan {
    pub configuration: TunnelDedicatedConfiguration,
    pub reference_session: SessionId,
    pub target: SshConnectionRecord,
    pub jumps: Vec<SshConnectionRecord>,
}
impl TunnelDedicatedPlan {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.configuration.validate()?;
        if self.reference_session.as_uuid().is_nil()
            || self.target.profile_id != self.configuration.profile_id
            || self.jumps.len() > cshell_domain::MAX_SSH_JUMP_HOPS
        {
            return Err("invalid dedicated route");
        }
        let expected = cshell_domain::ssh_jump_chain(&self.target, &self.jumps)
            .map_err(|_| "invalid dedicated route")?;
        if expected.len() != self.jumps.len()
            || !expected.iter().zip(&self.jumps).all(|(a, b)| *a == b)
        {
            return Err("dedicated route order mismatch");
        }
        let mut records = self.jumps.clone();
        records.push(self.target.clone());
        let profiles = records
            .iter()
            .map(|c| cshell_domain::ProfileRecord {
                id: c.profile_id,
                name: c.profile_id.to_string(),
                kind: cshell_domain::ProfileKind::Ssh,
                folder_id: None,
                tags: Default::default(),
                favorite: false,
                terminal: Default::default(),
            })
            .collect();
        crate::ProfileCatalog::from_snapshot(crate::CatalogSnapshot {
            profiles,
            ssh_connections: records,
            ..Default::default()
        })
        .map_err(|_| "invalid frozen dedicated Profile")?;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelKind {
    Local,
    Remote,
    Dynamic,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelDefinition {
    pub name: String,
    pub kind: TunnelKind,
    pub bind_address: String,
    pub bind_port: u16,
    pub target_host: String,
    pub target_port: u16,
    pub allow_non_loopback: bool,
    pub max_connections: u32,
    pub socks_timeout_ms: u32,
}
impl Default for TunnelDefinition {
    fn default() -> Self {
        Self {
            name: "Tunnel".into(),
            kind: TunnelKind::Local,
            bind_address: "127.0.0.1".into(),
            bind_port: 0,
            target_host: "127.0.0.1".into(),
            target_port: 22,
            allow_non_loopback: false,
            max_connections: 32,
            socks_timeout_ms: 10000,
        }
    }
}
impl TunnelDefinition {
    pub fn validate(&self) -> Result<(), &'static str> {
        let ip: IpAddr = self
            .bind_address
            .parse()
            .map_err(|_| "bind address must be a literal IP")?;
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
            || !ip.is_loopback() && !self.allow_non_loopback
            || !(1..=128).contains(&self.max_connections)
            || !(1000..=30000).contains(&self.socks_timeout_ms)
            || self.kind == TunnelKind::Dynamic
                && (!self.target_host.is_empty() || self.target_port != 0)
            || self.kind != TunnelKind::Dynamic
                && (self.target_port == 0
                    || self.target_host.is_empty()
                    || self.target_host.len() > 255
                    || self.target_host.chars().any(|c| {
                        c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\' | '@')
                    }))
        {
            return Err("invalid tunnel definition or unacknowledged public bind");
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Unknown,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedicated: Option<TunnelDedicatedPlan>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dedicated_run: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reconnect_attempt: u32,
    pub id: TunnelId,
    pub definition: TunnelDefinition,
    pub state: TunnelState,
    pub session: Option<SessionId>,
    pub run: Option<SessionId>,
    pub peer: Option<ClipboardTrust>,
    pub bound_address: Option<String>,
    pub bound_port: Option<u16>,
    pub detail: String,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelSnapshot {
    pub format: String,
    pub version: u32,
    pub revision: u64,
    pub items: Vec<TunnelRecord>,
}
impl Default for TunnelSnapshot {
    fn default() -> Self {
        Self {
            format: "cshell.tunnels".into(),
            version: 1,
            revision: 1,
            items: Vec::new(),
        }
    }
}
impl TunnelSnapshot {
    pub fn confirm_reply(
        &self,
        previous: u64,
        expected: TunnelReplyExpectation<'_>,
    ) -> Result<(), &'static str> {
        if previous.checked_add(1) != Some(self.revision) {
            return Err("tunnel mutation revision mismatch");
        }
        let id = match &expected {
            TunnelReplyExpectation::Saved(id, _)
            | TunnelReplyExpectation::Starting(id, _)
            | TunnelReplyExpectation::Stopping(id)
            | TunnelReplyExpectation::Removed(id)
            | TunnelReplyExpectation::DedicatedConfigured(id, _)
            | TunnelReplyExpectation::DedicatedStarting(id)
            | TunnelReplyExpectation::DedicatedCleared(id) => *id,
        };
        let row = self.items.iter().find(|r| r.id == id);
        let matches = match expected {
            TunnelReplyExpectation::Saved(_, definition) => {
                row.is_some_and(|r| r.state == TunnelState::Stopped && &r.definition == definition)
            }
            TunnelReplyExpectation::Starting(_, session) => {
                row.is_some_and(|r| r.state == TunnelState::Starting && r.session == Some(session))
            }
            TunnelReplyExpectation::Stopping(_) => {
                row.is_some_and(|r| r.state == TunnelState::Stopping)
            }
            TunnelReplyExpectation::Removed(_) => row.is_none(),
            TunnelReplyExpectation::DedicatedConfigured(_, configuration) => row.is_some_and(|r| {
                r.state == TunnelState::Stopped
                    && r.dedicated
                        .as_ref()
                        .is_some_and(|p| &p.configuration == configuration)
            }),
            TunnelReplyExpectation::DedicatedStarting(_) => row.is_some_and(|r| {
                r.state == TunnelState::Starting && r.dedicated_run && r.dedicated.is_some()
            }),
            TunnelReplyExpectation::DedicatedCleared(_) => {
                row.is_some_and(|r| r.state == TunnelState::Stopped && r.dedicated.is_none())
            }
        };
        if !matches {
            return Err("tunnel mutation identity or effect mismatch");
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != "cshell.tunnels"
            || self.version != 1
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || self.items.len() > MAX_TUNNELS
        {
            return Err("invalid tunnel catalog");
        }
        let mut ids = BTreeSet::new();
        let mut active = 0usize;
        let mut per_session = BTreeMap::new();
        for row in &self.items {
            row.definition.validate()?;
            if let Some(plan) = &row.dedicated {
                plan.validate()?;
            }
            if row.id.as_uuid().is_nil()
                || !ids.insert(row.id)
                || row.session.is_some() != row.run.is_some()
                || row.session.is_some_and(|s| s.as_uuid().is_nil())
                || row.run.is_some_and(|s| s.as_uuid().is_nil())
                || row.bound_address.is_some() != row.bound_port.is_some()
                || row.bound_port == Some(0)
                || row
                    .bound_address
                    .as_ref()
                    .is_some_and(|s| s.parse::<IpAddr>().is_err())
                || row.detail.len() > 1024
                || row.detail.chars().any(char::is_control)
                || row.state != TunnelState::Stopped && (row.run.is_none() || row.peer.is_none())
                || row.state == TunnelState::Stopped && row.run.is_some()
                || row.peer.as_ref().is_some_and(|p| {
                    p.host.host.is_empty()
                        || p.host.host.len() > 255
                        || p.host.port == 0
                        || p.username.is_empty()
                        || p.username.len() > 128
                        || p.host_key_sha256.is_empty()
                        || p.host_key_sha256.len() > 256
                        || p.route.len() > 8192
                })
                || row.state == TunnelState::Running && row.bound_port.is_none()
                || row.dedicated.is_some() && row.peer.is_none()
                || row.dedicated_run
                    && (row.dedicated.is_none() || row.state == TunnelState::Stopped)
                || row.reconnect_attempt > 5
                || row.reconnect_attempt
                    > row
                        .dedicated
                        .as_ref()
                        .and_then(|p| p.configuration.reconnect.as_ref())
                        .map_or(0, |p| p.max_retries)
            {
                return Err("invalid tunnel state or binding");
            }
            if row.state != TunnelState::Stopped {
                active += 1;
                let count = per_session.entry(row.session).or_insert(0usize);
                *count += 1;
                if active > 32 || *count > 8 {
                    return Err("tunnel active/unknown budget exceeded");
                }
            }
        }
        Ok(())
    }
    pub fn changed(&mut self) -> Result<(), &'static str> {
        self.revision = self
            .revision
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or("tunnel revision exhausted")?;
        self.validate()
    }
}
pub fn decode_tunnels(bytes: &[u8]) -> Result<TunnelSnapshot, &'static str> {
    if bytes.len() > MAX_TUNNEL_DOCUMENT {
        return Err("tunnel catalog limit");
    }
    let value: TunnelSnapshot =
        serde_json::from_slice(bytes).map_err(|_| "invalid tunnel catalog JSON")?;
    value.validate()?;
    Ok(value)
}
macro_rules! redact { ($($t:ty),*) => { $(impl std::fmt::Debug for $t { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(concat!(stringify!($t), " { [REDACTED] }")) } })* }; }
fn is_false(value: &bool) -> bool {
    !*value
}
fn is_zero(value: &u32) -> bool {
    *value == 0
}
redact!(
    TunnelDefinition,
    TunnelRecord,
    TunnelSnapshot,
    TunnelDedicatedConfiguration,
    TunnelDedicatedPlan
);
