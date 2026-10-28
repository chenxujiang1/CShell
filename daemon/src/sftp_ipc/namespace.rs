//! Serialize namespace-changing operations against managed transfers across sessions.
use super::*;
#[derive(Clone, Default)]
pub(super) struct Namespaces {
    locks: Arc<Mutex<BTreeMap<String, Arc<tokio::sync::RwLock<()>>>>>,
    destinations: Arc<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}
impl std::fmt::Debug for Namespaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Namespaces").finish_non_exhaustive()
    }
}
impl Namespaces {
    pub(super) fn scope(
        &self,
        local: bool,
        peer: &cshell_domain::ClipboardTrust,
    ) -> Result<Arc<tokio::sync::RwLock<()>>, &'static str> {
        let key = if local {
            "local".into()
        } else {
            serde_json::to_string(&(&peer.host.host, &peer.host_key_sha256, &peer.username))
                .map_err(|_| "namespace identity unavailable")?
        };
        let mut locks = self
            .locks
            .lock()
            .map_err(|_| "namespace locks unavailable")?;
        if locks.len() >= 512 {
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        if locks.len() >= 512 && !locks.contains_key(&key) {
            return Err("namespace capacity reached");
        }
        Ok(locks.entry(key).or_default().clone())
    }
    pub(super) async fn destination(
        &self,
        draft: &cshell_application::SftpQueueDraft,
        peer: &cshell_domain::ClipboardTrust,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, &'static str> {
        let path = if draft.direction == cshell_application::SftpQueueDirection::Download {
            let path = std::path::Path::new(&draft.local_path);
            let parent =
                tokio::fs::canonicalize(path.parent().ok_or("invalid destination parent")?)
                    .await
                    .map_err(|_| "destination parent unavailable")?;
            let leaf = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("invalid destination name")?;
            #[cfg(windows)]
            let leaf = leaf.trim_end_matches(['.', ' ']).to_lowercase();
            #[cfg(not(windows))]
            let leaf = leaf.to_owned();
            format!(
                "local:{}",
                parent
                    .join(leaf)
                    .to_str()
                    .ok_or("destination is not UTF-8")?
            )
        } else {
            format!(
                "remote:{}:{}",
                serde_json::to_string(&(&peer.host.host, &peer.host_key_sha256, &peer.username))
                    .map_err(|_| "namespace identity unavailable")?,
                draft.remote_path.to_lowercase()
            )
        };
        let lock = {
            let mut locks = self
                .destinations
                .lock()
                .map_err(|_| "destination locks unavailable")?;
            if locks.len() >= 512 {
                locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            }
            if locks.len() >= 512 && !locks.contains_key(&path) {
                return Err("destination capacity reached");
            }
            locks.entry(path).or_default().clone()
        };
        Ok(lock.lock_owned().await)
    }
}
