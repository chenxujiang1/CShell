use crate::SessionId;
use serde::{Deserialize, Serialize};

/// One explicitly requested Linux sample. No process names or filesystem paths.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerResources {
    pub session: SessionId,
    pub transport_generation: u64,
    pub sampled_unix_ms: u64,
    pub cpu_basis_points: Option<u16>,
    pub memory_total_kib: u64,
    pub memory_available_kib: u64,
    pub swap_total_kib: u64,
    pub swap_free_kib: u64,
    pub load_milli: [u32; 3],
    pub uptime_seconds: u64,
}

impl ServerResources {
    pub fn validate(&self, session: SessionId) -> bool {
        self.session == session
            && !session.as_uuid().is_nil()
            && self.sampled_unix_ms > 0
            && self.cpu_basis_points.is_none_or(|value| value <= 10_000)
            && self.memory_total_kib > 0
            && self.memory_total_kib <= (1 << 50)
            && self.memory_available_kib <= self.memory_total_kib
            && self.swap_total_kib <= (1 << 50)
            && self.swap_free_kib <= self.swap_total_kib
    }
}
