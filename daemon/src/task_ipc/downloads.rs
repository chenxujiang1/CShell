use super::*;

impl TaskIpcService {
    pub(super) async fn cancel_download(&self, request: &TaskRequest) -> TaskResponse {
        let mut canonical = TaskRequest::default();
        canonical.operation = TaskOperation::DownloadCancel as i32;
        canonical.run_id.clone_from(&request.run_id);
        if canonical != *request {
            return response(TaskStatus::Invalid);
        }
        let Ok(bytes) = decode_id(&request.run_id) else {
            return response(TaskStatus::Invalid);
        };
        let id = TaskRunId::from_bytes(bytes);
        match self.0.downloads.cancel(id) {
            Ok(()) => reply!(run_id: request.run_id.clone()),
            Err(TaskRepositoryError::Conflict) => match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.0.downloads.cancel_pending(id),
            )
            .await
            {
                Ok(Ok(())) => reply!(run_id: request.run_id.clone()),
                Ok(Err(error)) => response(storage(error)),
                Err(_) => response(TaskStatus::Unavailable),
            },
            Err(error) => response(storage(error)),
        }
    }
    async fn download_view(&self, id: TaskRunId) -> Result<TaskResponse, TaskStatus> {
        let (snapshot, rows) = self.0.downloads.get(id).await.map_err(storage)?;
        let targets = snapshot
            .targets
            .targets()
            .iter()
            .zip(rows)
            .map(|(host, row)| DownloadStatusTarget {
                host_id: host.id,
                name: host.name.clone(),
                address: host.address.clone(),
                port: host.port,
                row,
            })
            .collect();
        let (active, recovery) = self.0.downloads.activity(id).map_err(storage)?;
        let document = DownloadStatusDocument {
            format: "cshell.batch-download-status".into(),
            version: 1,
            id,
            definition: snapshot.definition,
            created_at_ms: snapshot.created_at_ms,
            host_revision: snapshot.targets.host_revision(),
            profile_revision: snapshot.targets.profile_revision(),
            active,
            targets,
            recovery,
        };
        document.validate().map_err(|_| TaskStatus::Corrupt)?;
        let bytes = serde_json::to_vec(&document).map_err(|_| TaskStatus::Corrupt)?;
        if bytes.len() > MAX_TASK_SNAPSHOT_BYTES {
            return Err(TaskStatus::Limit);
        }
        Ok(reply!(run_id: id.as_uuid().as_bytes().to_vec(), document_json: bytes))
    }
    pub(super) async fn dispatch_download(
        &self,
        request: &TaskRequest,
        operation: TaskOperation,
    ) -> Result<TaskResponse, TaskStatus> {
        let mut canonical = TaskRequest::default();
        canonical.operation = request.operation;
        match operation {
            TaskOperation::DownloadCreate => {
                canonical
                    .definition_json
                    .clone_from(&request.definition_json);
                canonical.selection_json.clone_from(&request.selection_json);
                canonical.host_revision = request.host_revision;
                canonical.profile_revision = request.profile_revision;
            }
            TaskOperation::DownloadList => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.limit = request.limit;
            }
            TaskOperation::DownloadGet
            | TaskOperation::DownloadStart
            | TaskOperation::DownloadResume
            | TaskOperation::DownloadProbe
            | TaskOperation::DownloadCleanup
            | TaskOperation::DownloadAdopt => {
                canonical.run_id.clone_from(&request.run_id);
                if operation != TaskOperation::DownloadGet
                    && operation != TaskOperation::DownloadStart
                {
                    canonical.host_id.clone_from(&request.host_id);
                    canonical.expected_revision = request.expected_revision;
                }
                if operation != TaskOperation::DownloadGet
                    && operation != TaskOperation::DownloadProbe
                {
                    canonical.confirmed = request.confirmed;
                }
            }
            _ => return Err(TaskStatus::Invalid),
        }
        if canonical != *request {
            return Err(TaskStatus::Invalid);
        }
        if operation == TaskOperation::DownloadCreate {
            if request.definition_json.len() > 1024 * 1024 {
                return Err(TaskStatus::Limit);
            }
            let definition: BatchDownloadDefinition =
                serde_json::from_slice(&request.definition_json)
                    .map_err(|_| TaskStatus::Invalid)?;
            definition.validate().map_err(|_| TaskStatus::Invalid)?;
            let selection =
                decode_host_selection(&request.selection_json).map_err(|_| TaskStatus::Invalid)?;
            let _catalog = self.0.profiles.catalog_mutation.lock().await;
            let hosts = self
                .0
                .profiles
                .service
                .repository()
                .load_hosts()
                .await
                .map_err(|_| TaskStatus::Unavailable)?
                .unwrap_or(HostCatalogSnapshot {
                    revision: 0,
                    catalog: HostCatalog::default(),
                });
            let profiles = self
                .0
                .profiles
                .service
                .load()
                .await
                .map_err(|_| TaskStatus::Unavailable)?
                .snapshot();
            if hosts.revision != request.host_revision
                || profiles.revision != request.profile_revision
            {
                return Err(TaskStatus::Conflict);
            }
            let targets = freeze_hosts(&hosts, &profiles, selection, MAX_HOST_TARGETS)
                .map_err(|_| TaskStatus::Invalid)?;
            let snapshot = BatchDownloadSnapshot::new(
                definition,
                targets,
                self.0.clock.now_ms().map_err(|_| TaskStatus::Unavailable)?,
            )
            .map_err(|_| TaskStatus::Invalid)?;
            let id = self.0.downloads.create(snapshot).await.map_err(storage)?;
            return self.download_view(id).await;
        }
        if operation == TaskOperation::DownloadList {
            if request.limit == 0 || request.limit > 64 {
                return Err(TaskStatus::Invalid);
            }
            let after = if request.run_id.is_empty() {
                None
            } else {
                Some(TaskRunId::from_bytes(
                    decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?,
                ))
            };
            let rows = self
                .0
                .downloads
                .list(after, request.limit as usize)
                .await
                .map_err(storage)?;
            return Ok(
                reply!(document_json: serde_json::to_vec(&rows).map_err(|_| TaskStatus::Corrupt)?),
            );
        }
        let id =
            TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
        if operation == TaskOperation::DownloadGet {
            return self.download_view(id).await;
        }
        if operation == TaskOperation::DownloadStart {
            self.0
                .downloads
                .launch(id, request.confirmed, None)
                .await
                .map_err(storage)?;
        } else {
            let host =
                HostId::from_bytes(decode_id(&request.host_id).map_err(|_| TaskStatus::Invalid)?);
            if operation == TaskOperation::DownloadResume {
                self.0
                    .downloads
                    .launch(
                        id,
                        request.confirmed,
                        Some((host, request.expected_revision)),
                    )
                    .await
                    .map_err(storage)?;
            } else {
                let operation = match operation {
                    TaskOperation::DownloadProbe => DownloadRecoveryOperation::Probe,
                    TaskOperation::DownloadCleanup => DownloadRecoveryOperation::Cleanup,
                    TaskOperation::DownloadAdopt => DownloadRecoveryOperation::Adopt,
                    _ => return Err(TaskStatus::Invalid),
                };
                self.0
                    .downloads
                    .recover_target(
                        id,
                        host,
                        request.expected_revision,
                        request.confirmed,
                        operation,
                    )
                    .await
                    .map_err(storage)?;
            }
        }
        Ok(reply!(run_id: request.run_id.clone()))
    }
}
