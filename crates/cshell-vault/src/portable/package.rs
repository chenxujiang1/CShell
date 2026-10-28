//! Independent password-encrypted transfer format. No filesystem or active Vault state.
use super::*;

pub const MAX_CREDENTIAL_PACKAGE_BYTES: usize = 1024 * 1024;
pub const MAX_CREDENTIAL_PACKAGE_RECORDS: usize = 128;
pub const MAX_CREDENTIAL_PACKAGE_CONTEXT: usize = 256 * 1024;
const PACKAGE_MAGIC: &[u8; 8] = b"CSHCPK01";
const HEADER_BYTES: usize = 56;

pub struct CredentialPackageRecord {
    pub reference: VaultCredentialRef,
    binding: Zeroizing<String>,
    pub secret: Secret,
}
impl CredentialPackageRecord {
    pub fn new(reference: VaultCredentialRef, binding: String, secret: Secret) -> Self {
        Self {
            reference,
            binding: Zeroizing::new(binding),
            secret,
        }
    }
    pub fn binding(&self) -> &str {
        &self.binding
    }
}
impl std::fmt::Debug for CredentialPackageRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialPackageRecord")
            .field("kind", &self.reference.kind)
            .finish_non_exhaustive()
    }
}

pub struct CredentialPackage {
    context: Zeroizing<Vec<u8>>,
    records: Vec<CredentialPackageRecord>,
}
impl std::fmt::Debug for CredentialPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialPackage")
            .field("record_count", &self.records.len())
            .finish_non_exhaustive()
    }
}
impl CredentialPackage {
    pub fn context(&self) -> &[u8] {
        &self.context
    }
    pub fn records(&self) -> &[CredentialPackageRecord] {
        &self.records
    }

    /// Encrypt every field, including target metadata and the complete record set.
    pub fn seal(
        context: &[u8],
        records: &[CredentialPackageRecord],
        password: &Secret,
    ) -> Result<Vec<u8>, VaultError> {
        validate_password(password, true)?;
        if context.is_empty()
            || context.len() > MAX_CREDENTIAL_PACKAGE_CONTEXT
            || records.is_empty()
            || records.len() > MAX_CREDENTIAL_PACKAGE_RECORDS
        {
            return Err(VaultError::Capacity);
        }
        let mut length = 8 + context.len();
        let mut seen = std::collections::BTreeSet::new();
        for record in records {
            validate_binding(record.reference, record.binding())?;
            if matches!(record.reference.kind, VaultCredentialKind::ProfileMode | VaultCredentialKind::ManagedKey | VaultCredentialKind::VaultSettings)
                || !seen.insert(record.reference)
                || record.secret.expose().is_empty()
                || record.secret.expose().len() > record.reference.kind.maximum_bytes()
            {
                return Err(VaultError::InvalidInput);
            }
            length += 25 + record.binding().len() + record.secret.expose().len();
        }
        if length + HEADER_BYTES + TAG_BYTES > MAX_CREDENTIAL_PACKAGE_BYTES {
            return Err(VaultError::Capacity);
        }
        let salt: [u8; 16] = random()?;
        let nonce: [u8; 12] = random()?;
        let mut header = Vec::with_capacity(HEADER_BYTES);
        header.extend_from_slice(PACKAGE_MAGIC);
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(&salt);
        for value in [MEMORY_KIB, PASSES, LANES] {
            header.extend_from_slice(&value.to_be_bytes());
        }
        header.extend_from_slice(&nonce);
        push_length(&mut header, length + TAG_BYTES)?;
        let keys = derive(password, &salt)?;
        let mut plaintext = Zeroizing::new(Vec::with_capacity(length + TAG_BYTES));
        push_length(&mut plaintext, context.len())?;
        plaintext.extend_from_slice(context);
        push_length(&mut plaintext, records.len())?;
        for record in records {
            plaintext.extend_from_slice(&record.reference.profile_id);
            plaintext.push(record.reference.kind as u8);
            push_length(&mut plaintext, record.binding().len())?;
            plaintext.extend_from_slice(record.binding().as_bytes());
            push_length(&mut plaintext, record.secret.expose().len())?;
            plaintext.extend_from_slice(record.secret.expose());
        }
        let ciphertext = encrypt(&keys[..32], plaintext, &header, nonce)?;
        header.extend_from_slice(&ciphertext.ciphertext);
        Ok(header)
    }

    /// Authenticate the complete package before interpreting any target or credential.
    pub fn open(bytes: &[u8], password: &Secret) -> Result<Self, VaultError> {
        let (salt, record) = parse_header(bytes)?;
        validate_password(password, false)?;
        let keys = derive(password, &salt)?;
        let plaintext = decrypt(&keys[..32], &record, &bytes[..HEADER_BYTES])?;
        let mut reader = Reader::new(&plaintext);
        let context = reader.part(MAX_CREDENTIAL_PACKAGE_CONTEXT)?;
        if context.is_empty() {
            return Err(VaultError::Corrupt);
        }
        let context = Zeroizing::new(context.to_vec());
        let count = reader.u32()? as usize;
        if !(1..=MAX_CREDENTIAL_PACKAGE_RECORDS).contains(&count) {
            return Err(VaultError::Capacity);
        }
        let mut records = Vec::with_capacity(count);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..count {
            let profile_id = reader.array()?;
            let kind = match reader.take(1)?[0] {
                1 => VaultCredentialKind::Password,
                2 => VaultCredentialKind::KeyPassphrase,
                3 => VaultCredentialKind::PrivateKey,
                _ => return Err(VaultError::Corrupt),
            };
            let reference = VaultCredentialRef { profile_id, kind };
            let binding = std::str::from_utf8(reader.part(MAX_BINDING_BYTES)?)
                .map_err(|_| VaultError::Corrupt)?;
            validate_binding(reference, binding)?;
            let secret = reader.part(kind.maximum_bytes())?;
            if !seen.insert(reference) || secret.is_empty() {
                return Err(VaultError::Corrupt);
            }
            records.push(CredentialPackageRecord::new(
                reference,
                binding.to_owned(),
                Secret::new(secret.to_vec()),
            ));
        }
        if !reader.finished() {
            return Err(VaultError::Corrupt);
        }
        Ok(Self { context, records })
    }
}

fn parse_header(bytes: &[u8]) -> Result<([u8; 16], SealedRecord), VaultError> {
    if bytes.len() > MAX_CREDENTIAL_PACKAGE_BYTES {
        return Err(VaultError::Capacity);
    }
    let mut reader = Reader::new(bytes);
    if reader.take(8)? != PACKAGE_MAGIC {
        return Err(VaultError::Corrupt);
    }
    if reader.u32()? != 1 {
        return Err(VaultError::Unsupported);
    }
    let salt = reader.array()?;
    if [reader.u32()?, reader.u32()?, reader.u32()?] != [MEMORY_KIB, PASSES, LANES] {
        return Err(VaultError::Unsupported);
    }
    let nonce = reader.array()?;
    let ciphertext = reader.part(MAX_CREDENTIAL_PACKAGE_BYTES - HEADER_BYTES)?;
    if ciphertext.len() < TAG_BYTES || !reader.finished() {
        return Err(VaultError::Corrupt);
    }
    Ok((
        salt,
        SealedRecord {
            nonce,
            ciphertext: ciphertext.to_vec(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record() -> CredentialPackageRecord {
        CredentialPackageRecord::new(
            VaultCredentialRef {
                profile_id: [1; 16],
                kind: VaultCredentialKind::Password,
            },
            "BINDING_SENTINEL".into(),
            Secret::new(b"SECRET_SENTINEL".to_vec()),
        )
    }
    #[test]
    fn independent_package_roundtrip_hides_metadata_and_uses_fresh_encryption()
    -> Result<(), VaultError> {
        let password = Secret::new(b"independent-package-password".to_vec());
        let bytes = CredentialPackage::seal(b"CONTEXT_SENTINEL", &[record()], &password)?;
        let second = CredentialPackage::seal(b"CONTEXT_SENTINEL", &[record()], &password)?;
        assert_ne!(bytes, second);
        for marker in [
            b"CONTEXT_SENTINEL".as_slice(),
            b"BINDING_SENTINEL",
            b"SECRET_SENTINEL",
        ] {
            assert!(!bytes.windows(marker.len()).any(|part| part == marker));
        }
        let decoded = CredentialPackage::open(&bytes, &password)?;
        assert_eq!(decoded.context(), b"CONTEXT_SENTINEL");
        assert_eq!(decoded.records()[0].binding(), "BINDING_SENTINEL");
        assert_eq!(decoded.records()[0].secret.expose(), b"SECRET_SENTINEL");
        assert!(!format!("{decoded:?} {:?}", decoded.records()).contains("SENTINEL"));
        assert!(matches!(decode(&bytes), Err(VaultError::Corrupt)));
        Ok(())
    }
    #[test]
    fn package_authentication_and_hostile_headers_fail_closed() -> Result<(), VaultError> {
        let password = Secret::new(b"package-password".to_vec());
        let bytes = CredentialPackage::seal(b"context", &[record()], &password)?;
        assert!(matches!(
            CredentialPackage::open(&bytes, &Secret::new(b"wrong-password".to_vec())),
            Err(VaultError::AuthenticationFailed)
        ));
        let mut changed = bytes.clone();
        changed[28..32].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            CredentialPackage::open(&changed, &password),
            Err(VaultError::Unsupported)
        ));
        let mut changed = bytes.clone();
        changed[16] ^= 1;
        assert!(matches!(
            CredentialPackage::open(&changed, &password),
            Err(VaultError::AuthenticationFailed)
        ));
        let mut changed = bytes.clone();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        assert!(matches!(
            CredentialPackage::open(&changed, &password),
            Err(VaultError::AuthenticationFailed)
        ));
        let mut changed = bytes;
        changed.push(0);
        assert!(CredentialPackage::open(&changed, &password).is_err());
        assert!(
            CredentialPackage::open(&vec![0; MAX_CREDENTIAL_PACKAGE_BYTES + 1], &password).is_err()
        );
        Ok(())
    }
    #[test]
    fn package_rejects_mode_records_duplicates_and_limits_before_derivation() {
        let password = Secret::new(b"package-password".to_vec());
        let mut mode = record();
        mode.reference.kind = VaultCredentialKind::ProfileMode;
        assert!(CredentialPackage::seal(b"context", &[mode], &password).is_err());
        assert!(CredentialPackage::seal(b"context", &[record(), record()], &password).is_err());
        assert!(
            CredentialPackage::seal(
                &vec![0; MAX_CREDENTIAL_PACKAGE_CONTEXT + 1],
                &[record()],
                &password
            )
            .is_err()
        );
        let mut invalid = record();
        invalid.secret = Secret::new(vec![1; 4097]);
        assert!(CredentialPackage::seal(b"context", &[invalid], &password).is_err());
    }
}
