//! Explicit replacement of raw endpoint pins, preserving all other trust rules.
use super::*;
use sha2::{Digest, Sha256};
use std::io::Read;

#[derive(Clone)]
pub struct HostKeyUpdate {
    pub previous_fingerprints: Vec<String>,
    digest: [u8; 32],
    host: String,
    port: u16,
}

impl std::fmt::Debug for HostKeyUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostKeyUpdate")
            .field("previous_keys", &self.previous_fingerprints.len())
            .finish_non_exhaustive()
    }
}

fn read_regular(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("known_hosts must be a regular file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_KNOWN_HOSTS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_KNOWN_HOSTS_BYTES {
        return Err("known_hosts exceeds the 8 MiB limit".into());
    }
    Ok(bytes)
}

pub fn preview_host_key_update(
    path: &Path,
    host: &str,
    port: u16,
    new_key: &str,
) -> Result<HostKeyUpdate, String> {
    let bytes = read_regular(path)?;
    let contents = std::str::from_utf8(&bytes).map_err(|_| "known_hosts is not UTF-8")?;
    let verifier = KnownHostsVerifier::parse(contents, host, port).map_err(|e| e.to_string())?;
    if verifier.has_authority_entries() {
        return Err("CA-managed host trust cannot be replaced with a raw key".into());
    }
    if verifier.public_key_revoked(new_key)? {
        return Err("server host key is revoked; update blocked".into());
    }
    let key = new_key.parse::<PublicKey>().map_err(|e| e.to_string())?;
    if verifier.check(&PublicKeyOrCertificate::from(key)) == HostKeyCheck::Trusted {
        return Err("host already has a known_hosts entry; first-key import is blocked".into());
    }
    let mut previous_fingerprints = verifier
        .entries
        .iter()
        .filter(|e| e.marker.is_none())
        .map(|e| {
            format!(
                "{} {}",
                e.key.algorithm(),
                e.key.fingerprint(HashAlg::Sha256)
            )
        })
        .collect::<Vec<_>>();
    previous_fingerprints.sort();
    previous_fingerprints.dedup();
    if previous_fingerprints.is_empty() {
        return Err("no previous raw host key; use first-key confirmation".into());
    }
    if previous_fingerprints.len() > 64 {
        return Err("too many previous host keys to confirm".into());
    }
    Ok(HostKeyUpdate {
        previous_fingerprints,
        digest: Sha256::digest(&bytes).into(),
        host: host.to_ascii_lowercase(),
        port,
    })
}

pub fn update_confirmed_host_key(
    path: &Path,
    host: &str,
    port: u16,
    public_key_line: &str,
    confirmed_fingerprint: &str,
    profile_id: &str,
    preview: &HostKeyUpdate,
) -> Result<(), String> {
    if preview.host != host.to_ascii_lowercase() || preview.port != port {
        return Err("host-key update belongs to another endpoint".into());
    }
    if host.is_empty()
        || host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "#,*!?|".contains(c))
        || profile_id.is_empty()
        || !profile_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return Err("invalid host or Profile identifier for known_hosts".into());
    }
    let key = public_key_line
        .parse::<PublicKey>()
        .map_err(|e| e.to_string())?;
    if key.fingerprint(HashAlg::Sha256).to_string() != confirmed_fingerprint {
        return Err("confirmed fingerprint does not match the scanned host key".into());
    }
    let parent = path.parent().ok_or("known_hosts has no parent directory")?;
    let lock_path = path.with_file_name(format!(
        "{}.cshell.lock",
        path.file_name()
            .ok_or("known_hosts has no file name")?
            .to_string_lossy()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(lock_path).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let existing = read_regular(path)?;
    if <[u8; 32]>::from(Sha256::digest(&existing)) != preview.digest {
        return Err("known_hosts changed after preview; preview again".into());
    }
    let current = preview_host_key_update(path, host, port, public_key_line)?;
    if current.digest != preview.digest
        || current.previous_fingerprints != preview.previous_fingerprints
    {
        return Err("host trust changed after preview; preview again".into());
    }
    let lookup = if port == 22 {
        host.to_ascii_lowercase()
    } else {
        format!("[{}]:{port}", host.to_ascii_lowercase())
    };
    let contents = std::str::from_utf8(&existing).map_err(|_| "known_hosts is not UTF-8")?;
    let mut output = String::new();
    for line in contents.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(['#', '@']) {
            output.push_str(line);
            continue;
        }
        let pattern = trimmed
            .split_whitespace()
            .next()
            .ok_or("invalid known_hosts entry")?;
        let patterns = pattern
            .parse::<HostPatterns>()
            .map_err(|_| "invalid host patterns")?;
        if !host_matches(&patterns, &lookup) {
            output.push_str(line);
            continue;
        }
        // A hashed name represents only this endpoint. For aliases and globs,
        // exclude only this endpoint and retain their trust for all other hosts.
        if let HostPatterns::Patterns(items) = patterns {
            let mut retained = items
                .into_iter()
                .filter(|p| !p.eq_ignore_ascii_case(&lookup))
                .collect::<Vec<_>>();
            if retained.iter().any(|p| !p.starts_with('!')) {
                retained.insert(0, format!("!{lookup}"));
                let start = line.len() - line.trim_start().len();
                output.push_str(&line[..start]);
                output.push_str(&retained.join(","));
                output.push_str(&line[start + pattern.len()..]);
            }
        }
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    output.push_str(&format!("# CShell host-key-update v1 utc_unix={timestamp} profile={profile_id} previous={} fingerprint={confirmed_fingerprint}\n{lookup} {}\n", preview.previous_fingerprints.join(","), key.to_openssh().map_err(|e| e.to_string())?));
    if output.len() as u64 > MAX_KNOWN_HOSTS_BYTES {
        return Err("known_hosts exceeds the 8 MiB limit".into());
    }
    let checked = KnownHostsVerifier::parse(&output, host, port).map_err(|e| e.to_string())?;
    if checked.check(&PublicKeyOrCertificate::from(key)) != HostKeyCheck::Trusted {
        return Err("updated host key did not pass verification".into());
    }
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    staged
        .as_file()
        .set_permissions(
            std::fs::metadata(path)
                .map_err(|e| e.to_string())?
                .permissions(),
        )
        .map_err(|e| e.to_string())?;
    staged
        .write_all(output.as_bytes())
        .map_err(|e| e.to_string())?;
    staged.as_file().sync_all().map_err(|e| e.to_string())?;
    if read_regular(path)? != existing {
        return Err("known_hosts changed while saving; preview again".into());
    }
    staged.persist(path).map_err(|e| e.error.to_string())?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const PROFILE: &str = "12345678-1234-1234-1234-123456789abc";
    fn key() -> String {
        ssh_key::PrivateKey::random(&mut rand::rng(), ssh_key::Algorithm::Ed25519)
            .unwrap()
            .public_key()
            .to_openssh()
            .unwrap()
    }
    fn fingerprint(key: &str) -> String {
        key.parse::<PublicKey>()
            .unwrap()
            .fingerprint(HashAlg::Sha256)
            .to_string()
    }
    fn check(path: &Path, host: &str, port: u16, key: &str) -> HostKeyCheck {
        KnownHostsVerifier::load(path, host, port)
            .unwrap()
            .check(&PublicKeyOrCertificate::from(
                key.parse::<PublicKey>().unwrap(),
            ))
    }

    #[test]
    fn update_replaces_only_this_endpoint_preserving_aliases_globs_markers_and_comments() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("known_hosts");
        let (old, new, other) = (key(), key(), key());
        let preserved = format!(
            "# keep comment\n@revoked target.example {other}\n@cert-authority elsewhere.test {other}\n[target.example]:2200 {old}\n"
        );
        std::fs::write(&path, format!("{preserved}target.example,alias.example  {old} alias comment\n*.example,!blocked.example\t{old} wildcard comment\n")).unwrap();
        let preview = preview_host_key_update(&path, "target.example", 22, &new).unwrap();
        assert_eq!(preview.previous_fingerprints.len(), 1);
        update_confirmed_host_key(
            &path,
            "target.example",
            22,
            &new,
            &fingerprint(&new),
            PROFILE,
            &preview,
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(&preserved));
        assert!(
            text.contains("alias comment")
                && text.contains("wildcard comment")
                && text.contains("CShell host-key-update v1")
        );
        assert_eq!(
            check(&path, "target.example", 22, &new),
            HostKeyCheck::Trusted
        );
        assert_eq!(
            check(&path, "target.example", 22, &old),
            HostKeyCheck::Changed
        );
        assert_eq!(
            check(&path, "target.example", 2200, &old),
            HostKeyCheck::Trusted
        );
        assert_eq!(
            check(&path, "alias.example", 22, &old),
            HostKeyCheck::Trusted
        );
        assert_eq!(
            check(&path, "another.example", 22, &old),
            HostKeyCheck::Trusted
        );
        assert_eq!(
            check(&path, "blocked.example", 22, &old),
            HostKeyCheck::Unknown
        );
        assert_eq!(
            check(&path, "target.example", 22, &other),
            HostKeyCheck::Revoked
        );
    }

    #[test]
    fn update_handles_hashed_names_and_multiple_previous_keys() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("known_hosts");
        let (old, old2, new) = (key(), key(), key());
        std::fs::write(&path, format!("|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak= {old}\nexample.com {old2}\n")).unwrap();
        let preview = preview_host_key_update(&path, "example.com", 22, &new).unwrap();
        assert_eq!(preview.previous_fingerprints.len(), 2);
        update_confirmed_host_key(
            &path,
            "example.com",
            22,
            &new,
            &fingerprint(&new),
            PROFILE,
            &preview,
        )
        .unwrap();
        assert_eq!(check(&path, "example.com", 22, &new), HostKeyCheck::Trusted);
        assert_eq!(check(&path, "example.com", 22, &old), HostKeyCheck::Changed);
        assert_eq!(
            check(&path, "example.com", 22, &old2),
            HostKeyCheck::Changed
        );
    }

    #[test]
    fn stale_preview_wrong_fingerprint_or_endpoint_never_writes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("known_hosts");
        let (old, new) = (key(), key());
        let original = format!("[host.test]:2200 {old}\n");
        std::fs::write(&path, &original).unwrap();
        let preview = preview_host_key_update(&path, "host.test", 2200, &new).unwrap();
        for (host, port, fp) in [
            ("other.test", 2200, fingerprint(&new)),
            ("host.test", 22, fingerprint(&new)),
            ("host.test", 2200, "SHA256:wrong".into()),
        ] {
            assert!(
                update_confirmed_host_key(&path, host, port, &new, &fp, PROFILE, &preview).is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
        let changed = format!("{original}# external edit\n");
        std::fs::write(&path, &changed).unwrap();
        assert!(
            update_confirmed_host_key(
                &path,
                "host.test",
                2200,
                &new,
                &fingerprint(&new),
                PROFILE,
                &preview
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
    }

    #[test]
    fn revoked_or_ca_managed_keys_cannot_be_updated() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("known_hosts");
        let (old, new) = (key(), key());
        for marker in ["@revoked", "@cert-authority"] {
            let original = format!("host.test {old}\n{marker} host.test {new}\n");
            std::fs::write(&path, &original).unwrap();
            assert!(preview_host_key_update(&path, "host.test", 22, &new).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
        let verifier = KnownHostsVerifier::parse(&format!("host.test {old}\n"), "host.test", 22)
            .unwrap()
            .with_managed_authorities(&[crate::HostAuthorityRule {
                patterns: "host.test".into(),
                public_key: new.clone(),
                revoked: false,
            }])
            .unwrap();
        assert!(verifier.has_authority_entries());
        let verifier = KnownHostsVerifier::parse(&format!("host.test {old}\n"), "host.test", 22)
            .unwrap()
            .with_managed_authorities(&[crate::HostAuthorityRule {
                patterns: "host.test".into(),
                public_key: new.clone(),
                revoked: true,
            }])
            .unwrap();
        assert!(verifier.public_key_revoked(&new).unwrap());
    }

    #[test]
    fn concurrent_updates_only_one_preview_can_commit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("known_hosts");
        let (old, new) = (key(), key());
        std::fs::write(&path, format!("host.test {old}\n")).unwrap();
        let preview = preview_host_key_update(&path, "host.test", 22, &new).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let (path, new, preview, barrier) =
                    (path.clone(), new.clone(), preview.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    update_confirmed_host_key(
                        &path,
                        "host.test",
                        22,
                        &new,
                        &fingerprint(&new),
                        PROFILE,
                        &preview,
                    )
                    .is_ok()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            handles
                .into_iter()
                .filter_map(|h| h.join().ok())
                .filter(|ok| *ok)
                .count(),
            1
        );
        assert_eq!(check(&path, "host.test", 22, &new), HostKeyCheck::Trusted);
    }

    #[cfg(unix)]
    #[test]
    fn update_preserves_permissions_and_rejects_symlink_targets() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("known_hosts");
        let (old, new) = (key(), key());
        std::fs::write(&path, format!("host.test {old}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(preview_host_key_update(&link, "host.test", 22, &new).is_err());
        let preview = preview_host_key_update(&path, "host.test", 22, &new).unwrap();
        assert!(
            update_confirmed_host_key(
                &link,
                "host.test",
                22,
                &new,
                &fingerprint(&new),
                PROFILE,
                &preview
            )
            .is_err()
        );
        assert_eq!(check(&path, "host.test", 22, &old), HostKeyCheck::Trusted);
        update_confirmed_host_key(
            &path,
            "host.test",
            22,
            &new,
            &fingerprint(&new),
            PROFILE,
            &preview,
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
