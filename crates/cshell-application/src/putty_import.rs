//! Independent, bounded PuTTY session metadata conversion. No registry, file,
//! credential, key, trust store or subprocess access is possible in this module.
use crate::{
    CatalogSnapshot, MAX_PROFILE_IMPORT_BYTES, PROFILE_CONNECTION_IMPORT_VERSION,
    PROFILE_IMPORT_FORMAT, ProfileCatalog, ProfileImportDocument,
};
use cshell_domain::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use zeroize::Zeroizing;
const ROOT: &str = "HKEY_CURRENT_USER\\Software\\SimonTatham\\PuTTY\\Sessions";
const MAX_SESSIONS: usize = 512;
const MAX_LINES: usize = 32768;
const MAX_LINE: usize = 8192;
#[derive(Clone, Copy, Default)]
pub struct PuttyImportOptions<'a> {
    pub default_username: &'a str,
}
impl std::fmt::Debug for PuttyImportOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PuttyImportOptions").finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PuttyImportSummary {
    pub sessions_read: usize,
    pub imported: usize,
    pub defaults_skipped: usize,
    pub other_protocols_skipped: usize,
    pub empty_hosts_skipped: usize,
    pub values_omitted: usize,
    pub key_references_omitted: usize,
    pub trust_overrides_omitted: usize,
    pub logging_settings_omitted: usize,
}
impl PuttyImportSummary {
    /// Fixed wording and bounded counts only. Never echo registry values or keys.
    pub fn notices(&self) -> Vec<String> {
        let mut notices=vec![format!("PuTTY: converted {} SSH sessions. Authentication is password with no imported secret; check or rebind CShell credentials before opening.",self.imported),"Only name, SSH host/port, account and terminal type are converted. CShell trust, algorithms, clipboard and terminal policies apply; source settings are not reproduced.".into(),"Converted Profiles set the Logging override to false. Import does not change running sessions; recording for a new session follows its terminal-setting support.".into()];
        for (count, label) in [
            (self.defaults_skipped, "Default Settings entries skipped"),
            (self.other_protocols_skipped, "non-SSH sessions skipped"),
            (self.empty_hosts_skipped, "sessions without a host skipped"),
            (self.values_omitted, "source values omitted"),
            (
                self.key_references_omitted,
                "external key/certificate references omitted; bind compatible keys separately",
            ),
            (
                self.trust_overrides_omitted,
                "manual host trust settings omitted",
            ),
            (
                self.logging_settings_omitted,
                "source logging settings omitted",
            ),
        ] {
            if count > 0 {
                notices.push(format!("PuTTY: {count} {label}."));
            }
        }
        notices
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PuttyImportResult {
    pub document: ProfileImportDocument,
    pub summary: PuttyImportSummary,
}
impl std::fmt::Debug for PuttyImportResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PuttyImportResult")
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PuttyImportError {
    #[error("PuTTY import exceeds its byte, line, session or value limit")]
    Limit,
    #[error("PuTTY registry export must use UTF-8 or BOM-prefixed UTF-16LE")]
    Encoding,
    #[error("PuTTY import requires a .reg export of the Sessions subtree")]
    Namespace,
    #[error("invalid or unsupported registry syntax at line {line}")]
    Syntax { line: usize },
    #[error("PuTTY export contains duplicate session names or values")]
    Duplicate,
    #[error("a PuTTY SSH session has no account; provide a default account")]
    MissingUser,
    #[error(
        "unsupported active PuTTY connection behavior at line {line}; no sessions were converted"
    )]
    UnsupportedBehavior { line: usize },
    #[error("PuTTY export has no convertible SSH sessions")]
    NoSessions,
    #[error("converted PuTTY session metadata failed Profile validation")]
    InvalidTarget,
}
enum Value {
    Text(Zeroizing<String>),
    Number(u32),
}
struct Session {
    name: String,
    values: BTreeMap<String, (Value, usize)>,
}
fn decode(bytes: &[u8]) -> Result<Zeroizing<String>, PuttyImportError> {
    if bytes.len() > MAX_PROFILE_IMPORT_BYTES {
        return Err(PuttyImportError::Limit);
    }
    let text = if bytes.starts_with(&[0xff, 0xfe]) {
        if bytes.len() % 2 != 0 {
            return Err(PuttyImportError::Encoding);
        }
        let words = Zeroizing::new(
            bytes[2..]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>(),
        );
        let mut text = Zeroizing::new(String::new());
        for character in char::decode_utf16(words.iter().copied()) {
            text.push(character.map_err(|_| PuttyImportError::Encoding)?);
        }
        text
    } else {
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        Zeroizing::new(
            std::str::from_utf8(bytes)
                .map_err(|_| PuttyImportError::Encoding)?
                .to_owned(),
        )
    };
    if text.len() > MAX_PROFILE_IMPORT_BYTES {
        return Err(PuttyImportError::Limit);
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return Err(PuttyImportError::Encoding);
    }
    Ok(text)
}
fn quoted(value: &str, line: usize) -> Result<(Zeroizing<String>, &str), PuttyImportError> {
    let Some(value) = value.strip_prefix('"') else {
        return Err(PuttyImportError::Syntax { line });
    };
    let mut out = Zeroizing::new(String::new());
    let mut chars = value.char_indices();
    while let Some((offset, c)) = chars.next() {
        match c {
            '"' => return Ok((out, &value[offset + 1..])),
            '\\' => match chars.next() {
                Some((_, c @ ('\\' | '"'))) => out.push(c),
                _ => return Err(PuttyImportError::Syntax { line }),
            },
            c => out.push(c),
        }
    }
    Err(PuttyImportError::Syntax { line })
}
fn session_name(value: &str) -> Result<String, PuttyImportError> {
    let mut decoded = Zeroizing::new(Vec::new());
    let bytes = value.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'%' {
            let pair = bytes
                .get(cursor + 1..cursor + 3)
                .ok_or(PuttyImportError::Encoding)?;
            let a = char::from(pair[0])
                .to_digit(16)
                .ok_or(PuttyImportError::Encoding)?;
            let b = char::from(pair[1])
                .to_digit(16)
                .ok_or(PuttyImportError::Encoding)?;
            decoded.push((a * 16 + b) as u8);
            cursor += 3;
        } else {
            decoded.push(bytes[cursor]);
            cursor += 1;
        }
    }
    let name = std::str::from_utf8(&decoded).map_err(|_| PuttyImportError::Encoding)?;
    if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(PuttyImportError::InvalidTarget);
    }
    Ok(name.to_owned())
}
fn parse(text: &str) -> Result<Vec<Session>, PuttyImportError> {
    let mut lines = text.lines().enumerate();
    let (_, header) = lines.next().ok_or(PuttyImportError::Encoding)?;
    if header.len() > MAX_LINE {
        return Err(PuttyImportError::Limit);
    }
    if !matches!(
        header.trim(),
        "Windows Registry Editor Version 5.00" | "REGEDIT4"
    ) {
        return Err(PuttyImportError::Encoding);
    }
    let prefix = format!("{ROOT}\\");
    let mut sessions = Vec::<Session>::new();
    let mut current = None;
    let mut names = BTreeSet::new();
    for (index, raw) in lines {
        let line = index + 1;
        if line > MAX_LINES || raw.len() > MAX_LINE {
            return Err(PuttyImportError::Limit);
        }
        let text = raw.trim();
        if text.is_empty() || text.starts_with(';') {
            continue;
        }
        if text.starts_with('[') {
            let section = text
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .ok_or(PuttyImportError::Syntax { line })?;
            if section.eq_ignore_ascii_case(ROOT) {
                current = None;
                continue;
            }
            if section.len() <= prefix.len()
                || !section
                    .get(..prefix.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(&prefix))
            {
                return Err(PuttyImportError::Namespace);
            }
            let raw_name = &section[prefix.len()..];
            if raw_name.contains('\\') {
                return Err(PuttyImportError::Namespace);
            }
            let name = session_name(raw_name)?;
            if !names.insert(name.to_lowercase()) {
                return Err(PuttyImportError::Duplicate);
            }
            if sessions.len() >= MAX_SESSIONS {
                return Err(PuttyImportError::Limit);
            }
            sessions.push(Session {
                name,
                values: BTreeMap::new(),
            });
            current = Some(sessions.len() - 1);
            continue;
        }
        let Some(index) = current else {
            return Err(PuttyImportError::Namespace);
        };
        let (key, rest) = quoted(text, line)?;
        if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
            return Err(PuttyImportError::Syntax { line });
        }
        let value = rest
            .trim_start()
            .strip_prefix('=')
            .ok_or(PuttyImportError::Syntax { line })?
            .trim();
        let value = if value.starts_with('"') {
            let (value, rest) = quoted(value, line)?;
            if !rest.trim().is_empty() {
                return Err(PuttyImportError::Syntax { line });
            }
            Value::Text(value)
        } else if value
            .get(..6)
            .is_some_and(|v| v.eq_ignore_ascii_case("dword:"))
        {
            let value = &value[6..];
            if value.len() != 8 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(PuttyImportError::Syntax { line });
            }
            Value::Number(
                u32::from_str_radix(value, 16).map_err(|_| PuttyImportError::Syntax { line })?,
            )
        } else {
            return Err(PuttyImportError::Syntax { line });
        };
        if sessions[index].values.len() >= 512 {
            return Err(PuttyImportError::Limit);
        }
        if sessions[index]
            .values
            .insert(key.to_ascii_lowercase(), (value, line))
            .is_some()
        {
            return Err(PuttyImportError::Duplicate);
        }
    }
    Ok(sessions)
}
fn text<'a>(session: &'a Session, key: &str) -> Result<Option<&'a str>, PuttyImportError> {
    match session.values.get(key) {
        None => Ok(None),
        Some((Value::Text(value), _)) => Ok(Some(value.as_str())),
        Some((_, line)) => Err(PuttyImportError::Syntax { line: *line }),
    }
}
fn number(session: &Session, key: &str) -> Result<Option<u32>, PuttyImportError> {
    match session.values.get(key) {
        None => Ok(None),
        Some((Value::Number(value), _)) => Ok(Some(*value)),
        Some((_, line)) => Err(PuttyImportError::Syntax { line: *line }),
    }
}
fn reject_active(session: &Session) -> Result<(), PuttyImportError> {
    for key in [
        "remotecommand",
        "remotecommand2",
        "portforwardings",
        "environment",
        "loghost",
        "authplugin",
    ] {
        if text(session, key)?.is_some_and(|value| !value.is_empty()) {
            return Err(PuttyImportError::UnsupportedBehavior {
                line: session.values[key].1,
            });
        }
    }
    for key in [
        "proxymethod",
        "proxytype",
        "nopty",
        "sshnoshell",
        "agentfwd",
        "x11forward",
        "sshnoauth",
    ] {
        if number(session, key)?.is_some_and(|value| value != 0) {
            return Err(PuttyImportError::UnsupportedBehavior {
                line: session.values[key].1,
            });
        }
    }
    if number(session, "sshprot")?.is_some_and(|value| value != 2) {
        return Err(PuttyImportError::UnsupportedBehavior {
            line: session.values["sshprot"].1,
        });
    }
    Ok(())
}
pub fn import_putty_registry(
    bytes: &[u8],
    options: PuttyImportOptions<'_>,
) -> Result<PuttyImportResult, PuttyImportError> {
    let content = decode(bytes)?;
    let sessions = parse(&content)?;
    let mut summary = PuttyImportSummary {
        sessions_read: sessions.len(),
        ..Default::default()
    };
    let mut document = ProfileImportDocument {
        format: PROFILE_IMPORT_FORMAT.into(),
        version: PROFILE_CONNECTION_IMPORT_VERSION,
        folders: Vec::new(),
        profiles: Vec::new(),
        ssh_connections: Vec::new(),
        local_connections: Vec::new(),
    };
    for session in sessions {
        if session.name.eq_ignore_ascii_case("Default Settings") {
            summary.defaults_skipped += 1;
            continue;
        }
        if !text(&session, "protocol")?.is_some_and(|v| v.eq_ignore_ascii_case("ssh")) {
            summary.other_protocols_skipped += 1;
            continue;
        }
        let host = text(&session, "hostname")?.unwrap_or("");
        if host.is_empty() {
            summary.empty_hosts_skipped += 1;
            continue;
        }
        reject_active(&session)?;
        if host.contains(['@', '/', '\\', '#', '[', ']'])
            || (host.contains(':') && host.parse::<std::net::Ipv6Addr>().is_err())
        {
            return Err(PuttyImportError::InvalidTarget);
        }
        let username = text(&session, "username")?
            .filter(|v| !v.is_empty())
            .unwrap_or(options.default_username);
        if username.is_empty() {
            return Err(PuttyImportError::MissingUser);
        }
        let port = number(&session, "portnumber")?.unwrap_or(22);
        let port = u16::try_from(port)
            .ok()
            .filter(|v| *v > 0)
            .ok_or(PuttyImportError::InvalidTarget)?;
        let terminal_type = text(&session, "terminaltype")?.map(str::to_owned);
        for key in ["publickeyfile", "detachedcertificate"] {
            if text(&session, key)?.is_some_and(|v| !v.is_empty()) {
                summary.key_references_omitted += 1;
            }
        }
        if text(&session, "sshmanualhostkeys")?.is_some_and(|v| !v.is_empty()) {
            summary.trust_overrides_omitted += 1;
        }
        if number(&session, "logtype")?.is_some_and(|v| v != 0) {
            summary.logging_settings_omitted += 1;
        }
        summary.values_omitted += session
            .values
            .keys()
            .filter(|key| {
                !matches!(
                    key.as_str(),
                    "hostname" | "protocol" | "portnumber" | "username" | "terminaltype"
                )
            })
            .count();
        let id = ProfileId::new();
        document.profiles.push(ProfileRecord {
            id,
            name: session.name.clone(),
            kind: ProfileKind::Ssh,
            folder_id: None,
            tags: BTreeSet::from(["import:putty".into()]),
            favorite: false,
            terminal: TerminalOverrides {
                terminal_type,
                theme: None,
                logging: Some(false),
            },
        });
        document.ssh_connections.push(SshConnectionRecord {
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
        });
        summary.imported += 1;
    }
    if document.profiles.is_empty() {
        return Err(PuttyImportError::NoSessions);
    }
    ProfileCatalog::from_snapshot(CatalogSnapshot {
        profiles: document.profiles.clone(),
        ssh_connections: document.ssh_connections.clone(),
        ..Default::default()
    })
    .map_err(|_| PuttyImportError::InvalidTarget)?;
    let encoded = serde_json::to_vec(&document).map_err(|_| PuttyImportError::InvalidTarget)?;
    if encoded.len() > MAX_PROFILE_IMPORT_BYTES {
        return Err(PuttyImportError::Limit);
    }
    ProfileImportDocument::parse(&encoded).map_err(|_| PuttyImportError::InvalidTarget)?;
    Ok(PuttyImportResult { document, summary })
}
