//! RSA-only PPK format glue. KDF, AES, MAC and private operations are delegated
//! to pinned libraries; no RSA feature is enabled on the upstream SSH decoder.
use super::{Header, PpkError, blob, field};
use crate::rsa_signer::{RsaKeyError, RsaSigningKey, validate_public};
use hmac::{Hmac, Mac, digest::KeyInit};
use russh::keys::ssh_key::{
    Mpint, PrivateKey, PublicKey,
    private::{KeypairData, RsaKeypair, RsaPrivateKey},
};
use sha2::Digest;
use ssh_encoding::base64::{Base64, Encoding};
use std::sync::Arc;
use zeroize::Zeroizing;

pub(super) fn error(error: RsaKeyError) -> PpkError {
    match error {
        RsaKeyError::Busy => PpkError::Busy,
        RsaKeyError::Parameters | RsaKeyError::ModulusBits | RsaKeyError::PublicExponent => PpkError::RsaParameters,
        _ => PpkError::KeyConsistency,
    }
}
fn binary(lines: &[&str]) -> Result<Zeroizing<Vec<u8>>, PpkError> {
    let mut encoded = Zeroizing::new(String::new());
    for line in lines {
        encoded.push_str(line);
    }
    Base64::decode_vec(&encoded)
        .map(Zeroizing::new)
        .map_err(|_| PpkError::Format)
}
fn hex(text: &str) -> Result<Zeroizing<Vec<u8>>, PpkError> {
    if text.len() % 2 != 0 {
        return Err(PpkError::Format);
    }
    let mut result = Zeroizing::new(Vec::with_capacity(text.len() / 2));
    for pair in text.as_bytes().chunks_exact(2) {
        let high = char::from(pair[0]).to_digit(16).ok_or(PpkError::Format)?;
        let low = char::from(pair[1]).to_digit(16).ok_or(PpkError::Format)?;
        result.push(((high << 4) | low) as u8);
    }
    Ok(result)
}
fn integer(cursor: &mut &[u8]) -> Result<Zeroizing<Mpint>, PpkError> {
    let prefix: [u8; 4] = cursor
        .get(..4)
        .ok_or(PpkError::Format)?
        .try_into()
        .map_err(|_| PpkError::Format)?;
    let length = u32::from_be_bytes(prefix) as usize;
    // 4096 positive bits plus the optional sign byte, before allocation.
    if length == 0 || length > 513 {
        return Err(PpkError::RsaParameters);
    }
    let bytes = cursor.get(4..4 + length).ok_or(PpkError::Format)?;
    let value = Zeroizing::new(Mpint::from_bytes(bytes).map_err(|_| PpkError::Format)?);
    if value
        .as_positive_bytes()
        .is_none_or(|v| v.is_empty() || v.len() > 512)
    {
        return Err(PpkError::RsaParameters);
    }
    *cursor = &cursor[4 + length..];
    Ok(value)
}
fn take(value: &mut Zeroizing<Mpint>) -> Mpint {
    std::mem::replace(&mut **value, Mpint::from_positive_bytes(&[]))
}

pub(super) fn decode(
    encoded: &str,
    header: &Header,
    passphrase: Option<&str>,
) -> Result<(PrivateKey, Arc<RsaSigningKey>), PpkError> {
    // Parent preflight has already enforced the exact order, counts and budget.
    let lines = encoded.lines().collect::<Vec<_>>();
    let mut position = 1;
    let encryption = field(&lines, &mut position, "Encryption")?;
    let comment = field(&lines, &mut position, "Comment")?;
    let start = position + 1;
    blob(&lines, &mut position, "Public-Lines", 8192)?;
    let public_blob = binary(&lines[start..position])?;
    let public = PublicKey::from_bytes(&public_blob).map_err(|_| PpkError::Format)?;
    validate_public(&public).map_err(error)?;
    // Re-encoding excludes acceptance of trailing or noncanonical public fields.
    if public.to_bytes().map_err(|_| PpkError::Format)? != *public_blob {
        return Err(PpkError::Format);
    }
    let salt = if header.kdf.is_some() {
        for name in [
            "Key-Derivation",
            "Argon2-Memory",
            "Argon2-Passes",
            "Argon2-Parallelism",
        ] {
            field(&lines, &mut position, name)?;
        }
        hex(field(&lines, &mut position, "Argon2-Salt")?)?
    } else {
        Zeroizing::new(Vec::new())
    };
    let start = position + 1;
    blob(&lines, &mut position, "Private-Lines", 32768)?;
    let mut private_blob = binary(&lines[start..position])?;
    let expected_mac = hex(field(&lines, &mut position, "Private-MAC")?)?;
    if header.encrypted && (private_blob.is_empty() || private_blob.len() % 16 != 0) {
        return Err(PpkError::Format);
    }
    let phrase = if header.encrypted {
        passphrase.ok_or(PpkError::PassphraseRequired)?
    } else {
        ""
    };
    let mut cipher_key = Zeroizing::new([0u8; 32]);
    let mut iv = Zeroizing::new([0u8; 16]);
    let mac_key = if header.version == 2 {
        if header.encrypted {
            for index in 0u32..2 {
                let mut digest = sha1::Sha1::new();
                digest.update(index.to_be_bytes());
                digest.update(phrase.as_bytes());
                let mut hash = digest.finalize();
                if index == 0 {
                    cipher_key[..20].copy_from_slice(&hash);
                } else {
                    cipher_key[20..].copy_from_slice(&hash[..12]);
                }
                use zeroize::Zeroize;
                hash.as_mut_slice().zeroize();
            }
        }
        let mut digest = sha1::Sha1::new();
        digest.update(b"putty-private-key-file-mac-key");
        digest.update(phrase.as_bytes());
        let mut hash = digest.finalize();
        let result = Zeroizing::new(hash.to_vec());
        use zeroize::Zeroize;
        hash.as_mut_slice().zeroize();
        result
    } else if let Some(kdf) = &header.kdf {
        let algorithm = match kdf.algorithm.as_str() {
            "Argon2d" => argon2::Algorithm::Argon2d,
            "Argon2i" => argon2::Algorithm::Argon2i,
            "Argon2id" => argon2::Algorithm::Argon2id,
            _ => return Err(PpkError::Format),
        };
        let params = argon2::Params::new(kdf.memory_kib, kdf.passes, kdf.parallelism, Some(80))
            .map_err(|_| PpkError::Format)?;
        let mut material = Zeroizing::new([0u8; 80]);
        argon2::Argon2::new(algorithm, argon2::Version::V0x13, params)
            .hash_password_into(phrase.as_bytes(), &salt, &mut *material)
            .map_err(|_| PpkError::Authentication)?;
        cipher_key.copy_from_slice(&material[..32]);
        iv.copy_from_slice(&material[32..48]);
        Zeroizing::new(material[48..].to_vec())
    } else {
        Zeroizing::new(Vec::new())
    };
    if header.encrypted {
        ssh_cipher::Cipher::Aes256Cbc
            .decrypt(&*cipher_key, &*iv, &mut private_blob, None)
            .map_err(|_| PpkError::Authentication)?;
    }
    let parts: [&[u8]; 5] = [
        header.algorithm.as_bytes(),
        encryption.as_bytes(),
        comment.as_bytes(),
        &public_blob,
        &private_blob,
    ];
    // Stream the five SSH strings, including random private padding. Library
    // verify_slice performs the tag comparison; no custom MAC comparison.
    if header.version == 2 {
        let mut mac =
            Hmac::<sha1::Sha1>::new_from_slice(&mac_key).map_err(|_| PpkError::Authentication)?;
        for part in parts {
            mac.update(&(part.len() as u32).to_be_bytes());
            mac.update(part);
        }
        mac.verify_slice(&expected_mac)
            .map_err(|_| PpkError::Authentication)?;
    } else {
        let mut mac =
            Hmac::<sha2::Sha256>::new_from_slice(&mac_key).map_err(|_| PpkError::Authentication)?;
        for part in parts {
            mac.update(&(part.len() as u32).to_be_bytes());
            mac.update(part);
        }
        mac.verify_slice(&expected_mac)
            .map_err(|_| PpkError::Authentication)?;
    }
    let mut cursor = private_blob.as_slice();
    let mut d = integer(&mut cursor)?;
    let mut p = integer(&mut cursor)?;
    let mut q = integer(&mut cursor)?;
    let mut q_inv = integer(&mut cursor)?;
    if (!header.encrypted && !cursor.is_empty()) || (header.encrypted && cursor.len() >= 16) {
        return Err(PpkError::Format);
    }
    let public = public.key_data().rsa().ok_or(PpkError::Algorithm)?.clone();
    let private = RsaPrivateKey::new(take(&mut d), take(&mut q_inv), take(&mut p), take(&mut q))
        .map_err(|_| PpkError::KeyConsistency)?;
    let pair = RsaKeypair::new(public, private).map_err(|_| PpkError::KeyConsistency)?;
    // Comments participate in the MAC but are not copied into the returned key.
    let key = PrivateKey::new(KeypairData::Rsa(pair), "").map_err(|_| PpkError::KeyConsistency)?;
    let signing = RsaSigningKey::from_key(&key).map_err(error)?;
    Ok((key, signing))
}
