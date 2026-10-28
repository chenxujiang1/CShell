use cshell_application::{MAX_SSH_PROVIDER_REQUEST, decode_ssh_provider_report};
use cshell_domain::SshProviderRequirements;
use cshell_ipc::{
    DiscoveryRecord, Envelope, Handshake, RuntimePaths, SshProviderRequest, SshProviderStatus,
    client_handshake, envelope, features, read_envelope, write_envelope,
};
use std::{io::Read, time::Duration};

pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    let input: Option<SshProviderRequirements> = match args {
        [op] if op == "get" => None,
        [op, path] if op == "check" => {
            let file =
                std::fs::File::open(path).map_err(|_| "cannot open provider requirements")?;
            if !file
                .metadata()
                .map_err(|_| "cannot inspect requirements file")?
                .is_file()
            {
                return Err("requirements must be a regular file");
            }
            let mut bytes = Vec::new();
            file.take((MAX_SSH_PROVIDER_REQUEST + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| "cannot read provider requirements")?;
            if bytes.len() > MAX_SSH_PROVIDER_REQUEST {
                return Err("provider requirements limit");
            }
            let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
            Some(serde_json::from_slice(bytes).map_err(|_| "invalid provider requirements")?)
        }
        _ => return Err(crate::USAGE),
    };
    let requirements_json = input
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|_| "cannot encode requirements")?
        .unwrap_or_default();
    if requirements_json.len() > MAX_SSH_PROVIDER_REQUEST {
        return Err("provider requirements limit");
    }
    let paths = RuntimePaths::for_current_user().map_err(|_| "daemon discovery unavailable")?;
    let discovery =
        DiscoveryRecord::load(&paths).map_err(|_| "authenticated daemon discovery missing")?;
    let report = tokio::time::timeout(Duration::from_secs(20), async {
        #[cfg(windows)]
        let mut stream = cshell_ipc::transport::connect(&discovery.endpoint.to_string_lossy())
            .await
            .map_err(|_| "daemon connection failed")?;
        #[cfg(unix)]
        let mut stream = cshell_ipc::transport::connect(std::path::Path::new(&discovery.endpoint))
            .await
            .map_err(|_| "daemon connection failed")?;
        let mut hello = Handshake::new(
            discovery.daemon_instance_id.to_vec(),
            discovery.instance_token.to_vec(),
        );
        hello.feature_bits = features::SSH_PROVIDER_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon authentication failed")?;
        if negotiated.feature_bits & features::SSH_PROVIDER_CONTROL == 0 {
            return Err("daemon lacks SSH provider discovery");
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::SshProviderRequest(SshProviderRequest {
                    requirements_json,
                })),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "provider query failed")?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "provider reply failed")?;
        if response.request_id != 2 {
            return Err("provider response identity mismatch");
        }
        let Some(envelope::Payload::SshProviderResponse(response)) = response.payload else {
            return Err("unexpected provider response");
        };
        if response.detail.len() > 1024 {
            return Err("provider detail limit");
        }
        if response.status != SshProviderStatus::Ok as i32 {
            return Err("provider discovery refused or unavailable");
        }
        decode_ssh_provider_report(&response.document_json, input.as_ref())
    })
    .await
    .map_err(|_| "provider query timed out")??;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).map_err(|_| "provider reply encoding failed")?
    );
    // A valid negative evaluation is reported with its reasons and a failure exit.
    if report.evaluation.is_some_and(|e| e.selected.is_none()) {
        return Err("no SSH provider satisfies these requirements");
    }
    Ok(())
}
