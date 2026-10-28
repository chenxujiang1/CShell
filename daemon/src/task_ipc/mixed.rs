use super::*;
impl TaskIpcService {
    pub(super) async fn cancel_mixed(&self, request: &TaskRequest) -> TaskResponse {
        let mut canonical = TaskRequest::default();
        canonical.operation = TaskOperation::MixedCancel as i32;
        canonical.run_id.clone_from(&request.run_id);
        if canonical != *request {
            return response(TaskStatus::Invalid);
        }
        let Ok(bytes) = decode_id(&request.run_id) else {
            return response(TaskStatus::Invalid);
        };
        match self.0.mixed.cancel(TaskRunId::from_bytes(bytes)).await {
            Ok(()) => reply!(run_id: request.run_id.clone()),
            Err(error) => response(storage(error)),
        }
    }
    pub(super) async fn dispatch_mixed(
        &self,
        request: &TaskRequest,
        operation: TaskOperation,
    ) -> Result<TaskResponse, TaskStatus> {
        if matches!(
            operation,
            TaskOperation::MixedRolloutGet | TaskOperation::MixedRolloutApprove
        ) {
            let mut canonical = TaskRequest::default();
            canonical.operation = request.operation;
            canonical.run_id.clone_from(&request.run_id);
            if operation == TaskOperation::MixedRolloutApprove {
                canonical.expected_revision = request.expected_revision;
                canonical.gate_id.clone_from(&request.gate_id);
                canonical.confirmed = request.confirmed;
            }
            if canonical != *request {
                return Err(TaskStatus::Invalid);
            }
            let id =
                TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
            let document = if operation == TaskOperation::MixedRolloutApprove {
                if !request.confirmed || request.expected_revision == 0 {
                    return Err(TaskStatus::Invalid);
                }
                let gate = TaskGateId::from_bytes(
                    decode_id(&request.gate_id).map_err(|_| TaskStatus::Invalid)?,
                );
                self.0
                    .mixed
                    .approve_rollout(id, request.expected_revision, gate)
                    .await
                    .map_err(storage)?
            } else {
                self.0.mixed.rollout_document(id).await.map_err(storage)?
            };
            return Ok(
                reply!(run_id:request.run_id.clone(),document_json:serde_json::to_vec(&document).map_err(|_|TaskStatus::Corrupt)?),
            );
        }
        if operation == TaskOperation::MixedRetry {
            let mut canonical = TaskRequest::default();
            canonical.operation = request.operation;
            canonical.run_id.clone_from(&request.run_id);
            canonical
                .expected_targets_json
                .clone_from(&request.expected_targets_json);
            canonical.confirmed = request.confirmed;
            if canonical != *request || !request.confirmed {
                return Err(TaskStatus::Invalid);
            }
            let confirmation = decode_mixed_retry_confirmation(&request.expected_targets_json)
                .map_err(|_| TaskStatus::Invalid)?;
            if request.run_id.as_slice() != confirmation.run_id.as_uuid().as_bytes().as_slice() {
                return Err(TaskStatus::Invalid);
            }
            let snapshot = self
                .0
                .mixed
                .retry(&confirmation, self.0.clock.as_ref())
                .await
                .map_err(|error| match error {
                    crate::MixedRetryError::Storage(error) => storage(error),
                    crate::MixedRetryError::Plan(TaskError::Limit) => TaskStatus::Limit,
                    crate::MixedRetryError::Plan(TaskError::Transition) => TaskStatus::Conflict,
                    crate::MixedRetryError::Plan(TaskError::Clock) => TaskStatus::Unavailable,
                    crate::MixedRetryError::Plan(_) => TaskStatus::Invalid,
                })?;
            return Ok(
                reply!(run_id:snapshot.id().as_uuid().as_bytes().to_vec(),document_json:encode_mixed_snapshot(&snapshot).map_err(|_|TaskStatus::Limit)?),
            );
        }
        if operation.is_mixed_repair() {
            return self.dispatch_mixed_repair(request, operation).await;
        }
        if operation.is_mixed_results() {
            return self.dispatch_mixed_results(request, operation).await;
        }
        let mut canonical = TaskRequest::default();
        canonical.operation = request.operation;
        match operation {
            TaskOperation::MixedCreate => {
                canonical
                    .definition_json
                    .clone_from(&request.definition_json);
                canonical.selection_json.clone_from(&request.selection_json);
                canonical.host_revision = request.host_revision;
                canonical.profile_revision = request.profile_revision;
            }
            TaskOperation::MixedGet => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.host_id.clone_from(&request.host_id);
                canonical.limit = request.limit;
            }
            TaskOperation::MixedList => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.limit = request.limit;
            }
            TaskOperation::MixedReview => canonical.run_id.clone_from(&request.run_id),
            TaskOperation::MixedStart => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.confirmed = request.confirmed;
                canonical
                    .expected_targets_json
                    .clone_from(&request.expected_targets_json);
            }
            TaskOperation::MixedOutput => {
                canonical.run_id.clone_from(&request.run_id);
                canonical.host_id.clone_from(&request.host_id);
                canonical.step = request.step;
                canonical.stderr = request.stderr;
                canonical.offset = request.offset;
                canonical.limit = request.limit;
                canonical.expected_revision = request.expected_revision;
            }
            _ => return Err(TaskStatus::Invalid),
        }
        if canonical != *request {
            return Err(TaskStatus::Invalid);
        }
        if operation == TaskOperation::MixedCreate {
            if request.definition_json.len() > 1024 * 1024 {
                return Err(TaskStatus::Limit);
            }
            let definition: MixedTaskDefinition = serde_json::from_slice(&request.definition_json)
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
            let snapshot = MixedTaskSnapshot::new(
                definition,
                targets,
                "local-user".into(),
                self.0.clock.now_ms().map_err(|_| TaskStatus::Unavailable)?,
            )
            .map_err(|_| TaskStatus::Invalid)?;
            let document = encode_mixed_snapshot(&snapshot).map_err(|_| TaskStatus::Limit)?;
            let id = self.0.mixed.create(snapshot).await.map_err(storage)?;
            return Ok(reply!(run_id: id.as_uuid().as_bytes().to_vec(), document_json: document));
        }
        if operation == TaskOperation::MixedList {
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
                .mixed
                .list(after, request.limit as usize)
                .await
                .map_err(storage)?;
            return Ok(
                reply!(document_json: serde_json::to_vec(&rows).map_err(|_| TaskStatus::Corrupt)?),
            );
        }
        let id =
            TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
        match operation {
            TaskOperation::MixedGet => {
                if request.limit == 0 || request.limit as usize > MAX_MIXED_STATUS_TARGETS {
                    return Err(TaskStatus::Invalid);
                }
                let after = if request.host_id.is_empty() {
                    None
                } else {
                    Some(HostId::from_bytes(
                        decode_id(&request.host_id).map_err(|_| TaskStatus::Invalid)?,
                    ))
                };
                let page = self
                    .0
                    .mixed
                    .page(id, after, request.limit as usize)
                    .await
                    .map_err(storage)?;
                let bytes = serde_json::to_vec(&page).map_err(|_| TaskStatus::Corrupt)?;
                if bytes.len() > MAX_TASK_SNAPSHOT_BYTES {
                    return Err(TaskStatus::Limit);
                }
                Ok(reply!(run_id: request.run_id.clone(), document_json: bytes))
            }
            TaskOperation::MixedReview => {
                let (snapshot, _) = self.0.mixed.get(id).await.map_err(storage)?;
                Ok(
                    reply!(run_id: request.run_id.clone(), document_json: encode_mixed_snapshot(&snapshot).map_err(|_| TaskStatus::Limit)?),
                )
            }
            TaskOperation::MixedStart => {
                let confirmation = decode_mixed_confirmation(&request.expected_targets_json)
                    .map_err(|_| TaskStatus::Invalid)?;
                if confirmation.run_id != id || !request.confirmed {
                    return Err(TaskStatus::Invalid);
                }
                self.0
                    .mixed
                    .start_confirmed(&confirmation, true)
                    .await
                    .map_err(storage)?;
                Ok(reply!(run_id: request.run_id.clone()))
            }
            TaskOperation::MixedOutput => {
                if request.limit == 0
                    || request.limit > 64 * 1024
                    || request.offset > crate::MAX_TASK_STEP_OUTPUT
                {
                    return Err(TaskStatus::Invalid);
                }
                let host = HostId::from_bytes(
                    decode_id(&request.host_id).map_err(|_| TaskStatus::Invalid)?,
                );
                let (snapshot, row) = self.0.mixed.target(id, host).await.map_err(storage)?;
                if request.expected_revision != 0 && request.expected_revision != row.revision {
                    return Err(TaskStatus::Conflict);
                }
                let step = request.step as usize;
                let saved = row.steps.get(step).ok_or(TaskStatus::Invalid)?;
                let MixedReceipt::Command { target, .. } = &saved.receipt else {
                    return Err(TaskStatus::Invalid);
                };
                if saved.state == MixedTaskState::Pending
                    || target.steps[0].state == cshell_domain::TargetState::Pending
                {
                    return Err(TaskStatus::Conflict);
                }
                let MixedChildSnapshot::Command(child) = snapshot
                    .child(host, step)
                    .map_err(|_| TaskStatus::Corrupt)?
                else {
                    return Err(TaskStatus::Corrupt);
                };
                let (bytes, length) = self
                    .0
                    .output
                    .read(
                        child.id(),
                        host,
                        0,
                        request.stderr,
                        request.offset,
                        request.limit as usize,
                    )
                    .await
                    .map_err(|error| match error {
                        crate::TaskOutputError::Invalid => TaskStatus::Invalid,
                        crate::TaskOutputError::Limit => TaskStatus::Limit,
                        crate::TaskOutputError::Unavailable => TaskStatus::Unavailable,
                    })?;
                if request.offset > length {
                    return Err(TaskStatus::Invalid);
                }
                if request.expected_revision != 0 {
                    let (_, current) = self.0.mixed.target(id, host).await.map_err(storage)?;
                    if current.revision != request.expected_revision {
                        return Err(TaskStatus::Conflict);
                    }
                }
                let context = MixedOutputContext {
                    format: "cshell.mixed-output".into(),
                    version: 1,
                    run_id: id,
                    host_id: host,
                    child_id: child.id(),
                    step: request.step,
                    target_revision: row.revision,
                    step_state: saved.state,
                    stderr: request.stderr,
                    offset: request.offset,
                    next_offset: request.offset + bytes.len() as u64,
                    length,
                };
                let document = serde_json::to_vec(&context).map_err(|_| TaskStatus::Corrupt)?;
                decode_mixed_output(&document, &bytes).map_err(|_| TaskStatus::Corrupt)?;
                Ok(
                    reply!(run_id: request.run_id.clone(), revision: row.revision, document_json: document, output: bytes, output_length: length),
                )
            }
            _ => Err(TaskStatus::Invalid),
        }
    }
}
impl TaskIpcService {
    async fn dispatch_mixed_repair(
        &self,
        request: &TaskRequest,
        operation: TaskOperation,
    ) -> Result<TaskResponse, TaskStatus> {
        let mut canonical = TaskRequest::default();
        canonical.operation = request.operation;
        canonical.run_id.clone_from(&request.run_id);
        if operation != TaskOperation::MixedFileStatus {
            canonical.host_id.clone_from(&request.host_id);
            canonical.step = request.step;
            canonical.expected_revision = request.expected_revision;
            if operation != TaskOperation::MixedFileProbe {
                canonical.confirmed = request.confirmed;
            }
        }
        if canonical != *request {
            return Err(TaskStatus::Invalid);
        }
        let id =
            TaskRunId::from_bytes(decode_id(&request.run_id).map_err(|_| TaskStatus::Invalid)?);
        if operation == TaskOperation::MixedFileStatus {
            let document = self.0.mixed.repair_document(id).await.map_err(storage)?;
            return Ok(
                reply!(run_id:request.run_id.clone(),document_json:serde_json::to_vec(&document).map_err(|_|TaskStatus::Corrupt)?),
            );
        }
        if request.expected_revision == 0 || request.step as usize >= MAX_TASK_STEPS {
            return Err(TaskStatus::Invalid);
        }
        let host =
            HostId::from_bytes(decode_id(&request.host_id).map_err(|_| TaskStatus::Invalid)?);
        let action = match operation {
            TaskOperation::MixedFileProbe => MixedFileOperation::Probe,
            TaskOperation::MixedFileResume => MixedFileOperation::Resume,
            TaskOperation::MixedFileCleanup => MixedFileOperation::Cleanup,
            TaskOperation::MixedFileAdopt => MixedFileOperation::Adopt,
            _ => return Err(TaskStatus::Invalid),
        };
        self.0
            .mixed
            .repair(
                id,
                host,
                request.step as usize,
                request.expected_revision,
                action,
                request.confirmed,
            )
            .await
            .map_err(storage)?;
        Ok(reply!(run_id:request.run_id.clone()))
    }
}
