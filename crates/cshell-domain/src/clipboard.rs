use serde::{Deserialize, Serialize};

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardHost {
    pub host: String,
    pub port: u16,
}
impl std::fmt::Debug for ClipboardHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardHost")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl ClipboardHost {
    #[must_use]
    pub fn new(host: &str, port: u16) -> Option<Self> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty()
            || host.len() > 255
            || port == 0
            || host.chars().any(|c| {
                c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\' | '[' | ']' | '@')
            })
        {
            return None;
        }
        Some(Self { host, port })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_is_canonical_and_ports_remain_distinct() {
        assert_eq!(
            ClipboardHost::new("EXAMPLE.test.", 22),
            ClipboardHost::new("example.test", 22)
        );
        assert_ne!(
            ClipboardHost::new("example.test", 22),
            ClipboardHost::new("example.test", 2222)
        );
        for host in ["", ".", "a b", "a\n", "a/b", "user@host", "[::1]"] {
            assert!(ClipboardHost::new(host, 22).is_none());
        }
        assert!(ClipboardHost::new("::1", 22).is_some());
        assert!(ClipboardHost::new("host", 0).is_none());
    }
}
