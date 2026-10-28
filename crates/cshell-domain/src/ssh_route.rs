//! Resolve saved route references before opening any network connection.
use crate::{ProfileId, SshConnectionRecord, SshRoute};
use std::collections::BTreeSet;
use thiserror::Error;

pub const MAX_SSH_JUMP_HOPS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum SshRouteError {
    #[error("jump Profile is missing or ambiguous")]
    MissingProfile,
    #[error("SSH route contains a cycle")]
    Cycle,
    #[error("SSH route exceeds eight jump Profiles")]
    Depth,
}

/// Return the outermost jump first and the target's immediate jump last.
/// The first jump may use direct/SOCKS5/HTTP transport. Each subsequent hop
/// authenticates through a direct-tcpip stream opened by its preceding hop.
pub fn ssh_jump_chain<'a>(
    target: &SshConnectionRecord,
    connections: &'a [SshConnectionRecord],
) -> Result<Vec<&'a SshConnectionRecord>, SshRouteError> {
    let mut chain = Vec::new();
    let mut visited = BTreeSet::<ProfileId>::from([target.profile_id]);
    let mut route = &target.route;
    while let SshRoute::Jump { profile_id } = route {
        if !visited.insert(*profile_id) {
            return Err(SshRouteError::Cycle);
        }
        if chain.len() >= MAX_SSH_JUMP_HOPS {
            return Err(SshRouteError::Depth);
        }
        let mut matching = connections.iter().filter(|c| c.profile_id == *profile_id);
        let jump = matching.next().ok_or(SshRouteError::MissingProfile)?;
        if matching.next().is_some() {
            return Err(SshRouteError::MissingProfile);
        }
        chain.push(jump);
        route = &jump.route;
    }
    chain.reverse();
    Ok(chain)
}
