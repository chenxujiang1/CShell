use crate::ProfileIpcService;
use cshell_application::{
    QuickCommandRepository, QuickCommandRepositoryError, decode_quick_commands,
    encode_quick_commands,
};
use cshell_ipc::{
    QuickCommandOperation, QuickCommandRequest, QuickCommandResponse, QuickCommandStatus,
};

impl ProfileIpcService {
    pub(crate) async fn handle_quick_commands(
        &self,
        request: QuickCommandRequest,
        negotiated_features: u64,
    ) -> QuickCommandResponse {
        let _mutation = self.catalog_mutation.lock().await;
        let result = match QuickCommandOperation::try_from(request.operation) {
            Ok(QuickCommandOperation::Load)
                if request.document_json.is_empty() && request.expected_revision == 0 =>
            {
                self.service.repository().load_quick_commands().await
            }
            Ok(QuickCommandOperation::Save) => {
                let Ok(catalog) = decode_quick_commands(&request.document_json) else {
                    return response(QuickCommandStatus::Invalid);
                };
                if catalog.version == 2
                    && negotiated_features & cshell_ipc::features::QUICK_TASK_TEMPLATES == 0
                {
                    return response(QuickCommandStatus::Unsupported);
                }
                // An older client must not replace a catalog it cannot understand.
                match self.service.repository().load_quick_commands().await {
                    Ok(Some(current)) if current.catalog.version > catalog.version => {
                        return response(QuickCommandStatus::Unsupported);
                    }
                    Err(QuickCommandRepositoryError::Corrupt) => {
                        return response(QuickCommandStatus::Corrupt);
                    }
                    Err(_) => return response(QuickCommandStatus::Unavailable),
                    _ => {}
                }
                self.service
                    .repository()
                    .save_quick_commands(request.expected_revision, &catalog)
                    .await
                    .map(Some)
            }
            _ => return response(QuickCommandStatus::Invalid),
        };
        match result {
            Ok(None) => response(QuickCommandStatus::Ok),
            Ok(Some(snapshot))
                if snapshot.catalog.version == 2
                    && negotiated_features & cshell_ipc::features::QUICK_TASK_TEMPLATES == 0 =>
            {
                response(QuickCommandStatus::Unsupported)
            }
            Ok(Some(snapshot)) => match encode_quick_commands(&snapshot.catalog) {
                Ok(document_json) => QuickCommandResponse {
                    status: QuickCommandStatus::Ok as i32,
                    revision: snapshot.revision,
                    document_json,
                },
                Err(_) => response(QuickCommandStatus::Corrupt),
            },
            Err(error) => response(match error {
                QuickCommandRepositoryError::Conflict { .. } => QuickCommandStatus::Conflict,
                QuickCommandRepositoryError::Unavailable => QuickCommandStatus::Unavailable,
                QuickCommandRepositoryError::Corrupt => QuickCommandStatus::Corrupt,
            }),
        }
    }
}
pub(crate) fn response(status: QuickCommandStatus) -> QuickCommandResponse {
    QuickCommandResponse {
        status: status as i32,
        revision: 0,
        document_json: Vec::new(),
    }
}
