//! Independent, bounded reader for the plain INI Xshell session metadata subset.
//! No vendor runtime, secret decoding, include resolution or filesystem access.
use crate::{
    CatalogSnapshot, ForeignProfileImportResult, ForeignProfileImportSummary,
    MAX_PROFILE_IMPORT_BYTES, PROFILE_CONNECTION_IMPORT_VERSION, PROFILE_IMPORT_FORMAT,
    ProfileCatalog, ProfileImportDocument,
};
use cshell_domain::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use zeroize::Zeroizing;

const MAX_LINES: usize = 8192;
const MAX_LINE: usize = 8192;
const MAX_FIELDS: usize = 1024;
const MAX_SECTIONS: usize = 128;

#[derive(Clone, Copy)]
pub struct XshellImportOptions<'a> {
    pub session_name: &'a str,
    pub default_username: &'a str,
}
impl std::fmt::Debug for XshellImportOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XshellImportOptions")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XshellImportSummary {
    pub format: String,
    pub files_read: usize,
    pub imported: usize,
    pub values_omitted: usize,
    pub credential_fields_omitted: usize,
    pub key_references_omitted: usize,
    pub trust_settings_omitted: usize,
    pub terminal_settings_omitted: usize,
    pub logging_settings_omitted: usize,
}
impl XshellImportSummary {
    pub fn notices(&self) -> Vec<String> {
        let mut notices = vec![
            "Xshell: converted one explicit SSH session's filename, host, port and account. Source authentication is omitted; review and rebind CShell credentials before opening.".into(),
            "Source TERM/emulation, algorithms, trust, appearance, logging and other settings are omitted. Converted Profile uses Direct/Password and Logging=false; running sessions are unchanged.".into(),
        ];
        for (count, label) in [
            (self.values_omitted, "source fields omitted"),
            (
                self.credential_fields_omitted,
                "credential fields omitted without decoding",
            ),
            (
                self.key_references_omitted,
                "key/certificate references omitted",
            ),
            (self.trust_settings_omitted, "host-trust fields omitted"),
            (self.terminal_settings_omitted, "terminal fields omitted"),
            (self.logging_settings_omitted, "logging fields omitted"),
        ] {
            if count > 0 {
                notices.push(format!("Xshell: {count} {label}."));
            }
        }
        notices
    }
    pub fn validate(&self) -> bool {
        self.format == "cshell.xshell-metadata-summary"
            && self.files_read == 1
            && self.imported == 1
            && self.values_omitted <= MAX_FIELDS
            && [
                self.credential_fields_omitted,
                self.key_references_omitted,
                self.trust_settings_omitted,
                self.terminal_settings_omitted,
                self.logging_settings_omitted,
            ]
            .iter()
            .all(|n| *n <= self.values_omitted)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum XshellImportError {
    #[error("Xshell import exceeds its byte, line, section or field limit")]
    Limit,
    #[error("Xshell session must be UTF-8 or BOM-prefixed UTF-16LE")]
    Encoding,
    #[error("invalid Xshell INI syntax at line {line}")]
    Syntax { line: usize },
    #[error("Xshell session contains duplicate section or field names")]
    Duplicate,
    #[error("Xshell import requires one explicit SSH session with host and decimal port")]
    Session,
    #[error("Xshell account is absent; provide an explicit default account")]
    MissingUser,
    #[error(
        "Xshell session contains unsupported routing, execution, forwarding or inheritance at line {line}"
    )]
    Behavior { line: usize },
    #[error("converted Xshell session does not satisfy CShell target or metadata limits")]
    InvalidTarget,
}

struct Field {
    value: Zeroizing<String>,
    line: usize,
}
type Fields = BTreeMap<(String, String), Field>;

fn decode(bytes: &[u8]) -> Result<Zeroizing<String>, XshellImportError> {
    if bytes.len() > MAX_PROFILE_IMPORT_BYTES {
        return Err(XshellImportError::Limit);
    }
    let mut text = Zeroizing::new(String::new());
    if bytes.starts_with(&[0xff, 0xfe]) {
        if bytes.len() % 2 != 0 {
            return Err(XshellImportError::Encoding);
        }
        let words = Zeroizing::new(
            bytes[2..]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>(),
        );
        for character in char::decode_utf16(words.iter().copied()) {
            text.push(character.map_err(|_| XshellImportError::Encoding)?);
            if text.len() > MAX_PROFILE_IMPORT_BYTES {
                return Err(XshellImportError::Limit);
            }
        }
    } else {
        text.push_str(
            std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
                .map_err(|_| XshellImportError::Encoding)?,
        );
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return Err(XshellImportError::Encoding);
    }
    Ok(text)
}

fn parse(text: &str) -> Result<Fields, XshellImportError> {
    let mut fields = Fields::new();
    let mut sections = BTreeSet::new();
    let mut section = None;
    for (index, raw) in text.split('\n').enumerate() {
        let line = index + 1;
        if line > MAX_LINES || raw.len() > MAX_LINE {
            return Err(XshellImportError::Limit);
        }
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.contains('\r') {
            return Err(XshellImportError::Syntax { line });
        }
        let raw = raw.trim_start_matches([' ', '\t']);
        if raw.is_empty() || raw.starts_with([';', '#']) {
            continue;
        }
        if raw.starts_with('[') {
            let name = raw
                .trim_end_matches([' ', '\t'])
                .strip_prefix('[')
                .and_then(|v| v.strip_suffix(']'))
                .ok_or(XshellImportError::Syntax { line })?;
            if name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-' | b' '))
            {
                return Err(XshellImportError::Syntax { line });
            }
            let name = name.to_ascii_lowercase();
            // Even empty include/inheritance sections are outside this reader.
            if ["include", "inherit", "parent", "defaultsession"]
                .iter()
                .any(|p| name.contains(*p))
            {
                return Err(XshellImportError::Behavior { line });
            }
            if !sections.insert(name.clone()) {
                return Err(XshellImportError::Duplicate);
            }
            if sections.len() > MAX_SECTIONS {
                return Err(XshellImportError::Limit);
            }
            section = Some(name);
            continue;
        }
        let section = section.as_ref().ok_or(XshellImportError::Syntax { line })?;
        let (key, value) = raw
            .split_once('=')
            .ok_or(XshellImportError::Syntax { line })?;
        let key = key.trim_end_matches([' ', '\t']);
        if key.is_empty()
            || key.len() > 128
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b' '))
        {
            return Err(XshellImportError::Syntax { line });
        }
        let identity = (section.clone(), key.to_ascii_lowercase());
        if fields.contains_key(&identity) {
            return Err(XshellImportError::Duplicate);
        }
        if fields.len() >= MAX_FIELDS {
            return Err(XshellImportError::Limit);
        }
        fields.insert(
            identity,
            Field {
                value: Zeroizing::new(value.into()),
                line,
            },
        );
    }
    Ok(fields)
}

fn get<'a>(fields: &'a Fields, section: &str, key: &str) -> Option<&'a str> {
    fields
        .get(&(section.into(), key.into()))
        .map(|f| f.value.as_str())
}
fn inactive(value: &str) -> bool {
    let value = value.trim();
    value.is_empty()
        || value == "0"
        || ["false", "no", "none"]
            .iter()
            .any(|v| value.eq_ignore_ascii_case(v))
}
fn rejected_behavior(section: &str, key: &str, value: &str) -> bool {
    // These are conservative omission guards, not implementations of vendor
    // behavior. Nonempty dormant paths/commands are refused too.
    let identity = format!("{section}:{key}");
    let family = [
        "proxy",
        "jump",
        "tunnel",
        "forward",
        "script",
        "command",
        "sendstring",
        "sendtext",
        "expect",
        "logon",
        "autologin",
        "environment",
        "nopty",
        "noshell",
        "x11",
    ]
    .iter()
    .any(|p| identity.contains(*p));
    let inheritance = ["include", "inherit", "parent", "defaultsession"]
        .iter()
        .any(|p| key.contains(*p));
    // An inheritance reference is unsupported even when its value is "0".
    inheritance || (family && !inactive(value))
}

pub(crate) fn batch_is_ssh(bytes: &[u8]) -> Result<bool, XshellImportError> {
    let content = decode(bytes)?;
    let fields = parse(&content)?;
    let protocol = get(&fields, "connection", "protocol")
        .filter(|value| !value.trim().is_empty())
        .ok_or(XshellImportError::Session)?;
    if protocol.eq_ignore_ascii_case("SSH") {
        return Ok(true);
    }
    if protocol.to_ascii_lowercase().contains("ssh") {
        return Err(XshellImportError::Session);
    }
    Ok(false)
}

pub fn import_xshell_session(
    bytes: &[u8],
    options: XshellImportOptions<'_>,
) -> Result<ForeignProfileImportResult, XshellImportError> {
    let content = decode(bytes)?;
    let fields = parse(&content)?;
    if !get(&fields, "connection", "protocol").is_some_and(|p| p.eq_ignore_ascii_case("SSH"))
        || options.session_name.trim().is_empty()
        || ["default", "default session", "default settings", "global"]
            .iter()
            .any(|v| options.session_name.trim().eq_ignore_ascii_case(v))
    {
        return Err(XshellImportError::Session);
    }
    for ((section, key), field) in &fields {
        if rejected_behavior(section, key, &field.value) {
            return Err(XshellImportError::Behavior { line: field.line });
        }
    }
    let host = get(&fields, "connection", "host")
        .filter(|v| !v.is_empty())
        .ok_or(XshellImportError::Session)?;
    if host.contains(['@', '/', '\\', '#', '[', ']'])
        || (host.contains(':') && host.parse::<std::net::Ipv6Addr>().is_err())
    {
        return Err(XshellImportError::InvalidTarget);
    }
    let port = get(&fields, "connection", "port").ok_or(XshellImportError::Session)?;
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(XshellImportError::InvalidTarget);
    }
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or(XshellImportError::InvalidTarget)?;
    let username = get(&fields, "connection:authentication", "username")
        .filter(|v| !v.is_empty())
        .unwrap_or(options.default_username);
    if username.is_empty() {
        return Err(XshellImportError::MissingUser);
    }
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
            tags: BTreeSet::from(["import:xshell".into()]),
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
    .map_err(|_| XshellImportError::InvalidTarget)?;
    document
        .fingerprint()
        .map_err(|_| XshellImportError::InvalidTarget)?;
    let mut summary = XshellImportSummary {
        format: "cshell.xshell-metadata-summary".into(),
        files_read: 1,
        imported: 1,
        values_omitted: 0,
        credential_fields_omitted: 0,
        key_references_omitted: 0,
        trust_settings_omitted: 0,
        terminal_settings_omitted: 0,
        logging_settings_omitted: 0,
    };
    for (section, key) in fields.keys() {
        if matches!(
            (section.as_str(), key.as_str()),
            ("connection", "host" | "port" | "protocol")
                | ("connection:authentication", "username")
        ) {
            continue;
        }
        summary.values_omitted += 1;
        if key.contains("password") || key.contains("passphrase") {
            summary.credential_fields_omitted += 1;
        }
        if key.contains("userkey") || key.contains("privatekey") || key.contains("certificate") {
            summary.key_references_omitted += 1;
        }
        if key.contains("hostkey") || key.contains("fingerprint") {
            summary.trust_settings_omitted += 1;
        }
        if section.starts_with("terminal") {
            summary.terminal_settings_omitted += 1;
        }
        if section.contains("logging") || key.starts_with("log") {
            summary.logging_settings_omitted += 1;
        }
    }
    Ok(ForeignProfileImportResult {
        document,
        summary: ForeignProfileImportSummary::Xshell(summary),
    })
}
