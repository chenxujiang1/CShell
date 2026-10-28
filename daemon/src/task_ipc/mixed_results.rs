//! Short-lived immutable captures. Tokens never refer to the mutable target table.
use super::*;
use std::time::{Duration, Instant};
const CACHE_BYTES: u64 = 128 * 1024 * 1024;
const CACHE_ENTRIES: usize = 8;
const TTL: Duration = Duration::from_secs(600);
#[derive(Debug)]
struct Capture {
    manifest: MixedResultManifest,
    document: MixedResultDocument,
    bytes: u64,
    expires: Instant,
}
#[derive(Debug, Default)]
pub(super) struct ResultCache(Mutex<BTreeMap<TaskRunId, Arc<Capture>>>);
impl ResultCache {
    fn insert(&self, capture: Capture) -> Result<(), TaskStatus> {
        let mut cache = self.0.lock().map_err(|_| TaskStatus::Unavailable)?;
        cache.retain(|_, item| item.expires > Instant::now());
        while cache.len() >= CACHE_ENTRIES
            || cache.values().map(|item| item.bytes).sum::<u64>() + capture.bytes > CACHE_BYTES
        {
            let oldest = cache
                .iter()
                .min_by_key(|(_, item)| item.expires)
                .map(|(id, _)| *id)
                .ok_or(TaskStatus::Limit)?;
            cache.remove(&oldest);
        }
        if cache
            .insert(capture.manifest.result_id, Arc::new(capture))
            .is_some()
        {
            return Err(TaskStatus::Corrupt);
        }
        Ok(())
    }
    fn get(&self, run: TaskRunId, result: TaskRunId) -> Result<Arc<Capture>, TaskStatus> {
        let mut cache = self.0.lock().map_err(|_| TaskStatus::Unavailable)?;
        cache.retain(|_, item| item.expires > Instant::now());
        let capture = cache.get(&result).ok_or(TaskStatus::Missing)?;
        if capture.manifest.header.run_id != run {
            return Err(TaskStatus::Invalid);
        }
        Ok(capture.clone())
    }
}
impl TaskIpcService {
    pub(super) async fn dispatch_mixed_results(
        &self,
        request: &TaskRequest,
        operation: TaskOperation,
    ) -> Result<TaskResponse, TaskStatus> {
        let mut canonical = TaskRequest::default();
        canonical.operation = request.operation;
        canonical.run_id.clone_from(&request.run_id);
        if operation == TaskOperation::MixedResultPage {
            canonical.result_id.clone_from(&request.result_id);
            canonical.host_id.clone_from(&request.host_id);
            canonical.limit = request.limit;
        } else if operation != TaskOperation::MixedResults {
            return Err(TaskStatus::Invalid);
        }
        if canonical != *request {
            return Err(TaskStatus::Invalid);
        }
        let run =
            TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
        let document = if operation == TaskOperation::MixedResults {
            let document = self
                .0
                .mixed
                .capture_results(run, self.0.clock.as_ref())
                .await
                .map_err(storage)?;
            let capture = tokio::task::spawn_blocking(move || {
                let (hash, bytes) = document
                    .fingerprint_and_size()
                    .map_err(|_| TaskStatus::Limit)?;
                let manifest = MixedResultManifest {
                    format: "cshell.mixed-result-manifest".into(),
                    version: 1,
                    result_id: TaskRunId::new(),
                    result_sha256: hash,
                    header: document.header.clone(),
                };
                manifest.validate().map_err(|_| TaskStatus::Corrupt)?;
                Ok::<_, TaskStatus>(Capture {
                    manifest,
                    document,
                    bytes,
                    expires: Instant::now() + TTL,
                })
            })
            .await
            .map_err(|_| TaskStatus::Unavailable)??;
            let bytes = serde_json::to_vec(&capture.manifest).map_err(|_| TaskStatus::Corrupt)?;
            if bytes.len() > 256 * 1024 {
                return Err(TaskStatus::Limit);
            }
            self.0.mixed_results.insert(capture)?;
            bytes
        } else {
            let result = TaskRunId::from_bytes(
                decode_id(&request.result_id).map_err(|_| TaskStatus::Invalid)?,
            );
            let after = if request.host_id.is_empty() {
                None
            } else {
                Some(HostId::from_bytes(
                    decode_id(&request.host_id).map_err(|_| TaskStatus::Invalid)?,
                ))
            };
            let capture = self.0.mixed_results.get(run, result)?;
            let page = capture
                .document
                .page(&capture.manifest, after, request.limit as usize)
                .map_err(|_| TaskStatus::Invalid)?;
            let bytes = serde_json::to_vec(&page).map_err(|_| TaskStatus::Corrupt)?;
            if bytes.len() > MAX_TASK_SNAPSHOT_BYTES {
                return Err(TaskStatus::Limit);
            }
            bytes
        };
        Ok(reply!(run_id:request.run_id.clone(),document_json:document))
    }
}
