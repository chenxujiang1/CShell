//! Read-only, versioned capability discovery and deterministic preflight.
use cshell_domain::{SshAlgorithmPolicy, SshCapability, SshProviderId, SshProviderRequirements};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_SSH_PROVIDER_DOCUMENT: usize = 32 * 1024;
pub const MAX_SSH_PROVIDER_REQUEST: usize = 4 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshProviderDescriptor {
    pub id: SshProviderId,
    pub capabilities: BTreeSet<SshCapability>,
    pub algorithm_policies: BTreeSet<SshAlgorithmPolicy>,
    pub max_jump_hops: u8,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshProviderCatalog {
    pub format: String,
    pub version: u32,
    /// Registration order is the automatic selection preference.
    pub providers: Vec<SshProviderDescriptor>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SshProviderRefusal {
    Unregistered {
        provider: SshProviderId,
    },
    MissingCapabilities {
        provider: SshProviderId,
        capabilities: BTreeSet<SshCapability>,
    },
    AlgorithmPolicy {
        provider: SshProviderId,
        policy: SshAlgorithmPolicy,
    },
    JumpDepth {
        provider: SshProviderId,
        requested: u8,
        maximum: u8,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshProviderEvaluation {
    pub requirements: SshProviderRequirements,
    pub selected: Option<SshProviderId>,
    pub refusals: Vec<SshProviderRefusal>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshProviderReport {
    pub catalog: SshProviderCatalog,
    pub evaluation: Option<SshProviderEvaluation>,
}
impl SshProviderCatalog {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != "cshell.ssh-providers"
            || self.version != 1
            || self.providers.is_empty()
            || self.providers.len() > 8
        {
            return Err("invalid SSH provider catalog");
        }
        let mut ids = BTreeSet::new();
        for p in &self.providers {
            if !ids.insert(p.id)
                || p.algorithm_policies.is_empty()
                || p.max_jump_hops as usize > cshell_domain::MAX_SSH_JUMP_HOPS
                || (p.max_jump_hops > 0
                    && (!p.capabilities.contains(&SshCapability::JumpHost)
                        || !p.capabilities.contains(&SshCapability::DirectTcpip)))
                || ((p.capabilities.contains(&SshCapability::OpenSshAgent)
                    || p.capabilities.contains(&SshCapability::Pageant))
                    && !p.capabilities.contains(&SshCapability::Agent))
            {
                return Err("inconsistent SSH provider descriptor");
            }
        }
        Ok(())
    }
    pub fn evaluate(&self, input: &SshProviderRequirements) -> SshProviderEvaluation {
        let mut result = SshProviderEvaluation {
            requirements: input.clone(),
            selected: None,
            refusals: Vec::new(),
        };
        if let Some(provider) = input.provider
            && !self.providers.iter().any(|p| p.id == provider)
        {
            result
                .refusals
                .push(SshProviderRefusal::Unregistered { provider });
            return result;
        }
        let required = input.effective_capabilities();
        for p in self
            .providers
            .iter()
            .filter(|p| input.provider.is_none_or(|id| id == p.id))
        {
            let before = result.refusals.len();
            let missing = required
                .difference(&p.capabilities)
                .copied()
                .collect::<BTreeSet<_>>();
            if !missing.is_empty() {
                result
                    .refusals
                    .push(SshProviderRefusal::MissingCapabilities {
                        provider: p.id,
                        capabilities: missing,
                    });
            }
            if !p.algorithm_policies.contains(&input.algorithm_policy) {
                result.refusals.push(SshProviderRefusal::AlgorithmPolicy {
                    provider: p.id,
                    policy: input.algorithm_policy,
                });
            }
            if input.jump_hops > p.max_jump_hops {
                result.refusals.push(SshProviderRefusal::JumpDepth {
                    provider: p.id,
                    requested: input.jump_hops,
                    maximum: p.max_jump_hops,
                });
            }
            if result.refusals.len() == before {
                result.selected = Some(p.id);
                break;
            }
        }
        result
    }
}
impl SshProviderReport {
    pub fn confirm(&self, input: Option<&SshProviderRequirements>) -> Result<(), &'static str> {
        self.catalog.validate()?;
        if self.evaluation != input.map(|input| self.catalog.evaluate(input)) {
            return Err("SSH provider reply scope or decision mismatch");
        }
        Ok(())
    }
}
pub fn decode_ssh_provider_report(
    bytes: &[u8],
    input: Option<&SshProviderRequirements>,
) -> Result<SshProviderReport, &'static str> {
    if bytes.len() > MAX_SSH_PROVIDER_DOCUMENT {
        return Err("SSH provider reply limit");
    }
    let report: SshProviderReport =
        serde_json::from_slice(bytes).map_err(|_| "invalid SSH provider reply")?;
    report.confirm(input)?;
    Ok(report)
}

/// The complete route is checked before reading any credentials or opening a socket.
pub fn ssh_connection_requirements(
    target: &cshell_domain::SshConnectionRecord,
    jump_hops: usize,
) -> Result<SshProviderRequirements, &'static str> {
    use cshell_domain::{SshAgentBackend as Agent, SshRoute};
    let mut requirements = SshProviderRequirements {
        jump_hops: u8::try_from(jump_hops).map_err(|_| "SSH jump depth overflow")?,
        ..Default::default()
    };
    for method in target.authentication_steps(){requirements.capabilities.insert(match method {cshell_domain::SshAuthStep::KeyboardInteractive=>SshCapability::KeyboardInteractive,cshell_domain::SshAuthStep::Password=>SshCapability::Password,cshell_domain::SshAuthStep::PrivateKey=>SshCapability::PublicKey,cshell_domain::SshAuthStep::Certificate=>SshCapability::UserCertificate,cshell_domain::SshAuthStep::Agent=>SshCapability::Agent});}
    if target.authentication_steps().contains(&cshell_domain::SshAuthStep::Agent) {
        match target.agent_backend {
            Agent::Auto => (),
            Agent::OpenSsh => {
                requirements
                    .capabilities
                    .insert(SshCapability::OpenSshAgent);
            }
            Agent::Pageant => {
                requirements.capabilities.insert(SshCapability::Pageant);
            }
        }
    }
    match target.route {
        SshRoute::Socks5 { .. } => {
            requirements.capabilities.insert(SshCapability::Socks5Proxy);
        }
        SshRoute::HttpConnect { .. } => {
            requirements
                .capabilities
                .insert(SshCapability::HttpConnectProxy);
        }
        SshRoute::Jump { .. } => {
            requirements.capabilities.insert(SshCapability::JumpHost);
        }
        SshRoute::Direct => (),
    }
    Ok(requirements)
}
