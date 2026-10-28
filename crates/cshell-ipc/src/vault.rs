use prost::{Enumeration, Message};
use zeroize::Zeroize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Enumeration)]
#[repr(i32)]
pub enum VaultOperation {
    Status = 0,
    Create = 1,
    Unlock = 2,
    Lock = 3,
    ChangePassword = 4,
    SetMode = 5,
    ImportPrivateKey = 6,
    DeletePrivateKey = 7,
    Reload = 8,
    CreateSystem = 9,
    UnlockSystem = 10,
    ExportPackage = 11,
    PreviewPackage = 12,
    CommitPackage = 13,
    KeyboardAuthPoll = 14,
    KeyboardAuthRespond = 15,
    GenerateManagedKey = 16,
    ImportManagedKey = 17,
    ExportManagedKey = 18,
    DeleteManagedKey = 19,
    RebuildPassword = 20,
    RebuildSystem = 21,
    ProtectWithSystem = 22,
    ProtectWithPassword = 23,
    FinishProtectionCleanup = 24,
    LockTransient = 25,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Enumeration)]
#[repr(i32)]
pub enum VaultControlStatus {
    Ok = 0,
    Invalid = 1,
    Unavailable = 2,
    Locked = 3,
    Conflict = 4,
    AuthenticationFailed = 5,
    ReloadRequired = 6,
    Busy = 7,
    Unsupported = 8,
    RootKeyMissing = 9,
    KeychainUnavailable = 10,
}

/// Native desktop availability, independent of whether the encrypted Vault exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Enumeration)]
#[repr(i32)]
pub enum VaultSessionState {
    Unsupported = 0,
    Active = 1,
    Locked = 2,
    Disconnected = 3,
    Suspended = 4,
    Unavailable = 5,
}

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct VaultRequest {
    #[prost(enumeration = "VaultOperation", tag = "1")]
    pub operation: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub secret: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub profile_id: Vec<u8>,
    #[prost(uint64, tag = "4")]
    pub expected_profile_revision: u64,
    #[prost(uint64, tag = "5")]
    pub expected_vault_revision: u64,
    #[prost(bool, tag = "6")]
    pub portable: bool,
    #[prost(uint64, tag = "7")]
    pub expected_lock_generation: u64,
    #[prost(bytes = "vec", tag = "8")]
    pub package: Vec<u8>,
    #[prost(enumeration = "crate::ProfileImportPolicy", tag = "9")]
    pub package_policy: i32,
    #[prost(bytes = "vec", repeated, tag = "10")]
    pub selected_profiles: Vec<Vec<u8>>,
    #[prost(bool, tag = "11")]
    pub include_private_keys: bool,
    #[prost(bytes = "vec", tag = "12")]
    pub preview_token: Vec<u8>,
    #[prost(message, optional, tag = "13")]
    pub auth_answer: Option<KeyboardAuthAnswer>,
    #[prost(message, optional, tag = "14")]
    pub managed_key: Option<ManagedKeyRequest>,
    #[prost(bool, tag = "15")]
    pub confirm_rebuild: bool,
}
impl Drop for VaultRequest {
    fn drop(&mut self) {
        self.secret.zeroize();
        self.preview_token.zeroize();
    }
}
impl VaultRequest {
    pub fn new(operation: VaultOperation) -> Self {
        let mut request = Self::default();
        request.operation = operation as i32;
        request
    }
    pub fn for_profile(mut self, profile_id: Vec<u8>) -> Self {
        self.profile_id = profile_id;
        self
    }
}
impl std::fmt::Debug for VaultRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultRequest")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rebuilding_requires_an_explicit_wire_confirmation_and_redacts_new_master_password() {
        let mut request = VaultRequest::new(VaultOperation::RebuildPassword);
        assert!(!request.confirm_rebuild);
        request.confirm_rebuild = true;
        request.secret = b"PRIVATE-NEW-MASTER".to_vec();
        request.expected_lock_generation = 45;
        let envelope = crate::Envelope { payload: Some(crate::envelope::Payload::VaultRequest(request)), ..Default::default() };
        assert_eq!(crate::Envelope::decode(envelope.encode_to_vec().as_slice()).unwrap(),envelope);
        assert!(!format!("{envelope:?}").contains("PRIVATE"));
    }
    #[test]
    fn managed_key_request_round_trips_without_logging_private_input() {
        let mut request = VaultRequest::new(VaultOperation::GenerateManagedKey);
        request.secret = b"PRIVATE-KEY-PASSPHRASE".to_vec();
        request.managed_key = Some(ManagedKeyRequest { name: "test".into(), algorithm: "ed25519".into(), ..Default::default() });
        request.expected_vault_revision = 7;
        request.expected_lock_generation = 19;
        let envelope = crate::Envelope { payload: Some(crate::envelope::Payload::VaultRequest(request)), ..Default::default() };
        assert_eq!(crate::Envelope::decode(envelope.encode_to_vec().as_slice()).unwrap(), envelope);
        assert!(!format!("{envelope:?}").contains("PRIVATE"));
    }
    #[test]
    fn vault_envelopes_round_trip_and_redact_secrets() -> Result<(), prost::DecodeError> {
        let mut request = VaultRequest::new(VaultOperation::Unlock);
        request.secret = b"PRIVATE-MASTER-PASSWORD".to_vec();
        request.expected_lock_generation = 17;
        let envelope = crate::Envelope {
            payload: Some(crate::envelope::Payload::VaultRequest(request)),
            ..Default::default()
        };
        assert_eq!(
            crate::Envelope::decode(envelope.encode_to_vec().as_slice())?,
            envelope
        );
        let debug = format!("{envelope:?}");
        assert!(!debug.contains("PRIVATE"));
        assert!(!debug.contains("80, 82"));
        let response = crate::Envelope {
            payload: Some(crate::envelope::Payload::VaultResponse(VaultResponse {
                package: b"PRIVATE-CIPHERTEXT".to_vec(),
                package_preview: Some(CredentialPackagePreview {
                    token: b"PRIVATE-TOKEN".to_vec(),
                    items: vec![CredentialPackageItem {
                        name: "PRIVATE-PEER".into(),
                        target: "PRIVATE-TARGET".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(
            crate::Envelope::decode(response.encode_to_vec().as_slice())?,
            response
        );
        let debug = format!("{response:?}");
        assert!(!debug.contains("PRIVATE"));
        assert!(!debug.contains("80, 82"));
        Ok(())
    }
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct VaultResponse {
    #[prost(enumeration = "VaultControlStatus", tag = "1")]
    pub status: i32,
    #[prost(bool, tag = "2")]
    pub exists: bool,
    #[prost(bool, tag = "3")]
    pub unlocked: bool,
    #[prost(uint64, tag = "4")]
    pub revision: u64,
    #[prost(bytes = "vec", tag = "5")]
    pub profile_id: Vec<u8>,
    #[prost(bool, tag = "6")]
    pub portable: bool,
    #[prost(bool, tag = "7")]
    pub has_password: bool,
    #[prost(bool, tag = "8")]
    pub has_key_passphrase: bool,
    #[prost(bool, tag = "9")]
    pub has_private_key: bool,
    #[prost(uint64, tag = "10")]
    pub lock_generation: u64,
    #[prost(bool, tag = "11")]
    pub system: bool,
    #[prost(bytes = "vec", tag = "12")]
    pub package: Vec<u8>,
    #[prost(message, optional, tag = "13")]
    pub package_preview: Option<CredentialPackagePreview>,
    #[prost(enumeration = "VaultSessionState", tag = "14")]
    pub session_state: i32,
    #[prost(message, optional, tag = "15")]
    pub auth_challenge: Option<KeyboardAuthChallenge>,
    #[prost(string,tag="16")]
    pub auth_notice: String,
    #[prost(uint64,tag="17")]
    pub auth_catalog_revision: u64,
    #[prost(message, repeated, tag = "18")]
    pub managed_keys: Vec<ManagedKeyInfo>,
    #[prost(string, tag = "19")]
    pub key_notice: String,
    #[prost(string, tag = "20")]
    pub storage_path: String,
    #[prost(bool, tag = "21")]
    pub manually_locked: bool,
    #[prost(bool, tag = "22")]
    pub protection_cleanup_pending: bool,
}

/// File paths and public metadata only; passphrases use VaultRequest.secret.
#[derive(Clone, PartialEq, Message)]
pub struct ManagedKeyRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub algorithm: String,
    #[prost(string, tag = "4")]
    pub private_path: String,
    #[prost(string, tag = "5")]
    pub public_path: String,
    #[prost(string, tag = "6")]
    pub export_directory: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ManagedKeyInfo {
    #[prost(bytes = "vec", tag = "1")]
    pub id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub algorithm: String,
    #[prost(string, tag = "4")]
    pub fingerprint: String,
    #[prost(string, tag = "5")]
    pub public_key: String,
    #[prost(bool, tag = "6")]
    pub encrypted: bool,
}

impl std::fmt::Debug for VaultResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultResponse")
            .field("status", &self.status)
            .field("revision", &self.revision)
            .field("unlocked", &self.unlocked)
            .finish_non_exhaustive()
    }
}

pub const MAX_CREDENTIAL_PACKAGE_BYTES: usize = 1024 * 1024;
pub const MAX_CREDENTIAL_PACKAGE_PROFILES: usize = 32;

#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct CredentialPackagePreview {
    #[prost(bytes = "vec", tag = "1")]
    pub token: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub profile_revision: u64,
    #[prost(uint64, tag = "3")]
    pub vault_revision: u64,
    #[prost(uint64, tag = "4")]
    pub lock_generation: u64,
    #[prost(bool, tag = "5")]
    pub can_commit: bool,
    #[prost(message, repeated, tag = "6")]
    pub items: Vec<CredentialPackageItem>,
}
impl Drop for CredentialPackagePreview {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}
impl std::fmt::Debug for CredentialPackagePreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialPackagePreview")
            .field("item_count", &self.items.len())
            .field("can_commit", &self.can_commit)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct CredentialPackageItem {
    #[prost(bytes = "vec", tag = "1")]
    pub profile_id: Vec<u8>,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub target: String,
    #[prost(uint32, repeated, tag = "4")]
    pub kinds: Vec<u32>,
    #[prost(enumeration = "crate::ProfileImportAction", tag = "5")]
    pub action: i32,
    #[prost(string, tag = "6")]
    pub reason: String,
}
impl std::fmt::Debug for CredentialPackageItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialPackageItem")
            .field("action", &self.action)
            .finish_non_exhaustive()
    }
}

/// Ephemeral SSH server prompts. Never persisted with profiles or session output.
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct KeyboardAuthChallenge {
    #[prost(bytes="vec", tag="1")]
    pub id: Vec<u8>,
    #[prost(string, tag="2")]
    pub target: String,
    #[prost(string, tag="3")]
    pub name: String,
    #[prost(string, tag="4")]
    pub instructions: String,
    #[prost(message, repeated, tag="5")]
    pub prompts: Vec<KeyboardAuthPrompt>,
    #[prost(bool,tag="6")]
    pub login: bool,
    #[prost(string,tag="7")]
    pub username: String,
    #[prost(bool,tag="8")]
    pub username_editable: bool,
    #[prost(bool,tag="9")]
    pub password_required: bool,
    #[prost(bool,tag="10")]
    pub rejected: bool,
    #[prost(bool,tag="11")]
    pub save_credentials: bool,
    #[prost(bool,tag="12")]
    pub vault_unlock: bool,
}
#[derive(Clone, PartialEq, Message)]
pub struct KeyboardAuthPrompt {
    #[prost(string, tag="1")]
    pub text: String,
    #[prost(bool, tag="2")]
    pub echo: bool,
}
#[derive(Clone, PartialEq, Message)]
#[prost(skip_debug)]
pub struct KeyboardAuthAnswer {
    #[prost(bytes="vec", tag="1")]
    pub id: Vec<u8>,
    #[prost(string, repeated, tag="2")]
    pub answers: Vec<String>,
    #[prost(bool, tag="3")]
    pub cancel: bool,
    #[prost(string,tag="4")]
    pub username: String,
    #[prost(bool,tag="5")]
    pub save_credentials: bool,
}
impl Drop for KeyboardAuthAnswer { fn drop(&mut self){ self.answers.zeroize(); } }
impl std::fmt::Debug for KeyboardAuthAnswer {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("KeyboardAuthAnswer([REDACTED])")}
}
impl std::fmt::Debug for KeyboardAuthChallenge {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("KeyboardAuthChallenge([REDACTED])")}
}

#[cfg(test)]
mod keyboard_auth_tests {
    use super::*;
    #[test]
    fn keyboard_answers_round_trip_and_debug_redacts_all_secrets(){
        let mut request=VaultRequest::new(VaultOperation::KeyboardAuthRespond);
        request.auth_answer=Some(KeyboardAuthAnswer{id:vec![9;16],answers:vec!["PRIVATE-OTP-654321".into(),"PRIVATE-PASSWORD".into()],cancel:false,username:String::new(),save_credentials:false});
        let envelope=crate::Envelope{payload:Some(crate::envelope::Payload::VaultRequest(request)),..Default::default()};
        assert_eq!(crate::Envelope::decode(envelope.encode_to_vec().as_slice()).unwrap(),envelope);
        let debug=format!("{envelope:?}");assert!(!debug.contains("PRIVATE"));assert!(!debug.contains("654321"));
    }
}
