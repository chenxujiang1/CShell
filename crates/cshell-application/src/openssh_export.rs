//! Standalone OpenSSH configuration from saved, public target metadata only.
use crate::{CatalogSnapshot, MAX_PROFILE_IMPORT_BYTES, ProfileCatalog};
use cshell_domain::{ProfileId, SshAgentBackend, SshAuthMethod, SshAuthStep, SshRoute};
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::Path;
use thiserror::Error;

pub const MAX_OPENSSH_EXPORT_HOSTS: usize = 512;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum OpenSshExportError {
    #[error("OpenSSH export requires a valid saved catalog")]
    Catalog,
    #[error("select saved SSH targets before exporting")]
    Selection,
    #[error("OpenSSH export exceeds its host or file limit")]
    Limit,
    #[error("SOCKS5/HTTP proxy routes cannot be exported as ProxyJump")]
    Route,
    #[error(
        "select an OpenSSH agent without fingerprint filtering; Pageant and Windows Auto cannot be exported"
    )]
    Agent,
    #[error("OpenSSH export requires absolute key references without environment expansion")]
    KeyPath,
    #[error("this authentication combination cannot be represented exactly in OpenSSH; use CShell profile export")]
    AuthenticationOrder,
    #[error("a target contains characters that cannot be safely represented in OpenSSH config")]
    Value,
}

pub fn openssh_export_alias(id: ProfileId) -> String {
    format!("cshell-{}", id.as_uuid().simple())
}

/// Empty selection means all configured SSH targets. Required jump targets are included.
/// The generated file is for `ssh -F <file> <alias>`, not merging into another config.
pub fn export_openssh_config(
    snapshot: &CatalogSnapshot,
    selected: &[ProfileId],
) -> Result<Vec<u8>, OpenSshExportError> {
    if selected.len() > MAX_OPENSSH_EXPORT_HOSTS {
        return Err(OpenSshExportError::Limit);
    }
    ProfileCatalog::from_snapshot(snapshot.clone()).map_err(|_| OpenSshExportError::Catalog)?;
    let mut ids: BTreeSet<_> = if selected.is_empty() {
        snapshot
            .ssh_connections
            .iter()
            .map(|target| target.profile_id)
            .collect()
    } else {
        selected.iter().copied().collect()
    };
    if ids.is_empty() || (!selected.is_empty() && ids.len() != selected.len()) {
        return Err(OpenSshExportError::Selection);
    }
    let mut pending: Vec<_> = ids.iter().copied().collect();
    while let Some(id) = pending.pop() {
        let target = snapshot
            .ssh_connections
            .iter()
            .find(|target| target.profile_id == id)
            .ok_or(OpenSshExportError::Selection)?;
        match target.route {
            SshRoute::Jump { profile_id } => {
                if ids.insert(profile_id) {
                    pending.push(profile_id);
                }
            }
            SshRoute::Direct => {}
            _ => return Err(OpenSshExportError::Route),
        }
        if ids.len() > MAX_OPENSSH_EXPORT_HOSTS {
            return Err(OpenSshExportError::Limit);
        }
    }
    let mut output = String::from(
        "# CShell OpenSSH export v1\n# Use as a standalone file: ssh -F <this-file> <Host-alias>\n# Credentials and known_hosts trust are not included. Key paths must exist locally.\n\n",
    );
    for id in ids {
        let target = snapshot
            .ssh_connections
            .iter()
            .find(|target| target.profile_id == id)
            .ok_or(OpenSshExportError::Selection)?;
        let profile = snapshot
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .ok_or(OpenSshExportError::Catalog)?;
        if target.username.is_empty() || !target
            .host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_:%".contains(c))
            || !target
                .username
                .chars()
                .all(|c| c.is_alphanumeric() || ".-_@\\".contains(c))
        {
            return Err(OpenSshExportError::Value);
        }
        writeln!(
            output,
            "# Profile: {}",
            serde_json::to_string(&profile.name).map_err(|_| OpenSshExportError::Value)?
        )
        .map_err(|_| OpenSshExportError::Limit)?;
        writeln!(
            output,
            "Host {}\n    HostName {}\n    User {}\n    Port {}",
            openssh_export_alias(id),
            quote(&target.host),
            quote(&target.username),
            target.port
        )
        .map_err(|_| OpenSshExportError::Limit)?;
        output.push_str("    StrictHostKeyChecking yes\n    UpdateHostKeys no\n    CheckHostIP no\n    AddKeysToAgent no\n");
        let steps = target.authentication_steps();
        let primary = match target.auth_method {
            SshAuthMethod::Password => SshAuthStep::Password,
            SshAuthMethod::PrivateKey => SshAuthStep::PrivateKey,
            SshAuthMethod::Certificate => SshAuthStep::Certificate,
            SshAuthMethod::Agent => SshAuthStep::Agent,
        };
        if (target.auth_method == SshAuthMethod::Password && steps.iter().any(|s| !matches!(s, SshAuthStep::Password | SshAuthStep::KeyboardInteractive)))
            || (target.auth_method != SshAuthMethod::Password && steps != [primary])
        { return Err(OpenSshExportError::AuthenticationOrder); }
        match target.auth_method {
            SshAuthMethod::Password => {
                let order = steps.iter().map(|step| if *step == SshAuthStep::KeyboardInteractive { "keyboard-interactive" } else { "password" }).collect::<Vec<_>>().join(",");
                writeln!(output, "    PreferredAuthentications {order}").map_err(|_| OpenSshExportError::Limit)?;
                output.push_str("    IdentityAgent none\n    IdentityFile none\n    CertificateFile none\n");
            },
            SshAuthMethod::PrivateKey | SshAuthMethod::Certificate => {
                output.push_str("    PreferredAuthentications publickey\n    IdentitiesOnly yes\n    IdentityAgent none\n");
                let key = target.private_key_path.as_deref().ok_or(OpenSshExportError::KeyPath)?;
                writeln!(output, "    IdentityFile {}", key_reference(key)?).map_err(|_| OpenSshExportError::Limit)?;
                if target.auth_method == SshAuthMethod::Certificate {
                    let certificate = target.certificate_path.as_deref().ok_or(OpenSshExportError::KeyPath)?;
                    writeln!(output, "    CertificateFile {}\n    PubkeyAcceptedKeyTypes *-cert-v01@openssh.com", key_reference(certificate)?).map_err(|_| OpenSshExportError::Limit)?;
                } else {
                    // Exclude certificate algorithms, including implicit <key>-cert.pub discovery.
                    output.push_str("    CertificateFile none\n    PubkeyAcceptedKeyTypes -*cert*\n");
                }
            },
            SshAuthMethod::Agent => {
                if target.agent_backend == SshAgentBackend::Pageant
                    || (cfg!(windows) && target.agent_backend == SshAgentBackend::Auto)
                    || target.agent_identity.is_some()
                {
                    return Err(OpenSshExportError::Agent);
                }
                output.push_str("    PreferredAuthentications publickey\n    IdentitiesOnly no\n    IdentityFile none\n    CertificateFile none\n");
            },
        }
        match target.route {
            SshRoute::Jump { profile_id } => {
                writeln!(output, "    ProxyJump {}", openssh_export_alias(profile_id))
                    .map_err(|_| OpenSshExportError::Limit)?
            }
            SshRoute::Direct => output.push_str("    ProxyJump none\n"),
            _ => return Err(OpenSshExportError::Route),
        }
        output.push('\n');
        if output.len() > MAX_PROFILE_IMPORT_BYTES {
            return Err(OpenSshExportError::Limit);
        }
    }
    Ok(output.into_bytes())
}
fn key_reference(value: &str) -> Result<String, OpenSshExportError> {
    if !Path::new(value).is_absolute() || value.contains("${") {
        return Err(OpenSshExportError::KeyPath);
    }
    Ok(quote(value))
}
fn quote(value: &str) -> String {
    let mut output = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' | '"' => {
                output.push('\\');
                output.push(c);
            }
            '%' => output.push_str("%%"),
            _ => output.push(c),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests;
