//! Key library primitives. Private material never belongs in public metadata.
use super::SshPrivateKey;
use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey, PublicKey};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PrivateKeyImportError {
    #[error("private-key format is invalid")]
    Format,
    #[error("selected file is a public key")]
    PublicKey,
    #[error("private-key passphrase is required")]
    PassphraseRequired,
    #[error("private-key decryption failed or encrypted data is damaged")]
    Decryption,
    #[error("private key is not encrypted")]
    NotEncrypted,
    #[error("private-key algorithm is unsupported")]
    Unsupported,
    #[error("private-key resource limit exceeded")]
    Limit,
    #[error("private-key consistency check failed")]
    Consistency,
    #[error("private-key decoder is busy")]
    Busy,
    #[error(transparent)]
    Legacy(#[from] crate::LegacyPemError),
}
impl PrivateKeyImportError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Format => "私钥文件格式无效或内容不完整，请检查选择的文件；文件后缀不影响导入",
            Self::PublicKey => {
                "选择的是公钥文件，请选择包含 PRIVATE KEY 或 PuTTY-User-Key-File 标记的私钥"
            }
            Self::PassphraseRequired => {
                "此私钥已加密，请填写私钥口令（不是凭据库主密码或服务器登录密码）"
            }
            Self::Decryption => "私钥解密失败：私钥口令不匹配，或加密文件已损坏",
            Self::NotEncrypted => "此私钥未加密，无需口令，请清空私钥口令后重试",
            Self::Unsupported => "私钥算法或加密格式暂不支持，请转换为 OpenSSH 格式后导入",
            Self::Limit => "私钥或解密参数超出资源上限，请检查密钥位数与 KDF 参数",
            Self::Consistency => "私钥一致性校验失败，文件可能已损坏或参数不匹配",
            Self::Busy => "密钥解析正在处理其他任务，请稍后重试",
            Self::Legacy(error) => error.message(),
        }
    }
}
pub(super) fn normalize(encoded: &str) -> Zeroizing<String> {
    let encoded = encoded.trim().trim_start_matches('\u{feff}').trim();
    let mut output = Zeroizing::new(String::with_capacity(encoded.len()));
    let mut chars = encoded.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            output.push('\n');
        } else {
            output.push(character);
        }
    }
    output
}

impl SshPrivateKey {
    pub fn validate_managed_import_cost(encoded: &str) -> Result<(), PrivateKeyImportError> {
        use russh::keys::ssh_key::Kdf;
        let normalized = normalize(encoded);
        let encoded = normalized.as_str();
        if encoded.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----") {
            let key =
                PrivateKey::from_openssh(encoded).map_err(|_| PrivateKeyImportError::Format)?;
            match key.kdf() {
                Kdf::None => {}
                Kdf::Bcrypt { salt, rounds } if (1..=128).contains(rounds) && salt.len() <= 64 => {}
                _ => {
                    return Err(PrivateKeyImportError::Limit);
                }
            }
        }
        Ok(())
    }
    pub fn decode_managed_private_key(
        encoded: &str,
        passphrase: Option<&str>,
    ) -> Result<Self, PrivateKeyImportError> {
        use crate::{PpkError, RsaKeyError, SshError};
        use russh::keys::Error as KeyError;
        if encoded.len() > 64 * 1024 || passphrase.is_some_and(|p| p.len() > 4096) {
            return Err(PrivateKeyImportError::Limit);
        }
        let normalized = normalize(encoded);
        let encoded = normalized.as_str();
        if encoded.starts_with("ssh-")
            || encoded.starts_with("ecdsa-")
            || encoded.starts_with("---- BEGIN SSH2 PUBLIC KEY ----")
            || encoded.contains("-----BEGIN PUBLIC KEY-----")
        {
            return Err(PrivateKeyImportError::PublicKey);
        }
        if !encoded.starts_with("PuTTY-User-Key-File-")
            && !encoded.lines().next().is_some_and(|line| {
                line.starts_with("-----BEGIN ") && line.ends_with("PRIVATE KEY-----")
            })
        {
            return Err(PrivateKeyImportError::Format);
        }
        Self::validate_managed_import_cost(encoded)?;
        let encrypted = if encoded.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----") {
            PrivateKey::from_openssh(encoded)
                .map_err(|_| PrivateKeyImportError::Format)?
                .is_encrypted()
        } else {
            encoded.contains("Proc-Type: 4,ENCRYPTED")
                || encoded.starts_with("-----BEGIN ENCRYPTED PRIVATE KEY-----")
                || encoded.contains("Encryption: aes256-cbc")
        };
        let phrase = passphrase.filter(|p| !p.is_empty());
        if encrypted && phrase.is_none() {
            return Err(PrivateKeyImportError::PassphraseRequired);
        }
        // PPK has its own strict header parser and typed passphrase errors.
        if !encrypted && phrase.is_some() && !encoded.starts_with("PuTTY-User-Key-File-") {
            return Err(PrivateKeyImportError::NotEncrypted);
        }
        Self::decode_private_key(encoded, phrase).map_err(|error| match error {
            SshError::LegacyPem(error) => PrivateKeyImportError::Legacy(error),
            SshError::Ppk(PpkError::PassphraseRequired)
            | SshError::PrivateKey(KeyError::KeyIsEncrypted) => {
                PrivateKeyImportError::PassphraseRequired
            }
            SshError::Ppk(PpkError::UnexpectedPassphrase) => PrivateKeyImportError::NotEncrypted,
            SshError::Ppk(PpkError::Algorithm | PpkError::Version)
            | SshError::PrivateKey(
                KeyError::UnsupportedKeyType { .. } | KeyError::UnknownAlgorithm(_),
            ) => PrivateKeyImportError::Unsupported,
            SshError::Ppk(PpkError::Limit | PpkError::RsaParameters)
            | SshError::RsaKey(RsaKeyError::Parameters) => PrivateKeyImportError::Limit,
            SshError::RsaKey(RsaKeyError::ModulusBits) => {
                PrivateKeyImportError::Legacy(crate::LegacyPemError::ModulusBits)
            }
            SshError::RsaKey(RsaKeyError::PublicExponent) => {
                PrivateKeyImportError::Legacy(crate::LegacyPemError::PublicExponent)
            }
            SshError::Ppk(PpkError::Busy) | SshError::RsaKey(RsaKeyError::Busy) => {
                PrivateKeyImportError::Busy
            }
            SshError::Ppk(PpkError::KeyConsistency)
            | SshError::RsaKey(_)
            | SshError::PrivateKey(KeyError::KeyIsCorrupt) => PrivateKeyImportError::Consistency,
            _ if encrypted => PrivateKeyImportError::Decryption,
            _ => PrivateKeyImportError::Format,
        })
    }
    pub fn generate_ed25519() -> Result<Self, String> {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .map_err(|_| "无法生成密钥，请重试".to_owned())?;
        Ok(Self {
            key: std::sync::Arc::new(key),
            rsa: None,
        })
    }

    pub fn public_openssh(&self) -> Result<String, String> {
        self.key
            .public_key()
            .to_openssh()
            .map_err(|_| "无法读取公钥".into())
    }

    pub fn algorithm_name(&self) -> String {
        self.key.algorithm().to_string()
    }

    pub fn matches_public_openssh(&self, public: &str) -> bool {
        PublicKey::from_openssh(public.trim())
            .is_ok_and(|key| key.key_data() == self.key.public_key().key_data())
    }

    pub fn encode_managed_openssh(
        &self,
        passphrase: Option<&str>,
    ) -> Result<Zeroizing<String>, String> {
        let encoded = match passphrase {
            Some(phrase) if !phrase.is_empty() => self
                .key
                .encrypt(&mut rand::rng(), phrase)
                .map_err(|_| "无法加密私钥".to_owned())?
                .to_openssh(LineEnding::LF),
            _ => self.key.to_openssh(LineEnding::LF),
        };
        encoded.map_err(|_| "无法编码私钥".into())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    // The PEM decoder and RSA signer deliberately reject excess concurrent work.
    // Keep functional assertions independent of the test runner's scheduling.
    static RSA_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());
    #[test]
    fn legacy_pem_and_openssh_import_classify_errors_and_normalize_text() {
        let _guard = RSA_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        use crate::LegacyPemError;
        let private = include_str!("../tests/fixtures/legacy_rsa_des3");
        let aes = include_str!("../tests/fixtures/legacy_rsa_aes128");
        let public = include_str!("../tests/fixtures/legacy_rsa_des3.pub");
        for input in [private, aes] {
            let with_bom_and_crlf = format!("\u{feff}{}\r\n", input.replace('\n', "\r\n"));
            let key = SshPrivateKey::decode_managed_private_key(
                &with_bom_and_crlf,
                Some("cshell-fixture-only"),
            )
            .unwrap();
            assert!(key.matches_public_openssh(public));
            assert_eq!(
                SshPrivateKey::decode_managed_private_key(input, None).unwrap_err(),
                PrivateKeyImportError::PassphraseRequired
            );
            assert_eq!(
                SshPrivateKey::decode_managed_private_key(input, Some("wrong")).unwrap_err(),
                PrivateKeyImportError::Legacy(LegacyPemError::Decryption)
            );
        }
        let unsupported = private.replace("DES-EDE3-CBC", "DES-CBC");
        assert_eq!(
            SshPrivateKey::decode_managed_private_key(&unsupported, Some("cshell-fixture-only"))
                .unwrap_err(),
            PrivateKeyImportError::Legacy(LegacyPemError::UnsupportedCipher)
        );
        let truncated = private.replace("-----END RSA PRIVATE KEY-----", "");
        assert_eq!(
            SshPrivateKey::decode_managed_private_key(&truncated, Some("cshell-fixture-only"))
                .unwrap_err(),
            PrivateKeyImportError::Legacy(LegacyPemError::Format)
        );
        assert_eq!(
            SshPrivateKey::decode_managed_private_key(public, None).unwrap_err(),
            PrivateKeyImportError::PublicKey
        );
        assert_eq!(
            SshPrivateKey::decode_managed_private_key("not a private key", Some("phrase"))
                .unwrap_err(),
            PrivateKeyImportError::Format
        );
        let generated = SshPrivateKey::generate_ed25519().unwrap();
        let phrase = " 密码 passphrase \t ";
        let encrypted = generated.encode_managed_openssh(Some(phrase)).unwrap();
        let normalized = format!("\u{feff}{}", encrypted.replace('\n', "\r\n"));
        assert_eq!(
            SshPrivateKey::decode_managed_private_key(&normalized, Some(phrase))
                .unwrap()
                .sha256_fingerprint(),
            generated.sha256_fingerprint()
        );
        assert!(
            SshPrivateKey::decode_managed_private_key(&normalized, Some(phrase.trim())).is_err()
        );
        let plain = generated.encode_managed_openssh(None).unwrap();
        assert_eq!(
            SshPrivateKey::decode_managed_private_key(&plain, Some("unneeded")).unwrap_err(),
            PrivateKeyImportError::NotEncrypted
        );
    }
    #[test]
    fn legacy_rsa4096_e35_has_a_specific_exponent_error_instead_of_a_size_error() {
        let _guard = RSA_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let private = include_str!("../tests/fixtures/legacy_rsa4096_e35_des3");
        let error = SshPrivateKey::decode_managed_private_key(private, Some("cshell-fixture-only"))
            .unwrap_err();
        assert_eq!(
            error,
            PrivateKeyImportError::Legacy(crate::LegacyPemError::PublicExponent)
        );
        assert!(error.message().contains("公有指数"));
        assert!(error.message().contains("4096 位长度本身受支持"));
    }
    #[test]
    fn rsa4096_traditional_des3_import_and_openssh_export_preserve_identity() {
        let _guard = RSA_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let private = include_str!("../tests/fixtures/legacy_rsa4096_des3");
        let public = include_str!("../tests/fixtures/legacy_rsa4096_des3.pub");
        let key = SshPrivateKey::decode_managed_private_key(private, Some("cshell-fixture-only"))
            .unwrap();
        assert!(key.matches_public_openssh(public));
        let exported = key
            .encode_managed_openssh(Some("cshell-fixture-only"))
            .unwrap();
        let restored =
            SshPrivateKey::decode_managed_private_key(&exported, Some("cshell-fixture-only"))
                .unwrap();
        assert_eq!(key.sha256_fingerprint(), restored.sha256_fingerprint());
    }
    #[test]
    fn traditional_rsa_des3_accepts_correct_passphrase_without_a_filename_extension() {
        let _guard = RSA_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let private = include_str!("../tests/fixtures/legacy_rsa_des3");
        let public = include_str!("../tests/fixtures/legacy_rsa_des3.pub");
        let key = SshPrivateKey::decode_private_key(private, Some("cshell-fixture-only")).unwrap();
        assert!(key.matches_public_openssh(public));
        assert!(SshPrivateKey::decode_private_key(private, Some("wrong")).is_err());
    }
    #[test]
    fn managed_pair_round_trip_with_optional_passphrase_and_pair_validation() {
        let key = SshPrivateKey::generate_ed25519().unwrap();
        let public = key.public_openssh().unwrap();
        assert!(key.matches_public_openssh(&public));
        assert!(
            !key.matches_public_openssh(
                &SshPrivateKey::generate_ed25519()
                    .unwrap()
                    .public_openssh()
                    .unwrap()
            )
        );
        for phrase in [None, Some("test-key-password")] {
            let private = key.encode_managed_openssh(phrase).unwrap();
            SshPrivateKey::validate_managed_import_cost(&private).unwrap();
            SshPrivateKey::validate_managed_import_cost(&format!("{}\n", private.as_str()))
                .unwrap();
            let imported = SshPrivateKey::decode_private_key(&private, phrase).unwrap();
            assert_eq!(imported.sha256_fingerprint(), key.sha256_fingerprint());
            if phrase.is_some() {
                assert!(SshPrivateKey::decode_private_key(&private, None).is_err());
                assert!(SshPrivateKey::decode_private_key(&private, Some("wrong")).is_err());
            }
        }
    }
    #[test]
    fn excessive_openssh_kdf_rounds_are_rejected_without_decrypting() {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let encrypted = key.encrypt(&mut rand::rng(), "test-password").unwrap();
        let mut bytes = encrypted.to_bytes().unwrap();
        let mut cursor = b"openssh-key-v1\0".len();
        // Skip cipher and KDF name, then patch the rounds inside KDF options.
        for _ in 0..2 {
            let len = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4 + len;
        }
        let options_len =
            u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        let rounds = cursor + 4 + options_len - 4;
        bytes[rounds..rounds + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        let hostile = PrivateKey::from_bytes(&bytes)
            .unwrap()
            .to_openssh(LineEnding::LF)
            .unwrap();
        assert!(SshPrivateKey::validate_managed_import_cost(&hostile).is_err());
    }
}
