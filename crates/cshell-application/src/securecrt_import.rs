//! Independent SecureCRT session INI metadata reader. No vendor code, registry,
//! key/decryption, environment, script, network or filesystem operations.
use crate::{
    CatalogSnapshot, MAX_PROFILE_IMPORT_BYTES, PROFILE_CONNECTION_IMPORT_VERSION,
    PROFILE_IMPORT_FORMAT, ProfileCatalog, ProfileImportDocument,
};
use cshell_domain::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use zeroize::Zeroizing;
const MAX_LINES: usize = 8192;
const MAX_LINE: usize = 8192;
const MAX_FIELDS: usize = 1024;
const MAX_BLOCK_LINES: usize = 1024;
#[derive(Clone, Copy)]
pub struct SecureCrtImportOptions<'a> {
    pub session_name: &'a str,
    pub default_username: &'a str,
}
impl std::fmt::Debug for SecureCrtImportOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureCrtImportOptions")
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecureCrtImportSummary {
    pub format: String,
    pub files_read: usize,
    pub imported: usize,
    pub values_omitted: usize,
    pub block_lines_omitted: usize,
    pub key_references_omitted: usize,
    pub trust_settings_omitted: usize,
    pub terminal_settings_omitted: usize,
    pub logging_settings_omitted: usize,
}
impl SecureCrtImportSummary {
    pub fn notices(&self) -> Vec<String> {
        let mut notices=vec!["SecureCRT: converted one SSH2 session's name, host, port and account. Source credentials and authentication chains are not imported; check or rebind CShell credentials before opening.".into(),"Source emulation/TERM, logging, algorithms, appearance, clipboard, trust and other settings are omitted; CShell settings apply. Converted Profile sets Logging=false without changing running sessions.".into()];
        for (count, label) in [
            (self.values_omitted, "source fields omitted"),
            (self.block_lines_omitted, "opaque block lines omitted"),
            (
                self.key_references_omitted,
                "key/certificate references omitted",
            ),
            (
                self.trust_settings_omitted,
                "source host-trust fields omitted",
            ),
            (
                self.terminal_settings_omitted,
                "emulation/TERM fields omitted",
            ),
            (self.logging_settings_omitted, "logging fields omitted"),
        ] {
            if count > 0 {
                notices.push(format!("SecureCRT: {count} {label}."));
            }
        }
        notices
    }
    pub fn validate(&self) -> bool {
        self.format == "cshell.securecrt-metadata-summary"
            && self.files_read == 1
            && self.imported == 1
            && self.values_omitted <= MAX_FIELDS
            && self.block_lines_omitted <= MAX_LINES
            && [
                self.key_references_omitted,
                self.trust_settings_omitted,
                self.terminal_settings_omitted,
                self.logging_settings_omitted,
            ]
            .iter()
            .all(|n| *n <= MAX_FIELDS)
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum SecureCrtImportError {
    #[error("SecureCRT import exceeds its byte, line, field or block limit")]
    Limit,
    #[error("SecureCRT INI must be UTF-8 or BOM-prefixed UTF-16LE")]
    Encoding,
    #[error("invalid SecureCRT INI syntax or field type at line {line}")]
    Syntax { line: usize },
    #[error("SecureCRT INI contains duplicate field names")]
    Duplicate,
    #[error("SecureCRT import requires one SSH2 session with an explicit hostname")]
    Session,
    #[error("SecureCRT account is absent; provide an explicit default account")]
    MissingUser,
    #[error(
        "unsupported nonempty SecureCRT script, command, route or forwarding setting at line {line}"
    )]
    Behavior { line: usize },
    #[error("converted SecureCRT session failed Profile validation")]
    InvalidTarget,
}
enum Value {
    Text(Zeroizing<String>),
    Number(u32),
    Block(usize),
}
struct Field {
    value: Value,
    line: usize,
}
fn decode(bytes: &[u8]) -> Result<Zeroizing<String>, SecureCrtImportError> {
    if bytes.len() > MAX_PROFILE_IMPORT_BYTES {
        return Err(SecureCrtImportError::Limit);
    }
    let mut text = Zeroizing::new(String::new());
    if bytes.starts_with(&[0xff, 0xfe]) {
        if bytes.len() % 2 != 0 {
            return Err(SecureCrtImportError::Encoding);
        }
        let words = Zeroizing::new(
            bytes[2..]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>(),
        );
        for character in char::decode_utf16(words.iter().copied()) {
            text.push(character.map_err(|_| SecureCrtImportError::Encoding)?);
        }
    } else {
        text.push_str(
            std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
                .map_err(|_| SecureCrtImportError::Encoding)?,
        );
    }
    if text.len() > MAX_PROFILE_IMPORT_BYTES {
        return Err(SecureCrtImportError::Limit);
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(SecureCrtImportError::Encoding);
    }
    Ok(text)
}
fn parse(content: &str) -> Result<BTreeMap<String, Field>, SecureCrtImportError> {
    let lines = content.lines().collect::<Vec<_>>();
    if lines.len() > MAX_LINES || lines.iter().any(|line| line.len() > MAX_LINE) {
        return Err(SecureCrtImportError::Limit);
    }
    let mut fields = BTreeMap::new();
    let mut position = 0;
    while let Some(raw) = lines.get(position) {
        let line = position + 1;
        position += 1;
        let raw = raw.trim_start();
        if raw.is_empty() || raw.starts_with('#') || raw.starts_with(';') {
            continue;
        }
        let bytes = raw.as_bytes();
        if bytes.len() < 5
            || bytes[1] != b':'
            || bytes[2] != b'"'
            || !matches!(bytes[0], b'S' | b'D' | b'Z')
        {
            return Err(SecureCrtImportError::Syntax { line });
        }
        let end = raw[3..]
            .find("\"=")
            .map(|i| i + 3)
            .ok_or(SecureCrtImportError::Syntax { line })?;
        let key = &raw[3..end];
        if key.is_empty()
            || key.len() > 128
            || key.contains('"')
            || key.chars().any(char::is_control)
        {
            return Err(SecureCrtImportError::Syntax { line });
        }
        let key = key.to_ascii_lowercase();
        let value = &raw[end + 2..];
        let value = match bytes[0] {
            b'S' => Value::Text(Zeroizing::new(value.to_owned())),
            b'D' | b'Z' => {
                if value.len() != 8 || !value.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return Err(SecureCrtImportError::Syntax { line });
                }
                let number = u32::from_str_radix(value, 16)
                    .map_err(|_| SecureCrtImportError::Syntax { line })?;
                if bytes[0] == b'Z' {
                    let count = usize::try_from(number).map_err(|_| SecureCrtImportError::Limit)?;
                    if count > MAX_BLOCK_LINES
                        || position
                            .checked_add(count)
                            .is_none_or(|end| end > lines.len())
                    {
                        return Err(SecureCrtImportError::Syntax { line });
                    }
                    position += count;
                    Value::Block(count)
                } else {
                    Value::Number(number)
                }
            }
            _ => return Err(SecureCrtImportError::Syntax { line }),
        };
        if fields.len() >= MAX_FIELDS {
            return Err(SecureCrtImportError::Limit);
        }
        if fields.insert(key, Field { value, line }).is_some() {
            return Err(SecureCrtImportError::Duplicate);
        }
    }
    Ok(fields)
}
fn text<'a>(
    fields: &'a BTreeMap<String, Field>,
    key: &str,
) -> Result<Option<&'a str>, SecureCrtImportError> {
    match fields.get(key) {
        None => Ok(None),
        Some(Field {
            value: Value::Text(value),
            ..
        }) => Ok(Some(value.as_str())),
        Some(field) => Err(SecureCrtImportError::Syntax { line: field.line }),
    }
}
fn number(
    fields: &BTreeMap<String, Field>,
    key: &str,
) -> Result<Option<u32>, SecureCrtImportError> {
    match fields.get(key) {
        None => Ok(None),
        Some(Field {
            value: Value::Number(value),
            ..
        }) => Ok(Some(*value)),
        Some(field) => Err(SecureCrtImportError::Syntax { line: field.line }),
    }
}
fn rejected_behavior(key: &str, field: &Field) -> bool {
    if key == "port forward filter" {
        return false;
    }
    // Conservative rejection for persisted execution/route families, including
    // nonempty dormant references. Unknown settings never become CShell actions.
    let family = [
        "script",
        "command",
        "firewall",
        "proxy",
        "dependent session",
        "forward",
        "initial prompt",
        "expect send",
        "auto login",
        "automate logon",
        "send string",
        "send on connect",
        "environment",
        "x11",
        "no pty",
        "nopty",
        "no shell",
    ]
    .iter()
    .any(|fragment| key.contains(fragment));
    if !family {
        return false;
    }
    match &field.value {
        Value::Text(value) => {
            !value.is_empty()
                && !((key.contains("firewall") || key.contains("proxy"))
                    && value.eq_ignore_ascii_case("none"))
        }
        Value::Number(value) => *value != 0,
        Value::Block(count) => *count != 0,
    }
}
pub(crate) fn batch_is_ssh(bytes: &[u8]) -> Result<bool, SecureCrtImportError> {
    let content = decode(bytes)?;
    let fields = parse(&content)?;
    let protocol = text(&fields, "protocol name")?
        .filter(|value| !value.trim().is_empty())
        .ok_or(SecureCrtImportError::Session)?;
    if protocol.eq_ignore_ascii_case("SSH2") {
        return Ok(true);
    }
    if protocol.to_ascii_lowercase().contains("ssh") {
        return Err(SecureCrtImportError::Session);
    }
    Ok(false)
}

pub fn import_securecrt_ini(
    bytes: &[u8],
    options: SecureCrtImportOptions<'_>,
) -> Result<crate::ForeignProfileImportResult, SecureCrtImportError> {
    let content = decode(bytes)?;
    let fields = parse(&content)?;
    if !text(&fields, "protocol name")?.is_some_and(|p| p.eq_ignore_ascii_case("SSH2")) {
        return Err(SecureCrtImportError::Session);
    }
    for (key, field) in &fields {
        if rejected_behavior(key, field) {
            return Err(SecureCrtImportError::Behavior { line: field.line });
        }
    }
    let host = text(&fields, "hostname")?
        .filter(|v| !v.is_empty())
        .ok_or(SecureCrtImportError::Session)?;
    if host.contains(['@', '/', '\\', '#', '[', ']'])
        || (host.contains(':') && host.parse::<std::net::Ipv6Addr>().is_err())
    {
        return Err(SecureCrtImportError::InvalidTarget);
    }
    let username = text(&fields, "username")?
        .filter(|v| !v.is_empty())
        .unwrap_or(options.default_username);
    if username.is_empty() {
        return Err(SecureCrtImportError::MissingUser);
    }
    if options.session_name.trim().is_empty()
        || options.session_name.trim().eq_ignore_ascii_case("Default")
        || options.session_name.trim().eq_ignore_ascii_case("Global")
    {
        return Err(SecureCrtImportError::Session);
    }
    let port = number(&fields, "[ssh2] port")?.unwrap_or(22);
    let port = u16::try_from(port)
        .ok()
        .filter(|p| *p > 0)
        .ok_or(SecureCrtImportError::InvalidTarget)?;
    let id = ProfileId::new();
    let document = ProfileImportDocument {
        format: PROFILE_IMPORT_FORMAT.into(),
        version: PROFILE_CONNECTION_IMPORT_VERSION,
        folders: Vec::new(),
        local_connections: Vec::new(),
        profiles: vec![ProfileRecord {
            id,
            name: options.session_name.into(),
            kind: ProfileKind::Ssh,
            folder_id: None,
            tags: BTreeSet::from(["import:securecrt".into()]),
            favorite: false,
            terminal: TerminalOverrides {
                terminal_type: None,
                theme: None,
                logging: Some(false),
            },
        }],
        ssh_connections: vec![SshConnectionRecord {
            options: Default::default(),
            profile_id: id,
            host: host.into(),
            port,
            username: username.into(),
            auth_method: SshAuthMethod::Password,
            private_key_path: None,
            certificate_path: None,
            agent_backend: SshAgentBackend::Auto,
            agent_identity: None,
            route: SshRoute::Direct,
        }],
    };
    ProfileCatalog::from_snapshot(CatalogSnapshot {
        profiles: document.profiles.clone(),
        ssh_connections: document.ssh_connections.clone(),
        ..Default::default()
    })
    .map_err(|_| SecureCrtImportError::InvalidTarget)?;
    document
        .fingerprint()
        .map_err(|_| SecureCrtImportError::InvalidTarget)?;
    let mut summary = SecureCrtImportSummary {
        format: "cshell.securecrt-metadata-summary".into(),
        files_read: 1,
        imported: 1,
        values_omitted: 0,
        block_lines_omitted: 0,
        key_references_omitted: 0,
        trust_settings_omitted: 0,
        terminal_settings_omitted: 0,
        logging_settings_omitted: 0,
    };
    for (key, field) in &fields {
        if !matches!(
            key.as_str(),
            "hostname" | "username" | "protocol name" | "[ssh2] port"
        ) {
            summary.values_omitted += 1;
        }
        if let Value::Block(count) = &field.value {
            summary.block_lines_omitted += count;
        }
        if key.contains("identity") || key.contains("certificate") {
            summary.key_references_omitted += 1;
        }
        if key.contains("host key") || key.contains("hostkey") {
            summary.trust_settings_omitted += 1;
        }
        if key.contains("emulation") || key.contains("terminal type") {
            summary.terminal_settings_omitted += 1;
        }
        if key.contains("log ") || key.starts_with("log") {
            summary.logging_settings_omitted += 1;
        }
    }
    Ok(crate::ForeignProfileImportResult {
        document,
        summary: crate::ForeignProfileImportSummary::SecureCrt(summary),
    })
}
