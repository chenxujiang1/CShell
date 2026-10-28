use cshell_application::{MAX_SSH_PROVIDER_DOCUMENT, MAX_SSH_PROVIDER_REQUEST, SshProviderReport};
use cshell_domain::SshProviderRequirements;
use cshell_ipc::{SshProviderRequest, SshProviderResponse, SshProviderStatus};

pub(crate) fn handle(request: &SshProviderRequest) -> SshProviderResponse {
    let input: Option<SshProviderRequirements> = if request.requirements_json.is_empty() {
        None
    } else if request.requirements_json.len() > MAX_SSH_PROVIDER_REQUEST {
        return invalid("SSH provider requirements exceed the limit");
    } else {
        match serde_json::from_slice(&request.requirements_json) {
            Ok(input) => Some(input),
            Err(_) => return invalid("Invalid SSH provider requirements"),
        }
    };
    let catalog = cshell_ssh::registered_ssh_providers();
    let evaluation = input
        .as_ref()
        .map(|requirements| catalog.evaluate(requirements));
    let report = SshProviderReport {
        catalog,
        evaluation,
    };
    match report
        .confirm(input.as_ref())
        .and_then(|_| serde_json::to_vec(&report).map_err(|_| "SSH provider encoding failed"))
    {
        Ok(document_json) if document_json.len() <= MAX_SSH_PROVIDER_DOCUMENT => {
            SshProviderResponse {
                status: SshProviderStatus::Ok as i32,
                document_json,
                detail: String::new(),
            }
        }
        _ => SshProviderResponse {
            status: SshProviderStatus::Unavailable as i32,
            detail: "SSH provider catalog unavailable".into(),
            ..Default::default()
        },
    }
}
fn invalid(detail: &str) -> SshProviderResponse {
    SshProviderResponse {
        status: SshProviderStatus::Invalid as i32,
        detail: detail.into(),
        ..Default::default()
    }
}
