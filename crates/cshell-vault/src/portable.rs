mod package;
use crate::Secret;
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{AeadInOut, KeyInit},
};
use argon2::{Algorithm, Argon2, Block, Params, Version};
pub use package::{
    CredentialPackage, CredentialPackageRecord, MAX_CREDENTIAL_PACKAGE_BYTES,
    MAX_CREDENTIAL_PACKAGE_CONTEXT, MAX_CREDENTIAL_PACKAGE_RECORDS,
};
use rand::{TryRng, rngs::SysRng};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use zeroize::Zeroizing;

const MAGIC: &[u8; 8] = b"CSHVLT01";
const FORMAT_VERSION: u32 = 1;
const MEMORY_KIB: u32 = 65_536;
const PASSES: u32 = 3;
const LANES: u32 = 4;
const MANIFEST_CHECK: &[u8] = b"CSHELL-VAULT-CHECK-v1";
const TAG_BYTES: usize = 16;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_RECORDS: usize = 128;
const MAX_BINDING_BYTES: usize = 4096;
const MAX_KEY_BYTES: usize = 64 * 1024;
const MAX_RECORD_BYTES: usize = 8 + MAX_BINDING_BYTES + MAX_KEY_BYTES + TAG_BYTES;
type Keys = Zeroizing<[u8; 64]>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[repr(u8)]
pub enum VaultCredentialKind {
    Password = 1,
    KeyPassphrase = 2,
    PrivateKey = 3,
    ProfileMode = 4,
    ManagedKey = 5,
    VaultSettings = 6,
}

impl VaultCredentialKind {
    fn maximum_bytes(self) -> usize {
        if matches!(self, Self::PrivateKey | Self::ManagedKey) {
            MAX_KEY_BYTES
        } else {
            4096
        }
    }
}

/// Profile identity and purpose are both authenticated; this contains no secret.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct VaultCredentialRef {
    pub profile_id: [u8; 16],
    pub kind: VaultCredentialKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VaultStatus {
    pub unlocked: bool,
    pub reload_required: bool,
    pub revision: u64,
    pub record_count: usize,
}

/// Stable diagnostics never include paths, passwords, bindings or backend text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum VaultError {
    #[error("old system key cleanup is incomplete; protection may already have changed")]
    RootCleanupRequired,
    #[error("system Vault root key is missing")]
    RootKeyMissing,
    #[error("system credential store is unavailable or access was denied")]
    RootKeyUnavailable,
    #[error("Vault is locked")]
    Locked,
    #[error("Vault must be reloaded before use")]
    ReloadRequired,
    #[error("Vault is already in use")]
    Busy,
    #[error("Vault already exists")]
    AlreadyExists,
    #[error("Vault credential does not exist")]
    Missing,
    #[error("Vault credential binding does not match")]
    BindingMismatch,
    #[error("invalid Vault input")]
    InvalidInput,
    #[error("unsupported Vault format or KDF parameters")]
    Unsupported,
    #[error("invalid Vault file")]
    Corrupt,
    #[error("Vault password or authentication check failed")]
    AuthenticationFailed,
    #[error("Vault capacity exceeded")]
    Capacity,
    #[error("Vault random source unavailable")]
    EntropyUnavailable,
    #[error("Vault key derivation failed")]
    DerivationFailed,
    #[error("Vault I/O failed ({0:?})")]
    Io(std::io::ErrorKind),
    #[error("Vault write outcome is uncertain; reload before continuing")]
    WriteUncertain,
}
impl From<std::io::Error> for VaultError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.kind())
    }
}

#[derive(Clone)]
struct Header {
    id: [u8; 16],
    salt: [u8; 16],
    revision: u64,
}
#[derive(Clone)]
struct SealedRecord {
    nonce: [u8; 12],
    ciphertext: Vec<u8>,
}
#[derive(Clone)]
struct Document {
    header: Header,
    records: BTreeMap<VaultCredentialRef, SealedRecord>,
    manifest: SealedRecord,
    authenticated_prefix: Vec<u8>,
}

/// Exclusive, blocking storage backend. Only derived keys remain in unlocked memory;
/// individual plaintext records exist for the duration of a read/write operation.
/// The caller must use a protected daemon data directory and drop returned Secrets.
pub struct PortableVault {
    path: PathBuf,
    _ownership: File,
    document: Document,
    keys: Option<Keys>,
    reload_required: bool,
}

impl std::fmt::Debug for PortableVault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PortableVault")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl PortableVault {
    /// Creates a new encrypted file without replacing existing content; starts unlocked.
    pub fn create(path: impl AsRef<Path>, master_password: &Secret) -> Result<Self, VaultError> {
        validate_password(master_password, true)?;
        let path = path.as_ref().to_owned();
        let ownership = acquire(&path)?;
        if path.try_exists()? {
            return Err(VaultError::AlreadyExists);
        }
        let header = Header {
            id: random()?,
            salt: random()?,
            revision: 0,
        };
        let keys = derive(master_password, &header.salt)?;
        let document = Document {
            header,
            records: BTreeMap::new(),
            manifest: SealedRecord {
                nonce: [0; 12],
                ciphertext: Vec::new(),
            },
            authenticated_prefix: Vec::new(),
        };
        let encoded = encode(&document, &keys)?;
        let document = decode(&encoded)?;
        persist(&path, &encoded, true)?;
        Ok(Self {
            path,
            _ownership: ownership,
            document,
            keys: Some(keys),
            reload_required: false,
        })
    }

    /// Opens a bounded file while holding its advisory lock. Always starts locked.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, VaultError> {
        let path = path.as_ref().to_owned();
        let ownership = acquire(&path)?;
        let document = load(&path)?;
        Ok(Self {
            path,
            _ownership: ownership,
            document,
            keys: None,
            reload_required: false,
        })
    }

    #[must_use]
    pub fn status(&self) -> VaultStatus {
        VaultStatus {
            unlocked: self.keys.is_some(),
            reload_required: self.reload_required,
            revision: self.document.header.revision,
            record_count: self.document.records.len(),
        }
    }

    pub fn lock(&mut self) {
        self.keys = None;
    }

    /// Even a failed unlock of an already unlocked Vault discards its previous keys.
    pub fn unlock(&mut self, master_password: &Secret) -> Result<(), VaultError> {
        self.lock();
        if self.reload_required {
            return Err(VaultError::ReloadRequired);
        }
        validate_password(master_password, false)?;
        let keys = derive(master_password, &self.document.header.salt)?;
        let check = decrypt(
            &keys[32..],
            &self.document.manifest,
            &self.document.authenticated_prefix,
        )?;
        if check.as_slice() != MANIFEST_CHECK {
            return Err(VaultError::AuthenticationFailed);
        }
        for (reference, record) in &self.document.records {
            let plaintext = decrypt(
                &keys[..32],
                record,
                &record_aad(&self.document.header, *reference),
            )?;
            validate_plaintext(&plaintext, reference.kind)?;
        }
        self.keys = Some(keys);
        Ok(())
    }

    /// An explicit reload always locks first, including on I/O or parse failure.
    pub fn reload(&mut self) -> Result<(), VaultError> {
        self.lock();
        self.reload_required = true;
        let document = load(&self.path)?;
        self.document = document;
        self.reload_required = false;
        Ok(())
    }

    pub fn read(&self, reference: VaultCredentialRef, binding: &str) -> Result<Secret, VaultError> {
        let keys = self.active_keys()?;
        validate_binding(reference, binding)?;
        let record = self
            .document
            .records
            .get(&reference)
            .ok_or(VaultError::Missing)?;
        let plaintext = decrypt(
            &keys[..32],
            record,
            &record_aad(&self.document.header, reference),
        )?;
        let (stored_binding, secret) = validate_plaintext(&plaintext, reference.kind)?;
        if stored_binding != binding.as_bytes() {
            return Err(VaultError::BindingMismatch);
        }
        Ok(Secret::new(secret.to_vec()))
    }

    pub fn contains(&self, reference: VaultCredentialRef) -> Result<bool, VaultError> {
        self.active_keys()?;
        Ok(self.document.records.contains_key(&reference))
    }

    pub fn references(&self, kind: VaultCredentialKind) -> Result<Vec<VaultCredentialRef>, VaultError> {
        self.active_keys()?;
        Ok(self.document.records.keys().filter(|r| r.kind == kind).copied().collect())
    }

    pub fn write(
        &mut self,
        reference: VaultCredentialRef,
        binding: &str,
        secret: &Secret,
    ) -> Result<(), VaultError> {
        self.write_batch(&[(reference, binding, secret)])
    }

    pub fn write_batch(
        &mut self,
        entries: &[(VaultCredentialRef, &str, &Secret)],
    ) -> Result<(), VaultError> {
        self.update_batch(entries, &[])
    }
    /// Inserts and removes related credentials in one authenticated file replacement.
    pub fn update_batch(
        &mut self,
        entries: &[(VaultCredentialRef, &str, &Secret)],
        removals: &[VaultCredentialRef],
    ) -> Result<(), VaultError> {
        let keys = self.active_keys()?;
        if entries.len() + removals.len() == 0 || entries.len() + removals.len() > MAX_RECORDS {
            return Err(VaultError::InvalidInput);
        }
        let mut candidate = self.document.clone();
        candidate.header.revision = next_revision(&candidate.header)?;
        let mut seen = std::collections::BTreeSet::new();
        for reference in removals {
            if reference.profile_id == [0; 16] || !seen.insert(*reference) {
                return Err(VaultError::InvalidInput);
            }
            candidate.records.remove(reference);
        }
        for &(reference, binding, secret) in entries {
            validate_binding(reference, binding)?;
            if !seen.insert(reference)
                || secret.expose().is_empty()
                || secret.expose().len() > reference.kind.maximum_bytes()
            {
                return Err(VaultError::InvalidInput);
            }
            let mut plaintext = Zeroizing::new(Vec::with_capacity(
                8 + binding.len() + secret.expose().len() + TAG_BYTES,
            ));
            push_length(&mut plaintext, binding.len())?;
            plaintext.extend_from_slice(binding.as_bytes());
            push_length(&mut plaintext, secret.expose().len())?;
            plaintext.extend_from_slice(secret.expose());
            let record = encrypt(
                &keys[..32],
                plaintext,
                &record_aad(&candidate.header, reference),
                random()?,
            )?;
            candidate.records.insert(reference, record);
        }
        if candidate.records.len() > MAX_RECORDS {
            return Err(VaultError::Capacity);
        }
        self.commit(candidate, None)
    }

    /// Missing records are an idempotent no-op; a locked Vault still rejects deletion.
    pub fn delete(&mut self, reference: VaultCredentialRef) -> Result<(), VaultError> {
        self.active_keys()?;
        if !self.document.records.contains_key(&reference) {
            return Ok(());
        }
        let mut candidate = self.document.clone();
        candidate.header.revision = next_revision(&candidate.header)?;
        candidate.records.remove(&reference);
        self.commit(candidate, None)
    }

    /// Checks the current password without changing unlocked state or writing data.
    pub fn verify_password(&self, password: &Secret) -> Result<(), VaultError> {
        validate_password(password, false)?;
        let keys=derive(password, &self.document.header.salt)?;
        let check=decrypt(&keys[32..],&self.document.manifest,&self.document.authenticated_prefix)?;
        if check.as_slice()!=MANIFEST_CHECK {return Err(VaultError::AuthenticationFailed);}
        Ok(())
    }

    /// Copies every authenticated record into a newly protected, exclusive file.
    /// The source remains unchanged; plaintext never leaves this storage layer.
    pub fn copy_protected(&self, path: impl AsRef<Path>, password:&Secret) -> Result<Self,VaultError> {
        let old_keys=self.active_keys()?;
        let mut target=Self::create(path,password)?;
        let mut candidate=target.document.clone();
        candidate.header.revision=next_revision(&self.document.header)?;
        let new_keys=target.active_keys()?;
        for (reference,record) in &self.document.records {
            let plain=decrypt(&old_keys[..32],record,&record_aad(&self.document.header,*reference))?;
            validate_plaintext(&plain,reference.kind)?;
            candidate.records.insert(*reference,encrypt(&new_keys[..32],plain,&record_aad(&candidate.header,*reference),random()?)?);
        }
        target.commit(candidate,None)?;
        target.reload()?;target.unlock(password)?;
        Ok(target)
    }

    /// Re-encrypts all records with a new salt/key in a single file replacement.
    pub fn change_master_password(&mut self, new_password: &Secret) -> Result<(), VaultError> {
        let old_keys = self.active_keys()?;
        validate_password(new_password, true)?;
        let mut candidate = self.document.clone();
        candidate.header.salt = random()?;
        candidate.header.revision = next_revision(&candidate.header)?;
        let new_keys = derive(new_password, &candidate.header.salt)?;
        for (reference, record) in &mut candidate.records {
            let aad = record_aad(&candidate.header, *reference);
            let plaintext = decrypt(&old_keys[..32], record, &aad)?;
            validate_plaintext(&plaintext, reference.kind)?;
            // Decryption reserves tag space, so encryption cannot reallocate plaintext.
            *record = encrypt(&new_keys[..32], plaintext, &aad, random()?)?;
        }
        self.commit(candidate, Some(new_keys))
    }

    fn active_keys(&self) -> Result<&Keys, VaultError> {
        if self.reload_required {
            return Err(VaultError::ReloadRequired);
        }
        self.keys.as_ref().ok_or(VaultError::Locked)
    }

    fn commit(&mut self, candidate: Document, new_keys: Option<Keys>) -> Result<(), VaultError> {
        self.commit_with(candidate, new_keys, |path, encoded| {
            persist(path, encoded, false)
        })
    }

    fn commit_with(
        &mut self,
        candidate: Document,
        new_keys: Option<Keys>,
        writer: impl FnOnce(&Path, &[u8]) -> Result<(), VaultError>,
    ) -> Result<(), VaultError> {
        let keys = match &new_keys {
            Some(keys) => keys,
            None => self.active_keys()?,
        };
        let encoded = encode(&candidate, keys)?;
        let committed = decode(&encoded)?;
        if let Err(error) = writer(&self.path, &encoded) {
            if error == VaultError::WriteUncertain {
                self.lock();
                self.reload_required = true;
            }
            return Err(error);
        }
        self.document = committed;
        if let Some(keys) = new_keys {
            self.keys = Some(keys);
        }
        Ok(())
    }
}

fn random<const N: usize>() -> Result<[u8; N], VaultError> {
    let mut output = [0; N];
    SysRng
        .try_fill_bytes(&mut output)
        .map_err(|_| VaultError::EntropyUnavailable)?;
    Ok(output)
}

fn validate_password(password: &Secret, creating: bool) -> Result<(), VaultError> {
    let minimum = if creating { 8 } else { 1 };
    if !(minimum..=1024).contains(&password.expose().len()) {
        return Err(VaultError::InvalidInput);
    }
    Ok(())
}
fn validate_binding(reference: VaultCredentialRef, binding: &str) -> Result<(), VaultError> {
    if reference.profile_id == [0; 16]
        || binding.is_empty()
        || binding.len() > MAX_BINDING_BYTES
        || binding.contains('\0')
    {
        return Err(VaultError::InvalidInput);
    }
    Ok(())
}
fn next_revision(header: &Header) -> Result<u64, VaultError> {
    header.revision.checked_add(1).ok_or(VaultError::Capacity)
}

fn derive(password: &Secret, salt: &[u8; 16]) -> Result<Keys, VaultError> {
    let params = Params::new(MEMORY_KIB, PASSES, LANES, Some(64))
        .map_err(|_| VaultError::DerivationFailed)?;
    // Argon2's alloc helper does not wipe its backing blocks; own that memory explicitly.
    let mut blocks = Zeroizing::new(Vec::new());
    blocks
        .try_reserve_exact(params.block_count())
        .map_err(|_| VaultError::Capacity)?;
    blocks.resize(params.block_count(), Block::default());
    let mut keys = Zeroizing::new([0; 64]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(password.expose(), salt, &mut *keys, &mut *blocks)
        .map_err(|_| VaultError::DerivationFailed)?;
    Ok(keys)
}

fn cipher(key: &[u8]) -> Result<Aes256Gcm, VaultError> {
    Aes256Gcm::new_from_slice(key).map_err(|_| VaultError::DerivationFailed)
}
fn encrypt(
    key: &[u8],
    mut plaintext: Zeroizing<Vec<u8>>,
    aad: &[u8],
    nonce: [u8; 12],
) -> Result<SealedRecord, VaultError> {
    if plaintext.capacity() - plaintext.len() < TAG_BYTES {
        return Err(VaultError::Capacity);
    }
    cipher(key)?
        .encrypt_in_place(&Nonce::from(nonce), aad, &mut *plaintext)
        .map_err(|_| VaultError::AuthenticationFailed)?;
    Ok(SealedRecord {
        nonce,
        ciphertext: std::mem::take(&mut *plaintext),
    })
}
fn decrypt(
    key: &[u8],
    record: &SealedRecord,
    aad: &[u8],
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let mut plaintext = Zeroizing::new(Vec::with_capacity(record.ciphertext.len()));
    plaintext.extend_from_slice(&record.ciphertext);
    cipher(key)?
        .decrypt_in_place(&Nonce::from(record.nonce), aad, &mut *plaintext)
        .map_err(|_| VaultError::AuthenticationFailed)?;
    Ok(plaintext)
}
fn record_aad(header: &Header, reference: VaultCredentialRef) -> Vec<u8> {
    let mut aad = Vec::with_capacity(45);
    aad.extend_from_slice(MAGIC);
    aad.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    aad.extend_from_slice(&header.id);
    aad.extend_from_slice(&reference.profile_id);
    aad.push(reference.kind as u8);
    aad
}

fn validate_plaintext(
    plaintext: &[u8],
    kind: VaultCredentialKind,
) -> Result<(&[u8], &[u8]), VaultError> {
    let mut reader = Reader::new(plaintext);
    let binding = reader.part(MAX_BINDING_BYTES)?;
    let secret = reader.part(kind.maximum_bytes())?;
    if binding.is_empty()
        || binding.contains(&0)
        || std::str::from_utf8(binding).is_err()
        || secret.is_empty()
        || !reader.finished()
    {
        return Err(VaultError::Corrupt);
    }
    Ok((binding, secret))
}

fn push_length(output: &mut Vec<u8>, length: usize) -> Result<(), VaultError> {
    output.extend_from_slice(
        &u32::try_from(length)
            .map_err(|_| VaultError::Capacity)?
            .to_be_bytes(),
    );
    Ok(())
}

fn encode(document: &Document, keys: &Keys) -> Result<Vec<u8>, VaultError> {
    let estimated = 72
        + document
            .records
            .values()
            .map(|record| 33 + record.ciphertext.len())
            .sum::<usize>()
        + 12
        + MANIFEST_CHECK.len()
        + TAG_BYTES;
    if document.records.len() > MAX_RECORDS || estimated > MAX_FILE_BYTES {
        return Err(VaultError::Capacity);
    }
    let mut output = Vec::with_capacity(estimated);
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    output.extend_from_slice(&document.header.id);
    output.extend_from_slice(&document.header.salt);
    for value in [MEMORY_KIB, PASSES, LANES] {
        output.extend_from_slice(&value.to_be_bytes());
    }
    output.extend_from_slice(&document.header.revision.to_be_bytes());
    push_length(&mut output, document.records.len())?;
    for (reference, record) in &document.records {
        output.extend_from_slice(&reference.profile_id);
        output.push(reference.kind as u8);
        output.extend_from_slice(&record.nonce);
        push_length(&mut output, record.ciphertext.len())?;
        output.extend_from_slice(&record.ciphertext);
    }
    let mut check = Zeroizing::new(Vec::with_capacity(MANIFEST_CHECK.len() + TAG_BYTES));
    check.extend_from_slice(MANIFEST_CHECK);
    let manifest = encrypt(&keys[32..], check, &output, random()?)?;
    output.extend_from_slice(&manifest.nonce);
    output.extend_from_slice(&manifest.ciphertext);
    Ok(output)
}

fn decode(encoded: &[u8]) -> Result<Document, VaultError> {
    if encoded.len() > MAX_FILE_BYTES {
        return Err(VaultError::Capacity);
    }
    let mut reader = Reader::new(encoded);
    if reader.take(MAGIC.len())? != MAGIC {
        return Err(VaultError::Corrupt);
    }
    if reader.u32()? != FORMAT_VERSION {
        return Err(VaultError::Unsupported);
    }
    let id = reader.array()?;
    let salt = reader.array()?;
    if [reader.u32()?, reader.u32()?, reader.u32()?] != [MEMORY_KIB, PASSES, LANES] {
        return Err(VaultError::Unsupported);
    }
    let revision = u64::from_be_bytes(reader.array()?);
    let count = reader.u32()? as usize;
    if count > MAX_RECORDS {
        return Err(VaultError::Capacity);
    }
    let mut records = BTreeMap::new();
    for _ in 0..count {
        let profile_id = reader.array()?;
        if profile_id == [0; 16] {
            return Err(VaultError::Corrupt);
        }
        let kind = match reader.take(1)?[0] {
            1 => VaultCredentialKind::Password,
            2 => VaultCredentialKind::KeyPassphrase,
            3 => VaultCredentialKind::PrivateKey,
            4 => VaultCredentialKind::ProfileMode,
            5 => VaultCredentialKind::ManagedKey,
            6 => VaultCredentialKind::VaultSettings,
            _ => return Err(VaultError::Corrupt),
        };
        let nonce = reader.array()?;
        let ciphertext = reader.part(MAX_RECORD_BYTES)?;
        if ciphertext.len() < TAG_BYTES {
            return Err(VaultError::Corrupt);
        }
        let record = SealedRecord {
            nonce,
            ciphertext: ciphertext.to_vec(),
        };
        if records
            .insert(VaultCredentialRef { profile_id, kind }, record)
            .is_some()
        {
            return Err(VaultError::Corrupt);
        }
    }
    let authenticated_prefix = encoded[..reader.cursor].to_vec();
    let nonce = reader.array()?;
    let ciphertext = reader.take(MANIFEST_CHECK.len() + TAG_BYTES)?.to_vec();
    if !reader.finished() {
        return Err(VaultError::Corrupt);
    }
    Ok(Document {
        header: Header { id, salt, revision },
        records,
        manifest: SealedRecord { nonce, ciphertext },
        authenticated_prefix,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }
    fn take(&mut self, length: usize) -> Result<&'a [u8], VaultError> {
        let end = self.cursor.checked_add(length).ok_or(VaultError::Corrupt)?;
        let part = self
            .bytes
            .get(self.cursor..end)
            .ok_or(VaultError::Corrupt)?;
        self.cursor = end;
        Ok(part)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], VaultError> {
        self.take(N)?.try_into().map_err(|_| VaultError::Corrupt)
    }
    fn u32(&mut self) -> Result<u32, VaultError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn part(&mut self, maximum: usize) -> Result<&'a [u8], VaultError> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err(VaultError::Capacity);
        }
        self.take(length)
    }
    fn finished(&self) -> bool {
        self.cursor == self.bytes.len()
    }
}

fn acquire(path: &Path) -> Result<File, VaultError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(VaultError::InvalidInput);
    }
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(PathBuf::from(name))?;
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => VaultError::Busy,
        TryLockError::Error(error) => VaultError::from(error),
    })?;
    Ok(file)
}
fn load(path: &Path) -> Result<Document, VaultError> {
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_FILE_BYTES as u64 {
        return Err(VaultError::Capacity);
    }
    let mut encoded = Zeroizing::new(Vec::with_capacity(MAX_FILE_BYTES + 1));
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut encoded)?;
    decode(&encoded)
}
fn persist(path: &Path, encoded: &[u8], creating: bool) -> Result<(), VaultError> {
    let parent = path.parent().ok_or(VaultError::InvalidInput)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(encoded)?;
    temporary.as_file().sync_all()?;
    publish_with(
        temporary,
        creating,
        cfg!(windows),
        |file| {
            if creating {
                file.persist_noclobber(path)
            } else {
                file.persist(path)
            }
        },
        std::thread::sleep,
    )?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| VaultError::WriteUncertain)?;
    Ok(())
}

// Windows can briefly prevent replacement (OS codes 5/32/33).
// Retry only the same owned, synced ciphertext file before publication; never rebuild
// a transaction, change permissions, remove the destination or retry uncertain writes.
fn publish_with(
    mut temporary: tempfile::NamedTempFile,
    creating: bool,
    windows: bool,
    mut publish: impl FnMut(tempfile::NamedTempFile) -> Result<File, tempfile::PersistError>,
    mut wait: impl FnMut(std::time::Duration),
) -> Result<(), VaultError> {
    let mut attempts = 0;
    loop {
        match publish(temporary) {
            Ok(_) => return Ok(()),
            Err(error) => {
                if windows && !creating && matches!(error.error.raw_os_error(), Some(5 | 32 | 33)) {
                    if !error.file.path().try_exists().unwrap_or(false) {
                        return Err(VaultError::WriteUncertain);
                    }
                    if attempts < 7 {
                        temporary = error.file;
                        wait(std::time::Duration::from_millis(1 << attempts));
                        attempts += 1;
                        continue;
                    }
                }
                return Err(
                    if creating && error.error.kind() == std::io::ErrorKind::AlreadyExists {
                        VaultError::AlreadyExists
                    } else {
                        error.error.into()
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
