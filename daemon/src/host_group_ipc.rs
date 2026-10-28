use crate::ProfileIpcService;
use cshell_application::*;
use cshell_domain::HostCatalog;
use cshell_ipc::{HostGroupOperation, HostGroupRequest, HostGroupResponse, HostGroupStatus};

pub(crate) fn response(status: HostGroupStatus, detail: &str) -> HostGroupResponse {
    HostGroupResponse {
        status: status as i32,
        revision: 0,
        profile_revision: 0,
        document_json: Vec::new(),
        detail: detail.into(),
    }
}
fn invalid(error: HostCatalogError) -> HostGroupResponse {
    let status = match error {
        HostCatalogError::Limit | HostCatalogError::TooManyTargets => HostGroupStatus::Limit,
        HostCatalogError::MissingProfile | HostCatalogError::InvalidProfile => {
            HostGroupStatus::MissingProfile
        }
        _ => HostGroupStatus::Invalid,
    };
    response(status, &error.to_string())
}
fn storage(error: HostRepositoryError) -> HostGroupResponse {
    response(
        match error {
            HostRepositoryError::Conflict { .. } => HostGroupStatus::Conflict,
            HostRepositoryError::Corrupt => HostGroupStatus::Corrupt,
            HostRepositoryError::Unavailable => HostGroupStatus::Unavailable,
        },
        &error.to_string(),
    )
}
impl ProfileIpcService {
    pub(crate) async fn handle_host_groups(&self, request: HostGroupRequest) -> HostGroupResponse {
        let Ok(operation) = HostGroupOperation::try_from(request.operation) else {
            return response(HostGroupStatus::Invalid, "unknown host operation");
        };
        match operation {
            HostGroupOperation::Load
                if request.expected_revision != 0
                    || request.expected_profile_revision != 0
                    || !request.catalog_json.is_empty()
                    || !request.selection_json.is_empty()
                    || request.target_limit != 0 =>
            {
                return response(HostGroupStatus::Invalid, "mixed host load request");
            }
            HostGroupOperation::Save
                if !request.selection_json.is_empty() || request.target_limit != 0 =>
            {
                return response(HostGroupStatus::Invalid, "mixed host save request");
            }
            HostGroupOperation::Preview | HostGroupOperation::Freeze
                if !request.catalog_json.is_empty()
                    || request.selection_json.is_empty()
                    || request.target_limit == 0
                    || request.target_limit as usize > MAX_HOST_TARGETS =>
            {
                return response(HostGroupStatus::Invalid, "invalid host selection request");
            }
            _ => {}
        }
        let _guard = self.catalog_mutation.lock().await;
        let stored = match self.service.repository().load_hosts().await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => HostCatalogSnapshot {
                revision: 0,
                catalog: HostCatalog::default(),
            },
            Err(error) => return storage(error),
        };
        let profiles = match self.service.load().await {
            Ok(catalog) => catalog.snapshot(),
            Err(_) => return response(HostGroupStatus::Unavailable, "Profile storage unavailable"),
        };
        if operation != HostGroupOperation::Load
            && (stored.revision != request.expected_revision
                || profiles.revision != request.expected_profile_revision)
        {
            return response(
                HostGroupStatus::Conflict,
                "host or Profile catalog changed; reload and preview again",
            );
        }
        let revision = stored.revision;
        let profile_revision = profiles.revision;
        let result = match operation {
            HostGroupOperation::Load => encode_host_management(&HostManagementSnapshot {
                revision,
                profile_revision,
                catalog: stored.catalog,
                profiles: host_profile_hints(&profiles),
            })
            .map(|document_json| (revision, document_json)),
            HostGroupOperation::Save => {
                let catalog = match decode_hosts(&request.catalog_json) {
                    Ok(catalog) => catalog,
                    Err(error) => return invalid(error),
                };
                if let Err(error) = validate_host_profiles(&catalog, &profiles) {
                    return invalid(error);
                }
                let Some(next) = revision.checked_add(1) else {
                    return response(HostGroupStatus::Unavailable, "host revision exhausted");
                };
                let document = match encode_host_management(&HostManagementSnapshot {
                    revision: next,
                    profile_revision,
                    catalog: catalog.clone(),
                    profiles: host_profile_hints(&profiles),
                }) {
                    Ok(document) => document,
                    Err(error) => return invalid(error),
                };
                match self
                    .service
                    .repository()
                    .save_hosts(revision, &catalog)
                    .await
                {
                    Ok(saved) => Ok((saved.revision, document)),
                    Err(error) => return storage(error),
                }
            }
            HostGroupOperation::Preview | HostGroupOperation::Freeze => {
                let selection = match decode_host_selection(&request.selection_json) {
                    Ok(selection) => selection,
                    Err(error) => return invalid(error),
                };
                if operation == HostGroupOperation::Preview {
                    select_hosts(&stored.catalog, &selection, request.target_limit as usize)
                        .and_then(|hosts| {
                            encode_host_preview(&HostSelectionPreview {
                                host_revision: revision,
                                profile_revision,
                                selection,
                                hosts,
                            })
                        })
                        .map(|document_json| (revision, document_json))
                } else {
                    freeze_hosts(&stored, &profiles, selection, request.target_limit as usize)
                        .and_then(|targets| encode_frozen_hosts(&targets))
                        .map(|document_json| (revision, document_json))
                }
            }
        };
        match result {
            Ok((revision, document_json)) => HostGroupResponse {
                status: HostGroupStatus::Ok as i32,
                revision,
                profile_revision,
                document_json,
                detail: String::new(),
            },
            Err(error) => invalid(error),
        }
    }
}
