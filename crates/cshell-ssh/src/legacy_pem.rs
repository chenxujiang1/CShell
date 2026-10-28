//! Traditional RSA PEM import only. Legacy file encryption is never selected
//! for SSH transport or for newly stored/exported OpenSSH private keys.
use crate::{RsaKeyError, rsa_signer};
use pkcs8::der::{Decode, Reader, SliceReader, asn1::UintRef};
use russh::keys::ssh_key::{
    Mpint, PrivateKey,
    private::{KeypairData, RsaKeypair, RsaPrivateKey},
    public::RsaPublicKey,
};
use ssh_encoding::base64::{Base64, Encoding};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LegacyPemError {
    #[error("traditional RSA PEM envelope is malformed")]
    Format,
    #[error("traditional PEM encryption is unsupported")]
    UnsupportedCipher,
    #[error("encrypted private key requires its private-key passphrase")]
    PassphraseRequired,
    #[error("private-key decryption failed or encrypted data is damaged")]
    Decryption,
    #[error("private key exceeds the import resource limits")]
    Limit,
    #[error("RSA modulus must have 2048–4096 bits")]
    ModulusBits,
    #[error("RSA public exponent is outside the current signing backend range")]
    PublicExponent,
    #[error("RSA private component exceeds supported bounds")]
    ComponentLimit,
    #[error("RSA key parameters or consistency checks failed")]
    Consistency,
    #[error("private-key decoder is busy")]
    Busy,
}
impl LegacyPemError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Format => "传统 RSA PEM 文件格式不完整或已损坏，请检查私钥文件",
            Self::UnsupportedCipher => "此私钥的文件加密算法暂不支持；支持 DES-EDE3-CBC 与 AES CBC",
            Self::PassphraseRequired => {
                "此私钥已加密，请填写私钥口令（不是凭据库主密码或服务器登录密码）"
            }
            Self::Decryption => "私钥解密失败：私钥口令不匹配，或加密文件已损坏",
            Self::Limit => "私钥文件超过 64 KiB 或口令超过 4096 字节的资源上限",
            Self::ModulusBits => "RSA 模数位数不在支持范围（2048～4096 位）；这与文件加密头无关",
            Self::PublicExponent => {
                "RSA 公有指数不受当前签名后端支持（要求 65537～8589934591 的奇数）；4096 位长度本身受支持，修改文件头或转换格式不能解决此限制"
            }
            Self::ComponentLimit => "RSA 私钥组件超出支持范围；这不一定是模数位数超过 4096",
            Self::Consistency => "RSA 私钥参数或公私钥一致性检查失败，请检查文件是否完整",
            Self::Busy => "密钥解析正在处理其他任务，请稍后重试",
        }
    }
}
fn rsa_error(error: RsaKeyError) -> LegacyPemError {
    match error {
        RsaKeyError::Busy => LegacyPemError::Busy,
        RsaKeyError::ModulusBits => LegacyPemError::ModulusBits,
        RsaKeyError::PublicExponent => LegacyPemError::PublicExponent,
        RsaKeyError::Parameters => LegacyPemError::ComponentLimit,
        _ => LegacyPemError::Consistency,
    }
}
fn hex(text: &str) -> Result<Vec<u8>, LegacyPemError> {
    if !matches!(text.len(), 16 | 32) {
        return Err(LegacyPemError::Format);
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = char::from(pair[0])
                .to_digit(16)
                .ok_or(LegacyPemError::Format)?;
            let low = char::from(pair[1])
                .to_digit(16)
                .ok_or(LegacyPemError::Format)?;
            Ok(((high << 4) | low) as u8)
        })
        .collect()
}
fn derive(phrase: &str, iv: &[u8], length: usize) -> Zeroizing<Vec<u8>> {
    // OpenSSL's historical EVP_BytesToKey(MD5), required by Proc-Type PEM.
    // The header IV is supplied separately; only its first 8 bytes form the salt.
    let mut output = Zeroizing::new(Vec::with_capacity(32));
    let mut previous = Zeroizing::new(Vec::new());
    while output.len() < length {
        let mut input = Zeroizing::new(Vec::with_capacity(previous.len() + phrase.len() + 8));
        input.extend_from_slice(&previous);
        input.extend_from_slice(phrase.as_bytes());
        input.extend_from_slice(&iv[..8]);
        let digest = Zeroizing::new(md5::compute(&input).0);
        previous.clear();
        previous.extend_from_slice(&*digest);
        output.extend_from_slice(&*digest);
    }
    use zeroize::Zeroize;
    output[length..].zeroize();
    output.truncate(length);
    output
}
fn pkcs1(bytes: &[u8]) -> Result<PrivateKey, LegacyPemError> {
    let mut reader = SliceReader::new(bytes).map_err(|_| LegacyPemError::Format)?;
    let (version, values) = reader
        .sequence(|r| -> pkcs8::der::Result<_> {
            let version = u8::decode(r)?;
            let values = (0..8)
                .map(|_| UintRef::decode(r))
                .collect::<pkcs8::der::Result<Vec<_>>>()?;
            Ok((version, values))
        })
        .map_err(|_| LegacyPemError::Format)?;
    reader.finish().map_err(|_| LegacyPemError::Format)?;
    if version != 0
        || values.iter().any(|v| {
            v.as_bytes().is_empty()
                || v.as_bytes().len() > 512
                || v.as_bytes().iter().all(|b| *b == 0)
        })
    {
        return Err(LegacyPemError::ComponentLimit);
    }
    let component = |index: usize| Mpint::from_positive_bytes(values[index].as_bytes());
    let public =
        RsaPublicKey::new(component(1), component(0)).map_err(|_| LegacyPemError::Consistency)?;
    let private = RsaPrivateKey::new(component(2), component(7), component(3), component(4))
        .map_err(|_| LegacyPemError::Consistency)?;
    let pair = RsaKeypair::new(public, private).map_err(|_| LegacyPemError::Consistency)?;
    let key =
        PrivateKey::new(KeypairData::Rsa(pair), "").map_err(|_| LegacyPemError::Consistency)?;
    rsa_signer::validate_public(key.public_key()).map_err(rsa_error)?;
    let dp =
        rsa_signer::crt_exponent(values[2].as_bytes(), values[3].as_bytes()).map_err(rsa_error)?;
    let dq =
        rsa_signer::crt_exponent(values[2].as_bytes(), values[4].as_bytes()).map_err(rsa_error)?;
    if &dp[..] != values[5].as_bytes() || &dq[..] != values[6].as_bytes() {
        return Err(LegacyPemError::Consistency);
    }
    Ok(key)
}

pub(super) fn decode(encoded: &str, phrase: Option<&str>) -> Result<PrivateKey, LegacyPemError> {
    if encoded.len() > 64 * 1024 || phrase.is_some_and(|p| p.len() > 4096) {
        return Err(LegacyPemError::Limit);
    }
    static BUDGET: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let _permit = BUDGET
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| LegacyPemError::Busy)?;
    let mut lines = encoded.lines();
    if lines.next() != Some("-----BEGIN RSA PRIVATE KEY-----") {
        return Err(LegacyPemError::Format);
    }
    let mut proc_type = false;
    let mut encryption = None;
    let mut body = Zeroizing::new(String::new());
    let mut end = false;
    for line in lines.by_ref() {
        let line = line.trim();
        if line == "-----END RSA PRIVATE KEY-----" {
            end = true;
            break;
        }
        if line.is_empty() {
            continue;
        }
        if line == "Proc-Type: 4,ENCRYPTED" && !proc_type && body.is_empty() {
            proc_type = true;
            continue;
        }
        if let Some(dek) = line.strip_prefix("DEK-Info: ") {
            if encryption.is_some() || !body.is_empty() {
                return Err(LegacyPemError::Format);
            }
            let (name, iv) = dek.split_once(',').ok_or(LegacyPemError::Format)?;
            let cipher = match name {
                "DES-EDE3-CBC" => ssh_cipher::Cipher::TdesCbc,
                "AES-128-CBC" => ssh_cipher::Cipher::Aes128Cbc,
                "AES-192-CBC" => ssh_cipher::Cipher::Aes192Cbc,
                "AES-256-CBC" => ssh_cipher::Cipher::Aes256Cbc,
                _ => return Err(LegacyPemError::UnsupportedCipher),
            };
            encryption = Some((cipher, hex(iv)?));
            continue;
        }
        if !line
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        {
            return Err(LegacyPemError::Format);
        }
        body.push_str(line);
    }
    if !end
        || lines.any(|line| !line.trim().is_empty())
        || body.is_empty()
        || proc_type != encryption.is_some()
    {
        return Err(LegacyPemError::Format);
    }
    let mut bytes = Zeroizing::new(Base64::decode_vec(&body).map_err(|_| LegacyPemError::Format)?);
    let encrypted = encryption.is_some();
    if let Some((cipher, iv)) = encryption {
        let phrase = phrase
            .filter(|p| !p.is_empty())
            .ok_or(LegacyPemError::PassphraseRequired)?;
        let (key_length, iv_length) = cipher
            .key_and_iv_size()
            .ok_or(LegacyPemError::UnsupportedCipher)?;
        if iv.len() != iv_length || bytes.is_empty() || bytes.len() % cipher.block_size() != 0 {
            return Err(LegacyPemError::Format);
        }
        let key = derive(phrase, &iv, key_length);
        cipher
            .decrypt(&key, &iv, &mut bytes, None)
            .map_err(|_| LegacyPemError::Decryption)?;
        let padding = usize::from(*bytes.last().ok_or(LegacyPemError::Decryption)?);
        if padding == 0
            || padding > cipher.block_size()
            || bytes[bytes.len() - padding..]
                .iter()
                .any(|b| usize::from(*b) != padding)
        {
            return Err(LegacyPemError::Decryption);
        }
        // Do not leave decrypted padding in a truncated Vec's spare capacity.
        let length = bytes.len() - padding;
        use zeroize::Zeroize;
        bytes[length..].zeroize();
        bytes.truncate(length);
    }
    pkcs1(&bytes).map_err(|error| {
        if encrypted && error == LegacyPemError::Format {
            LegacyPemError::Decryption
        } else {
            error
        }
    })
}
