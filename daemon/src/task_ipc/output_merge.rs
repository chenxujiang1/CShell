use super::*;
impl TaskIpcService {
    pub(super) async fn dispatch_output_merge(
        &self,
        request: &TaskRequest,
    ) -> Result<TaskResponse, TaskStatus> {
        let mut canonical = TaskRequest::default();
        canonical.operation = TaskOperation::OutputMerge as i32;
        canonical.run_id.clone_from(&request.run_id);
        canonical
            .definition_json
            .clone_from(&request.definition_json);
        if canonical != *request {
            return Err(TaskStatus::Invalid);
        }
        let query = decode_output_merge_request(&request.definition_json)
            .map_err(|_| TaskStatus::Invalid)?;
        if request.run_id.as_slice() != query.scope.run_id.as_uuid().as_bytes() {
            return Err(TaskStatus::Invalid);
        }
        let (binding, right) = self.bind_output_analysis(&query.scope).await?;
        if right.is_some() {
            return Err(TaskStatus::Invalid);
        }
        let slot = self
            .0
            .analysis
            .slots()
            .try_acquire_owned()
            .map_err(|_| TaskStatus::Busy)?;
        let output = self.0.output.clone();
        let (page, mut body) = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            output.merged_page(query, binding)
        })
        .await
        .map_err(|_| TaskStatus::Unavailable)?
        .map_err(|error| match error {
            OutputAnalysisError::Missing => TaskStatus::Missing,
            OutputAnalysisError::Changed => TaskStatus::Conflict,
            OutputAnalysisError::Invalid => TaskStatus::Corrupt,
            _ => TaskStatus::Unavailable,
        })?;
        Ok(
            reply!(run_id:request.run_id.clone(),document_json:serde_json::to_vec(&page).map_err(|_|TaskStatus::Corrupt)?,output_length:body.0.len() as u64,output:std::mem::take(&mut body.0)),
        )
    }
}
