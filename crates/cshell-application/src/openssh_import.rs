//! Pure, bounded import of the representable OpenSSH client configuration subset.
//! No filesystem access, subprocesses, environment reads or host-key trust changes.

use crate::{
    CatalogSnapshot, MAX_PROFILE_IMPORT_BYTES, PROFILE_CONNECTION_IMPORT_VERSION,
    PROFILE_IMPORT_FORMAT, ProfileCatalog, ProfileImportDocument,
};
use cshell_domain::{
    ProfileId, ProfileKind, ProfileRecord, SshAgentBackend, SshAuthMethod, SshConnectionRecord,
    SshRoute,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use thiserror::Error;

const MAX_HOSTS: usize = 512;
const MAX_LINES: usize = 4096;
const MAX_LINE_BYTES: usize = 8192;
const MAX_EVALUATION_BYTES: usize = 64 * 1024 * 1024;
const MAX_PATTERN_STEPS: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Default)]
pub struct OpenSshImportOptions<'a> {
    pub default_username: &'a str,
    pub home_directory: Option<&'a Path>,
}

impl std::fmt::Debug for OpenSshImportOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenSshImportOptions")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum OpenSshImportError {
    #[error("OpenSSH import exceeds its size, host or evaluation limit")]
    Limit,
    #[error("OpenSSH configuration must be UTF-8 without control characters")]
    Encoding,
    #[error("invalid OpenSSH syntax at line {line}")]
    Syntax { line: usize },
    #[error("unsupported OpenSSH directive at line {line}; no profiles were imported")]
    UnsupportedDirective { line: usize },
    #[error("OpenSSH configuration has no explicit Host aliases")]
    NoHosts,
    #[error("OpenSSH Host aliases must be distinct ignoring case")]
    DuplicateAlias,
    #[error("unsupported OpenSSH value at line {line}; no profiles were imported")]
    UnsupportedValue { line: usize },
    #[error("a Host has no User; provide a default account before previewing")]
    MissingUser,
    #[error("key paths must be absolute or use ~/ with an available home directory")]
    KeyPath,
    #[error(
        "ProxyJump must name an explicit Host alias in an acyclic route of at most eight jumps"
    )]
    Jump,
    #[error("converted OpenSSH targets failed validation")]
    InvalidTarget,
}

#[derive(Debug)]
struct Directive {
    line: usize,
    keyword: String,
    value: String,
    patterns: Vec<String>,
}

/// Explicit aliases become Profiles; wildcard/negated Host entries only supply settings.
pub fn import_openssh_config(
    bytes: &[u8],
    options: OpenSshImportOptions<'_>,
) -> Result<ProfileImportDocument, OpenSshImportError> {
    if bytes.len() > MAX_PROFILE_IMPORT_BYTES {
        return Err(OpenSshImportError::Limit);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| OpenSshImportError::Encoding)?;
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return Err(OpenSshImportError::Encoding);
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut directives = Vec::new();
    let mut patterns = vec!["*".to_owned()];
    let mut aliases = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, source) in text.lines().enumerate() {
        let line = index + 1;
        if line > MAX_LINES || source.len() > MAX_LINE_BYTES {
            return Err(OpenSshImportError::Limit);
        }
        let source = source.trim_start();
        if source.is_empty() || source.starts_with('#') {
            continue;
        }
        let end = source
            .find([' ', '\t', '='])
            .ok_or(OpenSshImportError::Syntax { line })?;
        let keyword = source[..end].to_ascii_lowercase();
        let mut rest = source[end..].trim_start();
        if let Some(tail) = rest.strip_prefix('=') {
            rest = tail.trim_start();
        }
        let values = split_values(rest, line)?;
        if values.is_empty() {
            return Err(OpenSshImportError::Syntax { line });
        }
        if keyword == "host" {
            if values.len() > 128 || values.iter().any(|p| !valid_pattern(p)) {
                return Err(OpenSshImportError::Syntax { line });
            }
            for alias in values
                .iter()
                .filter(|p| !p.starts_with('!') && !p.contains(['*', '?']))
            {
                if aliases.iter().any(|old| old == alias) {
                    continue;
                }
                if !seen.insert(alias.to_ascii_lowercase()) {
                    return Err(OpenSshImportError::DuplicateAlias);
                }
                aliases.push(alias.clone());
            }
            if aliases.len() > MAX_HOSTS {
                return Err(OpenSshImportError::Limit);
            }
            patterns = values;
            continue;
        }
        if !matches!(
            keyword.as_str(),
            "hostname"
                | "user"
                | "port"
                | "identityfile"
                | "certificatefile"
                | "proxyjump"
                | "stricthostkeychecking"
                | "identitiesonly"
                | "preferredauthentications"
                | "identityagent"
                | "pubkeyacceptedkeytypes"
                | "pubkeyacceptedalgorithms"
                | "updatehostkeys"
                | "checkhostip"
                | "addkeystoagent"
        ) {
            return Err(OpenSshImportError::UnsupportedDirective { line });
        }
        if values.len() != 1 || values[0].is_empty() {
            return Err(OpenSshImportError::Syntax { line });
        }
        directives.push(Directive {
            line,
            keyword,
            value: values[0].clone(),
            patterns: patterns.clone(),
        });
    }
    if aliases.is_empty() {
        return Err(OpenSshImportError::NoHosts);
    }
    if bytes.len().saturating_mul(aliases.len()) > MAX_EVALUATION_BYTES {
        return Err(OpenSshImportError::Limit);
    }
    let ids: BTreeMap<_, _> = aliases
        .iter()
        .map(|alias| (alias.clone(), ProfileId::new()))
        .collect();
    let mut document = ProfileImportDocument {
        format: PROFILE_IMPORT_FORMAT.into(),
        version: PROFILE_CONNECTION_IMPORT_VERSION,
        folders: Vec::new(),
        profiles: Vec::new(),
        ssh_connections: Vec::new(),
        local_connections: Vec::new(),
    };
    let mut pattern_steps = MAX_PATTERN_STEPS;
    for alias in &aliases {
        let mut settings = BTreeMap::new();
        let mut identities = Vec::new();
        let mut certificates = Vec::new();
        for directive in &directives {
            if !matches_host(&directive.patterns, alias, &mut pattern_steps)? {
                continue;
            }
            match directive.keyword.as_str() {
                "identityfile" => {
                    if !identities
                        .iter()
                        .any(|old: &&Directive| old.value == directive.value)
                    {
                        identities.push(directive);
                    }
                }
                "certificatefile" => {
                    if !certificates
                        .iter()
                        .any(|old: &&Directive| old.value == directive.value)
                    {
                        certificates.push(directive);
                    }
                }
                _ => {
                    settings
                        .entry(directive.keyword.as_str())
                        .or_insert(directive);
                }
            }
        }
        let value = |key| {
            settings
                .get(key)
                .map(|directive: &&Directive| directive.value.as_str())
        };
        let unsupported = |key| OpenSshImportError::UnsupportedValue {
            line: settings.get(key).map_or(0, |directive| directive.line),
        };
        if identities.len() > 1 || certificates.len() > 1 {
            return Err(OpenSshImportError::UnsupportedValue {
                line: identities
                    .get(1)
                    .or_else(|| certificates.get(1))
                    .map_or(0, |d| d.line),
            });
        }
        if value("stricthostkeychecking").is_some_and(|v| !matches!(v, "yes" | "ask")) {
            return Err(unsupported("stricthostkeychecking"));
        }
        if value("identitiesonly").is_some_and(|v| !matches!(v, "yes" | "no")) {
            return Err(unsupported("identitiesonly"));
        }
        for key in ["updatehostkeys", "checkhostip", "addkeystoagent"] {
            if value(key).is_some_and(|v| v != "no") {
                return Err(unsupported(key));
            }
        }
        if value("identityagent").is_some_and(|v| v != "none") {
            return Err(unsupported("identityagent"));
        }
        let hostname = expand_hostname(value("hostname").unwrap_or(alias), alias)
            .ok_or_else(|| unsupported("hostname"))?;
        let username = value("user").unwrap_or(options.default_username);
        if username.is_empty() {
            return Err(OpenSshImportError::MissingUser);
        }
        if username.contains(['%', '$', '~']) {
            return Err(unsupported("user"));
        }
        let port = value("port")
            .unwrap_or("22")
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| unsupported("port"))?;
        let key_path = identities
            .first()
            .filter(|d| d.value != "none")
            .map(|d| {
                expand_key_path(
                    &d.value,
                    alias,
                    &hostname,
                    username,
                    port,
                    options.home_directory,
                )
            })
            .transpose()?;
        let certificate_path = certificates
            .first()
            .filter(|d| d.value != "none")
            .map(|d| {
                expand_key_path(
                    &d.value,
                    alias,
                    &hostname,
                    username,
                    port,
                    options.home_directory,
                )
            })
            .transpose()?;
        let preferred = value("preferredauthentications");
        let keyboard_interactive=matches!(preferred,Some("keyboard-interactive"|"keyboard-interactive,password"|"password,keyboard-interactive"));
        let password_auth=preferred==Some("password") || keyboard_interactive;
        if preferred.is_some_and(|v| !matches!(v, "publickey" | "password" | "keyboard-interactive" | "keyboard-interactive,password" | "password,keyboard-interactive")) {
            return Err(unsupported("preferredauthentications"));
        }
        if password_auth && (key_path.is_some() || certificate_path.is_some()) {
            return Err(unsupported("preferredauthentications"));
        }
        let auth_method = if password_auth {
            SshAuthMethod::Password
        } else if certificate_path.is_some() {
            if key_path.is_none() {
                return Err(OpenSshImportError::KeyPath);
            }
            SshAuthMethod::Certificate
        } else if key_path.is_some() {
            SshAuthMethod::PrivateKey
        } else {
            SshAuthMethod::Agent
        };
        if value("identitiesonly") == Some("no") && auth_method != SshAuthMethod::Agent {
            return Err(unsupported("identitiesonly"));
        }
        if value("identityagent") == Some("none") && auth_method == SshAuthMethod::Agent {
            return Err(unsupported("identityagent"));
        }
        for key in ["pubkeyacceptedkeytypes", "pubkeyacceptedalgorithms"] {
            if let Some(pattern) = value(key) {
                let valid = match auth_method {
                    SshAuthMethod::PrivateKey => pattern == "-*cert*",
                    SshAuthMethod::Certificate => pattern == "*-cert-v01@openssh.com",
                    _ => false,
                };
                if !valid {
                    return Err(unsupported(key));
                }
            }
        }
        let route = match value("proxyjump") {
            None | Some("none") => SshRoute::Direct,
            Some(jump) => SshRoute::Jump {
                profile_id: *ids.get(jump).ok_or(OpenSshImportError::Jump)?,
            },
        };
        document.profiles.push(ProfileRecord {
            id: ids[alias],
            name: alias.clone(),
            kind: ProfileKind::Ssh,
            folder_id: None,
            tags: BTreeSet::new(),
            favorite: false,
            terminal: Default::default(),
        });
        document.ssh_connections.push(SshConnectionRecord {
            options: cshell_domain::SshConnectionOptions {
                keyboard_interactive,
                authentication_order: preferred
                    .filter(|_| keyboard_interactive)
                    .map(|order| {
                        order.split(',').map(|method| {
                            if method == "password" {
                                cshell_domain::SshAuthStep::Password
                            } else {
                                cshell_domain::SshAuthStep::KeyboardInteractive
                            }
                        }).collect()
                    })
                    .unwrap_or_default(),
                ..Default::default()
            },
            profile_id: ids[alias],
            host: hostname,
            port,
            username: username.into(),
            auth_method,
            private_key_path: key_path,
            certificate_path,
            agent_backend: if auth_method == SshAuthMethod::Agent {
                SshAgentBackend::OpenSsh
            } else {
                SshAgentBackend::default()
            },
            agent_identity: None,
            route,
        });
    }
    for target in &document.ssh_connections {
        cshell_domain::ssh_jump_chain(target, &document.ssh_connections)
            .map_err(|_| OpenSshImportError::Jump)?;
    }
    ProfileCatalog::from_snapshot(CatalogSnapshot {
        profiles: document.profiles.clone(),
        ssh_connections: document.ssh_connections.clone(),
        ..Default::default()
    })
    .map_err(|_| OpenSshImportError::InvalidTarget)?;
    Ok(document)
}

fn valid_pattern(value: &str) -> bool {
    let value = value.strip_prefix('!').unwrap_or(value);
    !value.is_empty()
        && value.len() <= 253
        && value.is_ascii()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_:*?".contains(&c))
}

fn matches_host(
    patterns: &[String],
    alias: &str,
    remaining: &mut usize,
) -> Result<bool, OpenSshImportError> {
    let mut positive = false;
    for pattern in patterns {
        let (negated, pattern) = pattern
            .strip_prefix('!')
            .map_or((false, pattern.as_str()), |p| (true, p));
        if wildcard_match(pattern.as_bytes(), alias.as_bytes(), remaining)? {
            if negated {
                return Ok(false);
            }
            positive = true;
        }
    }
    Ok(positive)
}

fn wildcard_match(
    pattern: &[u8],
    text: &[u8],
    remaining: &mut usize,
) -> Result<bool, OpenSshImportError> {
    let (mut p, mut t, mut star, mut retry) = (0, 0, None, 0);
    while t < text.len() {
        *remaining = remaining.checked_sub(1).ok_or(OpenSshImportError::Limit)?;
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = t;
        } else if let Some(s) = star {
            retry += 1;
            t = retry;
            p = s + 1;
        } else {
            return Ok(false);
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        *remaining = remaining.checked_sub(1).ok_or(OpenSshImportError::Limit)?;
        p += 1;
    }
    Ok(p == pattern.len())
}

fn split_values(source: &str, line: usize) -> Result<Vec<String>, OpenSshImportError> {
    let mut values = Vec::new();
    let mut value = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '#' if quote.is_none() => break,
            ' ' | '\t' if quote.is_none() => {
                if started {
                    values.push(std::mem::take(&mut value));
                    started = false;
                }
            }
            '\\' if chars.peek().is_some_and(|next| {
                matches!(next, '\\' | '\'' | '"') || (quote.is_none() && *next == ' ')
            }) =>
            {
                if let Some(next) = chars.next() {
                    value.push(next);
                    started = true;
                }
            }
            '\'' | '"' if quote.is_none() => {
                quote = Some(c);
                started = true;
            }
            c if quote == Some(c) => {
                quote = None;
            }
            _ => {
                value.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err(OpenSshImportError::Syntax { line });
    }
    if started {
        values.push(value);
    }
    Ok(values)
}

fn expand_hostname(source: &str, alias: &str) -> Option<String> {
    let mut result = String::new();
    let mut chars = source.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next()? {
                '%' => result.push('%'),
                'h' => result.push_str(alias),
                _ => return None,
            }
        } else if c == '$' {
            return None;
        } else {
            result.push(c);
        }
        if result.len() > 253 {
            return None;
        }
    }
    Some(result)
}

fn expand_key_path(
    source: &str,
    alias: &str,
    host: &str,
    user: &str,
    port: u16,
    home: Option<&Path>,
) -> Result<String, OpenSshImportError> {
    let mut result = String::new();
    let mut chars = source.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next().ok_or(OpenSshImportError::KeyPath)? {
                '%' => result.push('%'),
                'h' => result.push_str(host),
                'n' => result.push_str(alias),
                'r' => result.push_str(user),
                'p' => result.push_str(&port.to_string()),
                'd' => result.push_str(
                    home.and_then(Path::to_str)
                        .ok_or(OpenSshImportError::KeyPath)?,
                ),
                _ => return Err(OpenSshImportError::KeyPath),
            }
        } else if c == '$' {
            return Err(OpenSshImportError::KeyPath);
        } else {
            result.push(c);
        }
        if result.len() > 4096 {
            return Err(OpenSshImportError::KeyPath);
        }
    }
    let path = if let Some(tail) = result.strip_prefix("~/") {
        home.ok_or(OpenSshImportError::KeyPath)?.join(tail)
    } else {
        PathBuf::from(&result)
    };
    if !path.is_absolute() {
        return Err(OpenSshImportError::KeyPath);
    }
    let path = path.to_str().ok_or(OpenSshImportError::KeyPath)?;
    if path.len() > 4096 {
        return Err(OpenSshImportError::KeyPath);
    }
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests;
