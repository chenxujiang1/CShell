//! Independent reader for a bounded subset of exported MobaXterm bookmarks.
//! All source values borrow a zeroizing decoded buffer; none are executed or
//! resolved as paths, credentials, shared files or vendor configuration.
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
const MAX_LINE: usize = 16384;
const MAX_SECTIONS: usize = 128;
const MAX_SESSIONS: usize = 512;
const MAX_FOLDERS: usize = 256;
const MAX_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobaXtermEncoding {
    /// UTF-8, or UTF-16LE with a BOM. No system-codepage guessing.
    #[default]
    Unicode,
    /// Explicit legacy export encoding; undefined CP1252 bytes are rejected.
    Windows1252,
}

#[derive(Clone, Copy)]
pub struct MobaXtermImportOptions<'a> {
    pub default_username: &'a str,
    pub encoding: MobaXtermEncoding,
}
impl std::fmt::Debug for MobaXtermImportOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobaXtermImportOptions")
            .field("encoding", &self.encoding)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MobaXtermImportSummary {
    pub format: String,
    pub source_encoding: MobaXtermEncoding,
    pub sections_read: usize,
    pub sessions_read: usize,
    pub imported: usize,
    pub other_protocols_skipped: usize,
    pub folders_created: usize,
    pub packed_fields_omitted: usize,
    pub terminal_fields_omitted: usize,
    pub private_key_references_omitted: usize,
    pub comments_omitted: usize,
    pub folder_icons_omitted: usize,
}
impl MobaXtermImportSummary {
    pub fn notices(&self) -> Vec<String> {
        let mut notices = vec![
            format!("MobaXterm: converted {} SSH sessions and {} logical folders; encoding {:?}. Source authentication is omitted; check or rebind CShell credentials before opening.", self.imported, self.folders_created, self.source_encoding),
            "TERM, logging, appearance, browser, compression, agent/algorithm preferences, comments and icons are omitted. New Profiles use Direct/Password and Logging=false; running sessions are unchanged.".into(),
            "Empty or <default> accounts, and accounts disabled by the source username flag, use only your explicit default account. No Windows-account, credential-name or shared-file lookup occurs.".into(),
        ];
        for (count, label) in [
            (self.other_protocols_skipped, "non-SSH sessions skipped"),
            (self.packed_fields_omitted, "packed fields omitted"),
            (
                self.terminal_fields_omitted,
                "terminal fields omitted (included in packed count)",
            ),
            (
                self.private_key_references_omitted,
                "private-key references omitted without reading files",
            ),
            (self.comments_omitted, "comments omitted"),
            (self.folder_icons_omitted, "folder icons omitted"),
        ] {
            if count > 0 {
                notices.push(format!("MobaXterm: {count} {label}."));
            }
        }
        notices
    }
    pub fn validate(&self) -> bool {
        self.format == "cshell.mobaxterm-metadata-summary"
            && (1..=MAX_SECTIONS).contains(&self.sections_read)
            && (1..=MAX_SESSIONS).contains(&self.imported)
            && self.sessions_read <= MAX_SESSIONS
            && self.other_protocols_skipped <= MAX_SESSIONS
            && self.imported + self.other_protocols_skipped == self.sessions_read
            && self.folders_created <= MAX_FOLDERS
            && self.packed_fields_omitted <= MAX_SESSIONS * 80
            && self.terminal_fields_omitted <= self.imported * 43
            && self.terminal_fields_omitted <= self.packed_fields_omitted
            && self.private_key_references_omitted <= self.imported
            && self.comments_omitted <= self.imported
            && self.folder_icons_omitted <= self.sections_read
    }
    pub(crate) fn validate_document(&self, document: &ProfileImportDocument) -> bool {
        self.validate()
            && self.imported == document.profiles.len()
            && self.folders_created == document.folders.len()
            && document
                .folders
                .iter()
                .all(|f| !f.id.as_uuid().is_nil() && f.terminal == TerminalOverrides::default())
            && document.profiles.iter().all(|p| {
                !p.favorite
                    && p.terminal.terminal_type.is_none()
                    && p.terminal.theme.is_none()
                    && p.tags.len() == 1
                    && p.tags.contains("import:mobaxterm")
            })
            && document
                .ssh_connections
                .iter()
                .all(|c| c.agent_backend == SshAgentBackend::Auto)
            && document.folders.iter().all(|folder| {
                let mut cursor = folder.parent_id;
                let mut depth = 1;
                while let Some(id) = cursor {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return false;
                    }
                    let Some(parent) = document.folders.iter().find(|f| f.id == id) else {
                        return false;
                    };
                    cursor = parent.parent_id;
                }
                true
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum MobaXtermImportError {
    #[error("MobaXterm import exceeds its byte, line, section, folder or session budget")]
    Limit,
    #[error("MobaXterm export does not match the selected Unicode or Windows-1252 encoding")]
    Encoding,
    #[error("invalid MobaXterm bookmark syntax at line {line}")]
    Syntax { line: usize },
    #[error(
        "MobaXterm export contains duplicate or case-ambiguous sections, folders or session names"
    )]
    Duplicate,
    #[error(
        "MobaXterm import accepts exported Bookmarks sections only, not a complete application INI"
    )]
    Section,
    #[error("MobaXterm SSH session uses an unsupported packed layout at line {line}")]
    Layout { line: usize },
    #[error(
        "MobaXterm SSH session has unsupported routing, command, environment, macro, forwarding or SSH-1 settings at line {line}"
    )]
    Behavior { line: usize },
    #[error("MobaXterm SSH session needs an explicit default account at line {line}")]
    MissingUser { line: usize },
    #[error("MobaXterm export contains no supported SSH session")]
    NoSessions,
    #[error("converted MobaXterm folder, account or target exceeds CShell metadata limits")]
    InvalidTarget,
}

fn decode(
    bytes: &[u8],
    encoding: MobaXtermEncoding,
) -> Result<Zeroizing<String>, MobaXtermImportError> {
    if bytes.len() > MAX_PROFILE_IMPORT_BYTES {
        return Err(MobaXtermImportError::Limit);
    }
    let mut text = Zeroizing::new(String::new());
    match encoding {
        MobaXtermEncoding::Unicode if bytes.starts_with(&[0xff, 0xfe]) => {
            if bytes.len() % 2 != 0 {
                return Err(MobaXtermImportError::Encoding);
            }
            let words = Zeroizing::new(
                bytes[2..]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect::<Vec<_>>(),
            );
            for character in char::decode_utf16(words.iter().copied()) {
                text.push(character.map_err(|_| MobaXtermImportError::Encoding)?);
                if text.len() > MAX_PROFILE_IMPORT_BYTES {
                    return Err(MobaXtermImportError::Limit);
                }
            }
        }
        MobaXtermEncoding::Unicode => text.push_str(
            std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
                .map_err(|_| MobaXtermImportError::Encoding)?,
        ),
        MobaXtermEncoding::Windows1252 => {
            if bytes.starts_with(&[0xff, 0xfe])
                || bytes.starts_with(&[0xfe, 0xff])
                || bytes.starts_with(&[0xef, 0xbb, 0xbf])
            {
                return Err(MobaXtermImportError::Encoding);
            }
            for byte in bytes {
                let character = match *byte {
                    0x80 => '€',
                    0x82 => '‚',
                    0x83 => 'ƒ',
                    0x84 => '„',
                    0x85 => '…',
                    0x86 => '†',
                    0x87 => '‡',
                    0x88 => 'ˆ',
                    0x89 => '‰',
                    0x8a => 'Š',
                    0x8b => '‹',
                    0x8c => 'Œ',
                    0x8e => 'Ž',
                    0x91 => '‘',
                    0x92 => '’',
                    0x93 => '“',
                    0x94 => '”',
                    0x95 => '•',
                    0x96 => '–',
                    0x97 => '—',
                    0x98 => '˜',
                    0x99 => '™',
                    0x9a => 'š',
                    0x9b => '›',
                    0x9c => 'œ',
                    0x9e => 'ž',
                    0x9f => 'Ÿ',
                    0x81 | 0x8d | 0x8f | 0x90 | 0x9d => return Err(MobaXtermImportError::Encoding),
                    other => char::from(other),
                };
                text.push(character);
                if text.len() > MAX_PROFILE_IMPORT_BYTES {
                    return Err(MobaXtermImportError::Limit);
                }
            }
        }
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return Err(MobaXtermImportError::Encoding);
    }
    Ok(text)
}

struct Section<'a> {
    path: Option<&'a str>,
    icon: Option<&'a str>,
    entries: Vec<(&'a str, &'a str, usize)>,
}
fn decimal(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}
fn parse(text: &str) -> Result<Vec<Section<'_>>, MobaXtermImportError> {
    let mut sections = Vec::<Section<'_>>::new();
    let mut section_ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut sessions = 0;
    for (index, raw) in text.split('\n').enumerate() {
        let line = index + 1;
        if line > MAX_LINES || raw.len() > MAX_LINE {
            return Err(MobaXtermImportError::Limit);
        }
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.contains('\r') {
            return Err(MobaXtermImportError::Syntax { line });
        }
        let raw = raw.trim_start_matches([' ', '\t']);
        if raw.is_empty() || raw.starts_with(';') {
            continue;
        }
        if raw.starts_with('[') {
            let name = raw
                .trim_end_matches([' ', '\t'])
                .strip_prefix('[')
                .and_then(|v| v.strip_suffix(']'))
                .ok_or(MobaXtermImportError::Syntax { line })?;
            let name = name.to_ascii_lowercase();
            let id = if name == "bookmarks" {
                0
            } else {
                let suffix = name
                    .strip_prefix("bookmarks_")
                    .ok_or(MobaXtermImportError::Section)?;
                if suffix.starts_with('0') {
                    return Err(MobaXtermImportError::Section);
                }
                decimal(suffix)
                    .filter(|n| *n > 0)
                    .ok_or(MobaXtermImportError::Section)?
            };
            if sections.is_empty() && id != 0 {
                return Err(MobaXtermImportError::Section);
            }
            if !section_ids.insert(id) {
                return Err(MobaXtermImportError::Duplicate);
            }
            if sections.len() >= MAX_SECTIONS {
                return Err(MobaXtermImportError::Limit);
            }
            sections.push(Section {
                path: None,
                icon: None,
                entries: Vec::new(),
            });
            names.clear();
            continue;
        }
        let section = sections.last_mut().ok_or(MobaXtermImportError::Section)?;
        let (name, value) = raw
            .split_once('=')
            .ok_or(MobaXtermImportError::Syntax { line })?;
        let name = name.trim_end_matches([' ', '\t']);
        if name.is_empty() || name.len() > 128 || name.contains(['[', ']']) {
            return Err(MobaXtermImportError::Syntax { line });
        }
        if !names.insert(name.to_lowercase()) {
            return Err(MobaXtermImportError::Duplicate);
        }
        if name.eq_ignore_ascii_case("SubRep") {
            section.path = Some(value);
        } else if name.eq_ignore_ascii_case("ImgNum") {
            if decimal(value).is_none() {
                return Err(MobaXtermImportError::Syntax { line });
            }
            section.icon = Some(value);
        } else {
            sessions += 1;
            if sessions > MAX_SESSIONS {
                return Err(MobaXtermImportError::Limit);
            }
            section.entries.push((name, value, line));
        }
    }
    if sections.is_empty() || sections.iter().any(|s| s.path.is_none()) {
        return Err(MobaXtermImportError::Section);
    }
    Ok(sections)
}

fn folder_for(
    path: &str,
    folders: &mut Vec<ProfileFolder>,
    ids: &mut BTreeMap<Vec<String>, FolderId>,
) -> Result<Option<FolderId>, MobaXtermImportError> {
    if path.is_empty() {
        return Ok(None);
    }
    let mut parent = None;
    let mut identity = Vec::new();
    for part in path.split('\\') {
        if identity.len() >= MAX_DEPTH {
            return Err(MobaXtermImportError::Limit);
        }
        if part.is_empty()
            || part.trim() != part
            || part.len() > 128
            || matches!(part, "." | "..")
            || part.contains(['/', ':'])
        {
            return Err(MobaXtermImportError::InvalidTarget);
        }
        identity.push(part.to_lowercase());
        let id = if let Some(id) = ids.get(&identity) {
            if !folders.iter().any(|f| f.id == *id && f.name == part) {
                return Err(MobaXtermImportError::Duplicate);
            }
            *id
        } else {
            if folders.len() >= MAX_FOLDERS {
                return Err(MobaXtermImportError::Limit);
            }
            let id = FolderId::new();
            folders.push(ProfileFolder {
                id,
                name: part.into(),
                parent_id: parent,
                terminal: TerminalOverrides::default(),
            });
            ids.insert(identity.clone(), id);
            id
        };
        parent = Some(id);
    }
    Ok(parent)
}

fn split_packed(
    text: &str,
    delimiter: char,
    max: usize,
    line: usize,
) -> Result<Vec<&str>, MobaXtermImportError> {
    let fields = text.split(delimiter).take(max + 1).collect::<Vec<_>>();
    if fields.len() > max {
        return Err(MobaXtermImportError::Layout { line });
    }
    Ok(fields)
}
fn boolean(value: &str) -> bool {
    matches!(value, "0" | "-1")
}
fn rgb(value: &str) -> bool {
    let mut parts = value.split(',');
    (0..3).all(|_| parts.next().and_then(decimal).is_some_and(|n| n <= 255))
        && parts.next().is_none()
}

struct Target<'a> {
    host: &'a str,
    port: u16,
    username: &'a str,
}
fn ssh_target<'a>(
    groups: &[&'a str],
    net: &[&'a str],
    default_username: &'a str,
    line: usize,
    summary: &mut MobaXtermImportSummary,
) -> Result<Target<'a>, MobaXtermImportError> {
    if !matches!(net.len(), 26 | 27 | 35) {
        return Err(MobaXtermImportError::Layout { line });
    }
    let term = split_packed(groups[3], '%', 43, line)?;
    let extra = match term.len() {
        25..=28
            if term[16]
                .strip_prefix("_Std_Colors_")
                .and_then(|v| v.strip_suffix('_'))
                .and_then(decimal)
                .is_some() =>
        {
            0
        }
        40..=43 if term[16..32].iter().all(|v| rgb(v)) => 15,
        _ => return Err(MobaXtermImportError::Layout { line }),
    };
    if !net[4].is_empty()
        || net[18] != "0"
        || term[3] != "0"
        || term[15] != "0"
        || ![6, 12, 16, 17, 24].iter().all(|i| boolean(net[*i]))
        || !matches!(net[25], "0" | "1" | "2" | "3")
        || decimal(net[21]).is_none()
        || (net.len() == 35 && ![32, 33].iter().all(|i| boolean(net[*i])))
    {
        return Err(MobaXtermImportError::Layout { line });
    }
    if net[5] != "0"
        || !net[7].is_empty()
        || !net[8].is_empty()
        || !matches!(net[9], "" | "22")
        || !net[10].is_empty()
        || net[11] != "0"
        || net[13] != "0"
        || !net[15].is_empty()
        || net[19] != "0"
        || !net[20].is_empty()
        || !net[22].is_empty()
        || net[23] != "0"
        || net.get(26).is_some_and(|v| !v.is_empty())
        || (net.len() == 35 && (!matches!(net[27], "0" | "1") || net[31] != "0" || net[34] != "0"))
        || !matches!(term[22 + extra], "" | "<none>")
        || !term[23 + extra].is_empty()
    {
        return Err(MobaXtermImportError::Behavior { line });
    }
    let host = net[1];
    if host.is_empty()
        || host.contains(['@', '/', '\\', '#', '[', ']'])
        || host.contains("__")
        || (host.contains(':') && host.parse::<std::net::Ipv6Addr>().is_err())
    {
        return Err(MobaXtermImportError::InvalidTarget);
    }
    let port = decimal(net[2])
        .filter(|n| *n != 0)
        .ok_or(MobaXtermImportError::InvalidTarget)?;
    let username = if net[12] == "-1" || net[3].is_empty() || net[3] == "<default>" {
        default_username
    } else {
        net[3]
    };
    if username.is_empty() {
        return Err(MobaXtermImportError::MissingUser { line });
    }
    // Named credentials and encoded separators are never treated as accounts.
    if username.contains(['<', '>', '#', '%', '[', ']'])
        || username.starts_with('@')
        || username.contains("__")
    {
        return Err(MobaXtermImportError::InvalidTarget);
    }
    summary.packed_fields_omitted += net.len() - 4 + term.len() + groups.len() - 2;
    summary.terminal_fields_omitted += term.len();
    summary.private_key_references_omitted += usize::from(!net[14].is_empty());
    summary.comments_omitted += usize::from(!groups[5].trim().is_empty());
    Ok(Target {
        host,
        port,
        username,
    })
}

pub fn import_mobaxterm_sessions(
    bytes: &[u8],
    options: MobaXtermImportOptions<'_>,
) -> Result<ForeignProfileImportResult, MobaXtermImportError> {
    let content = decode(bytes, options.encoding)?;
    let sections = parse(&content)?;
    let mut summary = MobaXtermImportSummary {
        format: "cshell.mobaxterm-metadata-summary".into(),
        source_encoding: options.encoding,
        sections_read: sections.len(),
        sessions_read: 0,
        imported: 0,
        other_protocols_skipped: 0,
        folders_created: 0,
        packed_fields_omitted: 0,
        terminal_fields_omitted: 0,
        private_key_references_omitted: 0,
        comments_omitted: 0,
        folder_icons_omitted: 0,
    };
    let mut document = ProfileImportDocument {
        format: PROFILE_IMPORT_FORMAT.into(),
        version: PROFILE_CONNECTION_IMPORT_VERSION,
        folders: Vec::new(),
        profiles: Vec::new(),
        ssh_connections: Vec::new(),
        local_connections: Vec::new(),
    };
    let mut folder_ids = BTreeMap::new();
    let mut section_paths = BTreeSet::new();
    for section in sections {
        let path = section.path.ok_or(MobaXtermImportError::Section)?;
        if !section_paths.insert(path.to_lowercase()) {
            return Err(MobaXtermImportError::Duplicate);
        }
        let folder_id = folder_for(path, &mut document.folders, &mut folder_ids)?;
        summary.folder_icons_omitted += usize::from(section.icon.is_some());
        for (name, value, line) in section.entries {
            summary.sessions_read += 1;
            let groups = split_packed(value.trim_start_matches([' ', '\t']), '#', 7, line)?;
            if !matches!(groups.len(), 6 | 7)
                || decimal(groups[1]).is_none()
                || !matches!(groups[0], "" | "; logout")
                || !matches!(groups[4], "0" | "1" | "2" | "3")
                || (groups.len() == 7 && groups[6] != "-1" && groups[6].parse::<u32>().is_err())
            {
                return Err(MobaXtermImportError::Layout { line });
            }
            let net = split_packed(groups[2], '%', 64, line)?;
            let kind = net
                .first()
                .and_then(|v| decimal(v))
                .ok_or(MobaXtermImportError::Layout { line })?;
            if kind != 0 {
                summary.other_protocols_skipped += 1;
                continue;
            }
            let target = ssh_target(&groups, &net, options.default_username, line, &mut summary)?;
            let id = ProfileId::new();
            document.profiles.push(ProfileRecord {
                id,
                name: name.into(),
                kind: ProfileKind::Ssh,
                folder_id,
                tags: BTreeSet::from(["import:mobaxterm".into()]),
                favorite: false,
                terminal: TerminalOverrides {
                    logging: Some(false),
                    ..Default::default()
                },
            });
            document.ssh_connections.push(SshConnectionRecord {
                options: Default::default(),
                profile_id: id,
                host: target.host.into(),
                port: target.port,
                username: target.username.into(),
                auth_method: SshAuthMethod::Password,
                private_key_path: None,
                certificate_path: None,
                agent_backend: SshAgentBackend::Auto,
                agent_identity: None,
                route: SshRoute::Direct,
            });
            summary.imported += 1;
        }
    }
    if summary.imported == 0 {
        return Err(MobaXtermImportError::NoSessions);
    }
    summary.folders_created = document.folders.len();
    ProfileCatalog::from_snapshot(CatalogSnapshot {
        folders: document.folders.clone(),
        profiles: document.profiles.clone(),
        ssh_connections: document.ssh_connections.clone(),
        ..Default::default()
    })
    .map_err(|_| MobaXtermImportError::InvalidTarget)?;
    document
        .fingerprint()
        .map_err(|_| MobaXtermImportError::InvalidTarget)?;
    if !summary.validate_document(&document) {
        return Err(MobaXtermImportError::InvalidTarget);
    }
    Ok(ForeignProfileImportResult {
        document,
        summary: ForeignProfileImportSummary::MobaXterm(summary),
    })
}
