//! Bounded selector parser; errors contain byte positions, never the query body.
use cshell_domain::{HostGroupId, HostId, HostPlatformHint, HostRecord, ProfileId};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use thiserror::Error;

pub const MAX_HOST_SELECTOR_BYTES: usize = 4096;
const MAX_TOKENS: usize = 256;
const MAX_NODES: usize = 128;
const MAX_DEPTH: usize = 32;
// A label predicate can contain a 64-byte key, '=', and a 256-byte value.
const MAX_LITERAL_BYTES: usize = 321;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("invalid host selector at byte {offset}: {kind:?}")]
pub struct HostSelectorError {
    pub offset: usize,
    pub kind: HostSelectorErrorKind,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostSelectorErrorKind {
    Syntax,
    UnknownField,
    InvalidValue,
    Limit,
}
fn error(offset: usize, kind: HostSelectorErrorKind) -> HostSelectorError {
    HostSelectorError { offset, kind }
}
#[derive(Clone)]
enum TokenKind {
    Word(String),
    Quoted(String),
    And,
    Or,
    Not,
    Left,
    Right,
}
#[derive(Clone)]
struct Token {
    kind: TokenKind,
    at: usize,
}
#[derive(Clone)]
enum Predicate {
    All,
    None,
    Tag(String),
    Label(String, String),
    Platform(HostPlatformHint),
    Name(String),
    Address(String),
    Host(HostId),
    Profile(ProfileId),
    Group(HostGroupId),
}
#[derive(Clone)]
enum Node {
    Predicate(Predicate),
    Not(usize),
    And(usize, usize),
    Or(usize, usize),
}
#[derive(Clone)]
pub struct HostSelector {
    nodes: Vec<Node>,
    root: usize,
}
impl fmt::Debug for HostSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostSelector")
            .field("nodes", &self.nodes.len())
            .field("query", &"[REDACTED]")
            .finish()
    }
}
impl HostSelector {
    pub fn parse(query: &str) -> Result<Self, HostSelectorError> {
        if query.is_empty() {
            return Err(error(0, HostSelectorErrorKind::Syntax));
        }
        if query.len() > MAX_HOST_SELECTOR_BYTES {
            return Err(error(0, HostSelectorErrorKind::Limit));
        }
        let tokens = lex(query)?;
        let mut parser = Parser {
            tokens,
            position: 0,
            nodes: Vec::new(),
            end: query.len(),
        };
        let root = parser.or(0)?;
        if parser.position != parser.tokens.len() {
            return Err(parser.failure(HostSelectorErrorKind::Syntax));
        }
        Ok(Self {
            nodes: parser.nodes,
            root,
        })
    }
    #[must_use]
    pub fn group_references(&self) -> BTreeSet<HostGroupId> {
        self.nodes
            .iter()
            .filter_map(|node| match node {
                Node::Predicate(Predicate::Group(id)) => Some(*id),
                _ => None,
            })
            .collect()
    }
    #[must_use]
    pub fn matches(
        &self,
        host: &HostRecord,
        groups: &BTreeMap<HostGroupId, BTreeSet<HostId>>,
    ) -> bool {
        // Arena order is topological, so evaluation cannot recurse on hostile input.
        let mut values: Vec<bool> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let value = match node {
                Node::Not(child) => !values[*child],
                Node::And(left, right) => values[*left] && values[*right],
                Node::Or(left, right) => values[*left] || values[*right],
                Node::Predicate(predicate) => match predicate {
                    Predicate::All => true,
                    Predicate::None => false,
                    Predicate::Tag(tag) => host.tags.contains(tag),
                    Predicate::Label(key, value) => host.labels.get(key) == Some(value),
                    Predicate::Platform(platform) => host.platform_hint == *platform,
                    Predicate::Name(name) => host.name == *name,
                    Predicate::Address(address) => host.address == *address,
                    Predicate::Host(id) => host.id == *id,
                    Predicate::Profile(id) => host.profile_id == *id,
                    Predicate::Group(id) => groups
                        .get(id)
                        .is_some_and(|members| members.contains(&host.id)),
                },
            };
            values.push(value);
        }
        values[self.root]
    }
}
fn lex(query: &str) -> Result<Vec<Token>, HostSelectorError> {
    let mut tokens = Vec::new();
    let mut chars = query.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        let kind = match ch {
            '(' => TokenKind::Left,
            ')' => TokenKind::Right,
            '!' => TokenKind::Not,
            '&' | '|' => {
                if chars.next().is_none_or(|(_, next)| next != ch) {
                    return Err(error(at, HostSelectorErrorKind::Syntax));
                }
                if ch == '&' {
                    TokenKind::And
                } else {
                    TokenKind::Or
                }
            }
            '"' => {
                let mut value = String::new();
                let mut closed = false;
                while let Some((position, next)) = chars.next() {
                    if next == '"' {
                        closed = true;
                        break;
                    }
                    if next == '\\' {
                        let Some((_, escaped)) = chars.next() else {
                            return Err(error(position, HostSelectorErrorKind::Syntax));
                        };
                        if !matches!(escaped, '\\' | '"') {
                            return Err(error(position, HostSelectorErrorKind::Syntax));
                        }
                        value.push(escaped);
                    } else {
                        if next.is_control() {
                            return Err(error(position, HostSelectorErrorKind::InvalidValue));
                        }
                        value.push(next);
                    }
                    if value.len() > MAX_LITERAL_BYTES {
                        return Err(error(at, HostSelectorErrorKind::Limit));
                    }
                }
                if !closed {
                    return Err(error(at, HostSelectorErrorKind::Syntax));
                }
                TokenKind::Quoted(value)
            }
            value => {
                if value.is_control() {
                    return Err(error(at, HostSelectorErrorKind::InvalidValue));
                }
                let mut word = String::from(value);
                while let Some(&(_, next)) = chars.peek() {
                    if next.is_whitespace() || "()!&|\"".contains(next) {
                        break;
                    }
                    if next.is_control() {
                        return Err(error(at, HostSelectorErrorKind::InvalidValue));
                    }
                    word.push(next);
                    chars.next();
                    if word.len() > MAX_LITERAL_BYTES + 16 {
                        return Err(error(at, HostSelectorErrorKind::Limit));
                    }
                }
                match word.to_ascii_uppercase().as_str() {
                    "AND" => TokenKind::And,
                    "OR" => TokenKind::Or,
                    "NOT" => TokenKind::Not,
                    _ => TokenKind::Word(word),
                }
            }
        };
        tokens.push(Token { kind, at });
        if tokens.len() > MAX_TOKENS {
            return Err(error(at, HostSelectorErrorKind::Limit));
        }
    }
    Ok(tokens)
}
struct Parser {
    tokens: Vec<Token>,
    position: usize,
    nodes: Vec<Node>,
    end: usize,
}
impl Parser {
    fn failure(&self, kind: HostSelectorErrorKind) -> HostSelectorError {
        error(
            self.tokens
                .get(self.position)
                .map_or(self.end, |token| token.at),
            kind,
        )
    }
    fn node(&mut self, node: Node) -> Result<usize, HostSelectorError> {
        if self.nodes.len() >= MAX_NODES {
            return Err(self.failure(HostSelectorErrorKind::Limit));
        }
        let id = self.nodes.len();
        self.nodes.push(node);
        Ok(id)
    }
    fn or(&mut self, depth: usize) -> Result<usize, HostSelectorError> {
        let mut left = self.and(depth)?;
        while self
            .tokens
            .get(self.position)
            .is_some_and(|token| matches!(token.kind, TokenKind::Or))
        {
            self.position += 1;
            let right = self.and(depth)?;
            left = self.node(Node::Or(left, right))?;
        }
        Ok(left)
    }
    fn and(&mut self, depth: usize) -> Result<usize, HostSelectorError> {
        let mut left = self.unary(depth)?;
        while self
            .tokens
            .get(self.position)
            .is_some_and(|token| matches!(token.kind, TokenKind::And))
        {
            self.position += 1;
            let right = self.unary(depth)?;
            left = self.node(Node::And(left, right))?;
        }
        Ok(left)
    }
    fn unary(&mut self, depth: usize) -> Result<usize, HostSelectorError> {
        if depth > MAX_DEPTH {
            return Err(self.failure(HostSelectorErrorKind::Limit));
        }
        let token = self
            .tokens
            .get(self.position)
            .cloned()
            .ok_or_else(|| self.failure(HostSelectorErrorKind::Syntax))?;
        self.position += 1;
        match token.kind {
            TokenKind::Not => {
                let child = self.unary(depth + 1)?;
                self.node(Node::Not(child))
            }
            TokenKind::Left => {
                let node = self.or(depth + 1)?;
                if !self
                    .tokens
                    .get(self.position)
                    .is_some_and(|token| matches!(token.kind, TokenKind::Right))
                {
                    return Err(self.failure(HostSelectorErrorKind::Syntax));
                }
                self.position += 1;
                Ok(node)
            }
            TokenKind::Word(word) => self.predicate(token.at, &word),
            _ => Err(error(token.at, HostSelectorErrorKind::Syntax)),
        }
    }
    fn predicate(&mut self, at: usize, word: &str) -> Result<usize, HostSelectorError> {
        let predicate = match word.to_ascii_lowercase().as_str() {
            "all" => Predicate::All,
            "none" => Predicate::None,
            _ => {
                let (field, inline) = word
                    .split_once(':')
                    .ok_or_else(|| error(at, HostSelectorErrorKind::Syntax))?;
                let value = if inline.is_empty() {
                    let Some(Token {
                        kind: TokenKind::Quoted(value),
                        ..
                    }) = self.tokens.get(self.position)
                    else {
                        return Err(self.failure(HostSelectorErrorKind::InvalidValue));
                    };
                    self.position += 1;
                    value.clone()
                } else {
                    inline.to_owned()
                };
                if value.is_empty()
                    || value.len() > MAX_LITERAL_BYTES
                    || value.chars().any(char::is_control)
                {
                    return Err(error(at, HostSelectorErrorKind::InvalidValue));
                }
                let invalid = || error(at, HostSelectorErrorKind::InvalidValue);
                match field {
                    "tag" => Predicate::Tag(value),
                    "env" => Predicate::Label("env".into(), value),
                    "label" => {
                        let (key, label) = value.split_once('=').ok_or_else(invalid)?;
                        if key.is_empty() || label.is_empty() {
                            return Err(invalid());
                        }
                        Predicate::Label(key.into(), label.into())
                    }
                    "platform" => Predicate::Platform(match value.as_str() {
                        "unknown" => HostPlatformHint::Unknown,
                        "posix" => HostPlatformHint::Posix,
                        "linux" => HostPlatformHint::Linux,
                        "macos" => HostPlatformHint::Macos,
                        "windows" => HostPlatformHint::Windows,
                        _ => return Err(invalid()),
                    }),
                    "name" => Predicate::Name(value),
                    "address" => Predicate::Address(value),
                    "host" => {
                        Predicate::Host(HostId::from_uuid(value.parse().map_err(|_| invalid())?))
                    }
                    "profile" => Predicate::Profile(ProfileId::from_uuid(
                        value.parse().map_err(|_| invalid())?,
                    )),
                    "group" => Predicate::Group(HostGroupId::from_uuid(
                        value.parse().map_err(|_| invalid())?,
                    )),
                    _ => return Err(error(at, HostSelectorErrorKind::UnknownField)),
                }
            }
        };
        self.node(Node::Predicate(predicate))
    }
}
