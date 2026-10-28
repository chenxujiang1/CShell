//! Explicit one-way synchronization. Metadata matches do not prove equal content.
use crate::SftpQueueDirection;
use cshell_domain::{SessionId, SftpVerification};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpSyncOptions {
    pub local: String,
    pub remote: String,
    pub direction: SftpQueueDirection,
    pub verification: SftpVerification,
    #[serde(default = "default_content_comparison")]
    pub compare_content: bool,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub preserve_modified: bool,
}
pub fn valid_sftp_exclusions(rules: &[String]) -> bool {
    rules.len() <= 16
        && rules.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 128
                && !p.starts_with('/')
                && !p.contains('\\')
                && !p.chars().any(char::is_control)
                && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
        })
}
pub fn sftp_excluded(path: &str, rules: &[String]) -> bool {
    fn glob(pattern: &str, value: &str) -> bool {
        let p = pattern.chars().collect::<Vec<_>>();
        let v = value.chars().collect::<Vec<_>>();
        let (mut a, mut b, mut star, mut retry) = (0, 0, None, 0);
        while b < v.len() {
            if a < p.len() && (p[a] == '?' || p[a] == v[b]) {
                a += 1;
                b += 1;
            } else if a < p.len() && p[a] == '*' {
                star = Some(a);
                a += 1;
                retry = b;
            } else if let Some(s) = star {
                retry += 1;
                b = retry;
                a = s + 1;
            } else {
                return false;
            }
        }
        while a < p.len() && p[a] == '*' {
            a += 1;
        }
        a == p.len()
    }
    rules.iter().any(|pattern| {
        if !pattern.contains('/') {
            return path.split('/').any(|part| glob(pattern, part));
        }
        let p = pattern.split('/').collect::<Vec<_>>();
        let v = path.split('/').collect::<Vec<_>>();
        let mut states = vec![false; v.len() + 1];
        states[0] = true;
        for part in p {
            if part == "**" {
                for index in 1..states.len() {
                    states[index] |= states[index - 1];
                }
            } else {
                for index in (1..states.len()).rev() {
                    states[index] = states[index - 1] && glob(part, v[index - 1]);
                }
                states[0] = false;
            }
        }
        states[v.len()]
    })
}
fn default_content_comparison() -> bool {
    true
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpSyncMetadata {
    pub directory: bool,
    pub size: u64,
    pub modified: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SftpSyncDifference {
    LocalOnly,
    RemoteOnly,
    Changed,
    MetadataMatch,
    ContentMatch,
    ContentChanged,
    TypeConflict,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpSyncEntry {
    pub relative: String,
    pub local: Option<SftpSyncMetadata>,
    pub remote: Option<SftpSyncMetadata>,
    pub difference: SftpSyncDifference,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpSyncPreview {
    pub id: SessionId,
    pub session: SessionId,
    pub options: SftpSyncOptions,
    pub entries: Vec<SftpSyncEntry>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpSyncApply {
    pub selected: Vec<String>,
    pub backup_replace: bool,
}
pub fn sftp_sync_entries(
    local: &std::collections::BTreeMap<String, SftpSyncMetadata>,
    remote: &std::collections::BTreeMap<String, SftpSyncMetadata>,
) -> Vec<SftpSyncEntry> {
    use SftpSyncDifference::*;
    let keys = local
        .keys()
        .chain(remote.keys())
        .collect::<std::collections::BTreeSet<_>>();
    keys.into_iter()
        .map(|key| {
            let a = local.get(key).cloned();
            let b = remote.get(key).cloned();
            let difference = match (&a, &b) {
                (Some(a), Some(b)) if a.directory != b.directory => TypeConflict,
                (Some(a), Some(b)) if a.directory || a == b && a.modified.is_some() => {
                    MetadataMatch
                }
                (Some(_), Some(_)) => Changed,
                (Some(_), None) => LocalOnly,
                _ => RemoteOnly,
            };
            SftpSyncEntry {
                relative: key.clone(),
                local: a,
                remote: b,
                difference,
            }
        })
        .collect()
}
impl SftpSyncEntry {
    pub fn eligible(&self, direction: SftpQueueDirection, replace: bool) -> bool {
        use SftpSyncDifference::*;
        matches!(
            (self.difference, direction),
            (LocalOnly, SftpQueueDirection::Upload) | (RemoteOnly, SftpQueueDirection::Download)
        ) || replace && matches!(self.difference, Changed | ContentChanged)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn exclusions_match_components_and_recursive_paths_without_escaping_root() {
        let rules = [".git/**", "*.tmp", "src/**/cache", "build?.log"].map(str::to_owned);
        assert!(valid_sftp_exclusions(&rules));
        for path in [
            ".git",
            ".git/objects/a",
            "nested/file.tmp",
            "src/cache",
            "src/a/b/cache",
            "build1.log",
        ] {
            assert!(sftp_excluded(path, &rules), "{path}");
        }
        for path in [
            ".github/workflow.yml",
            "nested/file.tmp.txt",
            "other/src/cache",
            "build10.log",
            "src/a/source.rs",
        ] {
            assert!(!sftp_excluded(path, &rules), "{path}");
        }
        for invalid in ["/etc", "../outside", "a/../b", "a//b", "a\\b", "a\nb"] {
            assert!(!valid_sftp_exclusions(&[invalid.into()]));
        }
        assert!(!valid_sftp_exclusions(&vec!["*.tmp".into(); 17]));
    }
    #[test]
    fn content_matches_are_never_overwritten_and_old_options_enable_bounded_comparison() {
        let mut value = serde_json::to_value(SftpSyncOptions {
            local: "C:/example".into(),
            remote: "/srv".into(),
            direction: SftpQueueDirection::Upload,
            verification: Default::default(),
            compare_content: false,
            excludes: Vec::new(),
            preserve_modified: false,
        })
        .unwrap();
        value.as_object_mut().unwrap().remove("compare_content");
        let options: SftpSyncOptions = serde_json::from_value(value).unwrap();
        assert!(options.compare_content);
        let mut entry = SftpSyncEntry {
            relative: "file".into(),
            local: None,
            remote: None,
            difference: SftpSyncDifference::ContentMatch,
        };
        assert!(!entry.eligible(SftpQueueDirection::Upload, true));
        entry.difference = SftpSyncDifference::ContentChanged;
        assert!(!entry.eligible(SftpQueueDirection::Upload, false));
        assert!(entry.eligible(SftpQueueDirection::Download, true));
    }
    #[test]
    fn comparison_distinguishes_metadata_from_content_and_never_selects_deletions() {
        let file = SftpSyncMetadata {
            directory: false,
            size: 12,
            modified: Some(1),
        };
        let mut local = BTreeMap::new();
        local.insert("same".into(), file.clone());
        local.insert("new".into(), file.clone());
        local.insert("conflict".into(), file.clone());
        let mut remote = BTreeMap::new();
        remote.insert("same".into(), file.clone());
        remote.insert("remote-only".into(), file.clone());
        remote.insert(
            "conflict".into(),
            SftpSyncMetadata {
                directory: true,
                ..file
            },
        );
        let result = sftp_sync_entries(&local, &remote);
        assert_eq!(
            result
                .iter()
                .find(|v| v.relative == "same")
                .unwrap()
                .difference,
            SftpSyncDifference::MetadataMatch
        );
        assert_eq!(
            result
                .iter()
                .filter(|v| v.eligible(SftpQueueDirection::Upload, false))
                .count(),
            1
        );
        assert!(
            !result
                .iter()
                .find(|v| v.relative == "conflict")
                .unwrap()
                .eligible(SftpQueueDirection::Upload, true)
        );
    }
}
